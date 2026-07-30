use crate::{decode, meta_cache, peak_cache};
use anyhow::Result;
use std::path::Path;

/// Number of peaks stored per track. The view resamples this down to however
/// many bars fit its width.
pub const PEAKS: usize = 220;

/// Samples folded into one coarse peak during the first pass.
const WINDOW: usize = 1024;

/// The peaks for `path`: out of the file's own extended attribute when one is
/// there and still matches it, decoded and written back when it is not. On a
/// hit this costs a `stat` and a `getxattr`; on a miss it costs the whole file.
///
/// Blocking either way, so it belongs on the background executor where
/// `PlayerView::load_waveform` puts it. The lookup is cheap but it is not free:
/// on the mount this was written for, a `getxattr` is a round trip.
///
/// Shaped like `Track::load`, which does the same thing for the tags, so the
/// two read alike: a failure comes back empty and is never cached.
pub fn load(path: &Path) -> Vec<f32> {
    let stamp = std::fs::metadata(path)
        .ok()
        .map(|metadata| meta_cache::stamp(&metadata));

    if let Some(stamp) = stamp
        && let Some(peaks) = peak_cache::read(path, stamp)
    {
        return peaks;
    }

    let peaks = compute(path).unwrap_or_default();
    if let Some(stamp) = stamp {
        peak_cache::write(path, stamp, &peaks);
    }
    peaks
}

/// Decodes the whole file and reduces it to `PEAKS` normalized amplitudes.
///
/// RMS, not peak: modern masters are compressed hard enough that peak amplitude
/// is pinned near the maximum for the whole track, which draws as a flat block.
/// RMS keeps the loudness contour visible.
///
/// Meant to run on the background executor: a five-minute track is tens of
/// millions of samples. [`load`] is the way in — it only comes here when the
/// file has no usable record.
fn compute(path: &Path) -> Result<Vec<f32>> {
    let decoder = decode::open(path)?;

    let mut coarse: Vec<f32> = Vec::new();
    let mut sum_squares = 0.0f64;
    let mut n = 0usize;

    for sample in decoder {
        sum_squares += (sample as f64) * (sample as f64);
        n += 1;
        if n == WINDOW {
            coarse.push((sum_squares / n as f64).sqrt() as f32);
            sum_squares = 0.0;
            n = 0;
        }
    }
    if n > 0 {
        coarse.push((sum_squares / n as f64).sqrt() as f32);
    }
    if coarse.is_empty() {
        return Ok(Vec::new());
    }

    // Second pass: fold the coarse windows into PEAKS buckets, keeping the
    // loudest window in each so transients survive the downsampling.
    let mut peaks = Vec::with_capacity(PEAKS);
    for bucket in 0..PEAKS {
        let start = bucket * coarse.len() / PEAKS;
        let end = ((bucket + 1) * coarse.len() / PEAKS)
            .max(start + 1)
            .min(coarse.len());
        let value = coarse[start..end].iter().copied().fold(0.0f32, f32::max);
        peaks.push(value);
    }

    let max = peaks.iter().copied().fold(0.0f32, f32::max);
    if max > 0.0 {
        for peak in &mut peaks {
            // Mild gamma: lifts quiet passages without flattening loud ones.
            *peak = (*peak / max).powf(0.7).clamp(0.05, 1.0);
        }
    }

    Ok(peaks)
}

/// Resamples the stored peaks to `bars` values in 0..=1.
pub fn resample(peaks: &[f32], bars: usize) -> Vec<f32> {
    if peaks.is_empty() || bars == 0 {
        return Vec::new();
    }
    (0..bars)
        .map(|bar| {
            let start = bar * peaks.len() / bars;
            let end = ((bar + 1) * peaks.len() / bars).max(start + 1).min(peaks.len());
            peaks[start..end].iter().copied().fold(0.0f32, f32::max)
        })
        .collect()
}
