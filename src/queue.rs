use crate::track::Track;
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
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

    pub fn contains(&self, path: &PathBuf) -> bool {
        self.tracks.iter().any(|track| &track.path == path)
    }

    /// Drops tracks whose files are gone. The playing track is kept even if it
    /// vanished — yanking it out from under the player would be worse than
    /// letting it finish.
    pub fn retain_existing(&mut self, existing: &HashSet<PathBuf>) {
        let playing = self.current_track().map(|track| track.path.clone());

        self.tracks
            .retain(|track| existing.contains(&track.path) || Some(&track.path) == playing.as_ref());

        // Indices shifted, so the current track is found again by path.
        self.current =
            playing.and_then(|path| self.tracks.iter().position(|track| track.path == path));
        self.reorder();
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
