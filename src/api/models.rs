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

/// One server-provided choice for a Wave tuning dimension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveOption {
    pub name: String,
    /// `None` is the server-marked default and is omitted from session seeds.
    pub seed: Option<String>,
}

/// Server-provided choices supported by the current Wave.
#[derive(Debug, Clone)]
pub struct WaveRestrictions {
    pub language: Vec<WaveOption>,
    pub mood_energy: Vec<WaveOption>,
    pub diversity: Vec<WaveOption>,
}

/// Tuning seeds used only for the current Wave session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaveSettings {
    pub language: WaveOption,
    pub diversity: WaveOption,
    pub mood_energy: WaveOption,
}

impl WaveSettings {
    pub fn language_label(&self) -> &str {
        &self.language.name
    }

    pub fn mood_label(&self) -> &str {
        &self.mood_energy.name
    }

    pub fn diversity_label(&self) -> &str {
        &self.diversity.name
    }

    pub fn seeds(&self) -> Vec<String> {
        let mut seeds = vec!["user:onyourwave".into()];
        for choice in [&self.language, &self.mood_energy, &self.diversity] {
            if let Some(seed) = &choice.seed {
                seeds.push(seed.clone());
            }
        }
        seeds
    }
}

impl WaveRestrictions {
    pub fn defaults(&self) -> Option<WaveSettings> {
        Some(WaveSettings {
            language: self
                .language
                .iter()
                .find(|option| option.seed.is_none())?
                .clone(),
            mood_energy: self
                .mood_energy
                .iter()
                .find(|option| option.seed.is_none())?
                .clone(),
            diversity: self
                .diversity
                .iter()
                .find(|option| option.seed.is_none())?
                .clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct RawStationResult {
    pub station: RawWaveStation,
}

#[derive(Debug, Deserialize)]
pub struct RawWaveStation {
    #[serde(rename = "restrictions2")]
    pub restrictions: RawWaveRestrictions,
}

#[derive(Debug, Deserialize)]
pub struct RawWaveRestrictions {
    pub language: RawEnumRestriction,
    pub diversity: RawEnumRestriction,
    #[serde(rename = "moodEnergy")]
    pub mood_energy: RawEnumRestriction,
}

#[derive(Debug, Deserialize)]
pub struct RawEnumRestriction {
    #[serde(rename = "possibleValues")]
    pub possible_values: Vec<RawRestrictionValue>,
}

#[derive(Debug, Deserialize)]
pub struct RawRestrictionValue {
    pub name: String,
    #[serde(rename = "serializedSeed")]
    pub serialized_seed: String,
    #[serde(default)]
    pub unspecified: bool,
}

#[cfg(test)]
mod wave_tests {
    use super::*;

    #[test]
    fn wave_settings_build_session_only_seeds() {
        let settings = WaveSettings {
            language: WaveOption {
                name: "Русский".into(),
                seed: Some("settingLanguage:russian".into()),
            },
            mood_energy: WaveOption {
                name: "Любое".into(),
                seed: None,
            },
            diversity: WaveOption {
                name: "Незнакомое".into(),
                seed: Some("settingDiversity:discover".into()),
            },
        };
        assert_eq!(
            settings.seeds(),
            [
                "user:onyourwave",
                "settingLanguage:russian",
                "settingDiversity:discover",
            ]
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
