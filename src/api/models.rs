use std::time::Duration;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// A track as the player shows and plays it.
#[derive(Debug, Clone)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artists: String,
    pub duration: Duration,
    pub available: bool,
}

impl Track {
    pub fn label(&self) -> String {
        if self.artists.is_empty() {
            self.title.clone()
        } else {
            format!("{} — {}", self.artists, self.title)
        }
    }
}

impl From<RawTrack> for Track {
    fn from(raw: RawTrack) -> Self {
        let artists = raw
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(" & ");
        Track {
            id: raw.id,
            title: raw.title.unwrap_or_else(|| "Untitled".to_string()),
            artists,
            duration: Duration::from_millis(raw.duration_ms.unwrap_or(0)),
            available: raw.available.unwrap_or(true),
        }
    }
}

/// Accept ids as both strings and numbers — the API is inconsistent.
fn flexible_id<'de, D: Deserializer<'de>>(de: D) -> Result<String, D::Error> {
    match Value::deserialize(de)? {
        Value::String(s) => Ok(s),
        Value::Number(n) => Ok(n.to_string()),
        other => Err(serde::de::Error::custom(format!(
            "expected a track id, got {other}"
        ))),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawArtist {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawTrack {
    #[serde(deserialize_with = "flexible_id")]
    pub id: String,
    pub title: Option<String>,
    #[serde(default)]
    pub artists: Vec<RawArtist>,
    #[serde(rename = "durationMs")]
    pub duration_ms: Option<u64>,
    pub available: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct Envelope<T> {
    pub result: T,
}

// --- My Wave -----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RawSequenceItem {
    pub track: RawTrack,
}

#[derive(Debug, Deserialize)]
pub struct RawWaveResult {
    #[serde(default)]
    pub sequence: Vec<RawSequenceItem>,
    #[serde(rename = "batchId")]
    pub batch_id: Option<String>,
    #[serde(rename = "radioSessionId")]
    pub session_id: Option<String>,
}

/// One batch of wave tracks along with its session identifiers.
#[derive(Debug, Clone)]
pub struct WaveBatch {
    pub tracks: Vec<Track>,
    pub batch_id: Option<String>,
    pub session_id: Option<String>,
}

/// One server-provided Wave preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveSettings {
    pub name: String,
    pub description: String,
    /// Sent back verbatim when a Rotor session is created.
    pub seeds: Vec<String>,
}

impl Default for WaveSettings {
    fn default() -> Self {
        Self {
            name: "My Wave".into(),
            description: "Your usual personalized mix".into(),
            seeds: vec!["user:onyourwave".into()],
        }
    }
}

/// Dynamic presets advertised by the wheel for the current Wave.
#[derive(Debug, Clone)]
pub struct WaveChoices {
    pub options: Vec<WaveSettings>,
}

impl WaveSettings {
    pub fn seeds(&self) -> Vec<String> {
        self.seeds.clone()
    }

    pub fn is_default(&self) -> bool {
        self.seeds.len() == 1 && self.seeds[0] == "user:onyourwave"
    }
}

#[derive(Debug, Deserialize)]
pub struct RawWheelResult {
    #[serde(default)]
    pub items: Vec<RawWheelItem>,
}

#[derive(Debug, Deserialize)]
pub struct RawWheelItem {
    #[serde(rename = "type")]
    pub item_type: String,
    pub data: RawWheelData,
}

#[derive(Debug, Deserialize)]
pub struct RawWheelData {
    pub wave: Option<RawWheel>,
}

#[derive(Debug, Deserialize)]
pub struct RawWheel {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub seeds: Vec<String>,
}

#[cfg(test)]
mod wave_tests {
    use super::*;

    #[test]
    fn wave_settings_preserve_server_seeds() {
        let settings = WaveSettings {
            name: "Aggressive in English".into(),
            description: "My Wave".into(),
            seeds: vec!["mood:aggressive".into(), "local-language:english".into()],
        };
        assert_eq!(
            settings.seeds(),
            ["mood:aggressive", "local-language:english"]
        );
    }
}

// --- Liked tracks ------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RawLikedTrack {
    #[serde(deserialize_with = "flexible_id")]
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct RawLibrary {
    #[serde(default)]
    pub tracks: Vec<RawLikedTrack>,
}

#[derive(Debug, Deserialize)]
pub struct RawLikesResult {
    pub library: RawLibrary,
}

// --- Account -----------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RawAccount {
    pub uid: u64,
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RawAccountStatus {
    pub account: RawAccount,
}

// --- File link ---------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct RawDownloadInfo {
    pub codec: String,
    pub url: String,
}

#[derive(Debug, Deserialize)]
pub struct RawFileInfoResult {
    #[serde(rename = "downloadInfo")]
    pub download_info: RawDownloadInfo,
}

/// A direct link to an audio file, plus its container.
#[derive(Debug, Clone)]
pub struct DownloadInfo {
    pub url: String,
    pub codec: String,
}

impl DownloadInfo {
    /// Whether the container is probed from its end.
    ///
    /// ISO-BMFF (everything in an MP4 box) makes symphonia jump past `mdat`
    /// to look for trailing atoms, so the tail has to be fetched separately
    /// before the first sample can be decoded. MP3 never looks there. Unknown
    /// codecs get the tail too, on the assumption that it might be needed.
    pub fn probes_tail(&self) -> bool {
        !matches!(self.extension(), "mp3")
    }

    /// Extension to use in the cache. Unknown codecs are stored as `.bin` —
    /// symphonia sniffs the real format from the content anyway.
    pub fn extension(&self) -> &'static str {
        match self.codec.as_str() {
            "flac-mp4" | "aac-mp4" | "he-aac-mp4" => "mp4",
            "mp3" => "mp3",
            "flac" => "flac",
            "aac" | "he-aac" => "aac",
            _ => "bin",
        }
    }
}
