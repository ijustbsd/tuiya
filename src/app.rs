use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use rand::Rng;
use ratatui::DefaultTerminal;
use ratatui::widgets::TableState;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::api::models::{Track, WaveBatch, WaveChoices, WaveSettings};
use crate::api::{Client, Feedback};
use crate::audio::Audio;
use crate::cache;
use crate::config::{Config, Preferences};
use crate::settings::{Action, Settings};
use crate::stream::TrackSource;
use crate::ui;
use crate::wave_settings::{Action as WaveSettingsAction, WaveSettingsDialog};

/// Audio polling interval. Discrete UI changes redraw immediately; while a
/// track is playing this also refreshes the progress bar.
const AUDIO_POLL_INTERVAL: Duration = Duration::from_millis(50);
/// How many tracks before the end of the queue we ask the wave for more.
const WAVE_REFILL_MARGIN: usize = 2;
const SEEK_STEP: i64 = 5;
const VOLUME_STEP: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Wave,
    Likes,
}

/// Which part of the main screen receives navigation keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Content,
}

/// The responsive layout selected by the renderer for the current terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    Wide,
    Compact,
    Minimal,
}

#[derive(Debug, Clone)]
pub struct SidebarState {
    pub selected: usize,
    pub wide_visible: bool,
    pub overlay_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Info,
    Success,
    Error,
}

#[derive(Debug, Clone)]
pub struct Notice {
    pub kind: NoticeKind,
    pub text: String,
    expires_at: Option<Instant>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self {
            selected: 0,
            wide_visible: true,
            overlay_open: false,
        }
    }
}

impl SidebarState {
    fn move_selection(&mut self, delta: isize) {
        self.selected = (self.selected as isize + delta).clamp(0, 1) as usize;
    }

    fn selected_view(&self) -> View {
        if self.selected == 0 {
            View::Wave
        } else {
            View::Likes
        }
    }
}

// Playback queues still use this name internally. Keeping the alias makes the
// view migration independent from the audio and media-control code.
pub type Tab = View;

/// A track list with a separate cursor (what is highlighted) and current
/// entry (what is playing) — they move independently so you can browse
/// while something plays.
#[derive(Debug)]
pub struct Queue {
    pub tracks: Vec<Track>,
    pub cursor: usize,
    pub playing: Option<usize>,
    /// What to show instead of an empty list: "Loading…" or a reason.
    pub placeholder: String,
    /// Kept between frames, otherwise the list jumps around when scrolling.
    pub state: TableState,
}

impl Default for Queue {
    fn default() -> Self {
        Queue {
            tracks: Vec::new(),
            cursor: 0,
            playing: None,
            placeholder: "Loading…".to_string(),
            state: TableState::default(),
        }
    }
}

impl Queue {
    fn move_cursor(&mut self, delta: isize) {
        if self.tracks.is_empty() {
            return;
        }
        let last = self.tracks.len() - 1;
        let next = self.cursor as isize + delta;
        self.cursor = next.clamp(0, last as isize) as usize;
    }
}

/// What is playing right now.
#[derive(Debug, Clone)]
pub struct Playing {
    pub tab: Tab,
    pub index: usize,
    pub track: Track,
    pub epoch: u64,
    /// trackStarted was already reported to the wave — do not repeat it.
    pub reported: bool,
}

/// Results from background tasks land here.
pub enum Message {
    Wave {
        generation: u64,
        result: Result<WaveBatch>,
    },
    WaveChoices(Result<WaveChoices>),
    Likes(Result<(Vec<Track>, HashSet<String>)>),
    Ready {
        epoch: u64,
        source: TrackSource,
    },
    Failed {
        epoch: u64,
        error: String,
    },
    LikeChanged {
        track_id: String,
        liked: bool,
    },
    Notice(String),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Media(crate::media::Event),
}

pub struct App {
    api: Arc<Client>,
    audio: Audio,
    cache_dir: PathBuf,
    cache_limit: u64,

    pub view: View,
    pub focus: Focus,
    pub layout_mode: LayoutMode,
    pub sidebar: SidebarState,
    pub wave: Queue,
    pub likes: Queue,
    pub liked: HashSet<String>,
    pub playing: Option<Playing>,
    pub notice: Option<Notice>,
    pub shuffle: bool,
    pub volume: f32,
    pub loading_track: bool,
    pub wave_settings: Option<WaveSettings>,
    pub wave_choices: Option<WaveChoices>,
    pub wave_settings_dialog: Option<WaveSettingsDialog>,
    pub help_open: bool,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pause_on_load: bool,
    /// Preferences last saved through the settings dialog.
    preferences: Preferences,
    pub settings: Option<Settings>,

    wave_batch_id: Option<String>,
    wave_session_id: Option<String>,
    wave_requested: bool,
    wave_feedbacks: Vec<Feedback>,
    wave_generation: u64,
    /// The wave ran dry: play as soon as the next batch arrives.
    wave_autoplay_pending: bool,

    epoch: u64,
    should_quit: bool,
    tx: mpsc::UnboundedSender<Message>,
    rx: mpsc::UnboundedReceiver<Message>,
}

impl App {
    pub fn new(
        api: Arc<Client>,
        audio: Audio,
        cache_dir: PathBuf,
        preferences: Preferences,
    ) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let volume = audio.state().volume;
        App {
            api,
            audio,
            cache_dir,
            cache_limit: preferences.cache_limit_mb.saturating_mul(1024 * 1024),
            view: View::Wave,
            focus: Focus::Content,
            layout_mode: LayoutMode::Wide,
            sidebar: SidebarState::default(),
            wave: Queue::default(),
            likes: Queue::default(),
            liked: HashSet::new(),
            playing: None,
            notice: Some(Notice {
                kind: NoticeKind::Info,
                text: "Starting the wave…".to_string(),
                expires_at: None,
            }),
            shuffle: false,
            volume,
            loading_track: false,
            wave_settings: None,
            wave_choices: None,
            wave_settings_dialog: None,
            help_open: false,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pause_on_load: false,
            preferences,
            settings: None,
            wave_batch_id: None,
            wave_session_id: None,
            wave_requested: false,
            wave_feedbacks: Vec::new(),
            wave_generation: 0,
            wave_autoplay_pending: false,
            epoch: 0,
            should_quit: false,
            tx,
            rx,
        }
    }

    fn set_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        let expires_at = match kind {
            NoticeKind::Info | NoticeKind::Success => Some(Instant::now() + Duration::from_secs(3)),
            NoticeKind::Error => None,
        };
        self.notice = Some(Notice {
            kind,
            text: text.into(),
            expires_at,
        });
    }

    fn expire_notice(&mut self) -> bool {
        let expired = self
            .notice
            .as_ref()
            .and_then(|notice| notice.expires_at)
            .is_some_and(|expires_at| Instant::now() >= expires_at);
        if expired {
            self.notice = None;
        }
        expired
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let media = match crate::media::Session::new(self.tx.clone()) {
            Ok(media) => Some(media),
            Err(error) => {
                self.set_notice(
                    NoticeKind::Error,
                    format!("System media controls unavailable: {error:#}"),
                );
                None
            }
        };
        self.load_likes();
        self.load_wave_choices();
        self.start_wave(false);

        let mut ticker = tokio::time::interval(AUDIO_POLL_INTERVAL);
        let mut events = EventStream::new();

        while !self.should_quit {
            tokio::select! {
                _ = ticker.tick() => {
                    let notice_expired = self.expire_notice();
                    self.poll_audio();
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    if let Some(media) = &media {
                        media.update(&self);
                    }
                    if self.playing.is_some() || notice_expired {
                        terminal.draw(|frame| ui::render(frame, &mut self))?;
                    }
                }
                Some(Ok(event)) = events.next() => {
                    match event {
                        TermEvent::Key(key) => {
                            self.on_key(key);
                            terminal.draw(|frame| ui::render(frame, &mut self))?;
                        }
                        TermEvent::Resize(_, _) => {
                            terminal.draw(|frame| ui::render(frame, &mut self))?;
                        }
                        _ => {}
                    }
                }
                Some(message) = self.rx.recv() => {
                    self.on_message(message);
                    terminal.draw(|frame| ui::render(frame, &mut self))?;
                },
            }
        }

        self.audio.stop();
        Ok(())
    }

    // --- Background loading -----------------------------------------------

    fn load_likes(&self) {
        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = async {
                let ids = api.liked_track_ids().await?;
                let liked: HashSet<String> = ids.iter().cloned().collect();
                let tracks = api.tracks_meta(&ids).await?;
                Ok((tracks, liked))
            }
            .await;
            let _ = tx.send(Message::Likes(result));
        });
    }

    fn load_wave_choices(&self) {
        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        let current = self.wave_settings.clone().unwrap_or_default();
        tokio::spawn(async move {
            let _ = tx.send(Message::WaveChoices(api.wave_choices(&current).await));
        });
    }

    fn start_wave(&mut self, autoplay: bool) {
        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        let generation = self.wave_generation;
        let settings = self.wave_settings.clone();
        self.wave_requested = true;
        self.wave_autoplay_pending = autoplay;
        tokio::spawn(async move {
            let _ = tx.send(Message::Wave {
                generation,
                result: api.start_wave(settings.as_ref()).await,
            });
        });
    }

    fn request_more_wave(&mut self) {
        if self.wave_requested {
            return;
        }
        let queue: Vec<String> = self
            .wave
            .tracks
            .iter()
            .rev()
            .take(2)
            .rev()
            .map(|track| track.radio_id())
            .collect();
        if queue.is_empty() {
            return;
        }
        let Some(session_id) = self.wave_session_id.clone() else {
            return;
        };
        self.wave_requested = true;

        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        let generation = self.wave_generation;
        let feedbacks = self.wave_feedbacks.clone();
        self.wave_feedbacks.clear();
        tokio::spawn(async move {
            let _ = tx.send(Message::Wave {
                generation,
                result: api.wave_tracks(&session_id, &queue, &feedbacks).await,
            });
        });
    }

    /// Open a track and report that it is ready to play.
    ///
    /// With streaming on this comes back after a fraction of the file, so the
    /// gap between pressing a key and hearing sound stays under a second.
    fn fetch_track(&self, track_id: String, epoch: u64) {
        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        let cache_dir = self.cache_dir.clone();
        let streaming = self.preferences.streaming;
        tokio::spawn(async move {
            match cache::open_track(api, cache_dir, &track_id, streaming).await {
                Ok(source) => {
                    let _ = tx.send(Message::Ready { epoch, source });
                }
                Err(e) => {
                    let _ = tx.send(Message::Failed {
                        epoch,
                        error: format!("{e:#}"),
                    });
                }
            }
        });
    }

    /// Quietly pull the next track into the cache while this one plays.
    fn prefetch(&self, tab: Tab, index: usize) {
        let queue = self.queue(tab);
        let Some(track) = queue.tracks.get(index + 1) else {
            return;
        };
        let track_id = track.id.clone();
        let api = Arc::clone(&self.api);
        let cache_dir = self.cache_dir.clone();
        tokio::spawn(async move {
            let _ = cache::ensure_track(&api, &cache_dir, &track_id).await;
        });
    }

    // --- Playback ---------------------------------------------------------

    pub fn queue(&self, tab: Tab) -> &Queue {
        match tab {
            Tab::Wave => &self.wave,
            Tab::Likes => &self.likes,
        }
    }

    pub fn queue_mut(&mut self, tab: Tab) -> &mut Queue {
        match tab {
            Tab::Wave => &mut self.wave,
            Tab::Likes => &mut self.likes,
        }
    }

    /// The nearest playable track at or after `start`.
    ///
    /// The wave only moves forward while the liked list wraps around, hence
    /// the two branches. Note this iterates rather than recurses: a run of
    /// unavailable tracks must not pile up the stack.
    fn next_available(&self, tab: Tab, start: usize) -> Option<usize> {
        let tracks = &self.queue(tab).tracks;
        let len = tracks.len();
        if len == 0 {
            return None;
        }
        match tab {
            Tab::Wave => (start..len).find(|&i| tracks[i].available),
            Tab::Likes => (0..len)
                .map(|offset| (start + offset) % len)
                .find(|&i| tracks[i].available),
        }
    }

    fn play_index(&mut self, tab: Tab, index: usize) {
        let Some(index) = self.next_available(tab, index) else {
            self.set_notice(NoticeKind::Info, "No playable tracks left");
            return;
        };
        let Some(track) = self.queue(tab).tracks.get(index).cloned() else {
            return;
        };

        self.epoch += 1;
        let epoch = self.epoch;

        // There must be exactly one ▶ marker — clear the other tab's.
        let other = match tab {
            Tab::Wave => Tab::Likes,
            Tab::Likes => Tab::Wave,
        };
        self.queue_mut(other).playing = None;
        self.queue_mut(tab).playing = Some(index);
        self.loading_track = true;
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            self.pause_on_load = false;
        }
        self.set_notice(NoticeKind::Info, format!("Loading \"{}\"…", track.label()));
        self.playing = Some(Playing {
            tab,
            index,
            track: track.clone(),
            epoch,
            reported: false,
        });

        self.fetch_track(track.id, epoch);

        if tab == Tab::Wave && index + WAVE_REFILL_MARGIN >= self.wave.tracks.len() {
            self.request_more_wave();
        }
    }

    /// Move to the track after `index` on the `tab` tab.
    fn advance(&mut self, tab: Tab, index: usize) {
        match tab {
            Tab::Wave => match self.next_available(Tab::Wave, index + 1) {
                Some(next) => self.play_index(Tab::Wave, next),
                None => {
                    self.wave_autoplay_pending = true;
                    self.set_notice(NoticeKind::Info, "The wave is picking the next tracks…");
                    self.request_more_wave();
                }
            },
            Tab::Likes => {
                if self.likes.tracks.is_empty() {
                    return;
                }
                let next = if self.shuffle {
                    rand::rng().random_range(0..self.likes.tracks.len())
                } else {
                    (index + 1) % self.likes.tracks.len()
                };
                self.play_index(Tab::Likes, next);
            }
        }
    }

    /// Seconds of the track that actually played — the wave wants this.
    fn played_secs(&self) -> f64 {
        self.audio.state().position.as_secs_f64()
    }

    fn report(&self, event: Feedback) {
        let Some(session_id) = self.wave_session_id.clone() else {
            return;
        };
        let api = Arc::clone(&self.api);
        let batch_id = if matches!(&event, Feedback::RadioStarted) {
            None
        } else {
            self.wave_batch_id.clone()
        };
        tokio::spawn(async move {
            let _ = api
                .wave_feedback(&session_id, batch_id.as_deref(), event)
                .await;
        });
    }

    fn report_play(&self, playing: &Playing, played_secs: f64, reason: &str) {
        if !playing.reported {
            return;
        }
        let api = Arc::clone(&self.api);
        let track_id = playing.track.radio_id();
        let duration_secs = playing.track.duration.as_secs_f64();
        let session_id = (playing.tab == Tab::Wave)
            .then(|| self.wave_session_id.clone())
            .flatten();
        let batch_id = (playing.tab == Tab::Wave)
            .then(|| self.wave_batch_id.clone())
            .flatten();
        let reason = reason.to_string();
        tokio::spawn(async move {
            let _ = api
                .play(
                    &track_id,
                    duration_secs,
                    played_secs,
                    &reason,
                    session_id.as_deref(),
                    batch_id.as_deref(),
                )
                .await;
        });
    }

    fn close_wave_session(&self) {
        let Some(session_id) = self.wave_session_id.clone() else {
            return;
        };
        let Some(playing) = self
            .playing
            .as_ref()
            .filter(|playing| playing.tab == Tab::Wave)
        else {
            return;
        };

        let api = Arc::clone(&self.api);
        let batch_id = self.wave_batch_id.clone();
        let event = Feedback::Skip {
            track_id: playing.track.radio_id(),
            played_secs: self.played_secs(),
        };
        tokio::spawn(async move {
            let _ = api
                .close_wave_session(&session_id, batch_id.as_deref(), event)
                .await;
        });
    }

    fn next_track(&mut self, skipped: bool) {
        let Some(playing) = self.playing.clone() else {
            return;
        };
        if playing.tab == Tab::Wave {
            let played_secs = self.played_secs();
            self.report_play(
                &playing,
                played_secs,
                if skipped { "skip" } else { "trackFinished" },
            );
            self.wave_feedbacks.push(if skipped {
                Feedback::Skip {
                    track_id: playing.track.radio_id(),
                    played_secs,
                }
            } else {
                Feedback::TrackFinished {
                    track_id: playing.track.radio_id(),
                    played_secs,
                }
            });
        } else {
            self.report_play(
                &playing,
                self.played_secs(),
                if skipped { "skip" } else { "trackFinished" },
            );
        }
        self.advance(playing.tab, playing.index);
    }

    fn previous_track(&mut self) {
        let Some(playing) = self.playing.clone() else {
            return;
        };
        if playing.index == 0 {
            self.set_notice(NoticeKind::Info, "This is the first track in the queue");
            return;
        }
        self.play_index(playing.tab, playing.index - 1);
    }

    /// Once per frame, check whether the track ended on its own.
    fn poll_audio(&mut self) {
        let state = self.audio.state();
        self.volume = state.volume;

        let Some(playing) = self.playing.clone() else {
            return;
        };
        if state.epoch != playing.epoch {
            return;
        }

        if let Some(notice) = self.audio.take_notice() {
            self.set_notice(NoticeKind::Info, notice);
        }

        if let Some(error) = state.error {
            self.set_notice(
                NoticeKind::Error,
                format!("Cannot play \"{}\": {error}", playing.track.label()),
            );
            self.loading_track = false;
            // One broken file is no reason to stop everything.
            self.advance(playing.tab, playing.index);
            return;
        }

        if state.loaded && !playing.reported {
            self.loading_track = false;
            self.notice = None;
            if let Some(current) = self.playing.as_mut() {
                current.reported = true;
            }
            if playing.tab == Tab::Wave {
                self.report(Feedback::TrackStarted {
                    track_id: playing.track.radio_id(),
                });
            }
            self.prefetch(playing.tab, playing.index);

            let _ = cache::prune(
                &self.cache_dir,
                self.cache_limit,
                Some(playing.track.id.as_str()),
            );
        }

        if state.ended {
            self.next_track(false);
        }
    }

    // --- Messages from background tasks -----------------------------------

    fn on_message(&mut self, message: Message) {
        match message {
            Message::Wave {
                generation,
                result: Ok(batch),
            } if generation == self.wave_generation => {
                self.wave_requested = false;
                if self
                    .notice
                    .as_ref()
                    .is_some_and(|notice| notice.kind == NoticeKind::Info)
                {
                    self.notice = None;
                }
                if batch.batch_id.is_some() {
                    self.wave_batch_id = batch.batch_id.clone();
                }
                if let Some(session_id) = batch.session_id {
                    self.wave_session_id = Some(session_id);
                    self.report(Feedback::RadioStarted);
                }
                let was_empty = self.wave.tracks.is_empty();
                let resume_from = self.wave.tracks.len();
                self.wave.tracks.extend(batch.tracks);

                if self.wave_autoplay_pending && !self.wave.tracks.is_empty() {
                    self.wave_autoplay_pending = false;
                    let start = if was_empty { 0 } else { resume_from };
                    if was_empty {
                        self.wave.cursor = 0;
                    }
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    let paused = self.pause_on_load;
                    self.play_index(Tab::Wave, start);
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    {
                        self.pause_on_load = paused;
                    }
                }
            }
            Message::Wave {
                generation,
                result: Err(e),
            } if generation == self.wave_generation => {
                self.wave_requested = false;
                let reason = format!("{e:#}");
                if self.wave.tracks.is_empty() {
                    self.wave.placeholder = format!("The wave is not answering: {reason}");
                }
                self.set_notice(
                    NoticeKind::Error,
                    format!("The wave is not answering: {reason}"),
                );
            }
            Message::Wave { .. } => {}
            Message::WaveChoices(Ok(choices)) => {
                if self.wave_settings.is_none() {
                    self.wave_settings = Some(WaveSettings::default());
                }
                self.wave_choices = Some(choices);
            }
            // Tuning is optional. A failure leaves the default Wave available
            // without exposing an incomplete settings dialog.
            Message::WaveChoices(Err(_)) => {}
            Message::Likes(Ok((tracks, liked))) => {
                self.likes.tracks = tracks;
                self.liked = liked;
                self.likes.placeholder = "Nothing liked yet".to_string();
                self.set_notice(
                    NoticeKind::Success,
                    format!("{} liked tracks", self.likes.tracks.len()),
                );
            }
            Message::Likes(Err(e)) => {
                let reason = format!("{e:#}");
                self.likes.placeholder = format!("Liked tracks failed to load: {reason}");
                self.set_notice(
                    NoticeKind::Error,
                    format!("Liked tracks failed to load: {reason}"),
                );
            }
            Message::Ready { epoch, source } => {
                if let Some(playing) = self.playing.as_ref()
                    && playing.epoch == epoch
                {
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    let paused = self.pause_on_load;
                    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                    let paused = false;
                    self.audio
                        .play(source, epoch, playing.track.duration, paused);
                }
            }
            Message::Failed { epoch, error } => {
                let stale = self.playing.as_ref().is_none_or(|p| p.epoch != epoch);
                if stale {
                    return;
                }
                self.loading_track = false;
                self.set_notice(NoticeKind::Error, format!("Track download failed: {error}"));
                // A single failed track must not stall the wave.
                if let Some(playing) = self.playing.clone() {
                    self.advance(playing.tab, playing.index);
                }
            }
            Message::LikeChanged { track_id, liked } => {
                if liked {
                    self.liked.insert(track_id);
                    self.set_notice(NoticeKind::Success, "Liked");
                } else {
                    self.liked.remove(&track_id);
                    self.set_notice(NoticeKind::Success, "Like removed");
                }
            }
            Message::Notice(text) => self.set_notice(NoticeKind::Error, text),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Message::Media(event) => self.on_media(event),
        }
    }

    // --- Keyboard ---------------------------------------------------------

    fn on_key(&mut self, key: KeyEvent) {
        if !key.kind.is_press() {
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if self.help_open {
            if matches!(
                key.code,
                KeyCode::Char('?') | KeyCode::F(1) | KeyCode::Char('q') | KeyCode::Esc
            ) {
                self.help_open = false;
            }
            return;
        }

        if let Some(settings) = &mut self.settings {
            match settings.on_key(key) {
                Action::Cancel => self.settings = None,
                Action::Save => self.save_settings(),
                Action::None => {}
            }
            return;
        }

        if let Some(dialog) = &mut self.wave_settings_dialog {
            match dialog.on_key(key) {
                WaveSettingsAction::Cancel => self.wave_settings_dialog = None,
                WaveSettingsAction::Apply => self.apply_wave_settings(),
                WaveSettingsAction::None => {}
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('b') {
            self.toggle_sidebar();
            return;
        }

        if self.focus == Focus::Sidebar {
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.sidebar.move_selection(1);
                    return;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.sidebar.move_selection(-1);
                    return;
                }
                KeyCode::Enter => {
                    let tab = self.sidebar.selected_view();
                    self.activate_tab(tab);
                    self.focus = Focus::Content;
                    self.sidebar.overlay_open = false;
                    return;
                }
                KeyCode::Esc if self.layout_mode != LayoutMode::Wide => {
                    self.sidebar.overlay_open = false;
                    self.focus = Focus::Content;
                    return;
                }
                _ => {}
            }
        }

        match key.code {
            KeyCode::Char('?') | KeyCode::F(1) => self.help_open = true,
            KeyCode::Char('o') => {
                let mut preferences = self.preferences.clone();
                preferences.volume = self.volume;
                self.settings = Some(Settings::new(preferences));
            }
            KeyCode::Char('w') if self.view == View::Wave => {
                if let (Some(settings), Some(choices)) = (&self.wave_settings, &self.wave_choices) {
                    self.wave_settings_dialog =
                        Some(WaveSettingsDialog::new(settings.clone(), choices.clone()));
                }
            }
            KeyCode::Char('R') if self.view == View::Wave && self.wave_is_custom() => {
                self.reset_wave_settings();
            }
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab | KeyCode::BackTab => self.toggle_focus(),
            KeyCode::Char('1') => self.activate_tab(Tab::Wave),
            KeyCode::Char('2') => self.activate_tab(Tab::Likes),
            KeyCode::Char('j') | KeyCode::Down => self.queue_mut(self.view).move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.queue_mut(self.view).move_cursor(-1),
            KeyCode::PageDown => self.queue_mut(self.view).move_cursor(10),
            KeyCode::PageUp => self.queue_mut(self.view).move_cursor(-10),
            KeyCode::Home | KeyCode::Char('g') => self.queue_mut(self.view).cursor = 0,
            KeyCode::End | KeyCode::Char('G') => {
                let queue = self.queue_mut(self.view);
                queue.cursor = queue.tracks.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                let (tab, index) = (self.view, self.queue(self.view).cursor);
                self.play_index(tab, index);
            }
            KeyCode::Char(' ') => {
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                {
                    self.pause_on_load = !self.pause_on_load;
                    self.audio.set_paused(self.pause_on_load);
                }
                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                self.audio.toggle_pause();
            }
            KeyCode::Char('n') => self.next_track(true),
            KeyCode::Char('b') => self.previous_track(),
            KeyCode::Right => self.audio.seek_by(SEEK_STEP as f64),
            KeyCode::Left => self.audio.seek_by(-SEEK_STEP as f64),
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_volume(VOLUME_STEP),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_volume(-VOLUME_STEP),
            KeyCode::Char('l') => self.toggle_like(),
            KeyCode::Char('s') => {
                self.shuffle = !self.shuffle;
                let message = if self.shuffle {
                    "Shuffling liked tracks"
                } else {
                    "Playing liked tracks in order"
                };
                self.set_notice(NoticeKind::Info, message);
            }
            KeyCode::Char('r') => {
                self.set_notice(NoticeKind::Info, "Refreshing liked tracks…");
                self.likes.placeholder = "Loading…".to_string();
                self.load_likes();
            }
            _ => {}
        }
    }

    fn save_settings(&mut self) {
        let values = self.settings.as_ref().expect("settings are open").values();
        let result = values.and_then(|values| {
            Config::save_preferences(&values)?;
            Ok(values)
        });
        let values = match result {
            Ok(values) => values,
            Err(error) => {
                self.settings.as_mut().expect("settings are open").error =
                    Some(format!("{error:#}"));
                return;
            }
        };
        let quality_changed = self.preferences.quality != values.quality;
        Arc::make_mut(&mut self.api).set_quality(&values.quality);
        self.cache_limit = values.cache_limit_mb * 1024 * 1024;
        self.volume = values.volume;
        self.audio.set_volume(values.volume);
        self.preferences = values;
        self.settings = None;
        self.set_notice(NoticeKind::Success, "Settings saved");
        if quality_changed && let Some(playing) = &self.playing {
            self.prefetch(playing.tab, playing.index);
        }
        let directory = self.cache_dir.clone();
        let limit = self.cache_limit;
        let keep = self.playing.as_ref().map(|p| p.track.id.clone());
        let events = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            if let Err(error) = cache::prune(&directory, limit, keep.as_deref()) {
                let _ = events.send(Message::Notice(format!("Cannot trim the cache: {error}")));
            }
        });
    }

    fn apply_wave_settings(&mut self) {
        let settings = self
            .wave_settings_dialog
            .take()
            .expect("Wave settings are open")
            .selected_settings();
        if self.wave_settings.as_ref() == Some(&settings) {
            self.set_notice(NoticeKind::Info, "Wave settings unchanged");
            return;
        }
        self.restart_wave_with(settings);
    }

    fn reset_wave_settings(&mut self) {
        self.restart_wave_with(WaveSettings::default());
    }

    fn restart_wave_with(&mut self, settings: WaveSettings) {
        self.close_wave_session();
        let autoplay = self
            .playing
            .as_ref()
            .is_some_and(|playing| playing.tab == Tab::Wave);
        self.wave_settings = Some(settings);
        self.wave_choices = None;
        self.wave_generation += 1;
        self.wave = Queue::default();
        self.wave_batch_id = None;
        self.wave_session_id = None;
        self.wave_requested = false;
        self.wave_feedbacks.clear();
        self.set_notice(NoticeKind::Info, "Starting a new Wave session…");
        self.start_wave(autoplay);
        self.load_wave_choices();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn on_media(&mut self, event: crate::media::Event) {
        use crate::media::Event;
        match event {
            Event::Play => {
                self.pause_on_load = false;
                if self.playing.is_some() {
                    self.audio.set_paused(false);
                } else {
                    self.play_index(self.view, self.queue(self.view).cursor);
                }
            }
            Event::Pause => {
                self.pause_on_load = true;
                self.audio.set_paused(true);
            }
            Event::Toggle => {
                if self.playing.is_some() {
                    self.pause_on_load = !self.pause_on_load;
                    self.audio.set_paused(self.pause_on_load);
                } else {
                    self.play_index(self.view, self.queue(self.view).cursor);
                }
            }
            Event::Next | Event::Previous => {
                let paused = self.pause_on_load;
                if event == Event::Next {
                    self.next_track(true);
                } else {
                    self.previous_track();
                }
                self.pause_on_load = paused;
                self.audio.set_paused(paused);
            }
            Event::Stop => {
                self.audio.stop();
                self.playing = None;
                self.wave.playing = None;
                self.likes.playing = None;
                self.loading_track = false;
                self.pause_on_load = false;
                self.wave_autoplay_pending = false;
                self.notice = None;
            }
            Event::SeekBy(seconds) => self.audio.seek_by(seconds),
            Event::SeekTo { epoch, position } => {
                if self.playing.as_ref().is_some_and(|playing| {
                    playing.epoch == epoch && position <= playing.track.duration
                }) && self.audio.state().epoch == epoch
                {
                    self.audio.seek_to(position);
                }
            }
            #[cfg(target_os = "linux")]
            Event::SetVolume(volume) => {
                if volume.is_finite() {
                    self.volume = volume.clamp(0.0, 2.0) as f32;
                    self.audio.set_volume(self.volume);
                }
            }
            #[cfg(target_os = "linux")]
            Event::Quit => self.should_quit = true,
        }
    }

    fn activate_tab(&mut self, tab: Tab) {
        self.view = tab;
        self.sidebar.selected = match tab {
            Tab::Wave => 0,
            Tab::Likes => 1,
        };
        if self.layout_mode != LayoutMode::Wide {
            self.sidebar.overlay_open = false;
        }
    }

    fn toggle_focus(&mut self) {
        match self.layout_mode {
            LayoutMode::Wide if self.sidebar.wide_visible => {
                self.focus = match self.focus {
                    Focus::Sidebar => Focus::Content,
                    Focus::Content => Focus::Sidebar,
                };
            }
            LayoutMode::Wide => self.focus = Focus::Content,
            LayoutMode::Compact | LayoutMode::Minimal => {
                if self.focus == Focus::Sidebar {
                    self.sidebar.overlay_open = false;
                    self.focus = Focus::Content;
                } else {
                    self.sidebar.overlay_open = true;
                    self.focus = Focus::Sidebar;
                }
            }
        }
    }

    fn toggle_sidebar(&mut self) {
        match self.layout_mode {
            LayoutMode::Wide => {
                self.sidebar.wide_visible = !self.sidebar.wide_visible;
                if !self.sidebar.wide_visible {
                    self.focus = Focus::Content;
                }
            }
            LayoutMode::Compact | LayoutMode::Minimal => {
                self.sidebar.overlay_open = !self.sidebar.overlay_open;
                self.focus = if self.sidebar.overlay_open {
                    Focus::Sidebar
                } else {
                    Focus::Content
                };
            }
        }
    }

    fn nudge_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 2.0);
        self.audio.set_volume(self.volume);
    }

    /// Likes the playing track, or the highlighted one if nothing plays.
    fn toggle_like(&mut self) {
        let track = self.playing.as_ref().map(|p| p.track.clone()).or_else(|| {
            self.queue(self.view)
                .tracks
                .get(self.queue(self.view).cursor)
                .cloned()
        });

        let Some(track) = track else { return };
        let liked = self.liked.contains(&track.id);

        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        let track_id = track.id.clone();
        tokio::spawn(async move {
            let result = if liked {
                api.unlike(&track_id).await
            } else {
                api.like(&track_id).await
            };
            let message = match result {
                Ok(()) => Message::LikeChanged {
                    track_id,
                    liked: !liked,
                },
                Err(e) => Message::Notice(format!("Like failed: {e:#}")),
            };
            let _ = tx.send(message);
        });
    }

    // --- For the UI -------------------------------------------------------

    pub fn audio_state(&self) -> crate::audio::AudioState {
        self.audio.state()
    }

    pub fn is_liked(&self, track_id: &str) -> bool {
        self.liked.contains(track_id)
    }

    pub fn wave_is_custom(&self) -> bool {
        self.wave_settings
            .as_ref()
            .is_some_and(|settings| !settings.is_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> App {
        App::new(
            Arc::new(Client::for_test()),
            Audio::for_test(0.8),
            PathBuf::new(),
            Preferences {
                quality: "high".into(),
                cache_limit_mb: 128,
                streaming: true,
                volume: 0.8,
            },
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn sidebar_selection_is_bounded_and_maps_to_views() {
        let mut sidebar = SidebarState::default();
        assert_eq!(sidebar.selected_view(), View::Wave);
        sidebar.move_selection(-1);
        assert_eq!(sidebar.selected, 0);
        sidebar.move_selection(1);
        assert_eq!(sidebar.selected_view(), View::Likes);
        sidebar.move_selection(1);
        assert_eq!(sidebar.selected, 1);
    }

    #[test]
    fn wide_sidebar_focuses_and_opens_a_view() {
        let mut app = test_app();
        app.layout_mode = LayoutMode::Wide;
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Sidebar);
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.view, View::Likes);
        assert_eq!(app.focus, Focus::Content);
    }

    #[test]
    fn compact_navigation_uses_an_overlay_and_shortcuts_stay_direct() {
        let mut app = test_app();
        app.layout_mode = LayoutMode::Compact;
        app.on_key(key(KeyCode::Tab));
        assert!(app.sidebar.overlay_open);
        assert_eq!(app.focus, Focus::Sidebar);
        app.on_key(key(KeyCode::Esc));
        assert!(!app.sidebar.overlay_open);
        assert_eq!(app.focus, Focus::Content);

        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.view, View::Likes);
        app.on_key(key(KeyCode::Char('1')));
        assert_eq!(app.view, View::Wave);
    }

    #[test]
    fn help_captures_input_until_it_is_closed() {
        let mut app = test_app();
        app.on_key(key(KeyCode::Char('?')));
        assert!(app.help_open);
        app.on_key(key(KeyCode::Char('2')));
        assert_eq!(app.view, View::Wave);
        app.on_key(key(KeyCode::Esc));
        assert!(!app.help_open);
    }
}
