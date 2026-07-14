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

## Usage

```
hark                    # loads the music folder (XDG_MUSIC_DIR, else ~/Music)
hark track.flac         # plays the given files or folders
```

Tracks can also be added by dropping them onto the window or with the `+` button.

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
