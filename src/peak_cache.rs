//! The waveform's peaks, stashed in the file's own `user.hark.peaks` extended
//! attribute.
//!
//! Drawing the seek bar means decoding the entire file: a five-minute track is
//! tens of millions of samples, and on a network mount it drags the whole thing
//! across a second time, on every single play of the same track. It is by far
//! the most expensive thing hark does and the only expensive thing it does over
//! and over. The answer is 237 bytes, so it is kept.
//!
//! A separate attribute rather than a few more fields on `user.hark.meta`,
//! for three reasons. The tags are read when the library is scanned and these
//! are computed when a track plays, so they have different writers and different
//! lifetimes. Keeping them apart leaves [`crate::meta_cache::VERSION`] alone,
//! so adding this invalidates nobody's tag records. And ext4 fits all of an
//! inode's attributes into one 4 KiB block, which the tags should not have to
//! share with a peak set that may yet grow.
//!
//! Nothing here reports failure, exactly as in [`crate::meta_cache`]: a
//! filesystem without extended attributes, a read-only mount, a record from
//! another version and a record whose file has changed since all mean the same
//! thing to the caller — decode the file. Remove a record by hand with
//! `setfattr -x user.hark.peaks <file>`.

use crate::meta_cache::Stamp;
use crate::waveform::PEAKS;
use std::path::Path;

const XATTR_NAME: &str = "user.hark.peaks";

/// Bumped when the record's shape changes — or when the computation behind it
/// does. That second half is what makes this different in kind from the tag
/// record: that one mirrors what is in the file, this one mirrors a
/// calculation. Change the gamma or the window size in
/// [`crate::waveform::compute`] and every stored record becomes a confidently
/// wrong answer that still passes its stamp, because the file it describes has
/// not moved. This byte is the only thing that catches that, so it moves with
/// the algorithm too.
const VERSION: u8 = 1;

/// The version byte, then the stamp.
const HEADER: usize = 1 + 8 + 8;

/// The whole record, which is fixed. That makes this the layout, the size
/// budget and the migration for a change to `PEAKS` all at once: a record of
/// any other length was written when `PEAKS` was a different number, and is
/// refused on sight.
const RECORD: usize = HEADER + PEAKS;

/// The peaks cached on `path`, if the file still matches `stamp`. A missing
/// attribute, an unsupported filesystem and an unreadable one are all just
/// "no record".
pub fn read(path: &Path, stamp: Stamp) -> Option<Vec<f32>> {
    let Ok(Some(bytes)) = xattr::get_deref(path, XATTR_NAME) else {
        return None;
    };
    decode(&bytes, stamp)
}

/// Stamps `path` with the peaks decoded out of it. A filesystem with no
/// extended attributes, a read-only mount and a full attribute block all fail
/// here quietly; the only cost is that the next play decodes the file again.
pub fn write(path: &Path, stamp: Stamp, peaks: &[f32]) {
    let Some(bytes) = encode(stamp, peaks) else {
        return;
    };
    let _ = xattr::set_deref(path, XATTR_NAME, &bytes);
}

/// Builds the record, or refuses to when the peaks are not a full set.
///
/// That refusal carries the whole "never cache a failure" rule.
/// [`crate::waveform::compute`] yields an empty vector when the decoder
/// produced no samples at all — which is a zero-length file, but is equally a
/// file half-copied onto the mount or a decoder that gave up on the first
/// frame, and by the time the result arrives here the three are
/// indistinguishable. Writing it would freeze a passing hiccup into the file as
/// a permanently flat waveform, curable only with `setfattr`. There is nothing
/// to gain either: a file that decodes to nothing decodes to nothing quickly. A
/// *silent* file is a different matter and is cached — `compute` skips its
/// normalization when it finds no signal, so silence arrives here as a full set
/// of honest zeros.
///
/// One byte per peak. `compute` clamps every value into `0.05..=1.0`, so a byte
/// resolves it to about a five-hundredth, and the bars are drawn 46 px tall —
/// under a fifth of a pixel, which no display can show.
fn encode(stamp: Stamp, peaks: &[f32]) -> Option<Vec<u8>> {
    if peaks.len() != PEAKS {
        return None;
    }

    let mut bytes = Vec::with_capacity(RECORD);
    bytes.push(VERSION);
    bytes.extend_from_slice(&stamp.mtime_ns.to_le_bytes());
    bytes.extend_from_slice(&stamp.size.to_le_bytes());
    bytes.extend(
        peaks
            .iter()
            .map(|peak| (peak.clamp(0.0, 1.0) * 255.0).round() as u8),
    );
    Some(bytes)
}

/// Reads a record back, refusing anything that does not describe exactly this
/// version of exactly this file. The length and the version are checked before
/// anything is indexed, so every slice below is already known to be in range —
/// a truncated, padded or foreign attribute is a miss rather than a panic.
fn decode(bytes: &[u8], stamp: Stamp) -> Option<Vec<f32>> {
    if bytes.len() != RECORD || bytes[0] != VERSION {
        return None;
    }

    let mtime_ns = i64::from_le_bytes(bytes[1..9].try_into().ok()?);
    let size = u64::from_le_bytes(bytes[9..HEADER].try_into().ok()?);
    if mtime_ns != stamp.mtime_ns || size != stamp.size {
        return None;
    }

    Some(
        bytes[HEADER..]
            .iter()
            .map(|&peak| peak as f32 / 255.0)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const STAMP: Stamp = Stamp {
        mtime_ns: 1_721_742_401_123_456_789,
        size: 8_123_456,
    };

    /// A set with the shape of a real track: quiet at the edges, loud in the
    /// middle, and never outside the range `compute` clamps to.
    fn peaks() -> Vec<f32> {
        (0..PEAKS)
            .map(|i| {
                let t = i as f32 / (PEAKS - 1) as f32;
                (0.05 + 0.95 * (t * std::f32::consts::PI).sin()).clamp(0.05, 1.0)
            })
            .collect()
    }

    #[test]
    fn a_full_record_survives_a_round_trip() {
        let peaks = peaks();
        let Some(bytes) = encode(STAMP, &peaks) else {
            panic!("encode refused a full set of peaks");
        };
        let Some(decoded) = decode(&bytes, STAMP) else {
            panic!("decode refused its own output");
        };

        assert_eq!(decoded.len(), PEAKS);
        for (original, decoded) in peaks.iter().zip(&decoded) {
            assert!(
                (original - decoded).abs() <= 1.0 / 255.0,
                "{original} came back as {decoded}"
            );
        }
    }

    #[test]
    fn the_record_is_the_size_it_claims() {
        let bytes = encode(STAMP, &peaks()).expect("encode refused a full set of peaks");
        assert_eq!(bytes.len(), RECORD);
        assert_eq!(bytes.len(), 237);
    }

    /// Quantization is lossy, so a second pass must not drift further than the
    /// first — otherwise a record rewritten often would decay.
    #[test]
    fn the_round_trip_is_a_fixed_point() {
        let once = encode(STAMP, &peaks()).expect("encode refused a full set of peaks");
        let decoded = decode(&once, STAMP).expect("decode refused its own output");
        let twice = encode(STAMP, &decoded).expect("encode refused a decoded set");

        assert_eq!(once, twice);
    }

    #[test]
    fn silence_and_full_scale_are_exact() {
        for value in [0.0f32, 1.0f32] {
            let bytes =
                encode(STAMP, &vec![value; PEAKS]).expect("encode refused a full set of peaks");
            let decoded = decode(&bytes, STAMP).expect("decode refused its own output");
            assert!(decoded.iter().all(|&peak| peak == value));
        }
    }

    #[test]
    fn a_changed_file_invalidates_the_record() {
        let bytes = encode(STAMP, &peaks()).expect("encode refused a full set of peaks");

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
        let mut bytes = encode(STAMP, &peaks()).expect("encode refused a full set of peaks");
        bytes[0] = VERSION + 1;
        assert!(decode(&bytes, STAMP).is_none());

        bytes[0] = 0;
        assert!(decode(&bytes, STAMP).is_none());
    }

    #[test]
    fn a_truncated_or_padded_record_is_a_miss() {
        let bytes = encode(STAMP, &peaks()).expect("encode refused a full set of peaks");

        let mut padded = bytes.clone();
        padded.push(0);

        assert!(decode(&[], STAMP).is_none());
        assert!(decode(&bytes[..HEADER], STAMP).is_none());
        assert!(decode(&bytes[..bytes.len() - 1], STAMP).is_none());
        assert!(decode(&padded, STAMP).is_none());
        assert!(decode(b"not a record", STAMP).is_none());
    }

    #[test]
    fn an_incomplete_set_of_peaks_is_never_written() {
        let peaks = peaks();

        assert!(encode(STAMP, &[]).is_none());
        assert!(encode(STAMP, &peaks[..PEAKS - 1]).is_none());

        let mut too_many = peaks.clone();
        too_many.push(0.5);
        assert!(encode(STAMP, &too_many).is_none());
    }
}
