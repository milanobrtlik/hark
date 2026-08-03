use crate::artwork;
use crate::audio::Audio;
use crate::fingerprint::{self, Identification};
use crate::library;
use crate::love;
use crate::mpris::{self, Mpris};
use crate::queue::{Queue, Repeat};
use crate::state;
use crate::theme;
use crate::track::{Chapter, Track, format_time};
use crate::ui::{bar, fraction_at, icon_button, overlay, rounded, toggle_button};
use crate::waveform;

use notify::{RecursiveMode, Watcher};

use gpui::{
    AnyWindowHandle, App, Bounds, Context, CursorStyle, Decorations, DragMoveEvent, Empty,
    ExternalPaths, FocusHandle, Focusable, HitboxBehavior, IntoElement, MouseButton, MouseDownEvent,
    ObjectFit,
    PathPromptOptions, Pixels, Point, Render, ResizeEdge, ScrollDelta, ScrollWheelEvent,
    SharedString, Size, Task, Window, actions,
    canvas, div, fill, img, point, prelude::*, px, size, svg,
};
use std::cell::Cell;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SHADOW_SIZE: Pixels = px(10.);
const CORNER: Pixels = px(14.);

/// How long a resume, pause or window-close takes to fade, and how often the
/// gain is stepped while it does. ~15 steps keeps the ramp free of zipper noise.
const FADE: Duration = Duration::from_millis(180);
const FADE_STEP: Duration = Duration::from_millis(12);

/// Horizontal inset shared by the waveform, volume and footer rows so they line
/// up at one width, a little narrower than the full content column.
const RAIL_INSET: Pixels = px(16.);

actions!(hark, [TogglePlay]);

/// Key context of the player window; the keymap binds `space` against it.
pub const KEY_CONTEXT: &str = "Player";

/// Drag markers. GPUI routes drag-move events by the payload type, so the seek
/// bar and the volume slider need distinct ones.
#[derive(Clone)]
struct SeekDrag;
#[derive(Clone)]
struct VolumeDrag;

/// What the two-stage cover/identify task found, applied back on the main thread.
enum Outcome {
    /// A cover decoded out of the file itself: fill that one track. `release`
    /// carries back the album key reserved before the task ran — iTunes was
    /// never asked, so a sibling track without a picture must still get to.
    TrackCover {
        path: PathBuf,
        image: Arc<gpui::Image>,
        release: Option<String>,
    },
    /// An iTunes cover for an album: fill every track sharing the key.
    AlbumCover { key: String, image: Arc<gpui::Image> },
    /// A fingerprint identity for one file: fill that file (cover + metadata).
    Identified {
        path: PathBuf,
        id: Identification,
        image: Option<Arc<gpui::Image>>,
    },
    /// A transient iTunes failure — free the album key so it retries.
    RetryAlbum { key: String },
    /// A transient fingerprint failure — free the path so it retries.
    RetryPath { path: PathBuf },
    Nothing,
}

/// Which filesystem events are worth a rescan. Metadata-only changes are not:
/// hark stamps every file it parses with an extended attribute, and every one of
/// those writes comes straight back as an attribute-change event. Without this
/// filter the first, cold scan would keep rescheduling itself for as long as it
/// ran — a full recursive walk of the folder roughly every second and a half,
/// alongside the scan already in flight. Content writes and renames do change
/// the set of files, so they stay.
fn is_library_change(kind: &notify::EventKind) -> bool {
    use notify::event::{EventKind, ModifyKind};

    if let EventKind::Modify(ModifyKind::Metadata(_)) = kind {
        return false;
    }
    kind.is_create() || kind.is_remove() || kind.is_modify()
}

/// Overwrites a track's metadata with what AcoustID recovered, leaving any field
/// the lookup didn't fill untouched. Titles land on the chapters of a full-album
/// file, or on the track itself when it has none.
fn apply_identity(track: &mut Track, id: &Identification) {
    if let Some(artist) = id.artist.as_deref().filter(|s| !s.is_empty()) {
        track.artist = artist.to_string().into();
    }
    if let Some(album) = id.album.as_deref().filter(|s| !s.is_empty()) {
        track.album = album.to_string().into();
    }
    if track.chapters.is_empty() {
        if let Some(title) = id
            .chapter_titles
            .first()
            .and_then(|t| t.as_deref())
            .filter(|s| !s.is_empty())
        {
            track.title = title.to_string().into();
        }
    } else {
        for (chapter, title) in track.chapters.iter_mut().zip(&id.chapter_titles) {
            if let Some(title) = title.as_deref().filter(|s| !s.is_empty()) {
                chapter.title = title.to_string().into();
            }
        }
    }
}

pub struct PlayerView {
    audio: Option<Audio>,
    error: Option<SharedString>,
    queue: Queue,
    /// Keeps the window focused on the player so key bindings reach it.
    focus_handle: FocusHandle,
    /// The desktop's media controls, including the keyboard's media keys.
    mpris: Mpris,
    /// Normalized peaks of the current track; empty while still decoding.
    peaks: Vec<f32>,
    /// Position being previewed while the seek bar is dragged, 0..=1. The seek
    /// itself only happens on release — seeking on every mouse move makes the
    /// decoder stutter.
    scrub: Option<f32>,
    show_playlist: bool,
    /// The folder panel. Not kept in the session, unlike `show_playlist`: it is
    /// a place you go to change something and then leave, and opening hark
    /// tomorrow into the folder list is not where anyone left off.
    show_folders: bool,
    /// The XDG music folder, scanned and watched on every start whether or not
    /// it was ever asked for. Held apart from `folders` rather than merged into
    /// it, because it is the one root the listener cannot take away — it comes
    /// back from the desktop's configuration on the next start regardless.
    music_dir: Option<PathBuf>,
    /// The library roots the listener added, and what the state file keeps.
    folders: Vec<Folder>,
    /// The queue panel is showing only the loved tracks. A lens on the panel
    /// rather than a mode of the player, which is why it is not kept in the
    /// session: coming back tomorrow to six rows of a thousand-track library,
    /// with nothing on screen saying why, reads as a lost library.
    show_loved: bool,
    /// Set once a fade-out-before-close is under way, so the close is only
    /// deferred once and the second attempt is let through.
    closing: bool,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    _waveform_task: Option<Task<()>>,
    /// Fetches a cover for a track that has none. Dropping it cancels the fetch.
    _cover_task: Option<Task<()>>,
    /// Normalized (artist, album) keys already fetched or attempted this session,
    /// so moving between tracks of one album doesn't refetch.
    cover_keys: HashSet<String>,
    /// Paths already run through the fingerprint fallback this session, so a
    /// badly-tagged file isn't re-identified every time it plays.
    fp_paths: HashSet<PathBuf>,
    /// Runs a play/pause volume ramp; replacing it supersedes a fade still going.
    _fade_task: Option<Task<()>>,
    _tick: Task<()>,
    /// Watches the music folder. Dropping it stops the notifications.
    _watcher: Option<notify::RecommendedWatcher>,
    /// The track the last session stopped on, waiting for a scan that contains
    /// it. Given up the moment anything else starts playing.
    resume: Option<Resume>,
    /// The window as the compositor last reported it. Kept because the quit hook
    /// runs with no `Window` left to ask.
    window: Bounds<Pixels>,
    /// The session as last written, and when. Compared every few seconds, so an
    /// idle player writes nothing at all.
    saved: state::State,
    saved_at: Instant,
}

/// Where the last session stopped, until a scan turns its file up.
struct Resume {
    path: PathBuf,
    position: Duration,
}

/// A remembered library root, and what the last scan made of it.
struct Folder {
    path: PathBuf,
    /// The last scan of this root could be believed. Starts true and is
    /// corrected by the first scan: showing a folder as reachable for the second
    /// before anything has looked at it is a far smaller lie than greying out a
    /// whole library because nothing has scanned yet.
    available: bool,
}

/// What adding a folder means for the roots already there.
enum Adopt {
    /// A root already covers it. Its music is in the queue either way, and a
    /// second root over the same tree would only be scanned and watched twice.
    Covered,
    /// It joins the roots, and any root it contains steps aside — keeping both
    /// would leave a folder that cannot be removed without removing its parent.
    Adopted { superseded: Vec<PathBuf> },
}

/// Decides `dir` against the roots already in use. `roots` includes the music
/// folder, which is why choosing it again changes nothing; `removable` is only
/// what the listener added, so the music folder is never superseded and adding
/// its parent leaves both in place. The two then overlap, which costs a second
/// walk and a few inotify watches over the shared tree — the price of never
/// producing a root that cannot be taken away again.
fn adopt(dir: &Path, roots: &[PathBuf], removable: &[PathBuf]) -> Adopt {
    // `dir.starts_with(dir)` holds, so this is also the exact-duplicate case.
    if roots.iter().any(|root| dir.starts_with(root)) {
        return Adopt::Covered;
    }

    Adopt::Adopted {
        superseded: removable
            .iter()
            .filter(|root| root.starts_with(dir))
            .cloned()
            .collect(),
    }
}

/// A track stopped within this of its end resumes from the start instead. It had
/// effectively finished, and its last few seconds are not where anyone left off.
const RESUME_MARGIN: Duration = Duration::from_secs(5);

/// How often the session is written while it is changing. The quit hook writes
/// the final state itself; this is what survives a signal, a crash or a logout,
/// which never reach that hook — so it is frequent enough to lose nothing anyone
/// would notice and rare enough to be invisible.
const SAVE_EVERY: Duration = Duration::from_secs(5);

impl PlayerView {
    pub fn new(
        paths: Vec<PathBuf>,
        state: state::State,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.observe_window_appearance(window, |_, window, _| window.refresh())
            .detach();

        cx.observe_window_bounds(window, |this, window, _| {
            // `get_bounds` reports a maximized window's restore size, which is
            // the one worth coming back to.
            this.window = window.window_bounds().get_bounds();
        })
        .detach();

        // The only save guaranteed to hold the final position. It runs for the
        // close button and for `cx.quit()`, which are the only polite ways out —
        // MPRIS cannot quit hark at all. Synchronous, because quit observers are
        // given a hundred milliseconds and a background spawn may never be
        // polled inside it.
        cx.on_app_quit(|this, _| {
            state::save(&this.snapshot());
            std::future::ready(())
        })
        .detach();

        let (audio, error) = match Audio::new(state.volume) {
            Ok(audio) => (Some(audio), None),
            Err(err) => (None, Some(SharedString::from(err.to_string()))),
        };

        let focus_handle = cx.focus_handle();
        window.focus(&focus_handle);

        // Fade the audio out before the window manager closes the window too.
        let weak = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            let handle = window.window_handle();
            weak.update(cx, |this, cx| this.begin_close(handle, cx))
                .unwrap_or(true)
        });

        let mut view = PlayerView {
            audio,
            error,
            queue: Queue::default(),
            focus_handle,
            mpris: Mpris::new(),
            peaks: Vec::new(),
            scrub: None,
            show_playlist: false,
            show_folders: false,
            music_dir: library::music_dir(),
            folders: state
                .folders
                .iter()
                .map(|path| Folder {
                    path: path.clone(),
                    available: true,
                })
                .collect(),
            show_loved: false,
            closing: false,
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            volume_bounds: Rc::new(Cell::new(Bounds::default())),
            _waveform_task: None,
            _cover_task: None,
            cover_keys: HashSet::new(),
            fp_paths: HashSet::new(),
            _fade_task: None,
            _tick: Self::spawn_tick(cx),
            _watcher: None,
            // Files named on the command line are what was actually asked for,
            // so the remembered session is not armed at all in that case. That
            // also settles the race: the argv batch and a restore can never both
            // want the player, whichever scan lands first.
            resume: paths
                .is_empty()
                .then(|| {
                    state.track.clone().map(|path| Resume {
                        path,
                        position: state.position,
                    })
                })
                .flatten(),
            window: window.window_bounds().get_bounds(),
            saved: state.clone(),
            saved_at: Instant::now(),
        };

        view.queue.set_shuffle(state.shuffle);
        view.queue.repeat = state.repeat;
        view.show_playlist = state.playlist;

        // Files named on the command line play straight away, and go in through
        // `add_paths` rather than `add_chosen`: naming a folder on the command
        // line is one run of hark, not a change to the library.
        if !paths.is_empty() {
            view.add_paths(paths, true, cx);
        }

        view.start_watching(cx);
        for root in view.roots() {
            view.watch(&root);
        }
        // One scan of every root rather than one `add_paths` per root: two roots
        // that overlap would otherwise each load the shared files, and both
        // batches pass a duplicate filter taken before either of them ran.
        view.rescan(cx);

        view
    }

    /// Every root the queue is built from: the music folder first, then what the
    /// listener added.
    fn roots(&self) -> Vec<PathBuf> {
        self.music_dir
            .iter()
            .chain(self.folders.iter().map(|folder| &folder.path))
            .cloned()
            .collect()
    }

    /// Creates the one watcher behind every root, and the loop that debounces
    /// what it reports.
    ///
    /// One watcher for all of them: `notify` already keeps a watch per path, so
    /// a second root is one more `watch` call. A watcher each would also mean a
    /// debounce loop each, and two loops racing to rescan the same roots is one
    /// copied file paying for two walks of the library.
    fn start_watching(&mut self, cx: &mut Context<Self>) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        match notify::recommended_watcher(move |event| {
            if let Ok(notify::Event { kind, .. }) = event
                && is_library_change(&kind)
            {
                let _ = tx.send(());
            }
        }) {
            Ok(watcher) => self._watcher = Some(watcher),
            Err(err) => {
                eprintln!("hark: cannot watch for changes: {err}");
                return;
            }
        }

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(700))
                    .await;

                if rx.try_recv().is_err() {
                    if this.upgrade().is_none() {
                        break;
                    }
                    continue;
                }

                // Let a batch of changes (a folder being copied in) settle
                // before rescanning, and swallow the events it produced.
                cx.background_executor()
                    .timer(Duration::from_millis(800))
                    .await;
                while rx.try_recv().is_ok() {}

                if this.update(cx, |this, cx| this.rescan(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Starts watching one root. A root that is not there — a mount that has not
    /// come up — is simply not watched and says nothing about it; the rescan any
    /// other root triggers picks its music up once it appears.
    fn watch(&mut self, dir: &Path) {
        let Some(watcher) = self._watcher.as_mut() else {
            return;
        };
        if let Err(err) = watcher.watch(dir, RecursiveMode::Recursive)
            && !matches!(err.kind, notify::ErrorKind::PathNotFound)
        {
            eprintln!("hark: cannot watch {}: {err}", dir.display());
        }
    }

    /// Stops watching a root the listener removed. A root whose `watch` never
    /// took is not worth a word: that is the unreachable folder, arriving
    /// exactly as expected.
    fn unwatch(&mut self, dir: &Path) {
        if let Some(watcher) = self._watcher.as_mut() {
            let _ = watcher.unwatch(dir);
        }
    }

    /// Rescans every root and merges the result into the queue. Startup and the
    /// watcher both come through here, so there is one way the queue is built
    /// rather than two that can disagree.
    fn rescan(&mut self, cx: &mut Context<Self>) {
        let roots = self.roots();
        if roots.is_empty() {
            return;
        }

        cx.spawn(async move |this, cx| {
            let scans = cx
                .background_executor()
                .spawn(async move { library::scan_roots(&roots) })
                .await;

            this.update(cx, |this, cx| this.library_changed(scans, cx)).ok();
        })
        .detach();
    }

    /// Merges a fresh scan of every root into the queue.
    fn library_changed(&mut self, scans: Vec<library::Scan>, cx: &mut Context<Self>) {
        for scan in &scans {
            if let Some(folder) = self
                .folders
                .iter_mut()
                .find(|folder| folder.path == scan.root)
            {
                folder.available = scan.complete;
            }
        }

        // Only the roots whose scan could be believed get to prune, and they
        // prune against everything those scans saw — a file that moved between
        // two roots is still in the library.
        let scanned: Vec<PathBuf> = scans
            .iter()
            .filter(|scan| scan.complete)
            .map(|scan| scan.root.clone())
            .collect();
        let found: HashSet<PathBuf> = scans
            .iter()
            .filter(|scan| scan.complete)
            .flat_map(|scan| scan.files.iter().cloned())
            .collect();
        self.queue.retain_scanned(&scanned, &found);

        // Files come from every scan, complete or not: a folder that is only
        // half readable still has music in it worth queueing.
        let known: HashSet<&PathBuf> = self.queue.tracks.iter().map(|track| &track.path).collect();
        let mut added: Vec<PathBuf> = scans
            .iter()
            .flat_map(|scan| scan.files.iter())
            .filter(|path| !known.contains(path))
            .cloned()
            .collect();
        added.sort();
        added.dedup();

        if added.is_empty() {
            cx.notify();
        } else {
            self.add_paths(added, false, cx);
        }
    }

    /// Drives the playback clock: repaints while a track plays and advances the
    /// queue when one ends. Runs for the lifetime of the view.
    fn spawn_tick(cx: &mut Context<Self>) -> Task<()> {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(33))
                    .await;

                let alive = this
                    .update(cx, |this, cx| {
                        this.sync_mpris(cx);
                        this.save_if_due(cx);

                        let finished = this.audio.as_ref().is_some_and(|a| a.finished());
                        if finished {
                            this.advance(false, cx);
                        } else if this.is_playing() {
                            cx.notify();
                        }
                    })
                    .is_ok();

                if !alive {
                    break;
                }
            }
        })
    }

    fn is_playing(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.is_playing())
    }

    // -- chapters ----------------------------------------------------------
    //
    // A full-album file marks each song as a chapter. The player then treats the
    // current chapter as if it were the track: the seek bar, times and title all
    // scope to it, and prev/next step between chapters. A file without chapters
    // behaves exactly as before — the "span" is simply the whole file.

    /// The current track's chapters, empty when it has none.
    fn chapters(&self) -> &[Chapter] {
        self.queue
            .current_track()
            .map(|t| t.chapters.as_slice())
            .unwrap_or(&[])
    }

    /// Which chapter the playback clock is inside, by the real position — never
    /// the scrub preview, since scrubbing moves within a chapter, not across.
    fn current_chapter(&self) -> Option<usize> {
        let chapters = self.chapters();
        if chapters.is_empty() {
            return None;
        }
        let pos = self.audio.as_ref().map(|a| a.position()).unwrap_or_default();
        Some(chapters.iter().rposition(|c| c.start <= pos).unwrap_or(0))
    }

    /// The stretch the seek bar and times cover: the current chapter, or the
    /// whole file when there are no chapters.
    fn active_span(&self) -> (Duration, Duration) {
        match self.current_chapter() {
            Some(i) => {
                let chapter = &self.chapters()[i];
                (chapter.start, chapter.end)
            }
            None => (Duration::ZERO, self.duration()),
        }
    }

    /// Absolute position in the file: the scrub preview mapped into the active
    /// span while dragging, otherwise the player's real clock.
    fn position(&self) -> Duration {
        if let Some(scrub) = self.scrub {
            let (start, end) = self.active_span();
            return start + end.saturating_sub(start).mul_f32(scrub);
        }
        self.audio
            .as_ref()
            .map(|a| a.position())
            .unwrap_or_default()
    }

    fn duration(&self) -> Duration {
        self.queue
            .current_track()
            .map(|t| t.duration)
            .unwrap_or_default()
    }

    /// Elapsed time within the active span, starting from zero at its beginning.
    fn elapsed(&self) -> Duration {
        let (start, end) = self.active_span();
        self.position()
            .saturating_sub(start)
            .min(end.saturating_sub(start))
    }

    /// Progress through the active span, 0..=1.
    fn progress(&self) -> f32 {
        let (start, end) = self.active_span();
        let len = end.saturating_sub(start).as_secs_f32();
        if len <= 0.0 {
            return 0.0;
        }
        (self.elapsed().as_secs_f32() / len).clamp(0.0, 1.0)
    }

    /// The slice of decoded peaks inside the active span, so the bars show just
    /// the current chapter.
    fn span_peaks(&self) -> Vec<f32> {
        let total = self.duration().as_secs_f32();
        if total <= 0.0 || self.peaks.is_empty() {
            return self.peaks.clone();
        }
        let (start, end) = self.active_span();
        let n = self.peaks.len();
        let a = (((start.as_secs_f32() / total) * n as f32).floor() as usize).min(n);
        let b = (((end.as_secs_f32() / total) * n as f32).ceil() as usize).clamp(a, n);
        self.peaks[a..b].to_vec()
    }

    // -- queue -------------------------------------------------------------

    fn open_files(&mut self, cx: &mut Context<Self>) {
        self.prompt_paths(
            PathPromptOptions {
                files: true,
                directories: true,
                multiple: true,
                prompt: Some("Add".into()),
            },
            cx,
        );
    }

    /// The folder panel's own way in. On Linux the two dialogs are the same
    /// dialog: GPUI hands the request to the desktop portal keyed on
    /// `directories` alone and never reads `files`, so the footer's `+` has
    /// always been a folder chooser here. The options still say what is meant.
    fn open_folder(&mut self, cx: &mut Context<Self>) {
        self.prompt_paths(
            PathPromptOptions {
                files: false,
                directories: true,
                multiple: true,
                prompt: Some("Add".into()),
            },
            cx,
        );
    }

    /// Runs a path prompt and adds whatever comes back.
    fn prompt_paths(&mut self, options: PathPromptOptions, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(options);

        cx.spawn(async move |this, cx| {
            match paths.await {
                Ok(Ok(Some(paths))) => {
                    this.update(cx, |this, cx| this.add_chosen(paths, cx)).ok();
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => eprintln!("hark: the file dialog failed: {err}"),
                Err(err) => eprintln!("hark: the file dialog was dismissed: {err}"),
            }
        })
        .detach();
    }

    /// Adds what the listener actually chose — the dialog, a drop on the window,
    /// the folder panel — and remembers the folders among it, so they are back
    /// tomorrow. Plain [`Self::add_paths`] stays the way in for everything hark
    /// decides by itself: the rescan of roots it already knows, and the files
    /// named on the command line, which are one run and not a library.
    ///
    /// Only folders are kept. A single file is a thing to hear now, and a state
    /// file that slowly fills with every file ever dropped on the window would
    /// be a library nobody assembled.
    fn add_chosen(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        self.add_paths(paths.clone(), true, cx);

        cx.spawn(async move |this, cx| {
            // Off the main thread: `is_dir` and `canonicalize` are stats, and on
            // the kind of mount this feature exists for a stat is a round trip.
            let dirs = cx
                .background_executor()
                .spawn(async move {
                    paths
                        .into_iter()
                        .filter(|path| path.is_dir())
                        // Resolved once, here, while the folder is certainly
                        // there — so `~/Music` and `/home/x/Music` cannot become
                        // two roots. Never on load: a remembered path has to
                        // survive its mount being down, and `canonicalize`
                        // cannot.
                        .map(|path| std::fs::canonicalize(&path).unwrap_or(path))
                        .collect::<Vec<_>>()
                })
                .await;

            this.update(cx, |this, cx| this.remember(dirs, cx)).ok();
        })
        .detach();
    }

    /// Takes folders into the remembered list and starts watching them.
    fn remember(&mut self, dirs: Vec<PathBuf>, cx: &mut Context<Self>) {
        let mut changed = false;

        for dir in dirs {
            let removable: Vec<PathBuf> =
                self.folders.iter().map(|folder| folder.path.clone()).collect();
            let Adopt::Adopted { superseded } = adopt(&dir, &self.roots(), &removable) else {
                continue;
            };

            for old in superseded {
                self.forget_quietly(&old);
            }
            self.watch(&dir);
            self.folders.push(Folder {
                path: dir,
                available: true,
            });
            changed = true;
        }

        if changed {
            self.save_now(cx);
            cx.notify();
        }
    }

    /// Takes a folder out of the library: it stops being watched, stops being
    /// scanned, and its tracks leave the queue.
    ///
    /// The track that is playing stays, on the same trade `retain_scanned`
    /// makes — removing a folder is a change to the library, not an instruction
    /// to stop the music. It is then an orphan no scan will ever prune, which is
    /// right: it belongs to no root now, exactly like a file dropped on the
    /// window.
    fn forget(&mut self, dir: &Path, cx: &mut Context<Self>) {
        if !self.forget_quietly(dir) {
            return;
        }
        self.save_now(cx);
        cx.notify();
    }

    /// The removal itself, without the save. Split out for `remember`, which
    /// supersedes a folder in the middle of adding its parent and writes once
    /// at the end.
    fn forget_quietly(&mut self, dir: &Path) -> bool {
        let Some(index) = self.folders.iter().position(|folder| folder.path == dir) else {
            return false;
        };

        self.folders.remove(index);
        self.unwatch(dir);
        self.queue.remove_under(dir);
        true
    }

    /// Loads `paths` (files or folders) into the queue off the main thread.
    /// With `autoplay`, playback starts if nothing is playing yet.
    fn add_paths(&mut self, paths: Vec<PathBuf>, autoplay: bool, cx: &mut Context<Self>) {
        let known: HashSet<PathBuf> = self.queue.tracks.iter().map(|t| t.path.clone()).collect();
        let started = Instant::now();

        cx.spawn(async move |this, cx| {
            let (tracks, cached) = cx
                .background_executor()
                .spawn(async move {
                    let mut files = Vec::new();
                    for path in &paths {
                        library::collect(path, &mut files);
                    }
                    files.sort();
                    files.dedup();

                    // How many came out of the files' own metadata cache rather
                    // than a parse. On a filesystem without extended attributes
                    // this stays at zero every run, which is the signal that the
                    // cache is doing nothing there.
                    let mut cached = 0usize;
                    let tracks: Vec<Track> = files
                        .into_iter()
                        .filter(|path| !known.contains(path))
                        .map(|path| {
                            let (track, hit) = Track::load(path);
                            cached += hit as usize;
                            track
                        })
                        .collect();
                    (tracks, cached)
                })
                .await;

            this.update(cx, |this, cx| {
                // The filter above saw the queue as it stood when this load
                // began. Several roots load at once now, and a rescan can land
                // between them, so the queue as it is has the last word.
                let queued: HashSet<PathBuf> =
                    this.queue.tracks.iter().map(|track| track.path.clone()).collect();
                let tracks: Vec<Track> = tracks
                    .into_iter()
                    .filter(|track| !queued.contains(&track.path))
                    .collect();

                if tracks.is_empty() {
                    return;
                }
                eprintln!(
                    "hark: tracks queued: {} ({cached} cached) in {:.1}s",
                    tracks.len(),
                    started.elapsed().as_secs_f32()
                );
                let idle = this.queue.current.is_none();
                this.queue.extend(tracks);
                if autoplay && idle {
                    this.play_index(0, cx);
                } else if idle {
                    this.restore(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Starts the track at `index`. Every path that begins playback comes
    /// through here, which is also where the remembered session is given up:
    /// once the listener has played something, restoring what they were doing
    /// yesterday would be taking the player away from them.
    fn play_index(&mut self, index: usize, cx: &mut Context<Self>) {
        self.resume = None;
        self.open(index, Duration::ZERO, true, cx);
    }

    /// Loads the track at `index`, positioned at `at`. With `audible` false it
    /// comes up shown but silent.
    fn open(&mut self, index: usize, at: Duration, audible: bool, cx: &mut Context<Self>) {
        let Some(track) = self.queue.tracks.get(index).cloned() else {
            return;
        };
        // A new track supersedes any play/pause fade still ramping.
        self._fade_task = None;
        let Some(audio) = self.audio.as_mut() else {
            return;
        };

        if let Err(err) = audio.play_file(&track.path, audible) {
            self.error = Some(err.to_string().into());
            return;
        }
        if !at.is_zero() {
            audio.seek(at);
        }

        self.error = None;
        self.queue.current = Some(index);
        self.peaks.clear();
        self.load_waveform(track.path, cx);
        self.ensure_cover(cx);
        cx.notify();
    }

    /// Picks the last session back up, once a scan has actually brought its file
    /// into the queue. Paused: the waveform, the cover and the clock come up
    /// where they were left, and nothing is heard until asked.
    ///
    /// Tried after every batch that lands while nothing is playing, because the
    /// file may arrive with the folder scan, with a rescan after the mount came
    /// up late, or never. Not finding it is not a failure — the latch stays
    /// armed and the player sits exactly as it does today with no arguments.
    fn restore(&mut self, cx: &mut Context<Self>) {
        let Some(resume) = self.resume.as_ref() else {
            return;
        };
        let Some(index) = self
            .queue
            .tracks
            .iter()
            .position(|track| track.path == resume.path)
        else {
            return;
        };

        let at = resume.position;
        self.resume = None;
        self.open(index, at, false, cx);
    }

    /// The session as it stands, in the shape the state file keeps it.
    fn snapshot(&self) -> state::State {
        // The real position in the file, never `elapsed()`: that one is measured
        // from the start of the current chapter, and chapters are something the
        // view does to a file the player knows only as a whole. Seeking back to
        // an absolute position puts the view in the right chapter by itself.
        let played = self
            .audio
            .as_ref()
            .map(|audio| audio.position())
            .unwrap_or_default();
        let duration = self.duration();
        let finished = !duration.is_zero() && played + RESUME_MARGIN >= duration;

        let (track, position) = match self.queue.current_track() {
            Some(track) => (
                Some(track.path.clone()),
                if finished { Duration::ZERO } else { played },
            ),
            // Nothing has played yet, so the session worth keeping is still the
            // one that was restored — or would have been, had its file turned
            // up. Without this, opening hark and closing it again forgets where
            // you were.
            None => match &self.resume {
                Some(resume) => (Some(resume.path.clone()), resume.position),
                None => (None, Duration::ZERO),
            },
        };

        state::State {
            track,
            position,
            volume: self
                .audio
                .as_ref()
                .map(|audio| audio.volume())
                .unwrap_or(state::DEFAULT_VOLUME),
            shuffle: self.queue.shuffle,
            repeat: self.queue.repeat,
            playlist: self.show_playlist,
            folders: self.folders.iter().map(|folder| folder.path.clone()).collect(),
            window: Some(self.window),
        }
    }

    /// Writes the session out now and then, so a signal or a crash — neither of
    /// which reaches the quit hook — costs at most the last few seconds. The
    /// comparison is what keeps this quiet: a paused player's snapshot does not
    /// change, so it writes nothing at all.
    fn save_if_due(&mut self, cx: &mut Context<Self>) {
        if self.saved_at.elapsed() < SAVE_EVERY {
            return;
        }
        self.save_now(cx);
    }

    /// Writes the session out now, for a change the listener made on purpose and
    /// would not forgive losing. A position moves on its own and is worth a few
    /// seconds of risk; a folder they just added is not.
    fn save_now(&mut self, cx: &mut Context<Self>) {
        self.saved_at = Instant::now();

        let snapshot = self.snapshot();
        if snapshot == self.saved {
            return;
        }
        self.saved = snapshot.clone();

        // Off the main thread: it is a few hundred bytes to a local disk, but
        // the tick this runs on is what keeps the seek bar moving.
        cx.background_executor()
            .spawn(async move { state::save(&snapshot) })
            .detach();
    }

    /// The lookup stays on the background executor with the decode, cheap as it
    /// is: on a network mount reading an attribute is a round trip, and a wedged
    /// mount must not be able to freeze the window on every track change.
    fn load_waveform(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // Replacing the task cancels a decode still running for the old track.
        // Its record is still written, which is deliberate: the record is keyed
        // on the file rather than on whatever is playing, so skipping through a
        // folder warms every track it passes.
        self._waveform_task = Some(cx.spawn(async move |this, cx| {
            let peaks = cx
                .background_executor()
                .spawn(async move { waveform::load(&path) })
                .await;

            this.update(cx, |this, cx| {
                this.peaks = peaks;
                cx.notify();
            })
            .ok();
        }));
    }

    /// Finds a cover — and, as a last resort, correct metadata — for the current
    /// track when it has none. Three stages run in one background task:
    ///
    /// 0. **The file's own picture**, when the scan saw one. The bytes were left
    ///    in the file so that scanning a library would not drag every cover into
    ///    memory; this is where they are finally read, for the one track playing.
    /// 1. **Tags → iTunes** when the artist and album are real: on a hit every
    ///    queued track sharing the album is filled; each album is tried once.
    /// 2. **Fingerprint → AcoustID** when the tags are unusable, or when iTunes
    ///    has no such album: the file is identified by its sound, which yields a
    ///    Cover Art Archive cover plus the correct artist, album and per-chapter
    ///    titles. This is heavier, so it is gated behind a configured API key and
    ///    deduplicated per file.
    fn ensure_cover(&mut self, cx: &mut Context<Self>) {
        let Some(track) = self.queue.current_track() else {
            return;
        };
        if track.art.is_some() {
            return;
        }

        let tag_known = track.artist != crate::track::UNKNOWN_ARTIST
            && track.album != crate::track::UNKNOWN_ALBUM;
        let fp_enabled = fingerprint::API_KEY.is_some();

        // Stage 0 needs no bookkeeping: it fills the one track it read, which
        // then has art and never comes back here. Stage 1 is deduped by album
        // key; the fingerprint fallback for a tag-less file is deduped by path.
        let run_embedded = track.has_art;
        let album_key = tag_known.then(|| artwork::key(&track.artist, &track.album));
        let run_itunes = match &album_key {
            Some(key) => self.cover_keys.insert(key.clone()),
            None => false,
        };
        let run_fingerprint_directly =
            !tag_known && fp_enabled && self.fp_paths.insert(track.path.clone());
        if !run_embedded && !run_itunes && !run_fingerprint_directly {
            return;
        }

        let path = track.path.clone();
        let artist = track.artist.to_string();
        let album = track.album.to_string();
        let track = track.clone();

        // Replacing the task cancels a fetch still running for the old track.
        self._cover_task = Some(cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    // Stage 0: the picture in the file. One open, no network.
                    if run_embedded {
                        if let Some(image) = crate::track::embedded_art(&path) {
                            return Outcome::TrackCover {
                                path,
                                image,
                                release: album_key,
                            };
                        }
                        // The picture went away between the scan and now. Fall
                        // through only into a stage that was actually reserved:
                        // the fingerprint below was never deduped for this path,
                        // and would run again on every play.
                        if !run_itunes && !run_fingerprint_directly {
                            return Outcome::Nothing;
                        }
                    }

                    // Stage 1: tags → iTunes.
                    if run_itunes && let Some(key) = album_key {
                        let (image, transient) = artwork::load_or_fetch(&artist, &album);
                        if let Some(image) = image {
                            return Outcome::AlbumCover { key, image };
                        }
                        if transient {
                            return Outcome::RetryAlbum { key };
                        }
                        // A definitive miss falls through to the fingerprint path.
                    }

                    // Stage 2: fingerprint → AcoustID.
                    let Some(api_key) = fingerprint::API_KEY else {
                        return Outcome::Nothing;
                    };
                    let Some(id) = fingerprint::identify(&track, api_key) else {
                        return Outcome::RetryPath { path };
                    };
                    let (image, _transient) = artwork::load_or_fetch_release_group(&id.mbid);
                    Outcome::Identified { path, id, image }
                })
                .await;

            this.update(cx, |this, cx| match outcome {
                Outcome::TrackCover {
                    path,
                    image,
                    release,
                } => {
                    // Only this file: the picture came out of it.
                    if let Some(track) =
                        this.queue.tracks.iter_mut().find(|track| track.path == path)
                        && track.art.is_none()
                    {
                        track.art = Some(image);
                    }
                    // iTunes was never asked about this album, so hand its key
                    // back — a sibling track carrying no picture of its own
                    // still needs someone to ask.
                    if let Some(key) = release {
                        this.cover_keys.remove(&key);
                    }
                    cx.notify();
                }
                Outcome::AlbumCover { key, image } => {
                    // Indices may have shifted since the fetch began, so match by
                    // key; this also fills sibling and per-chapter tracks at once.
                    // A track with a picture of its own is left alone: it decodes
                    // that one when it plays, and after the scan every track has
                    // `art: None`, so nothing else would tell them apart.
                    for track in &mut this.queue.tracks {
                        if track.art.is_none()
                            && !track.has_art
                            && artwork::key(&track.artist, &track.album) == key
                        {
                            track.art = Some(image.clone());
                        }
                    }
                    cx.notify();
                }
                Outcome::Identified { path, id, image } => {
                    // Fill only the file that was fingerprinted — two unrelated
                    // badly-tagged files must not share one identity. `has_art`
                    // is deliberately not consulted here: this path is reached
                    // precisely when the file's own picture did not work out.
                    for track in &mut this.queue.tracks {
                        if track.path != path {
                            continue;
                        }
                        if let Some(image) = &image
                            && track.art.is_none()
                        {
                            track.art = Some(image.clone());
                        }
                        apply_identity(track, &id);
                    }
                    cx.notify();
                }
                // Transient failures should retry on a later play.
                Outcome::RetryAlbum { key } => {
                    this.cover_keys.remove(&key);
                }
                Outcome::RetryPath { path } => {
                    this.fp_paths.remove(&path);
                }
                Outcome::Nothing => {}
            })
            .ok();
        }));
    }

    fn advance(&mut self, manual: bool, cx: &mut Context<Self>) {
        match self.queue.next(manual) {
            Some(index) => self.play_index(index, cx),
            None => {
                self._fade_task = None;
                if let Some(audio) = self.audio.as_mut() {
                    audio.stop();
                }
                cx.notify();
            }
        }
    }

    fn previous(&mut self, cx: &mut Context<Self>) {
        // Mirrors every other player: within the first few seconds, "previous"
        // means the previous track; after that it restarts the current one.
        if self.position() > Duration::from_secs(3) {
            if let Some(audio) = self.audio.as_mut() {
                audio.seek(Duration::ZERO);
            }
            cx.notify();
            return;
        }
        if let Some(index) = self.queue.previous() {
            self.play_index(index, cx);
        }
    }

    fn seek_to(&mut self, pos: Duration, cx: &mut Context<Self>) {
        if let Some(audio) = self.audio.as_mut() {
            audio.seek(pos);
        }
        cx.notify();
    }

    /// Next: within a chaptered file, jump to the next chapter; at the last one
    /// (or a plain file) move on to the next track in the queue.
    fn skip_next(&mut self, cx: &mut Context<Self>) {
        if let Some(i) = self.current_chapter() {
            let chapters = self.chapters();
            if i + 1 < chapters.len() {
                let start = chapters[i + 1].start;
                self.seek_to(start, cx);
                return;
            }
        }
        self.advance(true, cx);
    }

    /// Previous, chapter-aware: a few seconds into a chapter it restarts that
    /// chapter, otherwise it steps back a chapter; before the first chapter it
    /// falls back to the previous track.
    fn skip_previous(&mut self, cx: &mut Context<Self>) {
        let Some(i) = self.current_chapter() else {
            self.previous(cx);
            return;
        };
        let chapters = self.chapters();
        let start = chapters[i].start;
        let pos = self.audio.as_ref().map(|a| a.position()).unwrap_or_default();

        if pos.saturating_sub(start) > Duration::from_secs(3) {
            self.seek_to(start, cx);
        } else if i > 0 {
            let prev = chapters[i - 1].start;
            self.seek_to(prev, cx);
        } else {
            self.previous(cx);
        }
    }

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.queue.current.is_none() {
            if !self.queue.is_empty() {
                self.play_index(0, cx);
            }
            return;
        }
        self.fade_to(!self.is_playing(), cx);
        cx.notify();
    }

    /// Loves the track that is loaded, or stops loving it, and writes that onto
    /// the file. Nothing to do when nothing is loaded — the heart is dimmed in
    /// that state, and the queue sitting there unplayed is how hark opens.
    ///
    /// On this thread and blocking, which is not how hark treats a mount
    /// anywhere else. It is one `setxattr` on one file, asked for by a click,
    /// and this same thread already opens a file and spins up a decoder every
    /// time a track starts. Spawning it would buy back one repaint frame and
    /// cost the only thing that matters here: nothing changes in memory unless
    /// the write worked, so the heart never shows a state the file does not
    /// have, and two quick presses cannot land out of order.
    fn toggle_loved(&mut self, cx: &mut Context<Self>) {
        let Some((index, track)) = self.queue.current.zip(self.queue.current_track()) else {
            return;
        };
        let loved = !track.loved;
        let path = track.path.clone();

        match love::write(&path, loved) {
            Ok(()) => {
                self.queue.tracks[index].loved = loved;
                self.error = None;
            }
            // The one attribute hark writes that somebody is watching.
            // Everywhere else a read-only mount costs a recomputation and says
            // nothing; here it costs the press, so it is said out loud.
            Err(err) => {
                self.error = Some(format!("could not write the heart onto the file: {err}").into());
            }
        }
        cx.notify();
    }

    /// Resumes or pauses the current track, easing the volume in or out so the
    /// change is never an abrupt cut. Play starts the audio straight away and
    /// ramps up; pause ramps down first and only then stops the player.
    fn fade_to(&mut self, audible: bool, cx: &mut Context<Self>) {
        let Some(audio) = self.audio.as_mut() else {
            return;
        };

        let from = audio.fade();
        if audible {
            audio.resume();
        } else {
            audio.begin_pause();
        }

        let target = if audible { 1.0 } else { 0.0 };
        let steps = (FADE.as_millis() / FADE_STEP.as_millis()).max(1) as u32;

        // Replacing the task cancels a fade still ramping for the opposite move.
        self._fade_task = Some(cx.spawn(async move |this, cx| {
            for step in 1..=steps {
                cx.background_executor().timer(FADE_STEP).await;
                let fade = from + (target - from) * (step as f32 / steps as f32);
                if this
                    .update(cx, |this, cx| {
                        if let Some(audio) = this.audio.as_mut() {
                            audio.set_fade(fade);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }

            // Land exactly on the target and, for a pause, stop the player now
            // that it has gone quiet.
            this.update(cx, |this, cx| {
                if let Some(audio) = this.audio.as_mut() {
                    audio.set_fade(target);
                    if !audible {
                        audio.commit_pause();
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Routes the close button through the same fade-out-then-close as the window
    /// manager's close.
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.begin_close(window.window_handle(), cx) {
            window.remove_window();
        }
    }

    /// Fades the audio out and then closes `handle`. Returns `true` if the window
    /// may close at once — nothing is playing, or a close is already under way —
    /// and `false` when the close is deferred until the fade completes.
    fn begin_close(&mut self, handle: AnyWindowHandle, cx: &mut Context<Self>) -> bool {
        if self.closing {
            return true;
        }
        self.closing = true;

        let from = self.audio.as_ref().map(|a| a.fade()).unwrap_or(0.0);
        if !self.is_playing() || from <= 0.0 {
            return true;
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.begin_pause();
        }

        let steps = (FADE.as_millis() / FADE_STEP.as_millis()).max(1) as u32;
        self._fade_task = Some(cx.spawn(async move |this, cx| {
            for step in 1..=steps {
                cx.background_executor().timer(FADE_STEP).await;
                let fade = from * (1.0 - step as f32 / steps as f32);
                if this
                    .update(cx, |this, cx| {
                        if let Some(audio) = this.audio.as_mut() {
                            audio.set_fade(fade);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            handle.update(cx, |_, window, _| window.remove_window()).ok();
        }));

        false
    }

    // -- desktop media controls --------------------------------------------

    /// Carries out what the desktop asked for and mirrors the player back onto
    /// the bus. Runs on every tick.
    fn sync_mpris(&mut self, cx: &mut Context<Self>) {
        for command in self.mpris.commands() {
            match command {
                mpris::Command::TogglePlay => self.toggle_play(cx),
                mpris::Command::Play if !self.is_playing() => self.toggle_play(cx),
                mpris::Command::Pause if self.is_playing() => self.toggle_play(cx),
                mpris::Command::Play | mpris::Command::Pause => {}
                mpris::Command::Next => self.skip_next(cx),
                mpris::Command::Previous => self.skip_previous(cx),
                mpris::Command::SetVolume(volume) => {
                    if let Some(audio) = self.audio.as_mut() {
                        audio.set_volume(volume);
                    }
                    cx.notify();
                }
            }
        }

        self.mpris.publish(self.mpris_state(), self.position());
    }

    fn mpris_state(&self) -> mpris::State {
        mpris::State {
            playing: self.is_playing(),
            volume: self.audio.as_ref().map(|a| a.volume()).unwrap_or(0.0),
            track: self
                .queue
                .current
                .zip(self.queue.current_track())
                .map(|(index, track)| mpris::TrackInfo {
                    index,
                    title: track.title.to_string(),
                    artist: track.artist.to_string(),
                    album: track.album.to_string(),
                    duration: track.duration,
                }),
        }
    }

    // -- scrubbing ---------------------------------------------------------

    /// Moves the preview while dragging. No audio is touched yet.
    fn scrub_to_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if self.duration().is_zero() {
            return;
        }
        self.scrub = Some(fraction_at(self.seek_bounds.get(), x));
        cx.notify();
    }

    /// Commits the preview: one seek, on release, within the active span.
    fn commit_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(fraction) = self.scrub.take() else {
            return;
        };
        let (start, end) = self.active_span();
        let target = start + end.saturating_sub(start).mul_f32(fraction);
        if let Some(audio) = self.audio.as_mut() {
            audio.seek(target);
        }
        cx.notify();
    }

    fn set_volume_at_x(&mut self, x: Pixels, cx: &mut Context<Self>) {
        let fraction = fraction_at(self.volume_bounds.get(), x);
        if let Some(audio) = self.audio.as_mut() {
            audio.set_volume(fraction);
        }
        cx.notify();
    }

    /// Nudges the volume by a wheel scroll — up for louder. `set_volume` clamps,
    /// so it is safe to run past either end of the range.
    fn nudge_volume(&mut self, delta: &ScrollDelta, cx: &mut Context<Self>) {
        let step = scroll_volume_step(delta);
        if step == 0.0 {
            return;
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.set_volume(audio.volume() + step);
            cx.notify();
        }
    }

    // -- rendering ---------------------------------------------------------

    fn header(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let decorated = matches!(window.window_decorations(), Decorations::Client { .. });

        div()
            .flex()
            .flex_none()
            .h(px(44.))
            .w_full()
            .px_2()
            .items_center()
            .justify_end()
            // The empty space left of the buttons drags the window.
            .when(decorated, |this| {
                this.on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            })
            .child(
                div()
                    .flex()
                    .gap_1p5()
                    .child(icon_button(
                        "minimize",
                        "icons/minimize.svg",
                        px(26.),
                        px(11.),
                        |_, window, _| window.minimize_window(),
                    ))
                    .child(icon_button(
                        "maximize",
                        "icons/maximize.svg",
                        px(26.),
                        px(11.),
                        |_, window, _| window.zoom_window(),
                    ))
                    .child(icon_button(
                        "close",
                        "icons/close.svg",
                        px(26.),
                        px(11.),
                        cx.listener(|this, _, window, cx| this.close(window, cx)),
                    )),
            )
    }

    fn cover(&self) -> impl IntoElement {
        let art = self.queue.current_track().and_then(|t| t.art.clone());

        rounded(
            div()
                .flex()
                .flex_none()
                .size(px(196.))
                .items_center()
                .justify_center()
                .overflow_hidden()
                .bg(theme::control())
                .shadow(vec![gpui::BoxShadow {
                    color: theme::shadow(),
                    offset: point(px(0.), px(3.)),
                    blur_radius: px(6.),
                    spread_radius: px(0.),
                }]),
            CORNER,
        )
        .map(|this| match art {
            Some(art) => this.child(rounded(
                img(art).size_full().object_fit(ObjectFit::Cover),
                CORNER,
            )),
            None => this.child(
                svg()
                    .path("icons/note.svg")
                    .size(px(56.))
                    .text_color(theme::text_faint()),
            ),
        })
    }

    fn waveform(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let peaks = self.span_peaks();
        let progress = self.progress();
        let bounds_cell = self.seek_bounds.clone();

        let canvas = canvas(
            move |bounds, _window, _cx| bounds_cell.set(bounds),
            move |bounds, _, window, _cx| {
                let bar_width = px(2.);
                let gap = px(2.);
                let stride = bar_width + gap;
                let count = (bounds.size.width / stride).floor().max(1.) as usize;
                let played_until = bounds.origin.x + bounds.size.width * progress;
                let center = bounds.origin.y + bounds.size.height / 2.;

                let bars = waveform::resample(&peaks, count);

                for index in 0..count {
                    let x = bounds.origin.x + stride * index as f32;
                    // Before the decode finishes there is nothing to show, so
                    // draw a thin idle line rather than a fake waveform.
                    let peak = bars.get(index).copied().unwrap_or(0.06);
                    let height = (bounds.size.height * peak).max(px(2.));

                    let color = if x <= played_until {
                        theme::wave_played()
                    } else {
                        theme::wave_pending()
                    };

                    window.paint_quad(fill(
                        Bounds {
                            origin: point(x, center - height / 2.),
                            size: size(bar_width, height),
                        },
                        color,
                    ));
                }
            },
        )
        .h(px(46.))
        .w_full();

        div()
            .id("seek")
            .w_full()
            .h(px(46.))
            .cursor_pointer()
            .child(canvas)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, _, cx| {
                    this.scrub_to_x(event.position.x, cx)
                }),
            )
            .on_drag(SeekDrag, |_, _, _, cx| cx.new(|_| Empty))
            .on_drag_move(
                cx.listener(|this, event: &DragMoveEvent<SeekDrag>, _, cx| {
                    this.scrub_to_x(event.event.position.x, cx)
                }),
            )
    }

    fn times(&self) -> impl IntoElement {
        let (start, end) = self.active_span();
        let elapsed = self.elapsed();
        let remaining = end.saturating_sub(start).saturating_sub(elapsed);

        div()
            .flex()
            .w_full()
            .justify_between()
            .text_size(px(12.))
            .text_color(theme::text_dim())
            .child(format_time(elapsed))
            .child(format!("−{}", format_time(remaining)))
    }

    fn meta(&self) -> impl IntoElement {
        let track = self.queue.current_track();
        let chapter = self.current_chapter();

        // With chapters the song title leads and the file's own title steps back
        // to the album line; the counter shows the position within the album.
        let (title, artist, album) = match track {
            Some(track) => {
                let title = match chapter {
                    Some(i) => self.chapters()[i].title.clone(),
                    None => track.title.clone(),
                };
                (title, track.artist.clone(), track.album.clone())
            }
            None => ("Nothing playing".into(), "".into(), "".into()),
        };
        let counter = chapter.map(|i| format!("{} / {}", i + 1, self.chapters().len()));

        div()
            .flex()
            .flex_col()
            .items_center()
            .gap_1()
            .w_full()
            .child(
                div()
                    .text_size(px(19.))
                    .font_weight(gpui::FontWeight::BOLD)
                    .text_color(theme::text())
                    .child(title),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .text_size(px(13.))
                    .text_color(theme::text_dim())
                    .child(artist)
                    .child(album),
            )
            .when_some(counter, |this, counter| {
                this.child(
                    div()
                        .mt_0p5()
                        .text_size(px(11.))
                        .text_color(theme::text_faint())
                        .child(counter),
                )
            })
    }

    fn controls(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let playing = self.is_playing();

        div()
            .flex()
            .items_center()
            .justify_center()
            .gap_4()
            .child(icon_button(
                "prev",
                "icons/previous.svg",
                px(40.),
                px(16.),
                cx.listener(|this, _, _, cx| this.skip_previous(cx)),
            ))
            .child(icon_button(
                "play",
                if playing {
                    "icons/pause.svg"
                } else {
                    "icons/play.svg"
                },
                px(52.),
                px(20.),
                cx.listener(|this, _, _, cx| this.toggle_play(cx)),
            ))
            .child(icon_button(
                "next",
                "icons/next.svg",
                px(40.),
                px(16.),
                cx.listener(|this, _, _, cx| this.skip_next(cx)),
            ))
    }

    fn volume(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let volume = self.audio.as_ref().map(|a| a.volume()).unwrap_or(0.0);
        let bounds_cell = self.volume_bounds.clone();

        let measure = canvas(
            move |bounds, _window, _cx| bounds_cell.set(bounds),
            |_, _, _, _| {},
        )
        .absolute()
        .size_full();

        div()
            .flex()
            .items_center()
            .gap_3()
            .w_full()
            .px(RAIL_INSET)
            .child(
                svg()
                    .path("icons/volume_low.svg")
                    .size(px(20.))
                    .flex_none()
                    .text_color(theme::text_dim()),
            )
            .child(
                div()
                    .id("volume")
                    .relative()
                    .flex()
                    .flex_1()
                    .items_center()
                    .h(px(22.))
                    .cursor_pointer()
                    .child(measure)
                    .child(bar(
                        volume,
                        px(8.),
                        theme::slider_fill(),
                        theme::slider_track(),
                    ))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, cx| {
                            this.set_volume_at_x(event.position.x, cx)
                        }),
                    )
                    .on_drag(VolumeDrag, |_, _, _, cx| cx.new(|_| Empty))
                    .on_drag_move(cx.listener(
                        |this, event: &DragMoveEvent<VolumeDrag>, _, cx| {
                            this.set_volume_at_x(event.event.position.x, cx)
                        },
                    )),
            )
            .child(
                svg()
                    .path("icons/volume_high.svg")
                    .size(px(20.))
                    .flex_none()
                    .text_color(theme::text_dim()),
            )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let repeat_icon = match self.queue.repeat {
            Repeat::One => "icons/repeat_one.svg",
            _ => "icons/repeat.svg",
        };
        let loaded = self.queue.current.is_some();
        let loved = self.queue.current_track().is_some_and(|track| track.loved);

        div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .px(RAIL_INSET)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .gap_2()
                    .child(toggle_button(
                        "playlist",
                        "icons/playlist.svg",
                        px(34.),
                        px(15.),
                        self.show_playlist,
                        cx.listener(|this, _, _, cx| {
                            this.show_playlist = !this.show_playlist;
                            cx.notify();
                        }),
                    ))
                    .child(toggle_button(
                        "shuffle",
                        "icons/shuffle.svg",
                        px(34.),
                        px(15.),
                        self.queue.shuffle,
                        cx.listener(|this, _, _, cx| {
                            let shuffle = !this.queue.shuffle;
                            this.queue.set_shuffle(shuffle);
                            cx.notify();
                        }),
                    )),
            )
            // Between the two groups rather than in either of them: those four
            // are about the queue, this one is about the track. That lands it
            // in the middle, under the play button, the way `controls()` puts
            // the control that acts on what is playing in the centre.
            .child(
                toggle_button(
                    "loved",
                    if loved {
                        "icons/heart_filled.svg"
                    } else {
                        "icons/heart.svg"
                    },
                    px(34.),
                    px(15.),
                    loved,
                    cx.listener(|this, _, _, cx| this.toggle_loved(cx)),
                )
                // Nothing is loaded, so there is nothing to love. Dimmed rather
                // than taken away: hark opens on a full queue with nothing
                // playing, and a control that is missing exactly then hides the
                // feature from anyone seeing the app for the first time.
                .when(!loaded, |this| this.opacity(0.4).cursor_default()),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .justify_end()
                    .gap_2()
                    .child(toggle_button(
                        "repeat",
                        repeat_icon,
                        px(34.),
                        px(15.),
                        self.queue.repeat != Repeat::Off,
                        cx.listener(|this, _, _, cx| {
                            this.queue.repeat = this.queue.repeat.cycle();
                            cx.notify();
                        }),
                    ))
                    // Next to the `+`, and not beside the playlist toggle: those
                    // two are about the queue, these two are about the library
                    // the queue is built from.
                    .child(toggle_button(
                        "folders",
                        "icons/folder.svg",
                        px(34.),
                        px(15.),
                        self.show_folders,
                        cx.listener(|this, _, _, cx| {
                            this.show_folders = !this.show_folders;
                            cx.notify();
                        }),
                    ))
                    .child(icon_button(
                        "add",
                        "icons/add.svg",
                        px(34.),
                        px(15.),
                        cx.listener(|this, _, _, cx| this.open_files(cx)),
                    )),
            )
    }

    /// The queue, as a panel covering the player. Laid over it rather than
    /// squeezed in below the controls, which left it a row and a half tall in a
    /// small window and sprawling in fullscreen. The header switches it between
    /// the whole queue and the loved tracks; since the music folder is scanned
    /// into the queue at startup, that second list is the loved library.
    fn playlist(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.queue.current;
        let loved_only = self.show_loved;

        let rows = self
            .queue
            .tracks
            .iter()
            // Enumerated before the filter, never after: this index is the one
            // `play_index` takes, and a row's place in a filtered list is not
            // it.
            .enumerate()
            .filter(|(_, track)| !loved_only || track.loved)
            // And enumerated again for the number in the margin, which counts
            // what is on screen. In the full queue the two agree; in the loved
            // list, a track's place among a thousand others is not what anyone
            // is looking at, and would not fit an 18-pixel column either.
            .enumerate()
            .map(|(row, (index, track))| {
                let is_current = current == Some(index);
                rounded(
                    div()
                        .id(("track", index))
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .h(px(46.))
                        .px_2()
                        .cursor_pointer()
                        .when(is_current, |this| this.bg(theme::control_active()))
                        .hover(|this| this.bg(theme::control_hover()))
                        .child(
                            div()
                                .flex()
                                .flex_none()
                                .w(px(18.))
                                .justify_center()
                                .text_size(px(11.))
                                .text_color(theme::text_faint())
                                .child(if is_current {
                                    "▶".to_string()
                                } else {
                                    format!("{}", row + 1)
                                }),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(theme::text())
                                        .truncate()
                                        .child(track.title.clone()),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::text_dim())
                                        .truncate()
                                        .child(track.artist.clone()),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(theme::text_faint())
                                .child(format_time(track.duration)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.play_index(index, cx))),
                    px(8.),
                )
            })
            .collect::<Vec<_>>();

        let count = rows.len();

        overlay(CORNER)
        .child(
            div()
                .flex()
                .flex_none()
                .items_center()
                .justify_between()
                .w_full()
                .px_3()
                .h(px(44.))
                .child(
                    div()
                        .text_size(px(13.))
                        .text_color(theme::text_dim())
                        .child(if loved_only {
                            format!("Loved · {count}")
                        } else {
                            format!("Queue · {count}")
                        }),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(toggle_button(
                            "loved-filter",
                            if loved_only {
                                "icons/heart_filled.svg"
                            } else {
                                "icons/heart.svg"
                            },
                            px(26.),
                            px(11.),
                            loved_only,
                            cx.listener(|this, _, _, cx| {
                                this.show_loved = !this.show_loved;
                                cx.notify();
                            }),
                        ))
                        .child(icon_button(
                            "close-playlist",
                            "icons/close.svg",
                            px(26.),
                            px(11.),
                            cx.listener(|this, _, _, cx| {
                                this.show_playlist = false;
                                cx.notify();
                            }),
                        )),
                ),
        )
        .child(
            div()
                // One scroll offset per list. GPUI keeps it against the element
                // id, and row five hundred of the queue is not where the loved
                // list should open.
                .id(("playlist-scroll", loved_only as usize))
                .flex()
                .flex_col()
                .gap_0p5()
                .w_full()
                .flex_1()
                .min_h(px(0.))
                .px_2()
                .pb_2()
                .overflow_y_scroll()
                // Only reachable with the filter on: an empty queue never gets
                // this far, since `body` shows the drop zone instead.
                .when(rows.is_empty(), |this| {
                    this.items_center()
                        .justify_center()
                        .gap_1()
                        .child(
                            div()
                                .text_size(px(13.))
                                .text_color(theme::text_dim())
                                .child("Nothing loved yet"),
                        )
                        .child(
                            div()
                                .text_size(px(11.))
                                .text_color(theme::text_faint())
                                .child("Press the heart while a track is playing."),
                        )
                })
                .children(rows),
        )
    }

    /// The library roots, as a panel over the player. Everything hark scans is
    /// on this list — including the desktop's music folder, which is scanned
    /// whether it was asked for or not. It is shown without a way to remove it
    /// rather than left out: a panel reporting no folders over a queue of a
    /// thousand tracks reads as a lost library, and it is also what explains why
    /// adding a folder inside it appears to do nothing.
    fn folders_panel(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let entries: Vec<(PathBuf, bool, bool)> = self
            .music_dir
            .iter()
            .map(|dir| (dir.clone(), true, false))
            .chain(
                self.folders
                    .iter()
                    .map(|folder| (folder.path.clone(), folder.available, true)),
            )
            .collect();
        let count = entries.len();

        let rows = entries
            .into_iter()
            .enumerate()
            .map(|(index, (path, available, removable))| {
                let tracks = self
                    .queue
                    .tracks
                    .iter()
                    .filter(|track| track.path.starts_with(&path))
                    .count();
                let name = match path.file_name() {
                    Some(name) => name.to_string_lossy().to_string(),
                    None => path.display().to_string(),
                };
                let remove = path.clone();

                rounded(
                    div()
                        .flex()
                        .flex_none()
                        .items_center()
                        .gap_2()
                        .w_full()
                        .h(px(46.))
                        .px_2()
                        // Unreachable right now — a mount that is not up, a
                        // folder that will not open. Dimmed and left alone
                        // rather than labelled: a scan cannot tell that apart
                        // from a folder with no music in it, and its tracks are
                        // still in the queue either way.
                        .when(!available, |this| this.opacity(0.5))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(theme::text())
                                        .truncate()
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::text_dim())
                                        .truncate()
                                        .child(path.display().to_string()),
                                ),
                        )
                        .child(
                            div()
                                .flex_none()
                                .text_size(px(11.))
                                .text_color(theme::text_faint())
                                .child(format!("{tracks}")),
                        )
                        // The music folder has no cross: it comes back from the
                        // desktop's configuration on the next start, so removing
                        // it here would only be a promise hark cannot keep.
                        .when(removable, |this| {
                            this.child(icon_button(
                                ("remove-folder", index),
                                "icons/close.svg",
                                px(26.),
                                px(11.),
                                cx.listener(move |this, _, _, cx| this.forget(&remove, cx)),
                            ))
                        }),
                    px(8.),
                )
            })
            .collect::<Vec<_>>();

        overlay(CORNER)
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_between()
                    .w_full()
                    .px_3()
                    .h(px(44.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .text_color(theme::text_dim())
                            .child(format!("Folders · {count}")),
                    )
                    .child(icon_button(
                        "close-folders",
                        "icons/close.svg",
                        px(26.),
                        px(11.),
                        cx.listener(|this, _, _, cx| {
                            this.show_folders = false;
                            cx.notify();
                        }),
                    )),
            )
            .child(
                div()
                    .id("folders-scroll")
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .w_full()
                    .flex_1()
                    .min_h(px(0.))
                    .px_2()
                    .overflow_y_scroll()
                    .when(rows.is_empty(), |this| {
                        this.items_center()
                            .justify_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(13.))
                                    .text_color(theme::text_dim())
                                    .child("No folders yet"),
                            )
                            .child(
                                div()
                                    .text_size(px(11.))
                                    .text_color(theme::text_faint())
                                    .child("Add one below, or drop it on the window."),
                            )
                    })
                    .children(rows),
            )
            .child(
                div().flex().flex_none().w_full().p_2().child(
                    rounded(
                        div()
                            .id("add-folder")
                            .flex()
                            .w_full()
                            .justify_center()
                            .py_1p5()
                            .bg(theme::control())
                            .cursor_pointer()
                            .hover(|this| this.bg(theme::control_hover()))
                            .text_size(px(13.))
                            .text_color(theme::text())
                            .child("Add folder")
                            .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))),
                        px(9.),
                    ),
                ),
            )
    }

    fn body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.queue.is_empty() {
            return div()
                // The folder panel is positioned against this box too. The
                // footer is hidden while the queue is empty, so without this the
                // panel would vanish the moment removing the last folder emptied
                // the queue, with no way back to it.
                .relative()
                .flex()
                .flex_col()
                .flex_1()
                .min_h(px(0.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_h(px(0.))
                        .items_center()
                        .justify_center()
                        .gap_4()
                        .child(
                            svg()
                                .path("icons/note.svg")
                                .size(px(48.))
                                .text_color(theme::text_faint()),
                        )
                        .child(
                            div()
                                .text_size(px(14.))
                                .text_color(theme::text_dim())
                                .child("Drop music here"),
                        )
                        .child(
                            rounded(
                                div()
                                    .id("open")
                                    .px_4()
                                    .py_1p5()
                                    .bg(theme::control())
                                    .cursor_pointer()
                                    .hover(|this| this.bg(theme::control_hover()))
                                    .text_size(px(13.))
                                    .text_color(theme::text())
                                    .child("Choose files")
                                    .on_click(cx.listener(|this, _, _, cx| this.open_files(cx))),
                                px(9.),
                            ),
                        ),
                )
                .when(self.show_folders, |this| this.child(self.folders_panel(cx)))
                .into_any_element();
        }

        div()
            // The panels are positioned against this box.
            .relative()
            .flex()
            .flex_col()
            .flex_1()
            .min_h(px(0.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h(px(0.))
                    .items_center()
                    .justify_center()
                    .gap_4()
                    // Scrolling anywhere over the player changes the volume. An
                    // open panel covers this column, and scroll — unlike the
                    // other mouse events — falls through whatever is on top of
                    // it, so the handler has to step aside on its own.
                    .when(!self.show_playlist && !self.show_folders, |this| {
                        this.on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                            this.nudge_volume(&event.delta, cx)
                        }))
                    })
                    .child(self.cover())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .px(RAIL_INSET)
                            .gap_1()
                            .child(self.waveform(cx))
                            .child(self.times()),
                    )
                    .child(self.meta())
                    .child(self.controls(cx))
                    .child(self.volume(cx)),
            )
            .when(self.show_playlist, |this| this.child(self.playlist(cx)))
            // After the playlist, so it covers it: the two are independent
            // toggles, both reachable from the footer, and the one opened last
            // is the one being read.
            .when(self.show_folders, |this| this.child(self.folders_panel(cx)))
            .into_any_element()
    }
}

impl Focusable for PlayerView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PlayerView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let decorations = window.window_decorations();
        window.set_client_inset(SHADOW_SIZE);

        div()
            .id("window-backdrop")
            .size_full()
            .bg(gpui::transparent_black())
            .key_context(KEY_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &TogglePlay, _, cx| this.toggle_play(cx)))
            // A drag can end anywhere in the window, so the seek is committed
            // here rather than on the seek bar itself.
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.commit_scrub(cx)),
            )
            .map(|this| match decorations {
                Decorations::Server => this,
                Decorations::Client { tiling } => this
                    // Full-window hitbox so the cursor turns into a resize arrow
                    // over the shadow margin.
                    .child(
                        canvas(
                            |_bounds, window, _cx| {
                                window.insert_hitbox(
                                    Bounds::new(
                                        point(px(0.), px(0.)),
                                        window.window_bounds().get_bounds().size,
                                    ),
                                    HitboxBehavior::Normal,
                                )
                            },
                            move |_bounds, hitbox, window, _cx| {
                                let size = window.window_bounds().get_bounds().size;
                                let Some(edge) = resize_edge(window.mouse_position(), size) else {
                                    return;
                                };
                                window.set_cursor_style(
                                    match edge {
                                        ResizeEdge::Top | ResizeEdge::Bottom => {
                                            CursorStyle::ResizeUpDown
                                        }
                                        ResizeEdge::Left | ResizeEdge::Right => {
                                            CursorStyle::ResizeLeftRight
                                        }
                                        ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                            CursorStyle::ResizeUpLeftDownRight
                                        }
                                        ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                            CursorStyle::ResizeUpRightDownLeft
                                        }
                                    },
                                    &hitbox,
                                );
                            },
                        )
                        .size_full()
                        .absolute(),
                    )
                    .when(!(tiling.top || tiling.left), |this| this.rounded_tl(CORNER))
                    .when(!(tiling.top || tiling.right), |this| {
                        this.rounded_tr(CORNER)
                    })
                    .when(!tiling.top, |this| this.pt(SHADOW_SIZE))
                    .when(!tiling.bottom, |this| this.pb(SHADOW_SIZE))
                    .when(!tiling.left, |this| this.pl(SHADOW_SIZE))
                    .when(!tiling.right, |this| this.pr(SHADOW_SIZE))
                    .on_mouse_move(|_, window, _| window.refresh())
                    .on_mouse_down(MouseButton::Left, move |event, window, _| {
                        let size = window.window_bounds().get_bounds().size;
                        match resize_edge(event.position, size) {
                            Some(edge) => window.start_window_resize(edge),
                            None => window.start_window_move(),
                        }
                    }),
            })
            .child(
                rounded(
                    div()
                        .id("window-body")
                        .size_full()
                        .flex()
                        .flex_col()
                        .bg(gpui::linear_gradient(
                            135.,
                            gpui::linear_color_stop(theme::glass_dark(), 0.),
                            gpui::linear_color_stop(theme::glass_light(), 1.),
                        ))
                        .text_color(theme::text())
                        .cursor(CursorStyle::Arrow)
                        .when(matches!(decorations, Decorations::Client { .. }), |this| {
                            this.border_1().border_color(theme::border())
                        })
                        // Events inside the body must not fall through to the
                        // backdrop: its mouse-down starts a window move, which
                        // swallows the press and no click ever completes.
                        // The header re-enables dragging for itself.
                        .on_mouse_move(|_, _, cx| cx.stop_propagation())
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                            this.add_chosen(paths.paths().to_vec(), cx);
                        }))
                        .child(self.header(window, cx))
                        .child(
                            // Keeps the player a narrow column when the window
                            // is maximised instead of stretching it across the
                            // screen.
                            div()
                                .flex()
                                .flex_col()
                                .flex_1()
                                .min_h(px(0.))
                                .items_center()
                                .child(
                                    div()
                                        .flex()
                                        .flex_col()
                                        .flex_1()
                                        .min_h(px(0.))
                                        .w_full()
                                        .max_w(px(440.))
                                        .px_5()
                                        .pb_5()
                                        .gap_4()
                                        .child(self.body(cx))
                                        .when(!self.queue.is_empty(), |this| {
                                            this.child(self.footer(cx))
                                        })
                                        .when_some(self.error.clone(), |this, error| {
                                            this.child(
                                                div()
                                                    .text_size(px(11.))
                                                    .text_color(gpui::rgb(0xb3261e))
                                                    .child(error),
                                            )
                                        }),
                                ),
                        ),
                    CORNER,
                )
                .overflow_hidden(),
            )
    }
}

/// Volume change for one scroll event. Positive scrolls up (louder). Measured in
/// lines so a mouse wheel and a trackpad feel alike: a wheel notch is three lines
/// — about a 6% step — and a trackpad's pixel deltas fold back at roughly 20px a
/// line.
fn scroll_volume_step(delta: &ScrollDelta) -> f32 {
    const PER_LINE: f32 = 0.02;
    let lines = match delta {
        ScrollDelta::Lines(p) => p.y,
        ScrollDelta::Pixels(p) => f32::from(p.y) / 20.0,
    };
    lines * PER_LINE
}

/// Which window edge the pointer is over, if any. Copied from GPUI's
/// `window_shadow` example — the shadow margin doubles as the resize handle.
fn resize_edge(pos: Point<Pixels>, size: Size<Pixels>) -> Option<ResizeEdge> {
    let edge = if pos.y < SHADOW_SIZE && pos.x < SHADOW_SIZE {
        ResizeEdge::TopLeft
    } else if pos.y < SHADOW_SIZE && pos.x > size.width - SHADOW_SIZE {
        ResizeEdge::TopRight
    } else if pos.y < SHADOW_SIZE {
        ResizeEdge::Top
    } else if pos.y > size.height - SHADOW_SIZE && pos.x < SHADOW_SIZE {
        ResizeEdge::BottomLeft
    } else if pos.y > size.height - SHADOW_SIZE && pos.x > size.width - SHADOW_SIZE {
        ResizeEdge::BottomRight
    } else if pos.y > size.height - SHADOW_SIZE {
        ResizeEdge::Bottom
    } else if pos.x < SHADOW_SIZE {
        ResizeEdge::Left
    } else if pos.x > size.width - SHADOW_SIZE {
        ResizeEdge::Right
    } else {
        return None;
    };
    Some(edge)
}


#[cfg(test)]
mod tests {
    use super::*;

    fn paths(paths: &[&str]) -> Vec<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_folder_under_a_root_is_not_remembered_twice() {
        let roots = paths(&["/music"]);

        for dir in ["/music", "/music/rock/live"] {
            assert!(matches!(
                adopt(Path::new(dir), &roots, &roots),
                Adopt::Covered
            ));
        }
    }

    #[test]
    fn a_folder_that_contains_a_remembered_one_replaces_it() {
        let roots = paths(&["/music/rock", "/music/folk", "/elsewhere"]);

        let Adopt::Adopted { superseded } = adopt(Path::new("/music"), &roots, &roots) else {
            panic!("a folder covering two roots was taken for one of them");
        };
        assert_eq!(superseded, paths(&["/music/rock", "/music/folk"]));
    }

    /// The music folder comes back from the desktop's configuration on every
    /// start, so a root that swallowed it would leave hark scanning it twice
    /// with only one of the two removable.
    #[test]
    fn the_music_folder_is_never_superseded() {
        let roots = paths(&["/home/x/Music", "/home/x/more"]);
        let removable = paths(&["/home/x/more"]);

        let Adopt::Adopted { superseded } = adopt(Path::new("/home/x"), &roots, &removable) else {
            panic!("a folder above the music folder was refused");
        };
        assert_eq!(superseded, paths(&["/home/x/more"]));
    }
}
