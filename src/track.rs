use crate::meta_cache;
use gpui::{Image, SharedString};
use lofty::config::ParseOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::ogg::{OpusFile, VorbisComments, VorbisFile};
use lofty::prelude::Accessor;
use lofty::probe::Probe;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Placeholder metadata for a file with no artist or album tag. Also guards the
/// cover fetch: these are not real names, so they must not key a lookup.
pub const UNKNOWN_ARTIST: &str = "Unknown artist";
pub const UNKNOWN_ALBUM: &str = "Unknown album";

/// One chapter of a track — a full-album file marks each song this way. `end` is
/// the next chapter's start, or the track's end for the last one.
#[derive(Clone)]
pub struct Chapter {
    pub title: SharedString,
    pub start: Duration,
    pub end: Duration,
}

/// Everything about a file that survives a rescan: what its tags said, how long
/// it plays, where its chapters are and whether it carries a picture. This is
/// exactly the record `meta_cache` keeps in the file's extended attribute, so a
/// cache hit and a fresh parse produce the same `Track`.
#[derive(Clone, Default)]
pub struct Meta {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub duration: Duration,
    pub chapters: Vec<Chapter>,
    /// The file carries a picture hark can display. The picture itself is not
    /// read here: a scan would pull in the image bytes of the whole library for
    /// the sake of the one track that is playing.
    pub has_art: bool,
}

#[derive(Clone)]
pub struct Track {
    pub path: PathBuf,
    pub title: SharedString,
    pub artist: SharedString,
    pub album: SharedString,
    pub duration: Duration,
    /// The cover, once something has actually fetched or decoded one. A freshly
    /// scanned track has none even when its file holds one — see `has_art`.
    pub art: Option<Arc<Image>>,
    /// Embedded chapters, if any — empty for an ordinary single-song file.
    pub chapters: Vec<Chapter>,
    /// The file has a cover of its own, to be decoded when it starts playing.
    pub has_art: bool,
}

pub const SUPPORTED: [&str; 8] = ["mp3", "flac", "ogg", "oga", "wav", "m4a", "aac", "opus"];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

impl Track {
    /// Reads a track, preferring the record the last scan left in the file's
    /// extended attribute — on a hit the file is only stat'ed, never opened,
    /// which is what makes a rescan free on a network mount. The returned flag
    /// says whether that happened, so a scan can report how much it saved.
    pub fn load(path: PathBuf) -> (Track, bool) {
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|metadata| meta_cache::stamp(&metadata));

        if let Some(stamp) = stamp
            && let Some(meta) = meta_cache::read(&path, stamp)
        {
            return (Track::from_meta(path, meta), true);
        }

        let Some(meta) = parse(&path) else {
            // The file could not be read at all. That is a transient condition —
            // a half-copied file, a hiccup on the mount — and must not be frozen
            // into the file as a record saying it has nothing.
            return (Track::from_meta(path, Meta::default()), false);
        };

        if let Some(stamp) = stamp {
            meta_cache::write(&path, stamp, &meta);
        }
        (Track::from_meta(path, meta), false)
    }

    /// The one way a `Track` is built, so a cache hit and a cold parse cannot
    /// drift apart. Falls back to the file name when the file carries no title —
    /// which is why the cached record leaves an absent tag out instead of
    /// storing a placeholder: a renamed file has to follow its new name.
    pub fn from_meta(path: PathBuf, meta: Meta) -> Track {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown track")
            .to_string();

        Track {
            title: named(meta.title, stem),
            artist: named(meta.artist, UNKNOWN_ARTIST),
            album: named(meta.album, UNKNOWN_ALBUM),
            duration: meta.duration,
            art: None,
            chapters: meta.chapters,
            has_art: meta.has_art,
            path,
        }
    }
}

/// A tag's value, or the given stand-in when the file carried none.
fn named(value: Option<String>, fallback: impl Into<SharedString>) -> SharedString {
    match value {
        Some(value) => value.into(),
        None => fallback.into(),
    }
}

/// Reads the file itself. `None` means it could not be opened or parsed, and the
/// caller must not cache that. A file that opens cleanly but carries no tag is a
/// definitive answer rather than a failure, and comes back as `Some`.
fn parse(path: &Path) -> Option<Meta> {
    let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) else {
        return None;
    };

    let duration = tagged.properties().duration();
    let mut meta = Meta {
        duration,
        chapters: read_chapters(path, duration),
        ..Meta::default()
    };

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let Some(tag) = tag else { return Some(meta) };

    meta.title = tag
        .title()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.to_string());
    meta.artist = tag
        .artist()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.to_string());
    meta.album = tag
        .album()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.to_string());

    // Only whether a usable picture is there; the bytes stay in the file until
    // the track actually plays. Sniffing rather than decoding also keeps the
    // record honest — a tag holding nothing but a BMP does not decode here, so
    // recording it as art would send the player after bytes it cannot use.
    meta.has_art = tag
        .pictures()
        .iter()
        .any(|picture| crate::artwork::sniff(picture.data()).is_some());

    Some(meta)
}

/// Decodes the first usable picture embedded in a file. This opens the file, so
/// it is only ever called for the track about to play: the scan records that a
/// picture exists (`Track::has_art`) without keeping a single byte of it.
pub fn embedded_art(path: &Path) -> Option<Arc<Image>> {
    let Ok(tagged) = Probe::open(path).and_then(|p| p.read()) else {
        return None;
    };
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    tag.pictures()
        .iter()
        .find_map(|picture| crate::artwork::decode(picture.data().to_vec()))
}

/// `1:06`, and `-1:49` for the remaining side.
pub fn format_time(d: Duration) -> String {
    let total = d.as_secs();
    format!("{}:{:02}", total / 60, total % 60)
}

/// Reads embedded chapters — the Vorbis-comment `CHAPTERxxx` / `CHAPTERxxxNAME`
/// convention that ffmpeg and mpv write. Only Ogg Opus and Vorbis carry them
/// here; a file with fewer than two chapters is treated as having none.
fn read_chapters(path: &Path, track_duration: Duration) -> Vec<Chapter> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();

    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    let mut reader = BufReader::new(file);
    let options = ParseOptions::new();

    let marks = match extension.as_str() {
        "opus" => OpusFile::read_from(&mut reader, options)
            .ok()
            .map(|f| collect_marks(f.vorbis_comments())),
        "ogg" | "oga" => VorbisFile::read_from(&mut reader, options)
            .ok()
            .map(|f| collect_marks(f.vorbis_comments())),
        _ => None,
    };

    let Some(mut marks) = marks else {
        return Vec::new();
    };
    if marks.len() < 2 {
        return Vec::new();
    }

    marks.sort_by_key(|(start, _)| *start);
    (0..marks.len())
        .map(|i| {
            let (start, title) = marks[i].clone();
            // The last chapter runs to the end of the file.
            let end = marks
                .get(i + 1)
                .map(|(next, _)| *next)
                .unwrap_or(track_duration)
                .max(start);
            Chapter { title, start, end }
        })
        .collect()
}

/// Pairs `CHAPTERnnn` start times with their `CHAPTERnnnNAME` titles.
fn collect_marks(comments: &VorbisComments) -> Vec<(Duration, SharedString)> {
    let mut starts: HashMap<u32, Duration> = HashMap::new();
    let mut names: HashMap<u32, String> = HashMap::new();

    for (key, value) in comments.items() {
        let key = key.to_ascii_uppercase();
        let Some(rest) = key.strip_prefix("CHAPTER") else {
            continue;
        };
        if let Some(digits) = rest.strip_suffix("NAME") {
            if let Ok(n) = digits.parse::<u32>() {
                names.insert(n, value.to_string());
            }
        } else if let Ok(n) = rest.parse::<u32>() {
            if let Some(start) = parse_timecode(value) {
                starts.insert(n, start);
            }
        }
    }

    starts
        .into_iter()
        .map(|(n, start)| {
            let title = names
                .get(&n)
                .map(|name| clean_title(name))
                .unwrap_or_else(|| format!("Chapter {}", n + 1));
            (start, SharedString::from(title))
        })
        .collect()
}

/// Parses a `HH:MM:SS.mmm` chapter timecode.
fn parse_timecode(value: &str) -> Option<Duration> {
    let (hms, frac) = value.split_once('.').unwrap_or((value, ""));
    let mut parts = hms.split(':');
    let hours: u64 = parts.next()?.trim().parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds: u64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    // Pad or trim the fraction to exactly milliseconds.
    let millis: u64 = if frac.is_empty() {
        0
    } else {
        let mut f = frac.to_string();
        f.truncate(3);
        while f.len() < 3 {
            f.push('0');
        }
        f.parse().ok()?
    };

    Some(Duration::from_millis(
        ((hours * 60 + minutes) * 60 + seconds) * 1000 + millis,
    ))
}

/// Drops a leading `NN. ` track-number prefix — it duplicates the position
/// counter the UI already shows.
fn clean_title(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((num, rest)) = trimmed.split_once(". ")
        && !num.is_empty()
        && num.bytes().all(|b| b.is_ascii_digit())
    {
        return rest.trim().to_string();
    }
    trimmed.to_string()
}
