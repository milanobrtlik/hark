//! What AcoustID knew about a file, stashed in its own `user.hark.id` extended
//! attribute.
//!
//! Identifying a file means decoding two minutes of it, running a Chromaprint
//! fingerprint over the result and asking a rate-limited web service about it —
//! once per chapter, for a full-album rip. That is worth remembering, and it is
//! worth remembering *on the file*: the answer describes the audio, so it stays
//! true through a rename and, on a filesystem that replicates its own metadata,
//! it reaches another machine that would otherwise fingerprint the whole
//! badly-tagged half of the library over again.
//!
//! JSON here, where [`crate::peak_cache`] is binary. That is shape, not
//! inconsistency: the peaks are a fixed run of numbers and this is a handful of
//! optional strings of no fixed length. It also means the record can be read
//! straight out of `getfattr`.
//!
//! Nothing here reports failure, as everywhere else hark writes an attribute: a
//! filesystem without them, a read-only mount, a record from another version and
//! a record whose file has changed since all mean "ask again". Remove one by
//! hand with `setfattr -x user.hark.id <file>`.

use crate::fingerprint::Identification;
use crate::meta_cache::Stamp;
use std::path::Path;

const XATTR_NAME: &str = "user.hark.id";

/// Bumped when the record's shape changes; another version is ignored rather
/// than misread, and the next lookup overwrites it. That is the whole migration
/// strategy.
const VERSION: u64 = 1;

/// A sixty-track audiobook's titles fit inside this with room to spare, and past
/// it the file stays on the cold path forever — for one pathological file that
/// is the right outcome.
///
/// Smaller than the tag record's budget on purpose. The kernel's own ceiling is
/// `XATTR_SIZE_MAX`, 64 KiB, and nowhere near binding; what binds is ext4, which
/// fits *all* of an inode's attributes into one 4 KiB block that this record now
/// shares with the tags and the peaks. Of the three this is the one whose loss
/// costs least — a lookup, against a full decode — so it takes the smallest
/// share.
const MAX_RECORD: usize = 4 * 1024;

/// What the attribute had to say. [`read`] yielding `None` is the third answer:
/// this file has never been asked about.
pub enum Cached {
    Found(Identification),
    /// AcoustID was asked and had no confident match. Worth remembering, or
    /// every play fingerprints the file again for the same silence.
    Unknown,
}

/// The identification cached on `path`, if the file still matches `stamp`.
pub fn read(path: &Path, stamp: Stamp) -> Option<Cached> {
    let Ok(Some(bytes)) = xattr::get_deref(path, XATTR_NAME) else {
        return None;
    };
    decode(&bytes, stamp)
}

/// Remembers what AcoustID said about `path`.
pub fn write_found(path: &Path, stamp: Stamp, id: &Identification) {
    write(path, encode(stamp, Some(id)));
}

/// Remembers that AcoustID was asked about `path` and did not know. Only for a
/// definitive answer — a network failure must stay unrecorded so the next play
/// can try again.
pub fn write_unknown(path: &Path, stamp: Stamp) {
    write(path, encode(stamp, None));
}

fn write(path: &Path, record: Option<Vec<u8>>) {
    let Some(bytes) = record else {
        return;
    };
    let _ = xattr::set_deref(path, XATTR_NAME, &bytes);
}

/// Builds the record. `None` writes the negative answer, which is simply a
/// record carrying no MBID: the MBID *is* the answer, so its absence says the
/// question was asked and came back empty. A separate flag would only restate
/// what is already there.
fn encode(stamp: Stamp, id: Option<&Identification>) -> Option<Vec<u8>> {
    let mut record = serde_json::json!({
        "v": VERSION,
        "mtime_ns": stamp.mtime_ns,
        "size": stamp.size,
    });
    let object = record.as_object_mut()?;

    if let Some(id) = id {
        // An empty MBID would decode as the negative answer, so it is refused
        // outright rather than written and misread later.
        if id.mbid.is_empty() {
            return None;
        }
        object.insert("mbid".to_string(), id.mbid.as_str().into());

        // Absent rather than null, as in the tag record: a field that is not
        // there is one the caller leaves alone.
        for (key, value) in [("artist", &id.artist), ("album", &id.album)] {
            if let Some(value) = value.as_deref().filter(|value| !value.is_empty()) {
                object.insert(key.to_string(), value.into());
            }
        }

        // Nulls are meaningful here and are kept: a chapter AcoustID could not
        // place has to hold its position in the list, because the titles are
        // zipped onto the chapters by index.
        let titles: Vec<serde_json::Value> = id
            .chapter_titles
            .iter()
            .map(|title| match title {
                Some(title) => title.as_str().into(),
                None => serde_json::Value::Null,
            })
            .collect();
        object.insert("titles".to_string(), titles.into());
    }

    let bytes = serde_json::to_vec(&record).ok()?;
    (bytes.len() <= MAX_RECORD).then_some(bytes)
}

/// Reads a record back, refusing anything that does not describe exactly this
/// version of exactly this file — the stamp is always checked, so a filesystem
/// that mixes attributes up yields a miss rather than a guess.
///
/// A single malformed title voids the whole record, for the reason a malformed
/// chapter voids a tag record: `apply_identity` zips the titles onto the
/// chapters by position, so half a list writes the wrong names onto the wrong
/// songs.
fn decode(bytes: &[u8], stamp: Stamp) -> Option<Cached> {
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

    let mbid = match &record["mbid"] {
        serde_json::Value::Null => return Some(Cached::Unknown),
        serde_json::Value::String(mbid) if !mbid.is_empty() => mbid.clone(),
        // A number, an object, an empty string: hark did not write this. Refused
        // rather than guessed at — a wrong MBID fetches a wrong cover.
        _ => return None,
    };

    let mut chapter_titles = Vec::new();
    if let Some(titles) = record["titles"].as_array() {
        for title in titles {
            match title {
                serde_json::Value::Null => chapter_titles.push(None),
                serde_json::Value::String(title) => chapter_titles.push(Some(title.clone())),
                _ => return None,
            }
        }
    }

    Some(Cached::Found(Identification {
        mbid,
        artist: record["artist"].as_str().map(str::to_string),
        album: record["album"].as_str().map(str::to_string),
        chapter_titles,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: Stamp = Stamp {
        mtime_ns: 1_721_742_401_123_456_789,
        size: 8_123_456,
    };

    fn full() -> Identification {
        Identification {
            mbid: "b1a9c0e8-d59d-4f0a-9b8c-2e4f6a1d3c57".to_string(),
            artist: Some("Forndom".to_string()),
            album: Some("Flykt".to_string()),
            chapter_titles: vec![
                Some("Fajr".to_string()),
                // A chapter AcoustID could not place, in the middle where it can
                // shift everything after it if the position is not held.
                None,
                Some("Hemmet".to_string()),
            ],
        }
    }

    fn found(bytes: &[u8]) -> Identification {
        match decode(bytes, STAMP) {
            Some(Cached::Found(id)) => id,
            Some(Cached::Unknown) => panic!("decode read an answer as a miss"),
            None => panic!("decode refused a good record"),
        }
    }

    #[test]
    fn a_full_record_survives_a_round_trip() {
        let bytes = encode(STAMP, Some(&full())).expect("encode refused a small record");
        let id = found(&bytes);

        assert_eq!(id.mbid, "b1a9c0e8-d59d-4f0a-9b8c-2e4f6a1d3c57");
        assert_eq!(id.artist.as_deref(), Some("Forndom"));
        assert_eq!(id.album.as_deref(), Some("Flykt"));
        assert_eq!(id.chapter_titles.len(), 3);
        assert_eq!(id.chapter_titles[0].as_deref(), Some("Fajr"));
        assert_eq!(id.chapter_titles[1], None);
        assert_eq!(id.chapter_titles[2].as_deref(), Some("Hemmet"));
    }

    #[test]
    fn a_record_without_an_mbid_is_a_definitive_miss() {
        let bytes = encode(STAMP, None).expect("encode refused a miss");

        assert!(matches!(decode(&bytes, STAMP), Some(Cached::Unknown)));
    }

    #[test]
    fn a_changed_file_invalidates_the_record() {
        let bytes = encode(STAMP, Some(&full())).expect("encode refused a small record");

        let touched = Stamp {
            mtime_ns: STAMP.mtime_ns + 1,
            ..STAMP
        };
        let resized = Stamp {
            size: STAMP.size + 1,
            ..STAMP
        };

        assert!(decode(&bytes, touched).is_none());
        assert!(decode(&bytes, resized).is_none());
    }

    #[test]
    fn another_version_is_a_miss() {
        let record = serde_json::json!({
            "v": VERSION + 1,
            "mtime_ns": STAMP.mtime_ns,
            "size": STAMP.size,
            "mbid": "b1a9c0e8-d59d-4f0a-9b8c-2e4f6a1d3c57",
        });
        let bytes = serde_json::to_vec(&record).unwrap();

        assert!(decode(&bytes, STAMP).is_none());
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

    /// An MBID that is not a non-empty string is not something hark wrote, and
    /// guessing at it fetches the wrong album's cover.
    #[test]
    fn an_unusable_mbid_is_a_miss_rather_than_a_guess() {
        for mbid in [
            serde_json::json!(""),
            serde_json::json!(7),
            serde_json::json!({}),
            serde_json::json!([]),
        ] {
            let record = serde_json::json!({
                "v": VERSION,
                "mtime_ns": STAMP.mtime_ns,
                "size": STAMP.size,
                "mbid": mbid,
            });
            let bytes = serde_json::to_vec(&record).unwrap();

            assert!(decode(&bytes, STAMP).is_none());
        }
    }

    #[test]
    fn one_malformed_title_voids_the_whole_record() {
        let record = serde_json::json!({
            "v": VERSION,
            "mtime_ns": STAMP.mtime_ns,
            "size": STAMP.size,
            "mbid": "b1a9c0e8-d59d-4f0a-9b8c-2e4f6a1d3c57",
            "titles": ["Fajr", 7, "Hemmet"],
        });
        let bytes = serde_json::to_vec(&record).unwrap();

        assert!(decode(&bytes, STAMP).is_none());
    }

    #[test]
    fn an_empty_mbid_is_never_written() {
        let id = Identification {
            mbid: String::new(),
            ..full()
        };

        assert!(encode(STAMP, Some(&id)).is_none());
    }

    #[test]
    fn an_oversized_record_is_refused() {
        let id = Identification {
            chapter_titles: (0..500).map(|i| Some(format!("Chapter number {i}"))).collect(),
            ..full()
        };

        assert!(encode(STAMP, Some(&id)).is_none());
    }
}
