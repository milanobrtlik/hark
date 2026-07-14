use crate::theme;
use gpui::{
    App, Bounds, ClickEvent, Div, ElementId, Pixels, Rgba, Stateful, Styled, Window, div,
    prelude::*, px, svg,
};

/// Rounds all four corners to the same radius. GPUI only generates per-corner
/// setters for arbitrary radii.
pub fn rounded<T: Styled>(element: T, radius: Pixels) -> T {
    element
        .rounded_tl(radius)
        .rounded_tr(radius)
        .rounded_bl(radius)
        .rounded_br(radius)
}

/// A circular icon button, as used for every control in the player.
pub fn icon_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    diameter: Pixels,
    icon_size: Pixels,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    button_impl(id, icon, diameter, icon_size, false, on_click)
}

/// Same, but rendered in the "on" state — shuffle and repeat use this.
pub fn toggle_button(
    id: impl Into<ElementId>,
    icon: &'static str,
    diameter: Pixels,
    icon_size: Pixels,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    button_impl(id, icon, diameter, icon_size, active, on_click)
}

fn button_impl(
    id: impl Into<ElementId>,
    icon: &'static str,
    diameter: Pixels,
    icon_size: Pixels,
    active: bool,
    on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Stateful<Div> {
    let (background, foreground) = if active {
        (theme::control_active(), theme::text())
    } else {
        (theme::control(), theme::text_dim())
    };

    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(diameter)
        .rounded_full()
        .bg(background)
        .cursor_pointer()
        .hover(|this| this.bg(theme::control_hover()))
        .active(|this| this.bg(theme::control_active()))
        .child(svg().path(icon).size(icon_size).text_color(foreground))
        // Buttons sit on the draggable header; without this the press would
        // start a window move instead of a click.
        .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
}

/// Maps a horizontal mouse position onto a 0..=1 fraction of `bounds`.
pub fn fraction_at(bounds: Bounds<Pixels>, x: Pixels) -> f32 {
    if bounds.size.width <= px(0.) {
        return 0.0;
    }
    ((x - bounds.origin.x) / bounds.size.width).clamp(0.0, 1.0)
}

/// A horizontal bar: filled track up to `fraction`, muted remainder.
pub fn bar(fraction: f32, height: Pixels, fill: Rgba, track: Rgba) -> Div {
    let mut base = div().h(height).w_full().bg(track);
    base = rounded(base, height / 2.);

    let mut filled = div()
        .h_full()
        .w(gpui::relative(fraction.clamp(0.0, 1.0)))
        .bg(fill);
    filled = rounded(filled, height / 2.);

    base.child(filled)
}
