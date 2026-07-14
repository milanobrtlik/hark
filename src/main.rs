mod audio;
mod decode;
mod library;
mod opus_source;
mod player_view;
mod queue;
mod theme;
mod track;
mod ui;
mod waveform;

use anyhow::Context as _;
use gpui::{
    App, Application, AssetSource, Bounds, Result, SharedString, WindowBackgroundAppearance,
    WindowBounds, WindowDecorations, WindowOptions, prelude::*, px, size,
};
use player_view::PlayerView;
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

    Application::new().with_assets(Assets).run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(420.), px(690.)), cx);

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
            |window, cx| cx.new(|cx| PlayerView::new(paths, window, cx)),
        )
        .expect("failed to open the window");

        cx.on_window_closed(|cx| cx.quit()).detach();
        cx.activate(true);
    });
}
