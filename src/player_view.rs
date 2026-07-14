use crate::audio::Audio;
use crate::library;
use crate::queue::{Queue, Repeat};
use crate::theme;
use crate::track::{Track, format_time};
use crate::ui::{bar, fraction_at, icon_button, rounded, toggle_button};
use crate::waveform;

use notify::{RecursiveMode, Watcher};

use gpui::{
    Bounds, Context, CursorStyle, Decorations, DragMoveEvent, Empty, ExternalPaths, HitboxBehavior,
    IntoElement, MouseButton, MouseDownEvent, ObjectFit, PathPromptOptions, Pixels, Point, Render,
    ResizeEdge, SharedString, Size, Task, Window, canvas, div, fill, img, point, prelude::*, px,
    size, svg,
};
use std::cell::Cell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

const SHADOW_SIZE: Pixels = px(10.);
const CORNER: Pixels = px(14.);

/// Drag markers. GPUI routes drag-move events by the payload type, so the seek
/// bar and the volume slider need distinct ones.
#[derive(Clone)]
struct SeekDrag;
#[derive(Clone)]
struct VolumeDrag;

pub struct PlayerView {
    audio: Option<Audio>,
    error: Option<SharedString>,
    queue: Queue,
    /// Normalized peaks of the current track; empty while still decoding.
    peaks: Vec<f32>,
    /// Position being previewed while the seek bar is dragged, 0..=1. The seek
    /// itself only happens on release — seeking on every mouse move makes the
    /// decoder stutter.
    scrub: Option<f32>,
    show_playlist: bool,
    seek_bounds: Rc<Cell<Bounds<Pixels>>>,
    volume_bounds: Rc<Cell<Bounds<Pixels>>>,
    _waveform_task: Option<Task<()>>,
    _tick: Task<()>,
    /// Watches the music folder. Dropping it stops the notifications.
    _watcher: Option<notify::RecommendedWatcher>,
}

impl PlayerView {
    pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        cx.observe_window_appearance(window, |_, window, _| window.refresh())
            .detach();

        let (audio, error) = match Audio::new() {
            Ok(audio) => (Some(audio), None),
            Err(err) => (None, Some(SharedString::from(err.to_string()))),
        };

        let mut view = PlayerView {
            audio,
            error,
            queue: Queue::default(),
            peaks: Vec::new(),
            scrub: None,
            show_playlist: false,
            seek_bounds: Rc::new(Cell::new(Bounds::default())),
            volume_bounds: Rc::new(Cell::new(Bounds::default())),
            _waveform_task: None,
            _tick: Self::spawn_tick(cx),
            _watcher: None,
        };

        // Files named on the command line play straight away; the music folder
        // is only loaded into the queue.
        if !paths.is_empty() {
            view.add_paths(paths, true, cx);
        }

        if let Some(dir) = library::music_dir() {
            view.add_paths(vec![dir.clone()], false, cx);
            view.watch(dir, cx);
        }

        view
    }

    /// Keeps the queue in step with the music folder while the app runs.
    fn watch(&mut self, dir: PathBuf, cx: &mut Context<Self>) {
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        let mut watcher = match notify::recommended_watcher(move |event| {
            if let Ok(notify::Event { kind, .. }) = event
                && (kind.is_create() || kind.is_remove() || kind.is_modify())
            {
                let _ = tx.send(());
            }
        }) {
            Ok(watcher) => watcher,
            Err(err) => {
                eprintln!("hark: cannot watch the folder: {err}");
                return;
            }
        };

        if let Err(err) = watcher.watch(&dir, RecursiveMode::Recursive) {
            eprintln!("hark: cannot watch the folder: {err}");
            return;
        }
        self._watcher = Some(watcher);

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

                let dir = dir.clone();
                let files = cx
                    .background_executor()
                    .spawn(async move { library::scan(&dir) })
                    .await;

                if this
                    .update(cx, |this, cx| this.library_changed(files, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// Merges a fresh scan of the music folder into the queue.
    fn library_changed(&mut self, files: Vec<PathBuf>, cx: &mut Context<Self>) {
        let existing: HashSet<PathBuf> = files.iter().cloned().collect();
        self.queue.retain_existing(&existing);

        let added: Vec<PathBuf> = files
            .into_iter()
            .filter(|path| !self.queue.contains(path))
            .collect();

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

    /// Position shown in the UI: the drag preview while scrubbing, otherwise the
    /// player's real clock.
    fn position(&self) -> Duration {
        if let Some(scrub) = self.scrub {
            return self.duration().mul_f32(scrub);
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

    fn progress(&self) -> f32 {
        let duration = self.duration().as_secs_f32();
        if duration <= 0.0 {
            return 0.0;
        }
        (self.position().as_secs_f32() / duration).clamp(0.0, 1.0)
    }

    // -- queue -------------------------------------------------------------

    fn open_files(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Add".into()),
        });

        cx.spawn(async move |this, cx| {
            match paths.await {
                Ok(Ok(Some(paths))) => {
                    this.update(cx, |this, cx| this.add_paths(paths, true, cx)).ok();
                }
                Ok(Ok(None)) => {}
                Ok(Err(err)) => eprintln!("hark: the file dialog failed: {err}"),
                Err(err) => eprintln!("hark: the file dialog was dismissed: {err}"),
            }
        })
        .detach();
    }

    /// Loads `paths` (files or folders) into the queue off the main thread.
    /// With `autoplay`, playback starts if nothing is playing yet.
    fn add_paths(&mut self, paths: Vec<PathBuf>, autoplay: bool, cx: &mut Context<Self>) {
        let known: HashSet<PathBuf> = self.queue.tracks.iter().map(|t| t.path.clone()).collect();

        cx.spawn(async move |this, cx| {
            let tracks = cx
                .background_executor()
                .spawn(async move {
                    let mut files = Vec::new();
                    for path in &paths {
                        library::collect(path, &mut files);
                    }
                    files.sort();
                    files.dedup();
                    files
                        .into_iter()
                        .filter(|path| !known.contains(path))
                        .map(Track::load)
                        .collect::<Vec<_>>()
                })
                .await;

            this.update(cx, |this, cx| {
                if tracks.is_empty() {
                    return;
                }
                eprintln!("hark: tracks queued: {}", tracks.len());
                let idle = this.queue.current.is_none();
                this.queue.extend(tracks);
                if autoplay && idle {
                    this.play_index(0, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn play_index(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(track) = self.queue.tracks.get(index).cloned() else {
            return;
        };
        let Some(audio) = self.audio.as_mut() else {
            return;
        };

        if let Err(err) = audio.play_file(&track.path) {
            self.error = Some(err.to_string().into());
            return;
        }

        self.error = None;
        self.queue.current = Some(index);
        self.peaks.clear();
        self.load_waveform(track.path, cx);
        cx.notify();
    }

    fn load_waveform(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        // Replacing the task cancels a decode still running for the old track.
        self._waveform_task = Some(cx.spawn(async move |this, cx| {
            let peaks = cx
                .background_executor()
                .spawn(async move { waveform::compute(&path).unwrap_or_default() })
                .await;

            this.update(cx, |this, cx| {
                this.peaks = peaks;
                cx.notify();
            })
            .ok();
        }));
    }

    fn advance(&mut self, manual: bool, cx: &mut Context<Self>) {
        match self.queue.next(manual) {
            Some(index) => self.play_index(index, cx),
            None => {
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

    fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.queue.current.is_none() {
            if !self.queue.is_empty() {
                self.play_index(0, cx);
            }
            return;
        }
        if let Some(audio) = self.audio.as_mut() {
            audio.toggle_play();
        }
        cx.notify();
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

    /// Commits the preview: one seek, on release.
    fn commit_scrub(&mut self, cx: &mut Context<Self>) {
        let Some(fraction) = self.scrub.take() else {
            return;
        };
        let duration = self.duration();
        if let Some(audio) = self.audio.as_mut() {
            audio.seek(duration.mul_f32(fraction));
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

    // -- rendering ---------------------------------------------------------

    fn header(&self, window: &Window) -> impl IntoElement {
        let decorated = matches!(window.window_decorations(), Decorations::Client { .. });

        div()
            .flex()
            .flex_none()
            .h(px(44.))
            .w_full()
            .px_2()
            .items_center()
            .justify_between()
            // The empty space between the buttons drags the window.
            .when(decorated, |this| {
                this.on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
            })
            .child(icon_button(
                "close",
                "icons/close.svg",
                px(26.),
                px(11.),
                |_, window, _| window.remove_window(),
            ))
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
                    offset: point(px(0.), px(6.)),
                    blur_radius: px(16.),
                    spread_radius: px(0.),
                }]),
            CORNER,
        )
        .map(|this| match art {
            Some(art) => this.child(img(art).size_full().object_fit(ObjectFit::Cover)),
            None => this.child(
                svg()
                    .path("icons/note.svg")
                    .size(px(56.))
                    .text_color(theme::text_faint()),
            ),
        })
    }

    fn waveform(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let peaks = self.peaks.clone();
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
        let position = self.position();
        let remaining = self.duration().saturating_sub(position);

        div()
            .flex()
            .w_full()
            .justify_between()
            .text_size(px(12.))
            .text_color(theme::text_dim())
            .child(format_time(position))
            .child(format!("−{}", format_time(remaining)))
    }

    fn meta(&self) -> impl IntoElement {
        let track = self.queue.current_track();
        let (title, artist, album) = match track {
            Some(track) => (
                track.title.clone(),
                track.artist.clone(),
                track.album.clone(),
            ),
            None => ("Nothing playing".into(), "".into(), "".into()),
        };

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
                cx.listener(|this, _, _, cx| this.previous(cx)),
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
                cx.listener(|this, _, _, cx| this.advance(true, cx)),
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
            .child(
                svg()
                    .path("icons/volume_low.svg")
                    .size(px(16.))
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
                    .h(px(18.))
                    .cursor_pointer()
                    .child(measure)
                    .child(bar(
                        volume,
                        px(6.),
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
                    .size(px(16.))
                    .flex_none()
                    .text_color(theme::text_dim()),
            )
    }

    fn footer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let repeat_icon = match self.queue.repeat {
            Repeat::One => "icons/repeat_one.svg",
            _ => "icons/repeat.svg",
        };

        div()
            .flex()
            .items_center()
            .justify_between()
            .w_full()
            .child(
                div()
                    .flex()
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
            .child(
                div()
                    .flex()
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
    /// small window and sprawling in fullscreen.
    fn playlist(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let current = self.queue.current;

        let rows = self
            .queue
            .tracks
            .iter()
            .enumerate()
            .map(|(index, track)| {
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
                                    format!("{}", index + 1)
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

        let count = self.queue.tracks.len();

        rounded(
            div()
                .absolute()
                .top(px(0.))
                .left(px(0.))
                .size_full()
                .flex()
                .flex_col()
                .bg(theme::panel())
                .border_1()
                .border_color(theme::border()),
            CORNER,
        )
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
                        .child(format!("Queue · {count}")),
                )
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
        )
        .child(
            div()
                .id("playlist-scroll")
                .flex()
                .flex_col()
                .gap_0p5()
                .w_full()
                .flex_1()
                .min_h(px(0.))
                .px_2()
                .pb_2()
                .overflow_y_scroll()
                .children(rows),
        )
    }

    fn body(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.queue.is_empty() {
            return div()
                .flex()
                .flex_col()
                .flex_1()
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
                )
                .into_any_element();
        }

        div()
            // The playlist is positioned against this box.
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
                    .child(self.cover())
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .w_full()
                            .gap_1()
                            .child(self.waveform(cx))
                            .child(self.times()),
                    )
                    .child(self.meta())
                    .child(self.controls(cx))
                    .child(self.volume(cx)),
            )
            .when(self.show_playlist, |this| this.child(self.playlist(cx)))
            .into_any_element()
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
                        .bg(theme::GLASS)
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
                            this.add_paths(paths.paths().to_vec(), true, cx);
                        }))
                        .child(self.header(window))
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

