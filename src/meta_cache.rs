//! Per-file metadata cache, stashed in the file's own `user.hark.meta` extended
//! attribute.
//!
//! The music library can live on a network mount where opening a file costs a
//! multi-megabyte block fetch. Parsing tags is a one-time price; remembering the
//! result next to the file itself means a rescan needs a `stat` and a `getxattr`
//! per file and no opens at all. Keeping the record in the file rather than in a
//! side-car index also means a rename carries it along, and a filesystem that
//! replicates its metadata carries it to another machine for free.
//!
//! Nothing here reports failure. A filesystem without extended attributes, a
//! read-only mount, a record from another version and a record whose file has
//! changed since all mean the same thing to the caller: parse the file. That is
//! why `read` yields an `Option` and `write` yields nothing — a scan of a
//! thousand tracks must not be able to print a thousand lines. Remove a record
//! by hand with `setfattr -x user.hark.meta <file>`.
//!
//! Two rules the rest of the player depends on. A parse that *failed* is never
//! cached: a half-copied file or a hiccup on the mount is transient and must not
//! be frozen into the file as "this one has nothing". And whatever comes back
//! from here builds a `Track` through the same `Track::from_meta` a fresh parse
//! uses, so a cache hit cannot drift away from the real thing.

use crate::track::{Chapter, Meta};
use std::path::Path;
use std::time::Duration;

const XATTR_NAME: &str = "user.hark.meta";

/// Bumped when the record's shape changes; another version is ignored rather
/// than misread, and the next parse overwrites it. That is the whole migration
/// strategy.
const VERSION: u64 = 1;

/// A record past this is refused, and that file simply stays on the cold path
/// forever — it is a cache, and a pathological file paying full price is the
/// right outcome. Filesystems impose their own, smaller limits (ext4 fits all of
/// an inode's attributes in one 4 KiB block) and a write that exceeds them fails
/// silently, so this only bounds what is built in memory. 16 KiB holds a few
/// hundred chapters, which covers audiobooks and DJ mixes.
const MAX_RECORD: usize = 16 * 1024;

/// Identity of a file's contents: when it was last modified and how big it is.
/// Not the path — the record travels with the file, so a rename inside the
/// library keeps it. Not `ctime` either: writing the attribute moves `ctime`, so
/// a record keyed on it would invalidate itself the moment it was written.
///
/// Three caches key on this now — the tag record here, the waveform peaks and
/// the answer AcoustID gave — so the fields are open. Each writes them into a
/// wire format of its own, which an accessor pair would serve no better; there
/// is no invariant here to protect, only two numbers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Stamp {
    pub mtime_ns: i64,
    pub size: u64,
}

/// Nanoseconds rather than whole seconds because a batch re-tag rewrites a great
/// many files inside one second. The size covers filesystems whose timestamps
/// are coarser than that.
///
/// Infallible by design: the part that can fail is obtaining the `Metadata`, so
/// the `Option` belongs at the call site.
pub fn stamp(metadata: &std::fs::Metadata) -> Stamp {
    use std::os::unix::fs::MetadataExt;

    Stamp {
        // Saturating, so a nonsense timestamp cannot panic in a debug build. Any
        // stable number identifies the file just as well.
        mtime_ns: metadata
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.mtime_nsec()),
        size: metadata.size(),
    }
}

/// The metadata cached on `path`, if the file still matches `stamp`. A missing
/// attribute, an unsupported filesystem and an unreadable one are all just
/// "no record".
pub fn read(path: &Path, stamp: Stamp) -> Option<Meta> {
    let Ok(Some(bytes)) = xattr::get_deref(path, XATTR_NAME) else {
        return None;
    };
    decode(&bytes, stamp)
}

/// Stamps `path` with what was parsed out of it. A filesystem with no extended
/// attributes, a read-only mount and a full attribute block all fail here
/// quietly; the only cost is that the next scan parses the file again. There is
/// no process-wide "this mount has no xattrs" latch — a failing `setxattr` costs
/// on the order of a hundred microseconds, and a couple of thousand of them stay
/// under a second.
pub fn write(path: &Path, stamp: Stamp, meta: &Meta) {
    let Some(bytes) = encode(stamp, meta) else {
        return;
    };
    let _ = xattr::set_deref(path, XATTR_NAME, &bytes);
}

/// Builds the record. An absent tag is left out rather than written as null: the
/// record shares one attribute block with everything else on the file, and the
/// missing title is what lets a renamed file fall back to its new file name.
/// Never any cover bytes — that is the whole point of the size budget.
fn encode(stamp: Stamp, meta: &Meta) -> Option<Vec<u8>> {
    let mut record = serde_json::json!({
        "v": VERSION,
        "mtime_ns": stamp.mtime_ns,
        "size": stamp.size,
        "duration_ms": meta.duration.as_millis() as u64,
    });

    let object = record.as_object_mut()?;
    for (key, value) in [
        ("title", &meta.title),
        ("artist", &meta.artist),
        ("album", &meta.album),
    ] {
        if let Some(value) = value {
            object.insert(key.to_string(), serde_json::Value::from(value.as_str()));
        }
    }
    if meta.has_art {
        object.insert("art".to_string(), serde_json::Value::Bool(true));
    }
    // Chapters are the only part that can grow, so they get short keys and are
    // left out entirely for the overwhelming majority of files that have none.
    if !meta.chapters.is_empty() {
        let chapters: Vec<serde_json::Value> = meta
            .chapters
            .iter()
            .map(|chapter| {
                serde_json::json!({
                    "t": &chapter.title,
                    "s": chapter.start.as_millis() as u64,
                    "e": chapter.end.as_millis() as u64,
                })
            })
            .collect();
        object.insert("chapters".to_string(), serde_json::Value::Array(chapters));
    }

    let Ok(bytes) = serde_json::to_vec(&record) else {
        return None;
    };
    (bytes.len() <= MAX_RECORD).then_some(bytes)
}

/// Reads a record back, refusing anything that does not describe exactly this
/// version of exactly this file — the stamp is always checked, the name is never
/// trusted, so a filesystem that mixes attributes up yields a miss rather than a
/// guess. A single malformed chapter voids the whole record: chapters drive
/// seeking, and half a list is worse than none.
fn decode(bytes: &[u8], stamp: Stamp) -> Option<Meta> {
    let Ok(record) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return None;
    };
    if record["v"].as_u64() != Some(VERSION) {
        return None;
    }
    if record["mtime_ns"].as_i64() != Some(stamp.mtime_ns)
        || record["size"].as_u64() != Some(stamp.size)
    {
        return None;
    }
    let duration = Duration::from_millis(record["duration_ms"].as_u64()?);

    let mut chapters = Vec::new();
    if let Some(entries) = record["chapters"].as_array() {
        for entry in entries {
            let (Some(title), Some(start), Some(end)) = (
                entry["t"].as_str(),
                entry["s"].as_u64(),
                entry["e"].as_u64(),
            ) else {
                return None;
            };
            chapters.push(Chapter {
                title: title.to_string().into(),
                start: Duration::from_millis(start),
                end: Duration::from_millis(end),
            });
        }
    }

    Some(Meta {
        title: record["title"].as_str().map(str::to_string),
        artist: record["artist"].as_str().map(str::to_string),
        album: record["album"].as_str().map(str::to_string),
        duration,
        chapters,
        has_art: record["art"].as_bool().unwrap_or(false),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: Stamp = Stamp {
        mtime_ns: 1_721_742_401_123_456_789,
        size: 8_123_456,
    };

    fn full() -> Meta {
        Meta {
            title: Some("Sunday Morning".to_string()),
            artist: Some("The Velvet Underground".to_string()),
            album: Some("The Velvet Underground & Nico".to_string()),
            duration: Duration::from_millis(175_000),
            chapters: vec![
                Chapter {
                    title: "One".into(),
                    start: Duration::ZERO,
                    end: Duration::from_millis(90_000),
                },
                Chapter {
                    title: "Two".into(),
                    start: Duration::from_millis(90_000),
                    end: Duration::from_millis(175_000),
                },
            ],
            has_art: true,
        }
    }

    fn round_trip(meta: &Meta) -> Meta {
        let Some(bytes) = encode(STAMP, meta) else {
            panic!("encode refused a small record");
        };
        let Some(decoded) = decode(&bytes, STAMP) else {
            panic!("decode refused its own output");
        };
        decoded
    }

    #[test]
    fn a_full_record_survives_a_round_trip() {
        let meta = round_trip(&full());

        assert_eq!(meta.title.as_deref(), Some("Sunday Morning"));
        assert_eq!(meta.artist.as_deref(), Some("The Velvet Underground"));
        assert_eq!(meta.album.as_deref(), Some("The Velvet Underground & Nico"));
        assert_eq!(meta.duration, Duration::from_millis(175_000));
        assert!(meta.has_art);
        assert_eq!(meta.chapters.len(), 2);
        assert_eq!(meta.chapters[1].title, "Two");
        assert_eq!(meta.chapters[1].start, Duration::from_millis(90_000));
        assert_eq!(meta.chapters[1].end, Duration::from_millis(175_000));
    }

    #[test]
    fn a_bare_record_keeps_its_absent_tags_absent() {
        let meta = round_trip(&Meta {
            duration: Duration::from_millis(4_200),
            ..Meta::default()
        });

        assert_eq!(meta.title, None);
        assert_eq!(meta.artist, None);
        assert_eq!(meta.album, None);
        assert_eq!(meta.duration, Duration::from_millis(4_200));
        assert!(!meta.has_art);
        assert!(meta.chapters.is_empty());
    }

    #[test]
    fn a_changed_file_invalidates_the_record() {
        let Some(bytes) = encode(STAMP, &full()) else {
            panic!("encode refused a small record");
        };

        let retagged = Stamp {
            mtime_ns: STAMP.mtime_ns + 1,
            ..STAMP
        };
        let regrown = Stamp {
            size: STAMP.size + 1,
            ..STAMP
        };
        assert!(decode(&bytes, retagged).is_none());
        assert!(decode(&bytes, regrown).is_none());
    }

    #[test]
    fn another_version_is_a_miss() {
        for version in ["0", "2"] {
            let record = format!(
                r#"{{"v":{version},"mtime_ns":{},"size":{},"duration_ms":1}}"#,
                STAMP.mtime_ns, STAMP.size
            );
            assert!(decode(record.as_bytes(), STAMP).is_none());
        }
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
            assert!(decode(bytes, STAMP).is_none());
        }
    }

    #[test]
    fn a_record_without_a_duration_is_a_miss() {
        let record = format!(
            r#"{{"v":1,"mtime_ns":{},"size":{}}}"#,
            STAMP.mtime_ns, STAMP.size
        );
        assert!(decode(record.as_bytes(), STAMP).is_none());
    }

    #[test]
    fn one_malformed_chapter_voids_the_whole_record() {
        let record = format!(
            r#"{{"v":1,"mtime_ns":{},"size":{},"duration_ms":10,
               "chapters":[{{"t":"a","s":0,"e":5}},{{"t":"b","s":5}}]}}"#,
            STAMP.mtime_ns, STAMP.size
        );
        assert!(decode(record.as_bytes(), STAMP).is_none());
    }

    #[test]
    fn an_oversized_record_is_refused() {
        let mut meta = full();
        meta.chapters = (0..2_000)
            .map(|i| Chapter {
                title: format!("Chapter {i}").into(),
                start: Duration::from_secs(i),
                end: Duration::from_secs(i + 1),
            })
            .collect();

        assert!(encode(STAMP, &meta).is_none());
    }
}
