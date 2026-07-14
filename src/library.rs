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

/// Every playable file under `root`, recursively, in a stable order.
pub fn scan(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect(root, &mut files);
    files.sort();
    files
}

/// Expands a path — a file stays as-is, a directory is walked — into playable
/// files. Shared by the folder scan, the file dialog and drag & drop.
pub fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_dir() {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            collect(&entry.path(), out);
        }
    } else if is_supported(path) {
        out.push(path.to_path_buf());
    }
}
