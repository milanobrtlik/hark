mod artwork;
mod audio;
mod decode;
mod fingerprint;
mod id_cache;
mod library;
mod meta_cache;
mod mpris;
mod opus_source;
mod peak_cache;
mod player_view;
mod queue;
mod state;
mod theme;
mod track;
mod ui;
mod waveform;

use anyhow::Context as _;
use gpui::{
    App, Application, AssetSource, Bounds, KeyBinding, Result, SharedString,
    WindowBackgroundAppearance, WindowBounds, WindowDecorations, WindowOptions, prelude::*, px,
    size,
};
use player_view::{PlayerView, TogglePlay};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "assets"]
#[include = "icons/**/*"]
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Self::get(path)
            .map(|file| Some(file.data))
            .with_context(|| format!("missing asset {path:?}"))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter(|p| p.starts_with(path))
            .map(Into::into)
            .collect())
    }
}

fn main() {
    let paths: Vec<std::path::PathBuf> = std::env::args_os().skip(1).map(Into::into).collect();
    // Read once, here: the window is sized before the view that owns the rest of
    // the session exists, so both come out of one snapshot.
    let state = state::load();

    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        cx.bind_keys([KeyBinding::new(
            "space",
            TogglePlay,
            Some(player_view::KEY_CONTEXT),
        )]);

        // Wayland never tells a client where its window is — the origin comes
        // back as zero however the compositor placed it — so a zero origin means
        // "put it wherever you would have" rather than "the top left corner".
        let window = state.window;
        let extent = window.map_or(state::DEFAULT_SIZE, |window| window.size);
        let origin = window
            .map(|window| window.origin)
            .filter(|origin| origin.x > px(0.) || origin.y > px(0.));
        let bounds = match origin {
            Some(origin) => Bounds::new(origin, extent),
            None => Bounds::centered(None, extent, cx),
        };

        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                window_decorations: Some(WindowDecorations::Client),
                window_background: WindowBackgroundAppearance::Blurred,
                window_min_size: Some(size(px(360.), px(560.))),
                app_id: Some("dev.milan.hark".into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| PlayerView::new(paths, state, window, cx)),
        )
        .expect("failed to open the window");

        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
