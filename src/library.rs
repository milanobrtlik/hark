use crate::track::is_supported;
use std::path::{Path, PathBuf};

/// The user's music folder, honouring the XDG user-dirs configuration before
/// falling back to `~/Music`.
pub fn music_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_MUSIC_DIR").map(PathBuf::from)
        && dir.is_dir()
    {
        return Some(dir);
    }

    let home = PathBuf::from(std::env::var_os("HOME")?);

    // user-dirs.dirs holds lines like: XDG_MUSIC_DIR="$HOME/Music"
    let config = home.join(".config/user-dirs.dirs");
    if let Ok(contents) = std::fs::read_to_string(&config) {
        for line in contents.lines() {
            let Some(value) = line.strip_prefix("XDG_MUSIC_DIR=") else {
                continue;
            };
            let value = value.trim().trim_matches('"');
            let dir = match value.strip_prefix("$HOME/") {
                Some(rest) => home.join(rest),
                None => PathBuf::from(value),
            };
            if dir.is_dir() {
                return Some(dir);
            }
        }
    }

    let fallback = home.join("Music");
    fallback.is_dir().then_some(fallback)
}

/// One root's scan.
pub struct Scan {
    pub root: PathBuf,
    /// Every playable file under `root`, recursively, in a stable order.
    pub files: Vec<PathBuf>,
    /// The walk saw the whole of the root and found music in it, so what it did
    /// *not* find can be taken as gone.
    ///
    /// An empty result is distrusted on purpose. A mount point whose mount is
    /// not up is still a directory that `read_dir` answers cheerfully and
    /// emptily, and reading that as "the music is gone" would empty the queue
    /// every time hark starts before the mount does. The cost is a folder whose
    /// last track was deleted keeping its rows until something else in the
    /// library changes — a stale row against a library that vanishes.
    ///
    /// Rejected: asking `/proc/mounts`, or comparing `st_dev` against the
    /// parent. Both make hark guess at the mount table to answer a question
    /// about music, and neither helps a folder that is merely unreadable.
    pub complete: bool,
}

/// Walks one root.
pub fn scan(root: &Path) -> Scan {
    let mut files = Vec::new();
    let readable = collect(root, &mut files);
    files.sort();

    Scan {
        root: root.to_path_buf(),
        complete: readable && !files.is_empty(),
        files,
    }
}

/// Walks every root, in the order given.
pub fn scan_roots(roots: &[PathBuf]) -> Vec<Scan> {
    roots.iter().map(|root| scan(root)).collect()
}

/// Expands a path — a file stays as-is, a directory is walked — into playable
/// files. Shared by the folder scan, the file dialog and drag & drop.
///
/// Returns false when any directory along the way would not open. Callers
/// merely adding files have nothing to do with that; [`scan`] uses it to decide
/// whether the result is complete enough to prune a queue against.
pub fn collect(path: &Path, out: &mut Vec<PathBuf>) -> bool {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return false;
        };
        let mut readable = true;
        for entry in entries.flatten() {
            readable &= collect(&entry.path(), out);
        }
        return readable;
    }

    if is_supported(path) {
        out.push(path.to_path_buf());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder, removed when the test ends.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(name: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("hark-library-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("the scratch folder could not be made");
        Scratch(path)
    }

    #[test]
    fn a_folder_that_is_not_there_is_not_a_complete_scan() {
        let scan = scan(Path::new("/nowhere/hark-has-never-been-here"));

        assert!(scan.files.is_empty());
        assert!(!scan.complete);
    }

    /// The case the whole flag exists for: a mount point whose mount is not up
    /// reads as an ordinary empty folder, and pruning against it would empty the
    /// queue of a library that is merely offline.
    #[test]
    fn an_empty_folder_is_not_a_complete_scan() {
        let dir = scratch("empty");

        assert!(!scan(&dir.0).complete);
    }

    #[test]
    fn a_folder_with_music_in_it_is_a_complete_scan() {
        let dir = scratch("music");
        std::fs::create_dir(dir.0.join("album")).expect("the album folder could not be made");
        std::fs::write(dir.0.join("album/a.flac"), b"").expect("the file could not be written");
        std::fs::write(dir.0.join("notes.txt"), b"").expect("the file could not be written");

        let scan = scan(&dir.0);

        assert!(scan.complete);
        assert_eq!(scan.files, vec![dir.0.join("album/a.flac")]);
    }
}
