use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use rodio::{Decoder, MixerDeviceSink, Player};

use crate::stream::{SharedBuffer, TrackSource};

/// How far back from the downloaded edge a seek is held.
///
/// `Player::try_seek` blocks until the audio callback thread performs the
/// seek, and a read inside the not-yet-downloaded gap blocks that thread, so
/// overshooting here freezes playback rather than just failing.
const SEEK_MARGIN_SECS: f64 = 3.0;

/// How often the audio thread refreshes the position for the UI.
const TICK: Duration = Duration::from_millis(50);

enum Command {
    /// Play a source. `epoch` tells tracks apart, so the UI can see which
    /// track a given state snapshot belongs to.
    Play {
        source: TrackSource,
        epoch: u64,
        /// Track length as the API reports it. The decoder often cannot say
        /// (MP3 returns nothing), and the seek limiter needs a real number.
        duration: Duration,
    },
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
    /// A non-fatal complaint worth showing, such as a refused seek.
    pub notice: Option<String>,
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
            .spawn(
                move || match rodio::DeviceSinkBuilder::open_default_sink() {
                    Ok(sink) => {
                        let _ = ready_tx.send(Ok(()));
                        run(sink, rx, thread_state, volume);
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(format!("{e}")));
                    }
                },
            )
            .context("cannot start the audio thread")?;

        ready_rx
            .recv()
            .context("the audio thread did not answer")?
            .map_err(|e| anyhow::anyhow!("cannot open an audio device: {e}"))?;

        Ok(Audio { tx, state })
    }

    /// Takes the pending notice, if there is one.
    ///
    /// Reading clears it: these are one-off complaints, and the UI redraws
    /// ten times a second, so a notice left in place would pin itself to the
    /// status line and overwrite everything else said afterwards.
    pub fn take_notice(&self) -> Option<String> {
        self.state
            .lock()
            .expect("audio state mutex poisoned")
            .notice
            .take()
    }

    pub fn state(&self) -> AudioState {
        self.state
            .lock()
            .expect("audio state mutex poisoned")
            .clone()
    }

    pub fn play(&self, source: TrackSource, epoch: u64, duration: Duration) {
        let _ = self.tx.send(Command::Play {
            source,
            epoch,
            duration,
        });
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

/// What the seek limiter needs to know about the track being played.
struct Loaded {
    /// Present only while the track is still downloading.
    buffer: Option<Arc<SharedBuffer>>,
    duration: Duration,
}

fn run(sink: MixerDeviceSink, rx: Receiver<Command>, state: Arc<Mutex<AudioState>>, volume: f32) {
    let mut player: Option<Player> = None;
    let mut loaded: Option<Loaded> = None;
    let mut volume = volume;

    loop {
        match rx.recv_timeout(TICK) {
            Ok(Command::Play {
                source,
                epoch,
                duration,
            }) => {
                // Just drop the old Player: its Drop stops the sound without
                // blocking, and the new one starts right away.
                player = None;

                let mut snapshot = AudioState {
                    epoch,
                    volume,
                    ..Default::default()
                };

                let buffer = source.buffer();
                match open(source) {
                    Ok(decoder) => {
                        let fresh = Player::connect_new(sink.mixer());
                        fresh.set_volume(volume);
                        fresh.append(decoder);
                        fresh.play();
                        player = Some(fresh);
                        loaded = Some(Loaded { buffer, duration });
                        snapshot.loaded = true;
                    }
                    Err(e) => {
                        loaded = None;
                        snapshot.error = Some(format!("{e:#}"));
                    }
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
                loaded = None;
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
                    let wanted = (current + delta as f64).max(0.0);
                    let (target, clamped) = clamp_seek(wanted, loaded.as_ref());

                    let message = if let Err(e) = player.try_seek(Duration::from_secs_f64(target)) {
                        Some(format!("seek failed: {e}"))
                    } else if clamped {
                        Some("seek limited to the downloaded part".to_string())
                    } else {
                        None
                    };
                    if message.is_some() {
                        state.lock().expect("audio state mutex poisoned").notice = message;
                    }
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

/// Keeps a seek inside the downloaded part of a streaming track.
///
/// Reading from the gap ahead of the download blocks the audio callback
/// thread, which stalls playback outright, so a seek past it is pulled back
/// instead. A finished file has no gap and is never clamped.
fn clamp_seek(wanted: f64, loaded: Option<&Loaded>) -> (f64, bool) {
    let Some(loaded) = loaded else {
        return (wanted, false);
    };
    let Some(buffer) = &loaded.buffer else {
        return (wanted, false);
    };
    let duration = loaded.duration;

    let fraction = buffer.buffered_fraction();
    if fraction >= 1.0 {
        return (wanted, false);
    }

    // Margin so playback after the seek does not run straight into the gap.
    let limit = (duration.as_secs_f64() * fraction - SEEK_MARGIN_SECS).max(0.0);
    if wanted > limit {
        (limit, true)
    } else {
        (wanted, false)
    }
}

fn open(source: TrackSource) -> Result<Decoder<TrackSource>> {
    let byte_len = source.byte_len();
    let mut builder = Decoder::builder().with_data(source).with_seekable(true);
    if byte_len > 0 {
        // Symphonia seeks more accurately when it knows the full file size,
        // and a streamed source depends on it to resolve SeekFrom::End.
        builder = builder.with_byte_len(byte_len);
    }
    builder.build().context("unrecognised file format")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::SharedBuffer;

    fn streaming(len: u64, downloaded: usize, secs: u64) -> Loaded {
        let buffer = SharedBuffer::new(len);
        buffer.push(&vec![0u8; downloaded]);
        Loaded {
            buffer: Some(buffer),
            duration: Duration::from_secs(secs),
        }
    }

    #[test]
    fn a_finished_file_seeks_anywhere() {
        let loaded = Loaded {
            buffer: None,
            duration: Duration::from_secs(100),
        };
        assert_eq!(clamp_seek(90.0, Some(&loaded)), (90.0, false));
    }

    #[test]
    fn seeking_inside_the_downloaded_part_is_untouched() {
        // Half of a 100-second track has arrived, so 47s is comfortably safe.
        let loaded = streaming(1000, 500, 100);
        assert_eq!(clamp_seek(40.0, Some(&loaded)), (40.0, false));
    }

    #[test]
    fn seeking_past_the_download_is_pulled_back() {
        let loaded = streaming(1000, 500, 100);
        let (target, clamped) = clamp_seek(90.0, Some(&loaded));
        assert!(clamped);
        assert_eq!(target, 50.0 - SEEK_MARGIN_SECS);
    }

    #[test]
    fn barely_started_downloads_clamp_to_the_beginning() {
        let loaded = streaming(1000, 1, 100);
        assert_eq!(clamp_seek(30.0, Some(&loaded)), (0.0, true));
    }

    #[test]
    fn a_fully_downloaded_stream_stops_clamping() {
        let loaded = streaming(1000, 1000, 100);
        assert_eq!(clamp_seek(95.0, Some(&loaded)), (95.0, false));
    }
}
