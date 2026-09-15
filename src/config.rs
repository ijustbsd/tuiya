use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Player settings: the OAuth token and cache parameters.
///
/// The token is read from the `TUIYA_TOKEN` environment variable first,
/// then from `~/.config/tuiya/config.toml`.
#[derive(Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub token: String,
    /// Either `lossless` (flac-mp4, the default) or `high` (mp3 320).
    #[serde(default = "default_quality")]
    pub quality: String,
    /// Cache size limit, in megabytes.
    #[serde(default = "default_cache_limit")]
    pub cache_limit_mb: u64,
    /// Start playing while the track is still downloading. Turn this off to
    /// wait for the whole file, the way older versions behaved.
    #[serde(default = "default_streaming")]
    pub streaming: bool,
}

fn default_quality() -> String {
    "lossless".to_string()
}

fn default_cache_limit() -> u64 {
    4096
}

fn default_streaming() -> bool {
    true
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = Self::path()?;

        let mut config = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            toml::from_str::<Config>(&raw)
                .map_err(|_| anyhow::anyhow!("cannot parse {}", path.display()))?
        } else {
            Config {
                token: String::new(),
                quality: default_quality(),
                cache_limit_mb: default_cache_limit(),
                streaming: default_streaming(),
            }
        };

        if let Ok(token) = std::env::var("TUIYA_TOKEN") {
            config.token = token;
        }

        Ok(config)
    }

    pub fn save_token(token: &str) -> Result<PathBuf> {
        let path = Self::path()?;
        save_token_at(&path, token)?;
        Ok(path)
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

fn save_token_at(path: &Path, token: &str) -> Result<()> {
    let mut settings = match std::fs::read_to_string(path) {
        Ok(raw) => toml::from_str::<toml::Table>(&raw)
            .map_err(|_| anyhow::anyhow!("cannot parse the config; it has not been changed"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => toml::Table::new(),
        Err(error) => return Err(error).context("cannot read the config"),
    };
    settings.insert("token".into(), toml::Value::String(token.into()));
    let raw = toml::to_string_pretty(&settings).context("cannot encode the config")?;
    let parent = path.parent().context("config has no parent directory")?;
    std::fs::create_dir_all(parent).context("cannot create the config directory")?;
    let mut pending =
        tempfile::NamedTempFile::new_in(parent).context("cannot create a temporary config file")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        pending
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    pending
        .write_all(raw.as_bytes())
        .context("cannot write the config")?;
    pending
        .as_file()
        .sync_all()
        .context("cannot flush the config")?;
    pending
        .persist(path)
        .map_err(|error| error.error)
        .context("cannot replace the config")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_preserves_settings_and_secures_the_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "token = 'old'\nquality = 'high'\nstreaming = false\ncache_limit_mb = 123\n[extra]\nvalues = [1, 2]\n").unwrap();
        save_token_at(&path, "new-token").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let config: Config = toml::from_str(&raw).unwrap();
        assert_eq!(config.token, "new-token");
        assert_eq!(config.api_quality(), "high");
        assert!(!config.streaming);
        assert_eq!(config.cache_limit_mb, 123);
        let settings: toml::Table = toml::from_str(&raw).unwrap();
        assert_eq!(settings["extra"]["values"].as_array().unwrap().len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn login_creates_a_config_and_does_not_overwrite_malformed_settings() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tuiya/config.toml");
        save_token_at(&path, "test-token").unwrap();
        let config: Config = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.token, "test-token");
        assert_eq!(config.api_quality(), "lossless");
        std::fs::write(&path, "[broken").unwrap();
        assert!(save_token_at(&path, "replacement").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "[broken");
    }

    #[test]
    fn settings_without_a_token_can_start_login() {
        let config: Config = toml::from_str("quality = 'high'").unwrap();
        assert!(config.token.is_empty());
        assert_eq!(config.api_quality(), "high");
    }
}
