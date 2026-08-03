use crate::track::Track;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

impl Repeat {
    pub fn cycle(self) -> Repeat {
        match self {
            Repeat::Off => Repeat::All,
            Repeat::All => Repeat::One,
            Repeat::One => Repeat::Off,
        }
    }
}

#[derive(Default)]
pub struct Queue {
    pub tracks: Vec<Track>,
    /// Index into `tracks` of the track that is loaded in the player.
    pub current: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    /// Playback order when shuffling: a permutation of the track indices.
    order: Vec<usize>,
}

impl Queue {
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    pub fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.tracks.get(i))
    }

    pub fn extend(&mut self, tracks: impl IntoIterator<Item = Track>) {
        self.tracks.extend(tracks);
        self.reorder();
    }

    /// Drops the tracks `keep` rejects. The playing track is kept even when it
    /// is rejected — yanking it out from under the player would be worse than
    /// letting it finish, and `current` is how the whole player finds what it is
    /// showing, so removing it leaves the window saying nothing is playing over
    /// audible music.
    fn retain_where(&mut self, keep: impl Fn(&Track) -> bool) {
        let playing = self.current_track().map(|track| track.path.clone());

        self.tracks
            .retain(|track| keep(track) || Some(&track.path) == playing.as_ref());

        // Indices shifted, so the current track is found again by path.
        self.current =
            playing.and_then(|path| self.tracks.iter().position(|track| track.path == path));
        self.reorder();
    }

    /// Drops tracks that a scan of the root they came from no longer finds.
    /// `roots` holds only the roots whose scan could be believed, and `found`
    /// everything those scans saw.
    ///
    /// Two kinds of track survive on purpose. One that lies under no scanned
    /// root — a file dropped on the window, or named on the command line — was
    /// never the library's to remove. And a root left out of `roots` because its
    /// scan came back empty prunes nothing, which is what keeps a thousand rows
    /// on screen while the mount they live on is down.
    pub fn retain_scanned(&mut self, roots: &[PathBuf], found: &HashSet<PathBuf>) {
        self.retain_where(|track| {
            found.contains(&track.path) || !roots.iter().any(|root| track.path.starts_with(root))
        });
    }

    /// Drops every track that came from `root`, for a folder the listener took
    /// out of the library.
    pub fn remove_under(&mut self, root: &Path) {
        self.retain_where(|track| !track.path.starts_with(root));
    }

    pub fn set_shuffle(&mut self, shuffle: bool) {
        self.shuffle = shuffle;
        self.reorder();
    }

    /// The track after `current` in playback order, honouring shuffle and
    /// repeat. `None` means playback should stop.
    ///
    /// `manual` is true when the user pressed "next": repeat-one then behaves
    /// like repeat-all instead of replaying the same track forever.
    pub fn next(&self, manual: bool) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }
        let Some(current) = self.current else {
            return self.order.first().copied();
        };
        if self.repeat == Repeat::One && !manual {
            return Some(current);
        }

        let pos = self.position_of(current)?;
        if pos + 1 < self.order.len() {
            return Some(self.order[pos + 1]);
        }
        match self.repeat {
            Repeat::Off if manual => self.order.first().copied(),
            Repeat::Off => None,
            _ => self.order.first().copied(),
        }
    }

    pub fn previous(&self) -> Option<usize> {
        if self.tracks.is_empty() {
            return None;
        }
        let current = self.current?;
        let pos = self.position_of(current)?;
        if pos > 0 {
            Some(self.order[pos - 1])
        } else {
            self.order.last().copied()
        }
    }

    fn position_of(&self, track: usize) -> Option<usize> {
        self.order.iter().position(|&i| i == track)
    }

    /// Rebuilds the playback order, keeping the current track at the front of a
    /// freshly shuffled order so that "next" doesn't jump backwards.
    fn reorder(&mut self) {
        self.order = (0..self.tracks.len()).collect();
        if !self.shuffle {
            return;
        }

        shuffle(&mut self.order);
        if let Some(current) = self.current
            && let Some(pos) = self.order.iter().position(|&i| i == current)
        {
            self.order.swap(0, pos);
        }
    }
}

/// Fisher–Yates with a xorshift PRNG seeded from the clock. A music player does
/// not need a statistically serious random source, and this keeps the
/// dependency list short.
fn shuffle(items: &mut [usize]) {
    let mut state = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x2545_f491_4f6c_dd1d)
        | 1;

    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for i in (1..items.len()).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(path: &str) -> Track {
        Track {
            path: PathBuf::from(path),
            title: "".into(),
            artist: "".into(),
            album: "".into(),
            duration: std::time::Duration::ZERO,
            art: None,
            chapters: Vec::new(),
            has_art: false,
            loved: false,
        }
    }

    fn queue(paths: &[&str]) -> Queue {
        let mut queue = Queue::default();
        queue.extend(paths.iter().map(|path| track(path)));
        queue
    }

    fn paths(queue: &Queue) -> Vec<PathBuf> {
        queue.tracks.iter().map(|track| track.path.clone()).collect()
    }

    fn found(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_track_the_scan_no_longer_finds_is_dropped() {
        let mut queue = queue(&["/music/a.flac", "/music/b.flac"]);

        queue.retain_scanned(&[PathBuf::from("/music")], &found(&["/music/a.flac"]));

        assert_eq!(paths(&queue), vec![PathBuf::from("/music/a.flac")]);
    }

    /// A file dropped on the window belongs to no root, so no scan of the
    /// library has anything to say about it.
    #[test]
    fn a_track_from_outside_every_root_is_left_alone() {
        let mut queue = queue(&["/music/a.flac", "/elsewhere/b.flac"]);

        queue.retain_scanned(&[PathBuf::from("/music")], &found(&["/music/a.flac"]));

        assert_eq!(
            paths(&queue),
            vec![
                PathBuf::from("/music/a.flac"),
                PathBuf::from("/elsewhere/b.flac")
            ]
        );
    }

    /// The mount that is not up: its scan came back empty, so it was left out of
    /// the roots that get to prune, and its thousand rows stay.
    #[test]
    fn a_root_whose_scan_could_not_be_believed_keeps_its_tracks() {
        let mut queue = queue(&["/mnt/music/a.flac", "/music/b.flac"]);

        queue.retain_scanned(&[PathBuf::from("/music")], &found(&["/music/b.flac"]));

        assert_eq!(
            paths(&queue),
            vec![
                PathBuf::from("/mnt/music/a.flac"),
                PathBuf::from("/music/b.flac")
            ]
        );
    }

    #[test]
    fn the_playing_track_survives_its_folder_vanishing() {
        let mut queue = queue(&["/music/a.flac", "/music/b.flac"]);
        queue.current = Some(1);

        queue.retain_scanned(&[PathBuf::from("/music")], &found(&[]));

        assert_eq!(paths(&queue), vec![PathBuf::from("/music/b.flac")]);
        // Indices shifted underneath it, so this is the interesting half.
        assert_eq!(queue.current, Some(0));
    }

    #[test]
    fn removing_a_folder_takes_its_tracks_but_not_the_one_playing() {
        let mut queue = queue(&["/music/a.flac", "/music/b.flac", "/other/c.flac"]);
        queue.current = Some(1);

        queue.remove_under(Path::new("/music"));

        assert_eq!(
            paths(&queue),
            vec![PathBuf::from("/music/b.flac"), PathBuf::from("/other/c.flac")]
        );
        assert_eq!(queue.current, Some(0));
    }

    /// `starts_with` compares whole components, which is the only reason this
    /// holds — a prefix match on the string would take the sibling too.
    #[test]
    fn removing_a_folder_leaves_a_sibling_with_a_longer_name_alone() {
        let mut queue = queue(&["/music/rock/a.flac", "/music/rockabilly/b.flac"]);

        queue.remove_under(Path::new("/music/rock"));

        assert_eq!(paths(&queue), vec![PathBuf::from("/music/rockabilly/b.flac")]);
    }
}
