//! Linux desktop media controls (MPRIS), independent of the terminal's focus.

use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use mpris_server::{Metadata, PlaybackStatus, Player, Time, TrackId};
use tokio::sync::{mpsc, watch};

use crate::app::{App, Message};

use super::{Event, Snapshot};

/// D-Bus has its own thread: publishing a snapshot never waits for the desktop.
/// A watch channel keeps only the latest state, even if the bus is slow.
pub struct Session {
    tx: Option<watch::Sender<Snapshot>>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    pub fn new(events: mpsc::UnboundedSender<Message>) -> Result<Self> {
        let (tx, rx) = watch::channel(Snapshot::default());
        let thread = std::thread::Builder::new()
            .name("tuiya-mpris".into())
            .spawn(move || {
                let result = (|| {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()?;
                    tokio::task::LocalSet::new().block_on(&runtime, run(rx, events.clone()))
                })();
                if let Err(error) = result {
                    let _ = events.send(Message::Notice(format!(
                        "System media controls unavailable: {error:#}"
                    )));
                }
            })
            .context("cannot start the MPRIS thread")?;
        Ok(Self {
            tx: Some(tx),
            thread: Some(thread),
        })
    }

    pub fn update(&self, app: &App) {
        if let Some(tx) = &self.tx {
            tx.send_replace(Snapshot::from_app(app));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // Closing the channel shuts down the service and releases its bus name.
        self.tx.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn track_id(epoch: u64) -> TrackId {
    TrackId::try_from(format!("/tuiya/track/{epoch}"))
        .expect("numeric epoch is a valid object path")
}

fn time(duration: Duration) -> Time {
    Time::from_micros(duration.as_micros().min(i64::MAX as u128) as i64)
}

async fn run(
    mut snapshots: watch::Receiver<Snapshot>,
    events: mpsc::UnboundedSender<Message>,
) -> Result<()> {
    // Unique names allow several terminals to run tuiya at the same time.
    let player = Player::builder(&format!("tuiya.instance{}", std::process::id()))
        .identity("tuiya")
        .can_quit(true)
        .can_raise(false)
        .can_control(true)
        .build()
        .await?;

    macro_rules! connect {
        ($method:ident, $event:expr) => {{
            let events = events.clone();
            player.$method(move |_| {
                let _ = events.send(Message::Media($event));
            });
        }};
    }
    connect!(connect_play, Event::Play);
    connect!(connect_pause, Event::Pause);
    connect!(connect_play_pause, Event::Toggle);
    connect!(connect_next, Event::Next);
    connect!(connect_previous, Event::Previous);
    connect!(connect_stop, Event::Stop);
    connect!(connect_quit, Event::Quit);
    let seek_events = events.clone();
    player.connect_seek(move |_, offset| {
        let _ = seek_events.send(Message::Media(Event::SeekBy(
            offset.as_micros() as f64 / 1_000_000.0,
        )));
    });
    let position_events = events.clone();
    player.connect_set_position(move |player, id, position| {
        let metadata = player.metadata();
        if metadata.trackid().as_ref() != Some(id) || position.is_negative() {
            return;
        }
        if metadata.length().is_some_and(|length| position > length) {
            return;
        }
        if let Some(epoch) = id
            .as_str()
            .rsplit('/')
            .next()
            .and_then(|id| id.parse().ok())
        {
            let _ = position_events.send(Message::Media(Event::SeekTo {
                epoch,
                position: Duration::from_micros(position.as_micros() as u64),
            }));
        }
    });
    player.connect_set_volume(move |_, volume| {
        let _ = events.send(Message::Media(Event::SetVolume(volume)));
    });

    let handler = player.run();
    tokio::pin!(handler);
    let mut epoch = None;
    let mut seek = None;
    loop {
        tokio::select! {
            _ = &mut handler => break,
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = snapshots.borrow_and_update().clone();
                publish(&player, &snapshot, &mut epoch, &mut seek).await?;
            }
        }
    }
    Ok(())
}

async fn publish(
    player: &Player,
    snapshot: &Snapshot,
    epoch: &mut Option<u64>,
    seek: &mut Option<(u64, u64)>,
) -> Result<()> {
    let audio = &snapshot.audio;
    let active = audio.loaded && !audio.ended;
    if let Some(playing) = &snapshot.playing
        && active
        && playing.epoch == audio.epoch
        && *epoch != Some(playing.epoch)
    {
        let metadata = Metadata::builder()
            .trackid(track_id(playing.epoch))
            .title(&playing.track.title)
            .artist([&playing.track.artists])
            .length(time(playing.track.duration))
            .build();
        player.set_metadata(metadata).await?;
        *epoch = Some(playing.epoch);
    } else if !active && epoch.take().is_some() {
        player.set_metadata(Metadata::new()).await?;
    }
    // While a new track downloads, rodio can still be playing the old one.
    // Keep its metadata until the decoder actually switches epochs.
    let status = if !active {
        PlaybackStatus::Stopped
    } else if audio.paused {
        PlaybackStatus::Paused
    } else {
        PlaybackStatus::Playing
    };
    let position = if active {
        time(audio.position)
    } else {
        Time::ZERO
    };
    player.set_position(position);
    if active
        && seek.is_some_and(|(epoch, serial)| epoch == audio.epoch && serial != audio.seek_serial)
    {
        player.seeked(position).await?;
    }
    *seek = active.then_some((audio.epoch, audio.seek_serial));
    player.set_playback_status(status).await?;
    player.set_volume(audio.volume as f64).await?;
    player.set_can_play(snapshot.can_play).await?;
    player.set_can_pause(active).await?;
    player.set_can_seek(active).await?;
    player.set_can_go_next(snapshot.playing.is_some()).await?;
    player
        .set_can_go_previous(
            snapshot
                .playing
                .as_ref()
                .is_some_and(|playing| playing.index > 0),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use mpris_server::zbus::zvariant::OwnedValue;
    use mpris_server::zbus::{Connection, Proxy};
    use tokio::time::{sleep, timeout};

    use super::*;
    use crate::api::models::Track;
    use crate::app::{Playing, Tab};
    use crate::audio::AudioState;

    async fn read_metadata(proxy: &Proxy<'_>) -> mpris_server::zbus::Result<Metadata> {
        let values: HashMap<String, OwnedValue> = proxy.get_property("Metadata").await?;
        let mut metadata = Metadata::new();
        for (key, value) in values {
            metadata.set_value(&key, Some(value.into()));
        }
        Ok(metadata)
    }

    async fn wait_for(proxy: &Proxy<'_>, title: Option<&str>, status: &str, volume: f64) {
        let mut last = String::new();
        let result = timeout(Duration::from_secs(5), async {
            loop {
                let metadata = read_metadata(proxy).await;
                let playback = proxy.get_property::<String>("PlaybackStatus").await;
                let actual_volume = proxy.get_property::<f64>("Volume").await;
                last = format!("{metadata:?}, {playback:?}, {actual_volume:?}");
                if let (Ok(metadata), Ok(playback), Ok(actual_volume)) =
                    (metadata, playback, actual_volume)
                    && metadata.title() == title
                    && playback == status
                    && (actual_volume - volume).abs() < 0.000_001
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            result.is_ok(),
            "Expected {title:?}, {status}, {volume}; got {last}"
        );
    }

    async fn receive(rx: &mut mpsc::UnboundedReceiver<Message>) -> Event {
        match timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap()
        {
            Message::Media(event) => event,
            Message::Notice(error) => panic!("MPRIS service failed: {error}"),
            _ => panic!("unexpected application message"),
        }
    }

    #[tokio::test]
    #[ignore = "requires a private session bus; run with dbus-run-session"]
    async fn mpris_controls() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = Session::new(tx).unwrap();
        let connection = Connection::session().await.unwrap();
        let destination = format!(
            "org.mpris.MediaPlayer2.tuiya.instance{}",
            std::process::id()
        );
        // Position deliberately has no PropertiesChanged signal in MPRIS.
        let proxy = mpris_server::zbus::proxy::Builder::<Proxy<'_>>::new(&connection)
            .destination(destination.as_str())
            .unwrap()
            .path("/org/mpris/MediaPlayer2")
            .unwrap()
            .interface("org.mpris.MediaPlayer2.Player")
            .unwrap()
            .cache_properties(mpris_server::zbus::proxy::CacheProperties::No)
            .build()
            .await
            .unwrap();
        let mut snapshot = Snapshot {
            audio: AudioState {
                epoch: 1,
                loaded: true,
                position: Duration::from_millis(12_500),
                volume: 1.25,
                ..Default::default()
            },
            playing: Some(Playing {
                tab: Tab::Wave,
                index: 1,
                track: Track {
                    id: "42".into(),
                    title: "First track".into(),
                    artists: "Artist".into(),
                    duration: Duration::from_secs(120),
                    available: true,
                },
                epoch: 1,
                reported: true,
            }),
            can_play: true,
        };
        session.tx.as_ref().unwrap().send_replace(snapshot.clone());
        wait_for(&proxy, Some("First track"), "Playing", 1.25).await;
        let metadata = read_metadata(&proxy).await.unwrap();
        assert_eq!(metadata.trackid(), Some(track_id(1)));
        assert_eq!(metadata.length(), Some(Time::from_secs(120)));
        assert_eq!(metadata.artist(), Some(vec!["Artist".into()]));
        assert_eq!(
            proxy.get_property::<i64>("Position").await.unwrap(),
            12_500_000
        );

        for (method, expected) in [
            ("Play", Event::Play),
            ("Play", Event::Play),
            ("Pause", Event::Pause),
            ("Pause", Event::Pause),
            ("PlayPause", Event::Toggle),
            ("Next", Event::Next),
            ("Previous", Event::Previous),
            ("Stop", Event::Stop),
        ] {
            proxy.call::<_, _, ()>(method, &()).await.unwrap();
            assert_eq!(receive(&mut rx).await, expected);
        }
        proxy
            .call::<_, _, ()>("Seek", &(-500_000i64,))
            .await
            .unwrap();
        assert_eq!(receive(&mut rx).await, Event::SeekBy(-0.5));
        proxy.set_property("Volume", 0.75f64).await.unwrap();
        assert_eq!(receive(&mut rx).await, Event::SetVolume(0.75));
        proxy
            .call::<_, _, ()>("SetPosition", &(track_id(1), 1_250_000i64))
            .await
            .unwrap();
        assert_eq!(
            receive(&mut rx).await,
            Event::SeekTo {
                epoch: 1,
                position: Duration::from_millis(1250),
            }
        );
        for (id, position) in [
            (track_id(0), 1_000_000i64),
            (track_id(1), -1),
            (track_id(1), 121_000_000),
        ] {
            proxy
                .call::<_, _, ()>("SetPosition", &(id, position))
                .await
                .unwrap();
            assert!(
                timeout(Duration::from_millis(100), rx.recv())
                    .await
                    .is_err()
            );
        }

        snapshot.audio.paused = true;
        snapshot.audio.volume = 0.75;
        session.tx.as_ref().unwrap().send_replace(snapshot.clone());
        wait_for(&proxy, Some("First track"), "Paused", 0.75).await;

        // Selecting the next track must not relabel sound from the old decoder.
        let playing = snapshot.playing.as_mut().unwrap();
        playing.epoch = 2;
        playing.track.title = "Second track".into();
        snapshot.audio.volume = 0.8;
        session.tx.as_ref().unwrap().send_replace(snapshot.clone());
        wait_for(&proxy, Some("First track"), "Paused", 0.8).await;

        snapshot.audio.epoch = 2;
        snapshot.audio.position = Duration::ZERO;
        session.tx.as_ref().unwrap().send_replace(snapshot.clone());
        wait_for(&proxy, Some("Second track"), "Paused", 0.8).await;
        assert_eq!(
            read_metadata(&proxy).await.unwrap().trackid(),
            Some(track_id(2))
        );

        let mut signals = proxy.receive_signal("Seeked").await.unwrap();
        snapshot.audio.position = Duration::from_millis(250);
        snapshot.audio.seek_serial = 1;
        session.tx.as_ref().unwrap().send_replace(snapshot.clone());
        use tokio_stream::StreamExt;
        let signal = timeout(Duration::from_secs(5), signals.next())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            signal.body().deserialize::<(Time,)>().unwrap(),
            (Time::from_millis(250),)
        );

        snapshot.audio.loaded = false;
        snapshot.playing = None;
        session.tx.as_ref().unwrap().send_replace(snapshot);
        wait_for(&proxy, None, "Stopped", 0.8).await;
        assert_eq!(proxy.get_property::<i64>("Position").await.unwrap(), 0);
        drop(signals);
        drop(proxy);
        drop(session);
        let bus = mpris_server::zbus::fdo::DBusProxy::new(&connection)
            .await
            .unwrap();
        assert!(
            !bus.name_has_owner(destination.as_str().try_into().unwrap())
                .await
                .unwrap()
        );
    }
}
