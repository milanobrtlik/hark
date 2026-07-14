use crate::opus_source::OpusSource;
use anyhow::{Context as _, Result};
use rodio::{ChannelCount, Decoder, Sample, SampleRate, Source, source::SeekError};
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::time::Duration;

/// Every source the player can play. An enum rather than a boxed trait object
/// so that `try_seek` still reaches the concrete decoder.
pub enum AudioSource {
    /// MP3, FLAC, WAV, AAC/M4A, Ogg Vorbis — anything Symphonia handles.
    Symphonia(Box<Decoder<BufReader<File>>>),
    Opus(Box<OpusSource>),
}

/// Opens `path` with whichever decoder can read it.
pub fn open(path: &Path) -> Result<AudioSource> {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();

    // An Ogg container may hold Opus (which Symphonia cannot decode) or Vorbis
    // (which it can), so try Opus first and fall back on failure.
    if matches!(extension.as_str(), "opus" | "ogg" | "oga") {
        match OpusSource::open(path) {
            Ok(source) => return Ok(AudioSource::Opus(Box::new(source))),
            Err(err) if extension == "opus" => return Err(err),
            Err(_) => {}
        }
    }

    let file = File::open(path).with_context(|| format!("cannot open {path:?}"))?;
    let decoder = Decoder::try_from(BufReader::new(file))
        .with_context(|| format!("unsupported format: {path:?}"))?;
    Ok(AudioSource::Symphonia(Box::new(decoder)))
}

impl Iterator for AudioSource {
    type Item = Sample;

    #[inline]
    fn next(&mut self) -> Option<Sample> {
        match self {
            AudioSource::Symphonia(decoder) => decoder.next(),
            AudioSource::Opus(source) => source.next(),
        }
    }
}

impl Source for AudioSource {
    fn current_span_len(&self) -> Option<usize> {
        match self {
            AudioSource::Symphonia(decoder) => decoder.current_span_len(),
            AudioSource::Opus(source) => source.current_span_len(),
        }
    }

    fn channels(&self) -> ChannelCount {
        match self {
            AudioSource::Symphonia(decoder) => decoder.channels(),
            AudioSource::Opus(source) => source.channels(),
        }
    }

    fn sample_rate(&self) -> SampleRate {
        match self {
            AudioSource::Symphonia(decoder) => decoder.sample_rate(),
            AudioSource::Opus(source) => source.sample_rate(),
        }
    }

    fn total_duration(&self) -> Option<Duration> {
        match self {
            AudioSource::Symphonia(decoder) => decoder.total_duration(),
            AudioSource::Opus(source) => source.total_duration(),
        }
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        match self {
            AudioSource::Symphonia(decoder) => decoder.try_seek(pos),
            AudioSource::Opus(source) => source.try_seek(pos),
        }
    }
}
