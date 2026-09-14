mod api;
mod app;
mod audio;
mod cache;
mod config;
mod ui;

use std::sync::Arc;

use anyhow::{Context, Result};

use crate::app::App;
use crate::audio::Audio;
use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;

    let client = api::Client::new(&config.token, config.api_quality(), config.codecs())
        .await
        .context("cannot connect to Yandex Music")?;

    let cache_dir = Config::cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("cannot create the cache at {}", cache_dir.display()))?;

    let audio = Audio::new(1.0)?;
    let app = App::new(Arc::new(client), audio, cache_dir, config.cache_limit_mb);

    let terminal = ratatui::init();
    let result = app.run(terminal).await;
    ratatui::restore();

    result
}
