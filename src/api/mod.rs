pub mod models;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use models::*;

const API: &str = "https://api.music.yandex.net";
const ROTOR_API: &str = "https://api.music.yandex.ru";
const CLIENT_HEADER: &str = "YandexMusicWebNext/1.0.0";
const UI_LANGUAGE: &str = "en";
/// The key Yandex signs file-link requests with.
const SIGN_KEY: &[u8] = b"7tvSmFbyf5hJnIHhCimDDD";
/// Track metadata is fetched in batches — a liked list can be long.
const META_CHUNK: usize = 250;
/// How many times to retry a request that came back 429 or 5xx.
const RETRIES: u32 = 4;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(400);

/// Sends a request, surviving temporary refusals.
///
/// Yandex hands out 429 readily if you poke it often, so we retry with a
/// growing pause — otherwise the liked list fails to load for no good reason.
async fn send_retrying(request: reqwest::RequestBuilder, what: &str) -> Result<reqwest::Response> {
    let mut delay = FIRST_RETRY_DELAY;
    let mut last: Option<anyhow::Error> = None;

    for attempt in 0..RETRIES {
        let attempt_request = request
            .try_clone()
            .ok_or_else(|| anyhow!("{what}: request cannot be retried"))?;

        match attempt_request.send().await {
            Ok(response) => {
                let status = response.status();
                let retriable =
                    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error();
                if !retriable {
                    return response
                        .error_for_status()
                        .with_context(|| what.to_string());
                }
                last = Some(anyhow!("{what}: {status}"));
            }
            Err(e) => last = Some(anyhow!("{what}: {e}")),
        }

        if attempt + 1 < RETRIES {
            tokio::time::sleep(delay).await;
            delay *= 3;
        }
    }

    Err(last.unwrap_or_else(|| anyhow!("{what}: gave up")))
}

/// Events the wave expects from a player so it can tune what it serves.
#[derive(Debug, Clone)]
pub enum Feedback {
    RadioStarted,
    TrackStarted { track_id: String },
    TrackFinished { track_id: String, played_secs: f64 },
    Skip { track_id: String, played_secs: f64 },
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    token: String,
    quality: String,
    codecs: String,
    pub uid: u64,
    pub display_name: String,
}

impl Client {
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            http: reqwest::Client::new(),
            token: String::new(),
            quality: "high".into(),
            codecs: "mp3".into(),
            uid: 0,
            display_name: "Test".into(),
        }
    }

    fn timestamp() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    }

    pub fn quality(&self) -> &str {
        &self.quality
    }

    pub fn set_quality(&mut self, quality: &str) {
        let high = matches!(quality, "high" | "mp3");
        self.quality = if high { "high" } else { "lossless" }.into();
        self.codecs = if high { "mp3" } else { "flac-mp4,mp3" }.into();
    }

    pub async fn new(token: &str, quality: &str, codecs: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("tuiya/0.1")
            .build()
            .context("cannot build the http client")?;

        let mut client = Client {
            http,
            token: token.to_string(),
            quality: quality.to_string(),
            codecs: codecs.to_string(),
            uid: 0,
            display_name: String::new(),
        };

        let status: Envelope<RawAccountStatus> = send_retrying(
            client.get("/account/status"),
            "Yandex rejected the token — check it in the config",
        )
        .await?
        .json()
        .await
        .context("unexpected /account/status response")?;

        client.uid = status.result.account.uid;
        client.display_name = status
            .result
            .account
            .display_name
            .unwrap_or_else(|| "?".to_string());

        Ok(client)
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .get(format!("{API}{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("OAuth {}", self.token),
            )
            .header("X-Yandex-Music-Client", CLIENT_HEADER)
    }

    /// Downloads from the CDN: the link carries its own signature, no token needed.
    pub fn http_get(&self, url: &str) -> reqwest::RequestBuilder {
        self.http.get(url)
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{API}{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("OAuth {}", self.token),
            )
            .header("X-Yandex-Music-Client", CLIENT_HEADER)
    }

    fn rotor_post(&self, path: &str) -> reqwest::RequestBuilder {
        self.http
            .post(format!("{ROTOR_API}{path}"))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("OAuth {}", self.token),
            )
            .header("X-Yandex-Music-Client", CLIENT_HEADER)
            .header(reqwest::header::ACCEPT_LANGUAGE, UI_LANGUAGE)
    }

    /// Start a fresh, non-persistent "My Wave" session with tuning seeds.
    pub async fn start_wave(&self, settings: Option<&WaveSettings>) -> Result<WaveBatch> {
        let seeds = settings
            .map(WaveSettings::seeds)
            .unwrap_or_else(|| vec!["user:onyourwave".into()]);
        let request = self
            .rotor_post("/rotor/session/new")
            .json(&serde_json::json!({
                "seeds": seeds,
                "includeTracksInResponse": true,
                "includeWaveModel": true,
                "interactive": true,
            }));

        self.parse_wave_response(request).await
    }

    /// Discover the presets advertised by the official Wave wheel.
    pub async fn wave_choices(&self, current: &WaveSettings) -> Result<WaveChoices> {
        let response: RawWheelResult = send_retrying(
            self.rotor_post("/wheel/new").json(&serde_json::json!({
                "context": {
                    "type": "WAVE",
                    "data": { "seeds": current.seeds },
                },
                "feedbacks": [],
            })),
            "the wave settings failed to load",
        )
        .await?
        .json()
        .await
        .context("unexpected wave-settings response")?;

        wave_choices(current, response)
    }

    /// Continue an existing Wave session.
    pub async fn wave_tracks(
        &self,
        session_id: &str,
        queue: &[String],
        feedbacks: &[Feedback],
    ) -> Result<WaveBatch> {
        let request = self
            .rotor_post(&format!("/rotor/session/{session_id}/tracks"))
            .json(&serde_json::json!({
                "queue": queue,
                "feedbacks": feedbacks
                    .iter()
                    .map(|feedback| feedback.body(Self::timestamp()))
                    .collect::<Vec<_>>(),
            }));

        self.parse_wave_response(request).await
    }

    async fn parse_wave_response(&self, request: reqwest::RequestBuilder) -> Result<WaveBatch> {
        let response: Envelope<RawWaveResult> =
            send_retrying(request, "the wave did not return tracks")
                .await?
                .json()
                .await
                .context("unexpected wave response")?;

        let result = response.result;
        // The `liked` flag in the station response is unreliable, so hearts
        // are derived from the liked list instead.
        let tracks = result
            .sequence
            .into_iter()
            .map(|item| Track::from(item.track))
            .collect();

        Ok(WaveBatch {
            tracks,
            batch_id: result.batch_id,
            session_id: result.session_id,
        })
    }

    /// Ids of liked tracks, most recent first.
    pub async fn liked_track_ids(&self) -> Result<Vec<String>> {
        let response: Envelope<RawLikesResult> = send_retrying(
            self.get(&format!("/users/{}/likes/tracks", self.uid)),
            "the liked list failed to load",
        )
        .await?
        .json()
        .await
        .context("unexpected liked-list response")?;

        Ok(response
            .result
            .library
            .tracks
            .into_iter()
            .map(|t| t.id)
            .collect())
    }

    /// Track metadata by id, keeping the order of the input.
    pub async fn tracks_meta(&self, ids: &[String]) -> Result<Vec<Track>> {
        let mut tracks = Vec::with_capacity(ids.len());

        for chunk in ids.chunks(META_CHUNK) {
            let response: Envelope<Vec<RawTrack>> = send_retrying(
                self.post("/tracks").form(&[("track-ids", chunk.join(","))]),
                "track metadata did not arrive",
            )
            .await?
            .json()
            .await
            .context("unexpected track-metadata response")?;

            tracks.extend(response.result.into_iter().map(Track::from));
        }

        Ok(tracks)
    }

    /// A direct link to the track's audio file.
    pub async fn download_info(&self, track_id: &str) -> Result<DownloadInfo> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let transports = "raw";

        // The signature is computed over a string with no commas between
        // codecs, even though the query sends them comma-separated.
        let mut mac = Hmac::<Sha256>::new_from_slice(SIGN_KEY)
            .map_err(|e| anyhow!("cannot initialise hmac: {e}"))?;
        mac.update(
            format!(
                "{ts}{track_id}{}{}{transports}",
                self.quality,
                self.codecs.replace(',', "")
            )
            .as_bytes(),
        );
        let signature =
            base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());
        // Yandex expects the signature without its last base64 character.
        let signature = &signature[..signature.len().saturating_sub(1)];

        let request = self.get("/get-file-info").query(&[
            ("ts", ts.to_string().as_str()),
            ("trackId", track_id),
            ("quality", &self.quality),
            ("codecs", &self.codecs),
            ("transports", transports),
            ("sign", signature),
        ]);

        let response: Envelope<RawFileInfoResult> = send_retrying(request, "no file link returned")
            .await?
            .json()
            .await
            .context("unexpected file-link response")?;

        Ok(DownloadInfo {
            url: response.result.download_info.url,
            codec: response.result.download_info.codec,
        })
    }

    pub async fn like(&self, track_id: &str) -> Result<()> {
        send_retrying(
            self.post(&format!("/users/{}/likes/tracks/add-multiple", self.uid))
                .form(&[("track-ids", track_id)]),
            "the like was not accepted",
        )
        .await?;
        Ok(())
    }

    pub async fn unlike(&self, track_id: &str) -> Result<()> {
        send_retrying(
            self.post(&format!("/users/{}/likes/tracks/remove", self.uid))
                .form(&[("track-ids", track_id)]),
            "removing the like was not accepted",
        )
        .await?;
        Ok(())
    }

    /// Record a playback in the account history. This is separate from Rotor
    /// feedback: `/plays` records listening history, while Rotor tunes Wave.
    pub async fn play(
        &self,
        track_id: &str,
        duration_secs: f64,
        played_secs: f64,
        change_reason: &str,
        radio_session_id: Option<&str>,
        batch_id: Option<&str>,
    ) -> Result<()> {
        let request = self.post("/plays").json(&serde_json::json!({
            "trackId": track_id,
            "from": "tuiya",
            "timestamp": Self::timestamp(),
            "duration": duration_secs,
            "position": played_secs,
            "totalPlayedSeconds": played_secs,
            "changeReason": change_reason,
            "radioSessionId": radio_session_id,
            "batchId": batch_id,
        }));
        send_retrying(request, "the playback could not be recorded").await?;
        Ok(())
    }

    /// Tell the wave what is happening to a track. Failures here are not
    /// fatal: playback continues, the station just serves worse picks.
    pub async fn wave_feedback(
        &self,
        session_id: &str,
        batch_id: Option<&str>,
        event: Feedback,
    ) -> Result<()> {
        let timestamp = Self::timestamp();

        let body = event.body(timestamp);
        let request = self
            .rotor_post(&format!("/rotor/session/{session_id}/feedback/"))
            .json(&serde_json::json!({
                "event": body,
                "batchId": batch_id,
                "from": "tuiya-my-wave",
            }));
        send_retrying(request, "the wave rejected the event").await?;
        Ok(())
    }

    /// Close a Wave session while switching to another one. The web client
    /// uses the collection endpoint for this final feedback instead of
    /// sending it to the old session's regular feedback URL.
    pub async fn close_wave_session(
        &self,
        session_id: &str,
        batch_id: Option<&str>,
        event: Feedback,
    ) -> Result<()> {
        let request = self
            .rotor_post("/rotor/sessions/feedbacks/")
            .json(&serde_json::json!({
                "feedbacks": [{
                    "sessionId": session_id,
                    "batchId": batch_id,
                    "event": event.body(Self::timestamp()),
                    "from": "tuiya-my-wave",
                }],
            }));
        send_retrying(request, "the old wave session could not be closed").await?;
        Ok(())
    }
}

impl Feedback {
    fn body(&self, timestamp: f64) -> serde_json::Value {
        match self {
            Feedback::RadioStarted => serde_json::json!({
                "type": "radioStarted",
                "timestamp": timestamp,
                "from": "tuiya",
            }),
            Feedback::TrackStarted { track_id } => serde_json::json!({
                "type": "trackStarted",
                "timestamp": timestamp,
                "trackId": track_id,
            }),
            Feedback::TrackFinished {
                track_id,
                played_secs,
            } => serde_json::json!({
                "type": "trackFinished",
                "timestamp": timestamp,
                "trackId": track_id,
                "totalPlayedSeconds": played_secs,
            }),
            Feedback::Skip {
                track_id,
                played_secs,
            } => serde_json::json!({
                "type": "skip",
                "timestamp": timestamp,
                "trackId": track_id,
                "totalPlayedSeconds": played_secs,
            }),
        }
    }
}

fn wave_choices(current: &WaveSettings, response: RawWheelResult) -> Result<WaveChoices> {
    let mut options = vec![current.clone()];
    for item in response.items {
        if item.item_type != "WAVE" {
            continue;
        }
        let Some(wave) = item.data.wave else {
            continue;
        };
        if wave.seeds.is_empty() || options.iter().any(|option| option.seeds == wave.seeds) {
            continue;
        }
        options.push(WaveSettings {
            name: wave.name,
            description: wave.description,
            seeds: wave.seeds,
        });
    }
    if options.len() == 1 {
        return Err(anyhow!("the wave wheel returned no presets"));
    }
    Ok(WaveChoices { options })
}

#[cfg(test)]
mod wheel_tests {
    use super::*;

    #[test]
    fn keeps_only_launchable_wave_items_and_preserves_seeds() {
        let response: RawWheelResult = serde_json::from_value(serde_json::json!({
            "items": [
                {
                    "type": "SETTING",
                    "data": { "title": "Customize My Vibe" }
                },
                {
                    "type": "WAVE",
                    "data": { "wave": {
                        "name": "Aggressive in English",
                        "description": "My Vibe",
                        "seeds": ["mood:aggressive", "local-language:english"]
                    }}
                }
            ]
        }))
        .unwrap();

        let choices = wave_choices(&WaveSettings::default(), response).unwrap();
        assert_eq!(choices.options.len(), 2);
        assert_eq!(
            choices.options[1].seeds,
            ["mood:aggressive", "local-language:english"]
        );
    }
}
