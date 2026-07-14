# Hark

A minimalist music player built on [GPUI](https://www.gpui.rs/) — the UI framework behind Zed.

A narrow portrait window with client-side decorations, album art, a waveform that doubles as the
seek bar, and a queue with shuffle and repeat. The music folder is scanned on startup and watched
for changes while the app runs.

## Building

GPUI comes from crates.io (`gpui = "0.2.2"`), so no Zed checkout is needed — clone and build.
Note that this is the released snapshot of GPUI; Zed's `main` has since split the platform layer
into a separate `gpui_platform` crate that is not published, and its API has moved on.

System dependencies (Fedora):

```
sudo dnf install alsa-lib-devel opus-devel fontconfig-devel wayland-devel \
                 libxcb-devel libxkbcommon-x11-devel vulkan-loader clang cmake
```

`alsa-lib-devel` is needed by cpal (audio output), `opus-devel` by the Opus decoder, the rest by
GPUI. On Debian/Ubuntu these are `libasound2-dev` and `libopus-dev`.

```
cargo run --release
```

The release build is not just about speed: audio decoding is so slow in a debug build that
playback crackles. That is why `Cargo.toml` sets `opt-level = 2` for dependencies in the dev
profile as well.

## Installing

```
make install                    # into ~/.local, no root needed
make install PREFIX=/usr/local  # system-wide
make uninstall
```

This installs the binary, a desktop entry and its icon. The entry is named after the window's
Wayland app id (`dev.milan.hark`), which is how GNOME pairs the window with the app's name and
icon. The absolute path to the binary is baked into the launcher at install time, because GNOME
starts it with the session's `PATH`, which need not contain `~/.local/bin`.

The new icon may only reach the dash after the shell is restarted, which on Wayland means logging
out: a running GNOME Shell holds on to its list of applications.

## Usage

```
hark                    # loads the music folder (XDG_MUSIC_DIR, else ~/Music)
hark track.flac         # plays the given files or folders
```

Tracks can also be added by dropping them onto the window or with the `+` button.

Space toggles play/pause.

## Media keys

The keyboard's play/pause key cannot be handled as a key binding: the compositor grabs the
`XF86Audio*` keys for itself, so unlike ordinary keys they never reach the window, focused or not.
The desktop routes them over D-Bus instead, to whichever player owns an MPRIS name — so hark
registers one (`src/mpris.rs`). That also puts it in GNOME's system media controls and lets
`playerctl` drive it.

Only the transport controls are exposed: play, pause, next, previous and volume. The spec's
seeking methods are left out, and `CanSeek` says so.

## Formats

MP3, FLAC, WAV, AAC/M4A and Ogg Vorbis are handled by Symphonia (through rodio).

**Opus** is not supported by Symphonia, so it has its own decoder (`src/opus_source.rs`): the
container is demuxed by the `ogg` crate and the codec itself is decoded by libopus. It implements
`rodio::Source`, including seeking via the granule positions of Ogg pages.

## Audio

The output device is opened at the sample rate of the track being played. Without that, rodio
resamples with its own linear converter to the device rate, and PipeWire then resamples back to
its own — two needless conversions.

If playback skips, check the file first:

```
ffmpeg -v error -i track.flac -f null -
```

Every `decode_frame() failed` it reports is one dropped frame, i.e. roughly 85 ms of missing audio.
