use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::{Decoder, MixerDeviceSink, Player};

/// How often the audio thread refreshes the position for the UI.
const TICK: Duration = Duration::from_millis(50);

enum Command {
    /// Play a file. `epoch` tells tracks apart, so the UI can see which
    /// track a given state snapshot belongs to.
    Play { path: PathBuf, epoch: u64 },
    TogglePause,
    Stop,
    SetVolume(f32),
    SeekBy(i64),
}

/// A snapshot of playback state for the UI to read.
#[derive(Debug, Clone, Default)]
pub struct AudioState {
    pub epoch: u64,
    pub position: Duration,
    pub paused: bool,
    /// The track reached its end on its own.
    pub ended: bool,
    /// The file is in the decoder and playing (or paused).
    pub loaded: bool,
    pub volume: f32,
    /// The last decoding error, if there was one.
    pub error: Option<String>,
}

/// A handle to the audio thread. Every call here is non-blocking: the real
/// work (decoding, stopping) happens on a dedicated OS thread so the UI
/// never stalls.
pub struct Audio {
    tx: Sender<Command>,
    state: Arc<Mutex<AudioState>>,
}

impl Audio {
    pub fn new(volume: f32) -> Result<Self> {
        let state = Arc::new(Mutex::new(AudioState {
            volume,
            ..Default::default()
        }));
        let (tx, rx) = channel();

        // Open the device on the thread itself: the cpal stream lives as long
        // as its handle does.
        let (ready_tx, ready_rx) = channel();
        let thread_state = Arc::clone(&state);
        std::thread::Builder::new()
            .name("tuiya-audio".to_string())
            .spawn(move || match rodio::DeviceSinkBuilder::open_default_sink() {
                Ok(sink) => {
                    let _ = ready_tx.send(Ok(()));
                    run(sink, rx, thread_state, volume);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("{e}")));
                }
            })
            .context("cannot start the audio thread")?;

        ready_rx
            .recv()
            .context("the audio thread did not answer")?
            .map_err(|e| anyhow::anyhow!("cannot open an audio device: {e}"))?;

        Ok(Audio { tx, state })
    }

    pub fn state(&self) -> AudioState {
        self.state.lock().expect("audio state mutex poisoned").clone()
    }

    pub fn play(&self, path: PathBuf, epoch: u64) {
        let _ = self.tx.send(Command::Play { path, epoch });
    }

    pub fn toggle_pause(&self) {
        let _ = self.tx.send(Command::TogglePause);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(Command::Stop);
    }

    pub fn set_volume(&self, volume: f32) {
        let _ = self.tx.send(Command::SetVolume(volume.clamp(0.0, 2.0)));
    }

    pub fn seek_by(&self, seconds: i64) {
        let _ = self.tx.send(Command::SeekBy(seconds));
    }
}

fn run(sink: MixerDeviceSink, rx: Receiver<Command>, state: Arc<Mutex<AudioState>>, volume: f32) {
    let mut player: Option<Player> = None;
    let mut volume = volume;

    loop {
        match rx.recv_timeout(TICK) {
            Ok(Command::Play { path, epoch }) => {
                // Just drop the old Player: its Drop stops the sound without
                // blocking, and the new one starts right away.
                player = None;

                let mut snapshot = AudioState {
                    epoch,
                    volume,
                    ..Default::default()
                };

                match open(&path) {
                    Ok(source) => {
                        let fresh = Player::connect_new(sink.mixer());
                        fresh.set_volume(volume);
                        fresh.append(source);
                        fresh.play();
                        player = Some(fresh);
                        snapshot.loaded = true;
                    }
                    Err(e) => snapshot.error = Some(format!("{e:#}")),
                }

                *state.lock().expect("audio state mutex poisoned") = snapshot;
            }
            Ok(Command::TogglePause) => {
                if let Some(player) = &player {
                    if player.is_paused() {
                        player.play();
                    } else {
                        player.pause();
                    }
                }
            }
            Ok(Command::Stop) => {
                player = None;
                let mut snapshot = state.lock().expect("audio state mutex poisoned");
                snapshot.loaded = false;
                snapshot.position = Duration::ZERO;
            }
            Ok(Command::SetVolume(value)) => {
                volume = value;
                if let Some(player) = &player {
                    player.set_volume(value);
                }
                state.lock().expect("audio state mutex poisoned").volume = value;
            }
            Ok(Command::SeekBy(delta)) => {
                if let Some(player) = &player {
                    let current = player.get_pos().as_secs_f64();
                    let target = (current + delta as f64).max(0.0);
                    let _ = player.try_seek(Duration::from_secs_f64(target));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if let Some(active) = &player {
            let mut snapshot = state.lock().expect("audio state mutex poisoned");
            if snapshot.loaded {
                snapshot.position = active.get_pos();
                snapshot.paused = active.is_paused();
                if active.empty() {
                    snapshot.ended = true;
                    snapshot.loaded = false;
                }
            }
        }
    }
}

fn open(path: &PathBuf) -> Result<Decoder<std::io::BufReader<std::fs::File>>> {
    let file = std::fs::File::open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    let byte_len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let mut builder = Decoder::builder()
        .with_data(std::io::BufReader::new(file))
        .with_seekable(true);
    if byte_len > 0 {
        // Symphonia seeks more accurately when it knows the full file size.
        builder = builder.with_byte_len(byte_len);
    }
    builder.build().context("unrecognised file format")
}
