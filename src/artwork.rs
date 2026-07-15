//! Fetches album covers from the internet for tracks that carry no embedded art.
//!
//! The source is the iTunes Search API (no key, one request yields an artwork
//! URL). Results are cached under the user's cache dir so a cover is fetched at
//! most once per album. The HTTP client is the blocking `reqwest`, driven from
//! the GPUI background executor — a smol worker, not a tokio context, so its
//! internal runtime does not nest.

use gpui::{Image, ImageFormat};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Decodes image bytes into a GPUI image, sniffing the format from the leading
/// magic bytes rather than trusting a MIME string. Shared by embedded-tag art
/// (`track::Track::load`) and the internet cache.
pub fn decode(bytes: Vec<u8>) -> Option<Arc<Image>> {
    let format = sniff(&bytes)?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

fn sniff(b: &[u8]) -> Option<ImageFormat> {
    if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(ImageFormat::Jpeg)
    } else if b.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some(ImageFormat::Png)
    } else if b.starts_with(b"GIF8") {
        Some(ImageFormat::Gif)
    } else if b.len() >= 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

/// `$XDG_CACHE_HOME/hark/covers`, else `~/.cache/hark/covers`. Mirrors the
/// hand-rolled XDG resolution in `library::music_dir`. Not created here — the
/// write path creates it lazily.
pub fn cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from)
        && dir.is_absolute()
    {
        return Some(dir.join("hark/covers"));
    }
    let home = PathBuf::from(std::env::var_os("HOME")?);
    Some(home.join(".cache/hark/covers"))
}

/// A stable cache file name for an (artist, album) pair. The fields are
/// normalized and hashed with a separator between them, so `("a", "bc")` and
/// `("ab", "c")` never collide.
pub fn key(artist: &str, album: &str) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    artist.trim().to_lowercase().hash(&mut hasher);
    0xffu8.hash(&mut hasher);
    album.trim().to_lowercase().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

enum Cache {
    Hit(Arc<Image>),
    /// A `.miss` marker: iTunes has no cover, so don't ask again.
    Negative,
    Absent,
}

fn cached(key: &str) -> Cache {
    let Some(dir) = cache_dir() else {
        return Cache::Absent;
    };
    if let Ok(bytes) = std::fs::read(dir.join(key)) {
        // A corrupt entry is treated as absent, so it can be refetched.
        return match decode(bytes) {
            Some(image) => Cache::Hit(image),
            None => Cache::Absent,
        };
    }
    if dir.join(format!("{key}.miss")).exists() {
        return Cache::Negative;
    }
    Cache::Absent
}

pub(crate) fn client() -> &'static reqwest::blocking::Client {
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .user_agent("hark/0.1")
            .timeout(Duration::from_secs(10))
            .build()
            .expect("cover-art HTTP client")
    })
}

enum Fetch {
    Hit(Vec<u8>),
    /// The search returned no cover: a definitive miss worth remembering.
    Miss,
    /// A transient failure (offline, rate limit, bad body): retry later.
    Error,
}

fn fetch(artist: &str, album: &str) -> Fetch {
    let term = format!("{artist} {album}");
    let search = client()
        .get("https://itunes.apple.com/search")
        .query(&[("term", term.as_str()), ("entity", "album"), ("limit", "1")])
        .send();
    let Ok(search) = search else {
        return Fetch::Error;
    };
    if !search.status().is_success() {
        return Fetch::Error;
    }
    let Ok(json) = search.json::<serde_json::Value>() else {
        return Fetch::Error;
    };

    let url = json["results"]
        .get(0)
        .and_then(|r| r["artworkUrl100"].as_str());
    let Some(url) = url else {
        return Fetch::Miss;
    };

    // The 100px thumbnail URL scales up to a size that suits the display; the
    // replace is a harmless no-op if Apple ever changes the path segment.
    let big = url.replace("100x100bb", "600x600bb");
    fetch_image(&big)
}

/// Downloads and validates image bytes from `url`. A 404 is a definitive miss
/// (there is no such cover); any other failure — offline, 5xx, non-image body —
/// is transient and worth a later retry.
fn fetch_image(url: &str) -> Fetch {
    let image = client().get(url).send();
    let Ok(image) = image else {
        return Fetch::Error;
    };
    if image.status() == reqwest::StatusCode::NOT_FOUND {
        return Fetch::Miss;
    }
    if !image.status().is_success() {
        return Fetch::Error;
    }
    let Ok(bytes) = image.bytes() else {
        return Fetch::Error;
    };
    let bytes = bytes.to_vec();

    // Guard against a non-image body (e.g. an HTML error page) reaching the cache.
    if sniff(&bytes).is_none() {
        return Fetch::Error;
    }
    Fetch::Hit(bytes)
}

fn store(key: &str, bytes: &[u8]) {
    let Some(dir) = cache_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    // Write then rename, so a crash mid-write can't leave a truncated cover.
    let tmp = dir.join(format!("{key}.tmp"));
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join(key));
    }
}

fn store_miss(key: &str) {
    let Some(dir) = cache_dir() else { return };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = std::fs::File::create(dir.join(format!("{key}.miss")));
}

/// Returns a cover for the (artist, album) pair, from cache or the internet,
/// and whether the failure (if any) was transient. Runs blocking — call it on
/// the background executor. The bool lets the caller retry after a transient
/// error while leaving a definitive miss remembered.
pub fn load_or_fetch(artist: &str, album: &str) -> (Option<Arc<Image>>, bool) {
    let key = key(artist, album);
    match cached(&key) {
        Cache::Hit(image) => return (Some(image), false),
        Cache::Negative => return (None, false),
        Cache::Absent => {}
    }

    match fetch(artist, album) {
        Fetch::Hit(bytes) => {
            store(&key, &bytes);
            // Decoded after storing, so a cache-write failure still shows the art.
            (decode(bytes), false)
        }
        Fetch::Miss => {
            store_miss(&key);
            (None, false)
        }
        Fetch::Error => (None, true),
    }
}

/// Returns the Cover Art Archive front cover for a MusicBrainz release-group
/// MBID, from cache or the internet, and whether a failure (if any) was
/// transient. The MBID is a 36-char UUID, so it never collides with the 16-hex
/// `(artist, album)` keys sharing this cache dir. Used by the fingerprint
/// fallback once AcoustID has identified an album. Runs blocking — call it on
/// the background executor.
pub fn load_or_fetch_release_group(mbid: &str) -> (Option<Arc<Image>>, bool) {
    match cached(mbid) {
        Cache::Hit(image) => return (Some(image), false),
        Cache::Negative => return (None, false),
        Cache::Absent => {}
    }

    let url = format!("https://coverartarchive.org/release-group/{mbid}/front-500");
    match fetch_image(&url) {
        Fetch::Hit(bytes) => {
            store(mbid, &bytes);
            (decode(bytes), false)
        }
        // A release group with no front cover: remember it so we don't ask again.
        Fetch::Miss => {
            store_miss(mbid);
            (None, false)
        }
        Fetch::Error => (None, true),
    }
}
