use crate::decode;
use anyhow::{Context as _, Result};
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source};
use std::path::Path;
use std::time::Duration;

/// Owns the rodio output device and the player queue.
///
/// The device sink handle must stay alive for the whole lifetime of the app —
/// dropping it silences playback.
pub struct Audio {
    _device: MixerDeviceSink,
    player: Player,
    /// Sample rate the output device is currently open at.
    rate: SampleRate,
    volume: f32,
    /// A track has been loaded into the player at least once. Before that,
    /// `player.empty()` is trivially true and must not be read as "finished".
    loaded: bool,
}

/// Opens the default output device, preferring `rate` so that playback of a
/// track at that rate needs no resampling at all.
///
/// This matters: rodio resamples anything whose rate differs from the device's
/// with a cheap linear converter, and PipeWire then resamples back to the sink's
/// own rate. Matching rates end-to-end removes both conversions.
fn open_device(rate: Option<SampleRate>) -> Result<MixerDeviceSink> {
    let mut builder = DeviceSinkBuilder::from_default_device()
        .context("no default audio device found")?
        .with_error_callback(|err| eprintln!("hark: audio stream error: {err}"));

    if let Some(rate) = rate {
        builder = builder.with_sample_rate(rate);
    }

    let mut device = builder
        .open_sink_or_fallback()
        .context("could not open the audio output")?;
    // Switching tracks reopens the device; rodio's drop notice is expected here.
    device.log_on_drop(false);

    eprintln!(
        "hark: output {} Hz, {} channels, buffer {:?}",
        device.config().sample_rate(),
        device.config().channel_count(),
        device.config().buffer_size(),
    );
    Ok(device)
}

impl Audio {
    pub fn new() -> Result<Audio> {
        let device = open_device(None)?;
        let rate = device.config().sample_rate();
        let player = Player::connect_new(device.mixer());
        let volume = 0.7;
        player.set_volume(volume);

        Ok(Audio {
            _device: device,
            player,
            rate,
            volume,
            loaded: false,
        })
    }

    pub fn play_file(&mut self, path: &Path) -> Result<()> {
        let source = decode::open(path)?;

        // Reopen the device at the track's own rate when it differs, so the
        // samples reach the sink untouched.
        let track_rate = source.sample_rate();
        if track_rate != self.rate {
            let device = open_device(Some(track_rate))?;
            let player = Player::connect_new(device.mixer());
            player.set_volume(self.volume);
            self.rate = device.config().sample_rate();
            self._device = device;
            self.player = player;
        }

        self.player.clear();
        self.player.append(source);
        self.player.play();
        self.loaded = true;
        Ok(())
    }

    pub fn toggle_play(&mut self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    pub fn stop(&mut self) {
        self.player.clear();
        self.loaded = false;
    }

    pub fn is_playing(&self) -> bool {
        self.loaded && !self.player.is_paused() && !self.player.empty()
    }

    /// True once the current source has played to its end.
    pub fn finished(&self) -> bool {
        self.loaded && self.player.empty()
    }

    pub fn position(&self) -> Duration {
        if self.loaded {
            self.player.get_pos()
        } else {
            Duration::ZERO
        }
    }

    pub fn seek(&mut self, to: Duration) {
        if !self.loaded {
            return;
        }
        // Not every decoder supports seeking; ignore the failure rather than
        // tearing down playback.
        if let Err(err) = self.player.try_seek(to) {
            eprintln!("hark: seek failed: {err}");
        }
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.player.set_volume(self.volume);
    }
}
