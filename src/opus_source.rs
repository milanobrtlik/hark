use anyhow::{Context as _, Result, bail};
use ogg::reading::PacketReader;
use opus::{Channels, Decoder};
use rodio::{ChannelCount, Sample, SampleRate, Source, source::SeekError};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// Opus always decodes to 48 kHz regardless of what went in.
const RATE: u32 = 48_000;
/// The longest Opus frame is 120 ms, i.e. 5760 samples per channel at 48 kHz.
const MAX_FRAME: usize = 5760;

/// A rodio [`Source`] for Ogg Opus files.
///
/// Symphonia — the decoder behind `rodio::Decoder` — cannot decode Opus, so the
/// container is demuxed with `ogg` and the packets handed to libopus.
pub struct OpusSource {
    reader: PacketReader<BufReader<File>>,
    decoder: Decoder,
    serial: u32,
    channels: u16,
    /// Encoder delay, in samples per channel, to discard from the start.
    pre_skip: u64,
    skip_remaining: u64,
    /// Interleaved samples of the packet being played out.
    buffer: Vec<f32>,
    cursor: usize,
    ended: bool,
}

impl OpusSource {
    /// Fails if `path` is not an Ogg stream carrying Opus — callers use that to
    /// fall back to Symphonia for Ogg Vorbis.
    pub fn open(path: &Path) -> Result<OpusSource> {
        let file = File::open(path).with_context(|| format!("cannot open {path:?}"))?;
        let mut reader = PacketReader::new(BufReader::new(file));

        let head = reader
            .read_packet()?
            .context("empty ogg stream")?;
        if !head.data.starts_with(b"OpusHead") || head.data.len() < 19 {
            bail!("not an opus stream");
        }

        let channels = head.data[9] as u16;
        let pre_skip = u16::from_le_bytes([head.data[10], head.data[11]]) as u64;
        let serial = head.stream_serial();

        let mode = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            other => bail!("opus with {other} channels is not supported"),
        };
        let decoder = Decoder::new(RATE, mode).context("could not initialise libopus")?;

        // The comment header follows and carries no audio.
        let _tags = reader.read_packet()?;

        Ok(OpusSource {
            reader,
            decoder,
            serial,
            channels,
            pre_skip,
            skip_remaining: pre_skip,
            buffer: Vec::new(),
            cursor: 0,
            ended: false,
        })
    }

    /// Decodes packets until one yields audio. Returns false at end of stream.
    fn fill(&mut self) -> bool {
        let channels = self.channels as usize;

        loop {
            let packet = match self.reader.read_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    self.ended = true;
                    return false;
                }
                Err(err) => {
                    eprintln!("hark: ogg stream error: {err}");
                    self.ended = true;
                    return false;
                }
            };

            self.buffer.resize(MAX_FRAME * channels, 0.0);
            let frames = match self.decoder.decode_float(&packet.data, &mut self.buffer, false) {
                Ok(frames) => frames,
                Err(err) => {
                    // A damaged packet costs one frame, not the whole track.
                    eprintln!("hark: damaged opus packet: {err}");
                    continue;
                }
            };

            self.buffer.truncate(frames * channels);
            self.cursor = 0;

            if self.skip_remaining > 0 {
                let skip = (self.skip_remaining as usize * channels).min(self.buffer.len());
                self.cursor = skip;
                self.skip_remaining -= (skip / channels) as u64;
            }

            if self.cursor < self.buffer.len() {
                return true;
            }
        }
    }
}

impl Iterator for OpusSource {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        if self.cursor >= self.buffer.len() && (self.ended || !self.fill()) {
            return None;
        }
        let sample = self.buffer[self.cursor];
        self.cursor += 1;
        Some(sample)
    }
}

impl Source for OpusSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.buffer.len() - self.cursor)
    }

    fn channels(&self) -> ChannelCount {
        ChannelCount::new(self.channels).expect("opus stream has at least one channel")
    }

    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(RATE).expect("48000 is not zero")
    }

    fn total_duration(&self) -> Option<Duration> {
        // The UI takes durations from the file's tags instead.
        None
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        // Ogg granule positions for Opus count 48 kHz samples, offset by the
        // encoder delay.
        let granule = self.pre_skip + (pos.as_secs_f64() * RATE as f64) as u64;

        self.reader
            .seek_absgp(Some(self.serial), granule)
            .map_err(|err| SeekError::Other(Arc::new(err)))?;

        self.decoder
            .reset_state()
            .map_err(|err| SeekError::Other(Arc::new(err)))?;

        self.buffer.clear();
        self.cursor = 0;
        self.skip_remaining = 0;
        self.ended = false;
        Ok(())
    }
}
