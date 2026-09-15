//! Desktop metadata and media commands, independent of terminal focus.

use std::time::Duration;

use crate::app::{App, Playing};
use crate::audio::AudioState;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::Session;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Session;

#[derive(Debug, PartialEq)]
pub enum Event {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    SeekBy(f64),
    SeekTo {
        epoch: u64,
        position: Duration,
    },
    #[cfg(target_os = "linux")]
    SetVolume(f64),
    #[cfg(target_os = "linux")]
    Quit,
}

#[derive(Clone, Default)]
struct Snapshot {
    audio: AudioState,
    playing: Option<Playing>,
    can_play: bool,
}

impl Snapshot {
    fn from_app(app: &App) -> Self {
        Self {
            audio: app.audio_state(),
            playing: app.playing.clone(),
            can_play: app.playing.is_some()
                || app
                    .queue(app.tab)
                    .tracks
                    .iter()
                    .any(|track| track.available),
        }
    }
}
