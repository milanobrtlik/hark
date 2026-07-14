use crate::decode;
use anyhow::Result;
use std::path::Path;

/// Number of peaks stored per track. The view resamples this down to however
/// many bars fit its width.
pub const PEAKS: usize = 220;

/// Samples folded into one coarse peak during the first pass.
const WINDOW: usize = 1024;

/// Decodes the whole file and reduces it to `PEAKS` normalized amplitudes.
///
/// RMS, not peak: modern masters are compressed hard enough that peak amplitude
/// is pinned near the maximum for the whole track, which draws as a flat block.
/// RMS keeps the loudness contour visible.
///
/// Meant to run on the background executor: a five-minute track is tens of
/// millions of samples.
pub fn compute(path: &Path) -> Result<Vec<f32>> {
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
