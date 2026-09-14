use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// Player settings: the OAuth token and cache parameters.
///
/// The token is read from the `TUIYA_TOKEN` environment variable first,
/// then from `~/.config/tuiya/config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub token: String,
    /// Either `lossless` (flac-mp4, the default) or `high` (mp3 320).
    #[serde(default = "default_quality")]
    pub quality: String,
    /// Cache size limit, in megabytes.
    #[serde(default = "default_cache_limit")]
    pub cache_limit_mb: u64,
}

fn default_quality() -> String {
    "lossless".to_string()
}

fn default_cache_limit() -> u64 {
    4096
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::path()?;

        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            toml::from_str::<Config>(&raw)
                .with_context(|| format!("cannot parse {}", path.display()))?
        } else {
            Config {
                token: String::new(),
                quality: default_quality(),
                cache_limit_mb: default_cache_limit(),
            }
        };

        if let Ok(token) = std::env::var("TUIYA_TOKEN") {
            config.token = token;
        }

        if config.token.trim().is_empty() {
            bail!(
                "No Yandex Music OAuth token found.\n\n\
                 Put one in {} :\n\n\
                 \x20   token = \"y0_...\"\n\n\
                 or set the TUIYA_TOKEN environment variable.\n\n\
                 You can copy the token from any music.yandex.ru request\n\
                 (the Authorization: OAuth <token> header).",
                path.display()
            );
        }

        Ok(config)
    }

    /// Codecs to ask the API for, most preferred first.
    pub fn codecs(&self) -> &'static str {
        match self.quality.as_str() {
            "high" | "mp3" => "mp3",
            _ => "flac-mp4,mp3",
        }
    }

    pub fn api_quality(&self) -> &'static str {
        match self.quality.as_str() {
            "high" | "mp3" => "high",
            _ => "lossless",
        }
    }

    pub fn path() -> Result<PathBuf> {
        let dir = dirs::config_dir().context("cannot locate the config directory")?;
        Ok(dir.join("tuiya").join("config.toml"))
    }

    pub fn cache_dir() -> Result<PathBuf> {
        let dir = dirs::cache_dir().context("cannot locate the cache directory")?;
        Ok(dir.join("tuiya"))
    }
}
