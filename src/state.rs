//! What the last session was doing, kept in `~/.local/state/hark/state.json`.
//!
//! State, not cache: the covers under `~/.cache` can always be fetched again and
//! the records on the files can always be recomputed, but where you stopped
//! listening cannot be reconstructed from anything. That is why this lives under
//! `XDG_STATE_HOME` rather than beside them.
//!
//! Deliberately not in an extended attribute like everything else hark
//! remembers. This is about the player rather than about any one track, and it
//! is rewritten every few seconds while music plays — which is the last thing an
//! attribute on a file on a network mount should be asked to absorb.
//!
//! Nothing here reports failure, in the same spirit as [`crate::meta_cache`]: a
//! missing file, an unwritable directory, a half-written record and a record
//! from another version all mean the same thing to the caller — start as hark
//! always did. Delete the file to forget everything.

use crate::queue::Repeat;
use gpui::{Bounds, Pixels, Point, Size, px, size};
use std::path::PathBuf;
use std::time::Duration;

/// The volume hark opens at when it has never been told otherwise.
pub const DEFAULT_VOLUME: f32 = 0.7;

/// Bumped when the record's shape changes; another version is ignored and
/// overwritten by the next save. That is the whole migration strategy.
const VERSION: u64 = 1;

/// The window hark opens at when it has nothing to go on.
pub const DEFAULT_SIZE: Size<Pixels> = Size {
    width: px(420.),
    height: px(690.),
};

/// Smallest window the player lays out in, matching `window_min_size`. A stored
/// size under it was not written by a working hark.
const MIN_SIZE: Size<Pixels> = Size {
    width: px(360.),
    height: px(560.),
};

/// Larger than any real display, so a nonsense record cannot open a window that
/// cannot be closed.
const MAX_EDGE: Pixels = px(10_000.);

#[derive(Clone, PartialEq, Debug)]
pub struct State {
    /// The file that was loaded. A path rather than an index: the queue is
    /// rebuilt by a scan whose ordering nothing stored here can predict.
    pub track: Option<PathBuf>,
    /// Where in the *file*, never within a chapter. A chaptered album is one
    /// file to the player, so seeking to the absolute position lands the view in
    /// the right chapter by itself.
    pub position: Duration,
    pub volume: f32,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub playlist: bool,
    /// The library roots the listener added, in the order they were added. The
    /// XDG music folder is deliberately not among them: it is scanned and
    /// watched on every start regardless, so keeping a copy here would only
    /// produce a record that disagrees with the folder the desktop points at.
    pub folders: Vec<PathBuf>,
    /// `None` when nothing was stored, or when what was stored was absurd.
    pub window: Option<Bounds<Pixels>>,
}

impl Default for State {
    fn default() -> State {
        State {
            track: None,
            position: Duration::ZERO,
            volume: DEFAULT_VOLUME,
            shuffle: false,
            repeat: Repeat::Off,
            playlist: false,
            folders: Vec::new(),
            window: None,
        }
    }
}

/// `$XDG_STATE_HOME/hark`, else `~/.local/state/hark`. Mirrors the hand-rolled
/// XDG resolution in `library::music_dir`. Not created here — [`save`] creates
/// it lazily.
fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Some(dir.join("hark"));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join(".local/state/hark"))
}

fn state_file() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("state.json"))
}

/// What the last session left behind. Anything unreadable is the default, which
/// is how hark behaved before it remembered anything at all.
pub fn load() -> State {
    let Some(path) = state_file() else {
        return State::default();
    };
    let Ok(bytes) = std::fs::read(&path) else {
        return State::default();
    };
    decode(&bytes).unwrap_or_default()
}

/// Best effort. Written under a temporary name and renamed over the old one, so
/// an interrupted write cannot leave half a record behind — the same trick
/// `artwork::store` uses, and the one thing `setxattr` gives the other caches
/// for free.
pub fn save(state: &State) {
    let (Some(dir), Some(path), Some(bytes)) = (state_dir(), state_file(), encode(state)) else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }

    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

fn encode(state: &State) -> Option<Vec<u8>> {
    let mut record = serde_json::json!({
        "v": VERSION,
        "position_ms": state.position.as_millis() as u64,
        "volume": state.volume,
        "shuffle": state.shuffle,
        "repeat": match state.repeat {
            Repeat::Off => "off",
            Repeat::All => "all",
            Repeat::One => "one",
        },
        "playlist": state.playlist,
    });
    let object = record.as_object_mut()?;

    // A path that is not UTF-8 is left out and the rest of the session still
    // saves. `Track::from_meta` already falls back to a placeholder name for
    // such a file, and base64 in a file meant to be readable is a poor trade.
    if let Some(track) = state.track.as_deref().and_then(|path| path.to_str()) {
        object.insert("track".to_string(), track.into());
    }

    // Same rule as `track`, and the key is written even when the list is empty:
    // an explicit `[]` tells anyone reading the file that hark knows about
    // folders and has none, which a missing key does not.
    let folders: Vec<&str> = state
        .folders
        .iter()
        .filter_map(|folder| folder.to_str())
        .collect();
    object.insert("folders".to_string(), folders.into());

    if let Some(window) = state.window {
        object.insert(
            "window".to_string(),
            serde_json::json!({
                "x": f32::from(window.origin.x),
                "y": f32::from(window.origin.y),
                "width": f32::from(window.size.width),
                "height": f32::from(window.size.height),
            }),
        );
    }

    serde_json::to_vec_pretty(&record).ok()
}

/// Reads the record back. Unlike the caches on the files, a field that makes no
/// sense costs only itself: the record is voided outright only when it is not
/// JSON or not this version. A tag record voids whole because half a chapter
/// list actively misleads the seek bar, but these fields are unrelated, and
/// throwing away a good volume over a bad window height would be the wrong
/// trade for a convenience file.
fn decode(bytes: &[u8]) -> Option<State> {
    let record = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    if record["v"].as_u64() != Some(VERSION) {
        return None;
    }

    Some(State {
        track: record["track"].as_str().map(PathBuf::from),
        position: Duration::from_millis(record["position_ms"].as_u64().unwrap_or(0)),
        volume: record["volume"]
            .as_f64()
            .map(|volume| (volume as f32).clamp(0.0, 1.0))
            .unwrap_or(DEFAULT_VOLUME),
        shuffle: record["shuffle"].as_bool().unwrap_or(false),
        repeat: match record["repeat"].as_str() {
            Some("all") => Repeat::All,
            Some("one") => Repeat::One,
            // Including a mode from some future version: off is the safe read.
            _ => Repeat::Off,
        },
        playlist: record["playlist"].as_bool().unwrap_or(false),
        // A record written before hark remembered folders has no key here and
        // decodes to no folders, which is exactly what it meant. That is why
        // the field costs no version bump — and a bump would have thrown away
        // everyone's position to add a list they did not have yet.
        folders: record["folders"]
            .as_array()
            .map(|folders| {
                folders
                    .iter()
                    .filter_map(|folder| folder.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default(),
        window: window(&record["window"]),
    })
}

/// The stored window, if it describes one a person could use. A size below the
/// player's own minimum or past any real display was not written by a working
/// hark, and honouring it would open a window that cannot be used or closed.
fn window(record: &serde_json::Value) -> Option<Bounds<Pixels>> {
    let number = |key: &str| record[key].as_f64().map(|value| value as f32);
    let (x, y, width, height) = (
        number("x")?,
        number("y")?,
        number("width")?,
        number("height")?,
    );

    if ![x, y, width, height].iter().all(|value| value.is_finite()) {
        return None;
    }
    if width < f32::from(MIN_SIZE.width)
        || height < f32::from(MIN_SIZE.height)
        || width > f32::from(MAX_EDGE)
        || height > f32::from(MAX_EDGE)
    {
        return None;
    }

    Some(Bounds {
        origin: Point { x: px(x), y: px(y) },
        size: size(px(width), px(height)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> State {
        State {
            track: Some(PathBuf::from("/home/milan/Music/Forndom - Flykt.opus")),
            position: Duration::from_millis(913_500),
            volume: 0.42,
            shuffle: true,
            repeat: Repeat::One,
            playlist: true,
            folders: vec![
                PathBuf::from("/home/milan/hamstor/Music"),
                PathBuf::from("/home/milan/Downloads/new"),
            ],
            window: Some(Bounds {
                origin: Point {
                    x: px(120.),
                    y: px(64.),
                },
                size: size(px(480.), px(720.)),
            }),
        }
    }

    fn round_trip(state: &State) -> State {
        let Some(bytes) = encode(state) else {
            panic!("encode refused a state");
        };
        let Some(decoded) = decode(&bytes) else {
            panic!("decode refused its own output");
        };
        decoded
    }

    #[test]
    fn a_full_session_survives_a_round_trip() {
        let state = round_trip(&full());

        assert_eq!(
            state.track.as_deref(),
            Some(std::path::Path::new("/home/milan/Music/Forndom - Flykt.opus"))
        );
        assert_eq!(state.position, Duration::from_millis(913_500));
        assert_eq!(state.volume, 0.42);
        assert!(state.shuffle);
        assert_eq!(state.repeat, Repeat::One);
        assert!(state.playlist);
        assert_eq!(
            state.folders,
            vec![
                PathBuf::from("/home/milan/hamstor/Music"),
                PathBuf::from("/home/milan/Downloads/new"),
            ]
        );

        let window = state.window.expect("a usable window was dropped");
        assert_eq!(window.origin.x, px(120.));
        assert_eq!(window.size.width, px(480.));
        assert_eq!(window.size.height, px(720.));
    }

    #[test]
    fn a_session_that_never_played_anything_survives_too() {
        let state = round_trip(&State::default());

        assert!(state.track.is_none());
        assert_eq!(state.position, Duration::ZERO);
        assert_eq!(state.volume, DEFAULT_VOLUME);
        assert!(state.folders.is_empty());
        assert!(state.window.is_none());
    }

    /// The whole case for adding `folders` without bumping `VERSION`: a record
    /// written before hark remembered folders still restores the session it does
    /// describe, and simply has no folders in it.
    #[test]
    fn a_record_from_before_folders_were_remembered_still_loads() {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "v": VERSION,
            "volume": 0.3,
            "track": "/music/x.flac",
        }))
        .expect("the fixture did not serialize");

        let state = decode(&bytes).expect("an old record was refused outright");
        assert!(state.folders.is_empty());
        assert_eq!(state.volume, 0.3);
        assert!(state.track.is_some());
    }

    #[test]
    fn a_folder_that_is_not_a_path_is_dropped_without_losing_the_rest() {
        for (folders, expected) in [
            (
                serde_json::json!(["/music/a", 7, null, "/music/b"]),
                vec![PathBuf::from("/music/a"), PathBuf::from("/music/b")],
            ),
            (serde_json::json!("nonsense"), Vec::new()),
            (serde_json::json!(42), Vec::new()),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "v": VERSION,
                "volume": 0.3,
                "folders": folders,
            }))
            .expect("the fixture did not serialize");

            let state = decode(&bytes).expect("a bad folder list voided the whole record");
            assert_eq!(state.folders, expected);
            assert_eq!(state.volume, 0.3);
        }
    }

    /// The same trade the `track` field makes: base64 in a file meant to be read
    /// by a person would cost more than the folder it saves.
    #[test]
    fn a_folder_whose_name_is_not_utf8_is_left_out() {
        use std::os::unix::ffi::OsStringExt;

        let state = State {
            folders: vec![
                PathBuf::from("/music/a"),
                PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff, 0xfe])),
                PathBuf::from("/music/b"),
            ],
            ..State::default()
        };

        let decoded = round_trip(&state);
        assert_eq!(
            decoded.folders,
            vec![PathBuf::from("/music/a"), PathBuf::from("/music/b")]
        );
    }

    #[test]
    fn another_version_is_a_miss() {
        let bytes = serde_json::to_vec(&serde_json::json!({ "v": VERSION + 1, "volume": 0.1 }))
            .expect("the fixture did not serialize");

        assert!(decode(&bytes).is_none());
    }

    #[test]
    fn nonsense_decodes_to_nothing_without_panicking() {
        for bytes in [
            &b""[..],
            &b"not json"[..],
            &b"{"[..],
            &b"[]"[..],
            &b"null"[..],
            &b"\"\""[..],
        ] {
            assert!(decode(bytes).is_none());
        }
    }

    /// The whole point of decoding field by field: one nonsensical value must
    /// not cost the others.
    #[test]
    fn an_absurd_window_is_dropped_without_losing_the_session() {
        for window in [
            serde_json::json!({ "x": 0, "y": 0, "width": 1, "height": 1 }),
            serde_json::json!({ "x": 0, "y": 0, "width": 99_999, "height": 720 }),
            serde_json::json!({ "x": 0, "y": 0, "width": 480 }),
            serde_json::json!("nonsense"),
        ] {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "v": VERSION,
                "volume": 0.3,
                "track": "/music/x.flac",
                "window": window,
            }))
            .expect("the fixture did not serialize");

            let state = decode(&bytes).expect("a bad window voided the whole record");
            assert!(state.window.is_none());
            assert_eq!(state.volume, 0.3);
            assert!(state.track.is_some());
        }
    }

    #[test]
    fn an_unknown_repeat_mode_falls_back_to_off() {
        for repeat in [
            serde_json::json!("shuffle-all-forever"),
            serde_json::json!(7),
            serde_json::Value::Null,
        ] {
            let bytes =
                serde_json::to_vec(&serde_json::json!({ "v": VERSION, "repeat": repeat }))
                    .expect("the fixture did not serialize");

            let state = decode(&bytes).expect("a bad repeat mode voided the whole record");
            assert_eq!(state.repeat, Repeat::Off);
        }
    }

    #[test]
    fn a_volume_outside_the_range_is_clamped() {
        for (stored, expected) in [(-3.0, 0.0), (17.0, 1.0)] {
            let bytes = serde_json::to_vec(&serde_json::json!({ "v": VERSION, "volume": stored }))
                .expect("the fixture did not serialize");

            let state = decode(&bytes).expect("a bad volume voided the whole record");
            assert_eq!(state.volume, expected);
        }
    }
}
