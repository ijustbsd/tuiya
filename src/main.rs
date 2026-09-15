mod api;
mod app;
mod audio;
mod cache;
mod config;
#[cfg(any(target_os = "linux", target_os = "macos"))]
mod media;
mod stream;
mod ui;
mod update;

use std::sync::Arc;

use anyhow::{Context, Result, bail};

use crate::app::App;
use crate::audio::Audio;
use crate::config::Config;

#[tokio::main]
async fn main() -> Result<()> {
    match command(&std::env::args().skip(1).collect::<Vec<_>>())? {
        Command::Version => {
            println!("tuiya {}", env!("TUIYA_VERSION"));
            return Ok(());
        }
        Command::Help => {
            println!(
                "Usage: tuiya [COMMAND]\n\n\
                 Run without arguments to start the player.\n\n\
                 Commands:\n\
                 \x20 self-update  Install the latest release from GitHub\n\
                 \x20 version      Print the installed version\n\
                 \x20 help         Show this help"
            );
            return Ok(());
        }
        Command::Update => return update::run().await,
        Command::Play => {}
    }

    let config = Config::load()?;

    let client = api::Client::new(&config.token, config.api_quality(), config.codecs())
        .await
        .context("cannot connect to Yandex Music")?;

    let cache_dir = Config::cache_dir()?;
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("cannot create the cache at {}", cache_dir.display()))?;

    let audio = Audio::new(1.0)?;
    let app = App::new(
        Arc::new(client),
        audio,
        cache_dir,
        config.cache_limit_mb,
        config.streaming,
    );

    let terminal = ratatui::init();
    let result = app.run(terminal).await;
    ratatui::restore();

    result
}

#[derive(Debug, PartialEq)]
enum Command {
    Play,
    Version,
    Help,
    Update,
}

fn command(args: &[String]) -> Result<Command> {
    match args {
        [] => Ok(Command::Play),
        [arg] => match arg.as_str() {
            "version" => Ok(Command::Version),
            "help" => Ok(Command::Help),
            "self-update" => Ok(Command::Update),
            _ => bail!("Unknown command: {arg}. Run tuiya help for usage."),
        },
        _ => bail!("Unexpected arguments. Run tuiya help for usage."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_do_not_fall_through_to_player_startup() {
        assert_eq!(command(&[]).unwrap(), Command::Play);
        for (arg, expected) in [
            ("self-update", Command::Update),
            ("version", Command::Version),
            ("help", Command::Help),
        ] {
            assert_eq!(command(&[arg.into()]).unwrap(), expected);
        }
        for arg in ["self-updat", "--version", "-V", "--help", "-h"] {
            assert!(command(&[arg.into()]).is_err());
        }
        assert!(command(&["self-update".into(), "version".into()]).is_err());
    }
}
