//! Which tracks the listener loves, kept in each file's own `user.hark.loved`
//! extended attribute.
//!
//! The three attributes hark writes beside this one are caches: throw one away
//! and the next scan, the next decode or the next lookup puts it back, and the
//! only symptom is that hark is slower for a moment. Nothing anywhere can work
//! out that someone loved a track — not the tags, not the audio, not the
//! filesystem — so here the record *is* the fact, and two habits that make the
//! caches comfortable to live with are wrong.
//!
//! It carries no [`crate::meta_cache::Stamp`]. Every other record holds the
//! file's modification time and size and is refused when they no longer match,
//! because a re-tagged file really does have different tags. A re-tagged file
//! does not have a different opinion of itself, and a stamp here would quietly
//! un-love a whole library the day somebody ran a tagger over it.
//!
//! And it reports failure. [`crate::meta_cache::write`] swallows everything,
//! because a scan of a thousand tracks must not be able to print a thousand
//! lines and because the only cost is doing the work again. Here a write that
//! fails is a button that did nothing, so [`write`] hands the error back and
//! the view says so out loud.
//!
//! Remove a record by hand with `setfattr -x user.hark.loved <file>` — unlike
//! the caches, that one is not free.

use std::io;
use std::path::Path;

const XATTR_NAME: &str = "user.hark.loved";

/// What a loved file carries: the version, as one ASCII byte, and nothing else.
/// No JSON, and no room reserved for any — [`crate::id_cache`] is JSON for a
/// handful of optional strings of no fixed length and [`crate::peak_cache`] is
/// bytes for a fixed run of numbers. One bit has nothing to name.
const MARK: &[u8] = b"1";

/// Whether `path` is loved.
///
/// The value is never looked at, only its presence — which is also this
/// module's version policy, and the one place it refuses to copy the other
/// three. There, a record from an unrecognised version is ignored, and ignoring
/// it costs a parse. Here it would cost the fact itself: a later hark writing a
/// `2` would read back as never loved on this one, and the next press of the
/// heart would put today's shape over it. So the byte goes in for a future
/// reader to dispatch on, and today's reader asks only what it can answer
/// forever — is anything there?
///
/// A file that is gone, a filesystem with no extended attributes and a mount
/// that is down all read as not loved. There is no third answer to give: the
/// heart is lit or it is not. It is [`write`] that has to be honest, because
/// that is the one the listener is watching.
pub fn read(path: &Path) -> bool {
    matches!(xattr::get_deref(path, XATTR_NAME), Ok(Some(_)))
}

/// Records that `path` is loved, or clears it.
///
/// Clearing removes the attribute rather than writing a record that says "no",
/// so un-hearting a track leaves the file exactly as hark found it. That also
/// keeps [`read`] the one-liner it is: with a "no" record in the world,
/// presence would no longer be the answer.
pub fn write(path: &Path, loved: bool) -> io::Result<()> {
    if loved {
        return xattr::set_deref(path, XATTR_NAME, MARK);
    }

    match xattr::remove_deref(path, XATTR_NAME) {
        Ok(()) => Ok(()),
        // Removing an attribute that is not there fails — with `ENODATA`, an
        // errno `std::io` has no name for — and so does removing one from a
        // filesystem that cannot hold attributes at all. Neither is worth
        // troubling anyone with, so the question is asked the other way round:
        // whatever the kernel made of the request, the file is not loved, which
        // is what was asked for. Loving still fails loudly on such a
        // filesystem, and rightly — there, "not loved" is already the truth on
        // disk and "loved" cannot be made true.
        Err(_) if !read(path) => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A scratch file to hang an attribute on, removed when the test ends.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A file under `TMPDIR`, or `None` when the filesystem behind it cannot
    /// hold a user attribute at all — an overlay in a container, a tmpfs on an
    /// older kernel. There is nothing to assert in that case: the test would be
    /// reporting on the filesystem rather than on hark, and failing it would
    /// make the suite a test of where it was run.
    fn scratch(name: &str) -> Option<Scratch> {
        let path = std::env::temp_dir().join(format!("hark-love-{}-{name}", std::process::id()));
        std::fs::write(&path, b"not really audio").ok()?;
        let scratch = Scratch(path);

        // Probe with the real thing, then leave the file as it was found.
        if xattr::set_deref(&scratch.0, XATTR_NAME, MARK).is_err() {
            return None;
        }
        xattr::remove_deref(&scratch.0, XATTR_NAME).ok()?;
        Some(scratch)
    }

    fn has_attribute(path: &Path) -> bool {
        let Ok(names) = xattr::list_deref(path) else {
            return false;
        };
        names
            .map(|name| name.to_string_lossy().into_owned())
            .any(|name| name == XATTR_NAME)
    }

    #[test]
    fn a_file_starts_out_unloved() {
        let Some(file) = scratch("fresh") else { return };

        assert!(!read(&file.0));
    }

    #[test]
    fn loving_a_file_is_visible_to_the_next_read() {
        let Some(file) = scratch("loved") else { return };

        assert!(write(&file.0, true).is_ok());
        assert!(read(&file.0));
    }

    #[test]
    fn the_mark_is_the_one_byte_it_claims() {
        let Some(file) = scratch("mark") else { return };

        assert!(write(&file.0, true).is_ok());
        assert_eq!(
            xattr::get_deref(&file.0, XATTR_NAME)
                .ok()
                .flatten()
                .as_deref(),
            Some(MARK)
        );
    }

    #[test]
    fn unloving_a_file_leaves_no_attribute_behind() {
        let Some(file) = scratch("cleared") else {
            return;
        };

        assert!(write(&file.0, true).is_ok());
        assert!(write(&file.0, false).is_ok());
        assert!(!read(&file.0));
        assert!(!has_attribute(&file.0));
    }

    #[test]
    fn unloving_a_file_that_was_never_loved_is_not_a_failure() {
        let Some(file) = scratch("never") else { return };

        assert!(write(&file.0, false).is_ok());
        assert!(!read(&file.0));
    }

    #[test]
    fn loving_is_idempotent() {
        let Some(file) = scratch("twice") else { return };

        assert!(write(&file.0, true).is_ok());
        assert!(write(&file.0, true).is_ok());
        assert!(read(&file.0));
    }

    /// The whole reason this module keeps no stamp: a tagger rewriting the file
    /// moves both its modification time and its size, and must not be able to
    /// take the heart with it.
    #[test]
    fn the_flag_survives_a_change_to_the_file() {
        let Some(file) = scratch("retagged") else {
            return;
        };

        assert!(write(&file.0, true).is_ok());
        std::fs::write(&file.0, b"rewritten, longer, and at a later moment").unwrap();

        assert!(read(&file.0));
    }

    /// Presence is the answer, so a record hark did not write — an empty one, or
    /// one from a version that does not exist yet — is never refused.
    #[test]
    fn an_attribute_written_by_hand_reads_as_loved() {
        let Some(file) = scratch("by-hand") else {
            return;
        };

        for value in [&b""[..], &b"7"[..], &b"{\"v\":2}"[..]] {
            xattr::set_deref(&file.0, XATTR_NAME, value).unwrap();
            assert!(read(&file.0));
        }
    }

    #[test]
    fn a_file_that_is_gone_cannot_be_loved_but_can_be_unloved() {
        let missing = std::env::temp_dir().join(format!("hark-love-{}-gone", std::process::id()));
        let _ = std::fs::remove_file(&missing);

        assert!(write(&missing, true).is_err());
        assert!(write(&missing, false).is_ok());
        assert!(!read(&missing));
    }
}
