//! MPRIS, the D-Bus interface Linux desktops use to talk to media players.
//!
//! This is what makes the keyboard's play/pause key work. The compositor grabs
//! the `XF86Audio*` keys for itself, so unlike ordinary keys they never reach
//! the window — the desktop routes them over D-Bus to whichever player owns an
//! MPRIS bus name. Owning one also puts hark in the system media controls.
//!
//! Only the transport controls are exposed: play, pause, next, previous. The
//! spec's seeking methods are left out, and `CanSeek` says so.

use anyhow::Result;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use zbus::{connection, interface};

const PATH: &str = "/org/mpris/MediaPlayer2";
const BUS_NAME: &str = "org.mpris.MediaPlayer2.hark";

/// What the desktop asked the player to do.
#[derive(Clone, Copy)]
pub enum Command {
    TogglePlay,
    Play,
    Pause,
    Next,
    Previous,
    SetVolume(f32),
}

/// The slice of player state the desktop mirrors. Every change to it is
/// announced with a `PropertiesChanged` signal, so the playback position is
/// deliberately not part of it — that one moves 30 times a second and is
/// published separately, unannounced, as the spec requires.
#[derive(Clone, Default, PartialEq)]
pub struct State {
    pub playing: bool,
    pub volume: f32,
    pub track: Option<TrackInfo>,
}

#[derive(Clone, PartialEq)]
pub struct TrackInfo {
    /// Position in the queue. Only used to tell tracks apart on the bus.
    pub index: usize,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: Duration,
}

impl State {
    fn playback_status(&self) -> String {
        match (&self.track, self.playing) {
            (None, _) => "Stopped",
            (Some(_), true) => "Playing",
            (Some(_), false) => "Paused",
        }
        .to_string()
    }

    fn metadata(&self) -> HashMap<String, OwnedValue> {
        let Some(track) = &self.track else {
            return HashMap::new();
        };

        // `mpris:trackid` has to be an object path, and has to change when the
        // track does, or the desktop keeps showing the old one.
        let id = ObjectPath::try_from(format!("{PATH}/track/{}", track.index))
            .map(Value::from)
            .unwrap_or_else(|_| Value::from(ObjectPath::from_static_str_unchecked(PATH)));

        let entries = [
            ("mpris:trackid", id),
            ("mpris:length", Value::from(track.duration.as_micros() as i64)),
            ("xesam:title", Value::from(track.title.clone())),
            ("xesam:artist", Value::from(vec![track.artist.clone()])),
            ("xesam:album", Value::from(track.album.clone())),
        ];

        entries
            .into_iter()
            .filter_map(|(key, value)| Some((key.to_string(), OwnedValue::try_from(value).ok()?)))
            .collect()
    }
}

/// Hark's presence on the session bus. Dropping it releases the bus name.
pub struct Mpris {
    state: Arc<Mutex<State>>,
    /// Playback position in microseconds.
    position: Arc<AtomicU64>,
    commands: Receiver<Command>,
    /// Wakes the D-Bus thread to announce that `state` changed.
    wake: Sender<()>,
    /// What was last announced, so that unchanged ticks cost nothing.
    announced: State,
}

impl Mpris {
    pub fn new() -> Mpris {
        let state = Arc::new(Mutex::new(State::default()));
        let position = Arc::new(AtomicU64::new(0));
        let (command_tx, command_rx) = channel();
        let (wake_tx, wake_rx) = channel();

        let player = Player {
            state: state.clone(),
            position: position.clone(),
            commands: command_tx,
        };

        let thread = std::thread::Builder::new()
            .name("hark-mpris".into())
            .spawn(move || {
                if let Err(err) = serve(player, wake_rx) {
                    eprintln!("hark: the desktop media controls are unavailable: {err}");
                }
            });

        if let Err(err) = thread {
            eprintln!("hark: the desktop media controls are unavailable: {err}");
        }

        Mpris {
            state,
            position,
            commands: command_rx,
            wake: wake_tx,
            announced: State::default(),
        }
    }

    /// Everything the desktop asked for since the last call.
    pub fn commands(&self) -> Vec<Command> {
        self.commands.try_iter().collect()
    }

    /// Mirrors the player onto the bus. Called on every tick, so it stays quiet
    /// unless something the desktop shows has actually changed.
    pub fn publish(&mut self, state: State, position: Duration) {
        self.position
            .store(position.as_micros() as u64, Ordering::Relaxed);

        if state == self.announced {
            return;
        }

        // The new state has to be readable before the signal goes out, or the
        // desktop reads the properties back and finds the old one.
        if let Ok(mut shared) = self.state.lock() {
            shared.clone_from(&state);
        }
        self.announced = state;
        let _ = self.wake.send(());
    }
}

/// Serves the two MPRIS interfaces until the player is dropped.
///
/// zbus drives the connection on an executor thread of its own, so incoming
/// calls are answered while this thread sits on the channel, waking only to
/// announce a change.
fn serve(player: Player, wake: Receiver<()>) -> Result<()> {
    let builder = connection::Builder::session()?
        .name(BUS_NAME)?
        .serve_at(PATH, Root)?
        .serve_at(PATH, player)?;

    let connection = zbus::block_on(builder.build())?;
    let iface = zbus::block_on(connection.object_server().interface::<_, Player>(PATH))?;

    while wake.recv().is_ok() {
        zbus::block_on(async {
            let player = iface.get().await;
            let emitter = iface.signal_emitter();
            let _ = player.playback_status_changed(emitter).await;
            let _ = player.metadata_changed(emitter).await;
            let _ = player.volume_changed(emitter).await;
        });
    }

    Ok(())
}

/// `org.mpris.MediaPlayer2`: who we are. Hark neither quits nor raises its
/// window on request, and the `Can*` properties say so.
struct Root;

#[interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) {}

    fn quit(&self) {}

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn identity(&self) -> String {
        "hark".to_string()
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> String {
        "dev.milan.hark".to_string()
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

/// `org.mpris.MediaPlayer2.Player`: the transport controls the media keys hit.
///
/// The methods only queue a command — the player itself lives on the main
/// thread and picks them up on its next tick.
struct Player {
    state: Arc<Mutex<State>>,
    position: Arc<AtomicU64>,
    commands: Sender<Command>,
}

impl Player {
    fn state(&self) -> State {
        self.state.lock().map(|s| s.clone()).unwrap_or_default()
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn play_pause(&self) {
        self.send(Command::TogglePlay);
    }

    fn play(&self) {
        self.send(Command::Play);
    }

    fn pause(&self) {
        self.send(Command::Pause);
    }

    fn next(&self) {
        self.send(Command::Next);
    }

    fn previous(&self) {
        self.send(Command::Previous);
    }

    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.state().playback_status()
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        self.state().metadata()
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.state().volume as f64
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) {
        self.send(Command::SetVolume(volume.clamp(0.0, 1.0) as f32));
    }

    /// Changes continuously, so the spec forbids announcing it; the desktop
    /// polls this instead.
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.position.load(Ordering::Relaxed) as i64
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}
