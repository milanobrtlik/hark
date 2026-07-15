//! Identifies badly-tagged files by their sound, so a cover and correct
//! metadata can be recovered where the tag→iTunes path can't help.
//!
//! Flow: a Chromaprint acoustic fingerprint of the decoded audio → an AcoustID
//! lookup (free web service, ≤3 req/s) → a MusicBrainz release-group MBID plus
//! the correct artist, album and per-recording title. A full-album file is
//! fingerprinted one chapter at a time, so every song's title is recovered and
//! the whole file shares one album cover.
//!
//! This is the heavier "identify unknown music" path, so it runs only as a
//! fallback, on the GPUI background executor, and each file's result is cached.

use crate::artwork;
use crate::decode;
use crate::track::Track;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rodio::Source;
use rusty_chromaprint::{Configuration, FingerprintCompressor, Fingerprinter};
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The AcoustID application key, baked in from `.env` at build time (see
/// `build.rs`). `None` when none was configured — the caller then skips the
/// fallback entirely, so this whole module compiles to dead weight rather than
/// panicking a keyless build.
pub const API_KEY: Option<&str> = option_env!("ACOUSTID_API_KEY");

/// Longest stretch of one recording that gets fingerprinted, matching fpcalc's
/// default. A partial fingerprint from the start of a song still matches the
/// full-length one AcoustID stores.
const MAX_FINGERPRINT: Duration = Duration::from_secs(120);

/// A chapter-less file longer than this is treated as un-identifiable: a flat
/// concatenation of songs whose 120 s head matches no single recording, so
/// fingerprinting it only burns an API call. Chaptered rips and ordinary single
/// tracks are unaffected.
const MAX_FLAT_DURATION: Duration = Duration::from_secs(15 * 60);

/// AcoustID matches weaker than this are not trusted — a wrong cover and wrong
/// metadata are worse than leaving the placeholders in place.
const MIN_SCORE: f64 = 0.5;

/// Spacing between AcoustID requests, keeping under the 3 req/s service limit.
const THROTTLE: Duration = Duration::from_millis(350);

/// A file's identity, as recovered from AcoustID.
pub struct Identification {
    /// MusicBrainz release-group MBID — keys the Cover Art Archive cover.
    pub mbid: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// One title per chapter, in order (a single entry for a file with no
    /// chapters). `None` where a chapter could not be matched.
    pub chapter_titles: Vec<Option<String>>,
}

/// Identifies `track` from its audio, or returns `None` when it can't be matched
/// confidently. Blocking (decode + FFT + HTTP) — call it on the background
/// executor. The result is cached per file, so repeat plays hit disk, not the
/// network. `api_key` must be non-empty (the caller checks [`API_KEY`]).
pub fn identify(track: &Track, api_key: &str) -> Option<Identification> {
    // Key the cache on path + mtime, so a re-tagged file is retried rather than
    // served a stale answer.
    let cache_key = std::fs::metadata(&track.path)
        .and_then(|m| m.modified())
        .ok()
        .map(|mtime| cache_key(&track.path, mtime));

    if let Some(key) = &cache_key {
        match read_cache(key) {
            CacheResult::Hit(id) => return Some(id),
            CacheResult::Miss => return None,
            CacheResult::Absent => {}
        }
    }

    let resolved = resolve(track, api_key);

    if let Some(key) = &cache_key {
        match &resolved {
            Resolved::Found(id) => write_cache(key, id),
            // A definitive miss is remembered; a transient network error is not,
            // so the next play can try again.
            Resolved::Miss => write_miss(key),
            Resolved::Transient => {}
        }
    }

    match resolved {
        Resolved::Found(id) => Some(id),
        _ => None,
    }
}

enum Resolved {
    Found(Identification),
    /// Fingerprinted fine, but AcoustID had no confident match: cache it.
    Miss,
    /// The network failed: don't cache, retry later.
    Transient,
}

fn resolve(track: &Track, api_key: &str) -> Resolved {
    let Some(segments) = segments(track) else {
        return Resolved::Miss;
    };
    let config = Configuration::preset_test2();
    let Some(fingerprints) = compute_fingerprints(track, &segments, &config) else {
        return Resolved::Miss;
    };

    let mut matches: Vec<Option<SegmentMatch>> = Vec::with_capacity(segments.len());
    let mut any_transient = false;
    let mut any_lookup_ok = false;
    for (i, fingerprint) in fingerprints.iter().enumerate() {
        let Some(fingerprint) = fingerprint else {
            matches.push(None);
            continue;
        };
        if any_lookup_ok || any_transient {
            std::thread::sleep(THROTTLE);
        }
        match lookup(api_key, fingerprint, segments[i].report_duration) {
            Lookup::Ok(json) => {
                any_lookup_ok = true;
                matches.push(best_match(&json));
            }
            Lookup::ServiceError(message) => {
                // A bad key or malformed request fails every chapter the same
                // way, so surface it once and stop rather than hammering the
                // service (and leaving the user staring at nothing). Not cached,
                // so a corrected key retries on the next play.
                eprintln!("hark: AcoustID lookup failed: {message} (check ACOUSTID_API_KEY)");
                return Resolved::Transient;
            }
            Lookup::Transient => {
                any_transient = true;
                matches.push(None);
            }
        }
    }

    // The album is the release group that most chapters agree on.
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for m in matches.iter().flatten() {
        if let Some(mbid) = &m.mbid {
            *counts.entry(mbid.as_str()).or_default() += 1;
        }
    }
    let Some(mbid) = counts
        .into_iter()
        .max_by_key(|(_, count)| *count)
        .map(|(mbid, _)| mbid.to_string())
    else {
        // Nothing matched. If every lookup that ran failed on the network,
        // it's worth retrying; otherwise AcoustID genuinely knows nothing.
        return if any_transient && !any_lookup_ok {
            Resolved::Transient
        } else {
            Resolved::Miss
        };
    };

    let chosen = matches
        .iter()
        .flatten()
        .find(|m| m.mbid.as_deref() == Some(&mbid));
    let artist = chosen.and_then(|m| m.artist.clone());
    let album = chosen.and_then(|m| m.album.clone());
    let chapter_titles = matches
        .iter()
        .map(|m| m.as_ref().and_then(|m| m.title.clone()))
        .collect();

    eprintln!(
        "hark: identified {} — {}",
        artist.as_deref().unwrap_or("?"),
        album.as_deref().unwrap_or("?"),
    );
    Resolved::Found(Identification {
        mbid,
        artist,
        album,
        chapter_titles,
    })
}

/// A stretch of the file to fingerprint as one recording.
struct Segment {
    start: Duration,
    /// Where fingerprinting stops: the song's end, or 120 s in, whichever first.
    cap_end: Duration,
    /// The full recording length reported to AcoustID (the chapter's length, or
    /// the whole file when there are no chapters) — not the capped stretch.
    report_duration: Duration,
}

/// Splits `track` into the segments to fingerprint: one per chapter, or a single
/// segment for a plain file. `None` means the file isn't worth fingerprinting.
fn segments(track: &Track) -> Option<Vec<Segment>> {
    if track.chapters.is_empty() {
        if track.duration > MAX_FLAT_DURATION {
            return None;
        }
        // Duration is zero for a file whose tags carried none; still try, capped.
        let report = if track.duration.is_zero() {
            MAX_FINGERPRINT
        } else {
            track.duration
        };
        return Some(vec![Segment {
            start: Duration::ZERO,
            cap_end: report.min(MAX_FINGERPRINT),
            report_duration: report,
        }]);
    }

    Some(
        track
            .chapters
            .iter()
            .map(|chapter| Segment {
                start: chapter.start,
                cap_end: (chapter.start + MAX_FINGERPRINT).min(chapter.end),
                report_duration: chapter.end.saturating_sub(chapter.start),
            })
            .collect(),
    )
}

/// Decodes the file once, front to back, and fingerprints each segment's window.
/// Returns one entry per segment (`None` where a segment yielded no fingerprint),
/// or `None` if the file couldn't be decoded or produced no audio at all.
fn compute_fingerprints(
    track: &Track,
    segments: &[Segment],
    config: &Configuration,
) -> Option<Vec<Option<String>>> {
    let source = decode::open(&track.path).ok()?;
    // Read the format before the `for` loop consumes the source.
    let sample_rate = source.sample_rate().get();
    let channels = source.channels().get() as usize;
    if channels == 0 {
        return None;
    }

    let mut fingerprints: Vec<Option<String>> = vec![None; segments.len()];
    let mut segment_index = 0usize;
    // Samples of the current segment, awaiting a fingerprint at its end.
    let mut buffer: Vec<i16> = Vec::new();
    // One interleaved frame (`channels` samples), assembled sample by sample.
    let mut frame: Vec<i16> = Vec::with_capacity(channels);
    let mut frame_index: u64 = 0;

    for sample in source {
        frame.push(to_i16(sample));
        if frame.len() < channels {
            continue;
        }
        let time = frame_index as f64 / sample_rate as f64;
        frame_index += 1;

        // Close every segment this frame has moved past.
        while segment_index < segments.len()
            && time >= segments[segment_index].cap_end.as_secs_f64()
        {
            fingerprints[segment_index] =
                fingerprint_segment(&buffer, sample_rate, channels as u32, config);
            buffer.clear();
            segment_index += 1;
        }
        if segment_index >= segments.len() {
            break;
        }
        if time >= segments[segment_index].start.as_secs_f64() {
            buffer.extend_from_slice(&frame);
        }
        frame.clear();
    }
    // Flush a segment still open when the file ended.
    if segment_index < segments.len() && !buffer.is_empty() {
        fingerprints[segment_index] =
            fingerprint_segment(&buffer, sample_rate, channels as u32, config);
    }

    if fingerprints.iter().all(Option::is_none) {
        return None;
    }
    Some(fingerprints)
}

/// Runs Chromaprint over one segment's interleaved samples and returns the
/// compressed, base64 (URL-safe, no padding) fingerprint AcoustID expects.
fn fingerprint_segment(
    samples: &[i16],
    sample_rate: u32,
    channels: u32,
    config: &Configuration,
) -> Option<String> {
    if samples.is_empty() {
        return None;
    }
    let mut printer = Fingerprinter::new(config);
    printer.start(sample_rate, channels).ok()?;
    printer.consume(samples);
    printer.finish();
    let fingerprint = printer.fingerprint();
    if fingerprint.is_empty() {
        return None;
    }
    let compressed = FingerprintCompressor::from(config).compress(fingerprint);
    Some(URL_SAFE_NO_PAD.encode(compressed))
}

#[inline]
fn to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32767.0) as i16
}

enum Lookup {
    Ok(serde_json::Value),
    /// AcoustID replied with `status: error` — a request or configuration
    /// problem such as a bad API key. Permanent and actionable, so it carries
    /// the message for the user.
    ServiceError(String),
    /// A transport failure (offline, timeout, 5xx) — worth retrying later.
    Transient,
}

fn lookup(api_key: &str, fingerprint: &str, duration: Duration) -> Lookup {
    let seconds = duration.as_secs().max(1).to_string();
    let response = artwork::client()
        .get("https://api.acoustid.org/v2/lookup")
        .query(&[
            ("client", api_key),
            // Space-separated, not `+`: the query encoder turns a space into `+`,
            // which AcoustID decodes back to a separator. A literal `+` here would
            // be percent-encoded to `%2B` and arrive as one unrecognized token,
            // so the response would carry no recordings or release groups.
            ("meta", "recordings releasegroups"),
            ("duration", &seconds),
            ("fingerprint", fingerprint),
        ])
        .send();

    let Ok(response) = response else {
        return Lookup::Transient;
    };
    // AcoustID reports a bad key or malformed request as a 400 with a JSON error
    // body, so read the body before judging by status code.
    let Ok(json) = response.json::<serde_json::Value>() else {
        return Lookup::Transient;
    };
    match json["status"].as_str() {
        Some("ok") => Lookup::Ok(json),
        _ => Lookup::ServiceError(
            json["error"]["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string(),
        ),
    }
}

/// The corrected fields drawn from one segment's lookup.
struct SegmentMatch {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    mbid: Option<String>,
}

/// Picks the highest-scored result from a lookup response and pulls the recording
/// title/artist and release-group id/title out of it, or `None` when the best
/// match is too weak to trust.
fn best_match(json: &serde_json::Value) -> Option<SegmentMatch> {
    let results = json["results"].as_array()?;
    let best = results.iter().max_by(|a, b| {
        score(a)
            .partial_cmp(&score(b))
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;
    if score(best) < MIN_SCORE {
        return None;
    }

    let recording = best["recordings"].as_array().and_then(|r| r.first());
    let title = recording
        .and_then(|r| r["title"].as_str())
        .map(str::to_string);
    let artist = recording
        .and_then(|r| r["artists"].as_array())
        .and_then(|a| a.first())
        .and_then(|a| a["name"].as_str())
        .map(str::to_string);

    // Release groups may hang off the recording or off the result directly.
    let group = recording
        .and_then(|r| r["releasegroups"].as_array())
        .or_else(|| best["releasegroups"].as_array())
        .and_then(|groups| pick_release_group(groups));
    let mbid = group.and_then(|g| g["id"].as_str()).map(str::to_string);
    let album = group.and_then(|g| g["title"].as_str()).map(str::to_string);

    Some(SegmentMatch {
        title,
        artist,
        album,
        mbid,
    })
}

fn score(result: &serde_json::Value) -> f64 {
    result["score"].as_f64().unwrap_or(0.0)
}

/// Prefers a proper album over a single/compilation, so the cover is the album's.
fn pick_release_group(groups: &[serde_json::Value]) -> Option<&serde_json::Value> {
    groups
        .iter()
        .find(|g| g["type"].as_str() == Some("Album"))
        .or_else(|| groups.first())
}

// -- per-file resolution cache --------------------------------------------

/// `$XDG_CACHE_HOME/hark/fingerprints`, sharing the base dir with the cover
/// cache so both live under one `hark` folder.
fn cache_dir() -> Option<PathBuf> {
    artwork::cache_dir().and_then(|dir| dir.parent().map(|base| base.join("fingerprints")))
}

fn cache_key(path: &Path, mtime: SystemTime) -> String {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    mtime
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

enum CacheResult {
    Hit(Identification),
    /// A `.miss` marker: AcoustID couldn't identify this file, don't ask again.
    Miss,
    Absent,
}

fn read_cache(key: &str) -> CacheResult {
    let Some(dir) = cache_dir() else {
        return CacheResult::Absent;
    };
    if let Ok(bytes) = std::fs::read(dir.join(key)) {
        // A corrupt entry is treated as absent, so it can be recomputed.
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
            && let Some(mbid) = value["mbid"].as_str()
        {
            let chapter_titles = value["chapter_titles"]
                .as_array()
                .map(|titles| titles.iter().map(|t| t.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            return CacheResult::Hit(Identification {
                mbid: mbid.to_string(),
                artist: value["artist"].as_str().map(str::to_string),
                album: value["album"].as_str().map(str::to_string),
                chapter_titles,
            });
        }
        return CacheResult::Absent;
    }
    if dir.join(format!("{key}.miss")).exists() {
        return CacheResult::Miss;
    }
    CacheResult::Absent
}

fn write_cache(key: &str, id: &Identification) {
    let Some(dir) = cache_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let value = serde_json::json!({
        "mbid": id.mbid,
        "artist": id.artist,
        "album": id.album,
        "chapter_titles": id.chapter_titles,
    });
    let Ok(bytes) = serde_json::to_vec(&value) else {
        return;
    };
    // Write then rename, so a crash mid-write can't leave a truncated entry.
    let tmp = dir.join(format!("{key}.tmp"));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(key));
    }
}

fn write_miss(key: &str) {
    let Some(dir) = cache_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::File::create(dir.join(format!("{key}.miss")));
}
