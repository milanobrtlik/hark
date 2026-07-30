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

## What hark keeps on your files

Everything hark works out about a file is written back onto that file, as an extended attribute.
Not into the file — an attribute lives on the inode next to the permissions and the timestamps, so
the audio is untouched, the modification time does not move and no other program sees it. Two
things follow, and they are the whole reason for the design: a rename carries the record along, and
a filesystem that replicates its own metadata carries it to another machine without hark doing
anything.

### The tags — `user.hark.meta`

Reading a track's tags means opening and parsing the file, and a library scan does it once per
file on every launch. That is free on a local disk and expensive on a network mount, where an open
can pull down megabytes before the tags at the front of the file are even reachable.

So the result is remembered. After a file has been parsed once, hark writes what it found into
that file's own `user.hark.meta` extended attribute (`src/meta_cache.rs`), and later scans read it
back with a `stat` and a `getxattr` — no opens at all. The startup line says how many tracks came
from it:

```
hark: tracks queued: 1000 (964 cached) in 1.2s
```

The record holds the tags, the duration, the chapters and a single bit saying the file has a
cover — never the cover itself, which is read only when the track actually plays. It is stamped
with the file's modification time and size, and that stamp is checked on every read, so re-tagging
a file invalidates its record by itself.

### The waveform — `user.hark.peaks`

The bars under the cover art are 220 amplitudes measured across the track, and measuring them means
decoding the whole file: tens of millions of samples for a five-minute song, and on a network mount
the file comes across a second time to do it. Without a record that happens on every play of the
same track, which makes it far and away the most expensive thing hark does.

The record is 237 bytes — a version byte, the same stamp the tags carry, and one byte per bar. A
byte is finer than 46-pixel-tall bars can show. A file that decodes to no audio at all is never
remembered, on the same principle as a failed tag parse: it is more likely a half-copied file than a
real answer, and freezing it in would leave a flat line that only `setfattr` could clear. Silence,
which is a real answer, is remembered.

Its version byte moves when the measurement changes, not only when the record's shape does. Unlike
the tags, which mirror what is in the file, these mirror a calculation — so a changed calculation
makes every stored record wrong about a file that has not moved, and the stamp cannot catch it.

### What AcoustID said — `user.hark.id`

When a file's tags are too poor to find a cover from, hark identifies it by its sound instead:
two minutes of audio decoded, run through a Chromaprint fingerprint and put to a rate-limited web
service — once per chapter, for a full-album rip. The answer is the album's MusicBrainz id and the
titles of its songs, and it is kept.

A record carrying no id means AcoustID was asked and did not know, which is worth remembering just
as much: without it every play fingerprints the file again for the same silence. A lookup that
failed on the network is not recorded, so it is retried.

This replaces a cache under `~/.cache/hark/fingerprints`, which was keyed on the path, was lost on a
rename and never left the machine it was written on. **That folder is no longer used and can be
deleted.**

### Dropping a record

Nothing here is load-bearing. A filesystem without extended attributes, a read-only mount or a
record hark cannot make sense of all mean the same thing — do the work again, as before. The only
symptom is that every run reports `(0 cached)`. Note that ext4 fits *all* of an inode's attributes
into a single 4 KiB block, which these records now share; on a heavily chaptered file the last one
may simply not fit, and stays on the slow path.

```
setfattr -x user.hark.meta  track.flac   # the tags
setfattr -x user.hark.peaks track.flac   # the waveform
setfattr -x user.hark.id    track.flac   # what AcoustID said
```

Album covers are the exception to all of this. Image bytes are far too big for an attribute, whose
ceiling is 64 KiB, and fetching one again is a single HTTP request — so they stay in
`~/.cache/hark/covers`.

## Audio

The output device is opened at the sample rate of the track being played. Without that, rodio
resamples with its own linear converter to the device rate, and PipeWire then resamples back to
its own — two needless conversions.

If playback skips, check the file first:

```
ffmpeg -v error -i track.flac -f null -
```

Every `decode_frame() failed` it reports is one dropped frame, i.e. roughly 85 ms of missing audio.
