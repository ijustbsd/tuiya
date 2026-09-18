//! Public MediaPlayer APIs, serviced on the main thread by the TUI ticker.

use std::cell::RefCell;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_core_foundation::{CFRunLoop, kCFRunLoopDefaultMode};
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemPropertyArtist,
    MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoMediaType, MPNowPlayingInfoPropertyElapsedPlaybackTime,
    MPNowPlayingInfoPropertyMediaType, MPNowPlayingInfoPropertyPlaybackRate,
    MPNowPlayingPlaybackState, MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent,
    MPRemoteCommandHandlerStatus, MPSkipIntervalCommandEvent,
};
use tokio::sync::mpsc;

use super::{Event, Snapshot};
use crate::app::{App, Message, Playing};

#[derive(Clone, Copy)]
enum Control {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
    Seek,
}

struct Registration {
    control: Control,
    command: Retained<MPRemoteCommand>,
    target: Retained<AnyObject>,
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: The target is the opaque token returned for this command.
        unsafe {
            self.command.setEnabled(false);
            self.command.removeTarget(Some(&self.target));
        }
    }
}

#[derive(Default)]
struct Published {
    playing: Option<Playing>,
    last: Option<(u64, bool, u64, u64)>,
}

impl Published {
    fn refresh(&mut self, snapshot: &Snapshot) {
        let audio = &snapshot.audio;
        if !audio.loaded || audio.ended {
            self.playing = None;
        } else if let Some(playing) = &snapshot.playing
            && playing.epoch == audio.epoch
        {
            self.playing = Some(playing.clone());
        } else if self
            .playing
            .as_ref()
            .is_some_and(|p| p.epoch != audio.epoch)
        {
            self.playing = None;
        }
    }
}

/// The marker keeps this session (and App::run) on the main thread. Tokio's
/// main future is polled by Runtime::block_on on the calling OS thread.
pub struct Session {
    _main_thread: MainThreadMarker,
    center: Retained<MPNowPlayingInfoCenter>,
    registrations: Vec<Registration>,
    seek_track: Arc<Mutex<Option<(u64, Duration)>>>,
    published: RefCell<Published>,
}

impl Session {
    pub fn new(events: mpsc::UnboundedSender<Message>) -> Result<Self> {
        let main_thread = MainThreadMarker::new()
            .context("macOS media controls must start on the main thread")?;
        Ok(autoreleasepool(|_| {
            let app = NSApplication::sharedApplication(main_thread);
            app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);
            app.finishLaunching();

            // SAFETY: Shared MediaPlayer objects are used on the main thread.
            let (center, commands) = unsafe {
                (
                    MPNowPlayingInfoCenter::defaultCenter(),
                    MPRemoteCommandCenter::sharedCommandCenter(),
                )
            };
            let mut registrations = Vec::new();
            // Leave only commands for which this player has a handler enabled.
            unsafe {
                commands.seekForwardCommand().setEnabled(false);
                commands.seekBackwardCommand().setEnabled(false);
                commands.changePlaybackRateCommand().setEnabled(false);
                commands.changeRepeatModeCommand().setEnabled(false);
                commands.changeShuffleModeCommand().setEnabled(false);
                commands.enableLanguageOptionCommand().setEnabled(false);
                commands.disableLanguageOptionCommand().setEnabled(false);
                commands.ratingCommand().setEnabled(false);
                commands.likeCommand().setEnabled(false);
                commands.dislikeCommand().setEnabled(false);
                commands.bookmarkCommand().setEnabled(false);
            }
            macro_rules! connect {
                ($method:ident, $control:ident, $event:ident) => {
                    registrations.push(register(
                        unsafe { commands.$method() },
                        Control::$control,
                        events.clone(),
                        |_| Some(Event::$event),
                    ));
                };
            }
            connect!(playCommand, Play, Play);
            connect!(pauseCommand, Pause, Pause);
            connect!(togglePlayPauseCommand, Toggle, Toggle);
            connect!(nextTrackCommand, Next, Next);
            connect!(previousTrackCommand, Previous, Previous);
            connect!(stopCommand, Stop, Stop);

            for (command, direction) in unsafe {
                [
                    (commands.skipForwardCommand(), 1.0),
                    (commands.skipBackwardCommand(), -1.0),
                ]
            } {
                unsafe {
                    command.setPreferredIntervals(&NSArray::from_retained_slice(&[
                        NSNumber::new_f64(5.0),
                    ]))
                };
                registrations.push(register(
                    command.into_super(),
                    Control::Seek,
                    events.clone(),
                    move |event| {
                        let event = event.downcast_ref::<MPSkipIntervalCommandEvent>()?;
                        let interval = unsafe { event.interval() };
                        (interval.is_finite() && interval > 0.0)
                            .then_some(Event::SeekBy(direction * interval))
                    },
                ));
            }
            let seek_track = Arc::new(Mutex::new(None));
            let track = Arc::clone(&seek_track);
            registrations.push(register(
                unsafe { commands.changePlaybackPositionCommand() }.into_super(),
                Control::Seek,
                events,
                move |event| {
                    let event = event.downcast_ref::<MPChangePlaybackPositionCommandEvent>()?;
                    let (epoch, duration) = (*track.lock().ok()?)?;
                    seek_to(epoch, duration, unsafe { event.positionTime() })
                },
            ));
            Self {
                _main_thread: main_thread,
                center,
                registrations,
                seek_track,
                published: RefCell::new(Published::default()),
            }
        }))
    }

    pub fn update(&self, app: &App) {
        autoreleasepool(|_| {
            self.publish(&Snapshot::from_app(app));
            // Run the main dispatch queue and MediaPlayer's event sources for
            // at most 1ms per frame; playback and keyboard input stay in Tokio.
            CFRunLoop::run_in_mode(unsafe { kCFRunLoopDefaultMode }, 0.001, false);
        });
    }

    fn publish(&self, snapshot: &Snapshot) {
        let audio = &snapshot.audio;
        let active = audio.loaded && !audio.ended;
        let mut published = self.published.borrow_mut();
        published.refresh(snapshot);
        let playing = published
            .playing
            .as_ref()
            .filter(|p| p.epoch == audio.epoch);
        *self.seek_track.lock().expect("media seek mutex poisoned") =
            playing.map(|p| (p.epoch, p.track.duration));
        for registration in &self.registrations {
            let enabled = match registration.control {
                Control::Play | Control::Toggle => snapshot.can_play,
                Control::Pause => active,
                Control::Stop | Control::Next => snapshot.playing.is_some(),
                Control::Previous => snapshot.playing.as_ref().is_some_and(|p| p.index > 0),
                Control::Seek => playing.is_some(),
            };
            unsafe { registration.command.setEnabled(enabled) };
        }
        if let Some(playing) = playing {
            let key = (
                playing.epoch,
                audio.paused,
                audio.seek_serial,
                audio.position.as_secs(),
            );
            if published.last != Some(key) {
                let info = now_playing_info(playing, audio.position, audio.paused);
                unsafe {
                    self.center.setNowPlayingInfo(Some(&info));
                    if published
                        .last
                        .is_none_or(|last| last.0 != key.0 || last.1 != key.1)
                    {
                        self.center.setPlaybackState(if audio.paused {
                            MPNowPlayingPlaybackState::Paused
                        } else {
                            MPNowPlayingPlaybackState::Playing
                        });
                    }
                }
                published.last = Some(key);
            }
        } else if published.last.take().is_some() {
            unsafe {
                self.center.setNowPlayingInfo(None);
                self.center
                    .setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        autoreleasepool(|_| {
            self.registrations.clear();
            unsafe {
                self.center.setNowPlayingInfo(None);
                self.center
                    .setPlaybackState(MPNowPlayingPlaybackState::Unknown);
            }
        });
    }
}

fn register(
    command: Retained<MPRemoteCommand>,
    control: Control,
    events: mpsc::UnboundedSender<Message>,
    convert: impl Fn(&MPRemoteCommandEvent) -> Option<Event> + Send + Sync + 'static,
) -> Registration {
    let handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        // SAFETY: MediaPlayer supplies a live event for the duration of the call.
        let Some(event) = convert(unsafe { event.as_ref() }) else {
            return MPRemoteCommandHandlerStatus::CommandFailed;
        };
        if events.send(Message::Media(event)).is_ok() {
            MPRemoteCommandHandlerStatus::Success
        } else {
            MPRemoteCommandHandlerStatus::CommandFailed
        }
    });
    // SAFETY: MediaPlayer copies the block, whose captures are safe to share
    // across callback threads. Keep its returned token until unregistration.
    let target = unsafe {
        command.setEnabled(false);
        command.addTargetWithHandler(&handler)
    };
    Registration {
        control,
        command,
        target,
    }
}

fn seek_to(epoch: u64, duration: Duration, seconds: f64) -> Option<Event> {
    let position = Duration::try_from_secs_f64(seconds).ok()?;
    (position <= duration).then_some(Event::SeekTo { epoch, position })
}

fn now_playing_info(
    playing: &Playing,
    position: Duration,
    paused: bool,
) -> Retained<NSDictionary<NSString, AnyObject>> {
    let title = NSString::from_str(&playing.track.title);
    let artist = NSString::from_str(&playing.track.artists);
    let duration = NSNumber::new_f64(playing.track.duration.as_secs_f64());
    let elapsed = NSNumber::new_f64(position.as_secs_f64());
    let rate = NSNumber::new_f64(if paused { 0.0 } else { 1.0 });
    let media_type = NSNumber::new_usize(MPNowPlayingInfoMediaType::Audio.0);
    // SAFETY: All keys are NSString constants and values have the documented
    // NSString/NSNumber types. NSDictionary retains them before locals drop.
    unsafe {
        NSDictionary::from_slices(
            &[
                MPMediaItemPropertyTitle,
                MPMediaItemPropertyArtist,
                MPMediaItemPropertyPlaybackDuration,
                MPNowPlayingInfoPropertyElapsedPlaybackTime,
                MPNowPlayingInfoPropertyPlaybackRate,
                MPNowPlayingInfoPropertyMediaType,
            ],
            &[
                &*title,
                &*artist,
                &*duration,
                &*elapsed,
                &*rate,
                &*media_type,
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::models::Track;
    use crate::app::Tab;
    use crate::audio::AudioState;

    fn playing(epoch: u64, title: &str) -> Playing {
        Playing {
            tab: Tab::Wave,
            index: 1,
            track: Track {
                id: "42".into(),
                album_id: None,
                title: title.into(),
                artists: "Исполнитель".into(),
                duration: Duration::from_secs(120),
                available: true,
            },
            epoch,
            reported: true,
        }
    }

    #[test]
    fn metadata_follows_the_decoder_through_loading_and_stop() {
        let mut state = Published::default();
        let mut snapshot = Snapshot {
            audio: AudioState {
                epoch: 1,
                loaded: true,
                ..Default::default()
            },
            playing: Some(playing(1, "First track")),
            can_play: true,
        };
        state.refresh(&snapshot);
        assert_eq!(state.playing.as_ref().unwrap().track.title, "First track");
        snapshot.playing = Some(playing(2, "Second track"));
        state.refresh(&snapshot);
        assert_eq!(state.playing.as_ref().unwrap().track.title, "First track");
        snapshot.audio.epoch = 2;
        state.refresh(&snapshot);
        assert_eq!(state.playing.as_ref().unwrap().track.title, "Second track");
        snapshot.audio.ended = true;
        state.refresh(&snapshot);
        assert!(state.playing.is_none());
        snapshot.audio.ended = false;
        state.refresh(&snapshot);
        snapshot.audio.loaded = false;
        snapshot.playing = None;
        state.refresh(&snapshot);
        assert!(state.playing.is_none());
    }

    #[test]
    fn now_playing_dictionary_keeps_unicode_and_fractional_positions() {
        autoreleasepool(|_| {
            for paused in [false, true] {
                let info =
                    now_playing_info(&playing(1, "Песня 🎵"), Duration::from_millis(1250), paused);
                let title = info
                    .objectForKey(unsafe { MPMediaItemPropertyTitle })
                    .unwrap();
                assert_eq!(
                    title.downcast_ref::<NSString>().unwrap().to_string(),
                    "Песня 🎵"
                );
                let artist = info
                    .objectForKey(unsafe { MPMediaItemPropertyArtist })
                    .unwrap();
                assert_eq!(
                    artist.downcast_ref::<NSString>().unwrap().to_string(),
                    "Исполнитель"
                );
                for (key, expected) in unsafe {
                    [
                        (MPMediaItemPropertyPlaybackDuration, 120.0),
                        (MPNowPlayingInfoPropertyElapsedPlaybackTime, 1.25),
                        (
                            MPNowPlayingInfoPropertyPlaybackRate,
                            if paused { 0.0 } else { 1.0 },
                        ),
                        (MPNowPlayingInfoPropertyMediaType, 1.0),
                    ]
                } {
                    let value = info.objectForKey(key).unwrap();
                    assert_eq!(
                        value.downcast_ref::<NSNumber>().unwrap().doubleValue(),
                        expected
                    );
                }
            }
        });
    }

    #[test]
    fn absolute_seek_rejects_invalid_system_positions() {
        let duration = Duration::from_secs(120);
        assert_eq!(
            seek_to(7, duration, 1.25),
            Some(Event::SeekTo {
                epoch: 7,
                position: Duration::from_millis(1250),
            })
        );
        assert!(seek_to(7, duration, 120.0).is_some());
        for position in [-1.0, 120.1, f64::NAN, f64::INFINITY, f64::MAX] {
            assert!(seek_to(7, duration, position).is_none());
        }
    }
}
