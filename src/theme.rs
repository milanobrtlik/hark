use gpui::{Hsla, Rgba, rgb, rgba};

/// The two tones of the translucent window fill, used as a subtle diagonal
/// gradient (light corner to dark corner). The compositor blurs whatever is
/// behind it — enough alpha to read as glass, not enough to see the desktop
/// through the player.
pub fn glass_light() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 0.98,
        a: 0.95,
    }
}

pub fn glass_dark() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 0.83,
        a: 0.95,
    }
}

/// Fill of the playlist panel. Nearly opaque: it covers the player behind it.
pub fn panel() -> Rgba {
    rgba(0xeeeeeefa)
}

pub fn text() -> Rgba {
    rgb(0x1c1c1c)
}

pub fn text_dim() -> Rgba {
    rgb(0x6d6d6d)
}

pub fn text_faint() -> Rgba {
    rgb(0x8a8a8a)
}

/// Fill of the round control buttons.
pub fn control() -> Rgba {
    rgba(0x0000000d)
}

pub fn control_hover() -> Rgba {
    rgba(0x0000001a)
}

pub fn control_active() -> Rgba {
    rgba(0x00000029)
}

pub fn wave_played() -> Rgba {
    rgb(0x3b3b3b)
}

pub fn wave_pending() -> Rgba {
    rgba(0x00000030)
}

pub fn slider_track() -> Rgba {
    rgba(0x0000001f)
}

pub fn slider_fill() -> Rgba {
    rgb(0x5c5c5c)
}

pub fn border() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 1.,
        a: 0.35,
    }
}

pub fn shadow() -> Hsla {
    Hsla {
        h: 0.,
        s: 0.,
        l: 0.,
        a: 0.22,
    }
}
