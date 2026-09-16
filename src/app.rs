use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers};
use rand::Rng;
use ratatui::DefaultTerminal;
use ratatui::widgets::TableState;
use tokio::sync::mpsc;
use tokio_stream::StreamExt;

use crate::api::models::{Track, WaveBatch, WaveRestrictions, WaveSettings};
use crate::api::{Client, Feedback};
use crate::audio::Audio;
use crate::cache;
use crate::config::{Config, Preferences};
use crate::settings::{Action, Settings};
use crate::stream::TrackSource;
use crate::ui;
use crate::wave_settings::{Action as WaveSettingsAction, WaveSettingsDialog};

/// Redraw rate. Second-level progress would be plenty, but a smooth bar
/// looks more alive.
const FRAME: Duration = Duration::from_millis(100);
/// How many tracks before the end of the queue we ask the wave for more.
const WAVE_REFILL_MARGIN: usize = 2;
const SEEK_STEP: i64 = 5;
const VOLUME_STEP: f32 = 0.05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Wave,
    Likes,
}

impl Tab {
    pub fn title(self) -> &'static str {
        match self {
            Tab::Wave => "Wave",
            Tab::Likes => "Liked",
        }
    }
}

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
    WaveRestrictions(Result<WaveRestrictions>),
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

    pub tab: Tab,
    pub wave: Queue,
    pub likes: Queue,
    pub liked: HashSet<String>,
    pub playing: Option<Playing>,
    pub status: String,
    pub shuffle: bool,
    pub volume: f32,
    pub loading_track: bool,
    pub wave_settings: Option<WaveSettings>,
    pub wave_restrictions: Option<WaveRestrictions>,
    pub wave_settings_dialog: Option<WaveSettingsDialog>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pause_on_load: bool,
    /// Preferences last saved through the settings dialog.
    preferences: Preferences,
    pub settings: Option<Settings>,

    wave_batch_id: Option<String>,
    wave_session_id: Option<String>,
    wave_requested: bool,
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
            tab: Tab::Wave,
            wave: Queue::default(),
            likes: Queue::default(),
            liked: HashSet::new(),
            playing: None,
            status: "Starting the wave…".to_string(),
            shuffle: false,
            volume,
            loading_track: false,
            wave_settings: None,
            wave_restrictions: None,
            wave_settings_dialog: None,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pause_on_load: false,
            preferences,
            settings: None,
            wave_batch_id: None,
            wave_session_id: None,
            wave_requested: false,
            wave_generation: 0,
            wave_autoplay_pending: false,
            epoch: 0,
            should_quit: false,
            tx,
            rx,
        }
    }

    pub async fn run(mut self, mut terminal: DefaultTerminal) -> Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let media = match crate::media::Session::new(self.tx.clone()) {
            Ok(media) => Some(media),
            Err(error) => {
                self.status = format!("System media controls unavailable: {error:#}");
                None
            }
        };
        self.load_likes();
        self.load_wave_restrictions();
        self.start_wave(true);

        let mut ticker = tokio::time::interval(FRAME);
        let mut events = EventStream::new();

        while !self.should_quit {
            tokio::select! {
                _ = ticker.tick() => {
                    self.poll_audio();
                    #[cfg(any(target_os = "linux", target_os = "macos"))]
                    if let Some(media) = &media {
                        media.update(&self);
                    }
                    terminal.draw(|frame| ui::render(frame, &mut self))?;
                }
                Some(Ok(event)) = events.next() => {
                    if let TermEvent::Key(key) = event {
                        self.on_key(key);
                    }
                }
                Some(message) = self.rx.recv() => self.on_message(message),
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

    fn load_wave_restrictions(&self) {
        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(Message::WaveRestrictions(api.wave_restrictions().await));
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
        let Some(last) = self.wave.tracks.last().map(|t| t.id.clone()) else {
            return;
        };
        let Some(session_id) = self.wave_session_id.clone() else {
            return;
        };
        self.wave_requested = true;

        let api = Arc::clone(&self.api);
        let tx = self.tx.clone();
        let generation = self.wave_generation;
        tokio::spawn(async move {
            let _ = tx.send(Message::Wave {
                generation,
                result: api.wave_tracks(&session_id, &last).await,
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
            self.status = "No playable tracks left".to_string();
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
        self.status = format!("Loading \"{}\"…", track.label());
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
                    self.status = "The wave is picking the next tracks…".to_string();
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
        let batch_id = self.wave_batch_id.clone();
        tokio::spawn(async move {
            let _ = api
                .wave_feedback(&session_id, batch_id.as_deref(), event)
                .await;
        });
    }

    fn next_track(&mut self, skipped: bool) {
        let Some(playing) = self.playing.clone() else {
            return;
        };
        if playing.tab == Tab::Wave {
            let played_secs = self.played_secs();
            let track_id = playing.track.id.clone();
            self.report(if skipped {
                Feedback::Skip {
                    track_id,
                    played_secs,
                }
            } else {
                Feedback::TrackFinished {
                    track_id,
                    played_secs,
                }
            });
        }
        self.advance(playing.tab, playing.index);
    }

    fn previous_track(&mut self) {
        let Some(playing) = self.playing.clone() else {
            return;
        };
        if playing.index == 0 {
            self.status = "This is the first track in the queue".to_string();
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
            self.status = notice;
        }

        if let Some(error) = state.error {
            self.status = format!("Cannot play \"{}\": {error}", playing.track.label());
            self.loading_track = false;
            // One broken file is no reason to stop everything.
            self.advance(playing.tab, playing.index);
            return;
        }

        if state.loaded && !playing.reported {
            self.loading_track = false;
            self.status = String::new();
            if let Some(current) = self.playing.as_mut() {
                current.reported = true;
            }
            if playing.tab == Tab::Wave {
                self.report(Feedback::TrackStarted {
                    track_id: playing.track.id.clone(),
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
                self.status = format!("The wave is not answering: {reason}");
            }
            Message::Wave { .. } => {}
            Message::WaveRestrictions(Ok(restrictions)) => {
                if let Some(defaults) = restrictions.defaults() {
                    self.wave_settings = Some(defaults);
                    self.wave_restrictions = Some(restrictions);
                }
            }
            // Tuning is optional. A failure leaves the default Wave available
            // without exposing an incomplete settings dialog.
            Message::WaveRestrictions(Err(_)) => {}
            Message::Likes(Ok((tracks, liked))) => {
                self.likes.tracks = tracks;
                self.liked = liked;
                self.likes.placeholder = "Nothing liked yet".to_string();
                self.status = format!("{} liked tracks", self.likes.tracks.len());
            }
            Message::Likes(Err(e)) => {
                let reason = format!("{e:#}");
                self.likes.placeholder = format!("Liked tracks failed to load: {reason}");
                self.status = format!("Liked tracks failed to load: {reason}");
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
                self.status = format!("Track download failed: {error}");
                // A single failed track must not stall the wave.
                if let Some(playing) = self.playing.clone() {
                    self.advance(playing.tab, playing.index);
                }
            }
            Message::LikeChanged { track_id, liked } => {
                if liked {
                    self.liked.insert(track_id);
                    self.status = "Liked".to_string();
                } else {
                    self.liked.remove(&track_id);
                    self.status = "Like removed".to_string();
                }
            }
            Message::Notice(text) => self.status = text,
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

        match key.code {
            KeyCode::Char('o') => {
                let mut preferences = self.preferences.clone();
                preferences.volume = self.volume;
                self.settings = Some(Settings::new(preferences));
            }
            KeyCode::Char('w') if self.tab == Tab::Wave => {
                if let (Some(settings), Some(restrictions)) =
                    (&self.wave_settings, &self.wave_restrictions)
                {
                    self.wave_settings_dialog = Some(WaveSettingsDialog::new(
                        settings.clone(),
                        restrictions.clone(),
                    ));
                }
            }
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab | KeyCode::BackTab => self.switch_tab(),
            KeyCode::Char('1') => self.tab = Tab::Wave,
            KeyCode::Char('2') => self.tab = Tab::Likes,
            KeyCode::Char('j') | KeyCode::Down => self.queue_mut(self.tab).move_cursor(1),
            KeyCode::Char('k') | KeyCode::Up => self.queue_mut(self.tab).move_cursor(-1),
            KeyCode::PageDown => self.queue_mut(self.tab).move_cursor(10),
            KeyCode::PageUp => self.queue_mut(self.tab).move_cursor(-10),
            KeyCode::Home | KeyCode::Char('g') => self.queue_mut(self.tab).cursor = 0,
            KeyCode::End | KeyCode::Char('G') => {
                let queue = self.queue_mut(self.tab);
                queue.cursor = queue.tracks.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                let (tab, index) = (self.tab, self.queue(self.tab).cursor);
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
                self.status = if self.shuffle {
                    "Shuffling liked tracks".to_string()
                } else {
                    "Playing liked tracks in order".to_string()
                };
            }
            KeyCode::Char('r') => {
                self.status = "Refreshing liked tracks…".to_string();
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
        self.status = "Settings saved".into();
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
            .draft;
        if self.wave_settings.as_ref() == Some(&settings) {
            self.status = "Wave settings unchanged".into();
            return;
        }
        let autoplay = self
            .playing
            .as_ref()
            .is_some_and(|playing| playing.tab == Tab::Wave);
        self.wave_settings = Some(settings);
        self.wave_generation += 1;
        self.wave = Queue::default();
        self.wave_batch_id = None;
        self.wave_session_id = None;
        self.wave_requested = false;
        self.status = "Starting a new Wave session…".into();
        self.start_wave(autoplay);
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
                    self.play_index(self.tab, self.queue(self.tab).cursor);
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
                    self.play_index(self.tab, self.queue(self.tab).cursor);
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
                self.status.clear();
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

    fn switch_tab(&mut self) {
        self.tab = match self.tab {
            Tab::Wave => Tab::Likes,
            Tab::Likes => Tab::Wave,
        };
    }

    fn nudge_volume(&mut self, delta: f32) {
        self.volume = (self.volume + delta).clamp(0.0, 2.0);
        self.audio.set_volume(self.volume);
    }

    /// Likes the playing track, or the highlighted one if nothing plays.
    fn toggle_like(&mut self) {
        let track = self.playing.as_ref().map(|p| p.track.clone()).or_else(|| {
            self.queue(self.tab)
                .tracks
                .get(self.queue(self.tab).cursor)
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
}
