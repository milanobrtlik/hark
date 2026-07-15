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
    /// The user's chosen volume, 0..=1 — what the slider and MPRIS report.
    volume: f32,
    /// A cosmetic gain, 0..=1, ramped by the view to fade play/pause in and out.
    /// The player runs at `volume * fade`, so a fade never disturbs `volume`.
    fade: f32,
    /// Intended audible state. Kept apart from the rodio player so the UI can
    /// read "paused" the instant a fade-out begins, while the audio keeps
    /// flowing until the ramp reaches silence.
    playing: bool,
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
        let fade = 1.0;
        player.set_volume(volume * fade);

        Ok(Audio {
            _device: device,
            player,
            rate,
            volume,
            fade,
            playing: false,
            loaded: false,
        })
    }

    pub fn play_file(&mut self, path: &Path) -> Result<()> {
        let source = decode::open(path)?;
        let track_rate = source.sample_rate();

        // A fresh track always plays at full player gain; any play/pause fade
        // left over belongs to the previous track.
        self.fade = 1.0;

        // Reopen the device at the track's own rate when it differs, so the
        // samples reach the sink untouched.
        if track_rate != self.rate {
            let device = open_device(Some(track_rate))?;
            let player = Player::connect_new(device.mixer());
            player.set_volume(self.effective());
            self.rate = device.config().sample_rate();
            self._device = device;
            self.player = player;
        }

        self.player.clear();
        self.player.append(source);
        self.player.set_volume(self.effective());
        self.player.play();
        self.loaded = true;
        self.playing = true;
        Ok(())
    }

    /// Starts the audio flowing. The view eases it in with a fade.
    pub fn resume(&mut self) {
        self.playing = true;
        self.player.play();
    }

    /// Marks the player paused for the UI while leaving the audio running, so a
    /// fade-out can still be heard. [`Self::commit_pause`] stops it for real.
    pub fn begin_pause(&mut self) {
        self.playing = false;
    }

    /// Actually stops the audio, once the fade-out has reached silence.
    pub fn commit_pause(&mut self) {
        self.player.pause();
    }

    pub fn stop(&mut self) {
        self.player.clear();
        self.playing = false;
        self.loaded = false;
    }

    pub fn is_playing(&self) -> bool {
        self.loaded && self.playing && !self.player.empty()
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
        self.apply();
    }

    pub fn fade(&self) -> f32 {
        self.fade
    }

    pub fn set_fade(&mut self, fade: f32) {
        self.fade = fade.clamp(0.0, 1.0);
        self.apply();
    }

    /// The gain the player actually runs at: the user's volume scaled by the
    /// play/pause fade.
    fn effective(&self) -> f32 {
        self.volume * self.fade
    }

    fn apply(&self) {
        self.player.set_volume(self.effective());
    }
}
