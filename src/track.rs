use gpui::{Image, ImageFormat, SharedString};
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::prelude::Accessor;
use lofty::probe::Probe;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone)]
pub struct Track {
    pub path: PathBuf,
    pub title: SharedString,
    pub artist: SharedString,
    pub album: SharedString,
    pub duration: Duration,
    pub art: Option<Arc<Image>>,
}

pub const SUPPORTED: [&str; 8] = ["mp3", "flac", "ogg", "oga", "wav", "m4a", "aac", "opus"];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

impl Track {
    /// Reads tags, cover art and duration. Falls back to the file name when a
    /// file carries no tags at all.
    pub fn load(path: PathBuf) -> Track {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Unknown track")
            .to_string();

        let mut track = Track {
            title: stem.into(),
            artist: "Unknown artist".into(),
            album: "Unknown album".into(),
            duration: Duration::ZERO,
            art: None,
            path: path.clone(),
        };

        let Ok(tagged) = Probe::open(&path).and_then(|p| p.read()) else {
            return track;
        };

        track.duration = tagged.properties().duration();

        let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
        let Some(tag) = tag else { return track };

        if let Some(title) = tag.title().filter(|t| !t.trim().is_empty()) {
            track.title = title.to_string().into();
        }
        if let Some(artist) = tag.artist().filter(|t| !t.trim().is_empty()) {
            track.artist = artist.to_string().into();
        }
        if let Some(album) = tag.album().filter(|t| !t.trim().is_empty()) {
            track.album = album.to_string().into();
        }

        track.art = tag.pictures().iter().find_map(|picture| {
            let format = match picture.mime_type()?.as_str() {
                "image/jpeg" | "image/jpg" => ImageFormat::Jpeg,
                "image/png" => ImageFormat::Png,
                "image/webp" => ImageFormat::Webp,
                "image/gif" => ImageFormat::Gif,
                _ => return None,
            };
            Some(Arc::new(Image::from_bytes(
                format,
                picture.data().to_vec(),
            )))
        });

        track
    }
}

/// `1:06`, and `-1:49` for the remaining side.
pub fn format_time(d: Duration) -> String {
    let total = d.as_secs();
    format!("{}:{:02}", total / 60, total % 60)
}
