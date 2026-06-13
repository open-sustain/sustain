// SPDX-License-Identifier: MIT OR Apache-2.0
//! Harmonic-percussive separation (HPSS) for tonal analysis.
//!
//! **Sustain-authored — not vendored from stratum-dsp.** Re-derived from the
//! median-filtering HPSS of Fitzgerald (2010) and Driedger & Müller (2014); no
//! upstream HPSS code was copied (the vendoring deliberately left HPSS out, see
//! `PROVENANCE.md`). It is licensed `MIT OR Apache-2.0` to match the rest of
//! this crate rather than the workspace's GPL-3.0-or-later — `PROVENANCE.md`
//! records why every file here is permissive, so the header is intentional and
//! must not be "corrected" to GPL.
//!
//! The key detector's chroma front-end matches a pitch-class profile against
//! key templates; broadband percussive energy (drums, transients) spreads
//! across every pitch class and dilutes that profile. HPSS attenuates it before
//! chroma extraction, leaving the sustained tonal content the templates expect.
//!
//! # References
//!
//! - Fitzgerald, D. (2010). Harmonic/Percussive Separation using Median
//!   Filtering. *Proc. Int. Conf. on Digital Audio Effects (DAFx-10)*.
//! - Driedger, J., & Müller, M. (2014). Median-filtering harmonic-percussive
//!   separation (soft Wiener-style masking of the time/frequency medians).

use crate::error::AnalysisError;

/// Numerical floor: below this a bin carries no usable energy in either
/// estimate, so its mask is zero rather than the ratio of two near-zero
/// magnitudes.
const EPSILON: f32 = 1e-10;

/// Emphasize the harmonic content of a magnitude spectrogram, attenuating
/// percussive/broadband energy, and return a spectrogram of the same shape.
///
/// Median-filter HPSS (Fitzgerald 2010): a sustained tone is stable across
/// *time* at a fixed frequency — a horizontal ridge — while a percussive
/// transient is broadband at a fixed *time* — a vertical ridge.
/// Median-filtering the magnitude along time (window `time_kernel` frames)
/// estimates the harmonic component `H`; median-filtering along frequency
/// (window `freq_kernel` bins) estimates the percussive component `P`. A soft
/// Wiener mask `M = H² / (H² + P²)` then weights each bin by how harmonic it is
/// (the MMSE-optimal soft mask under a Gaussian-source model, Driedger &
/// Müller), and the returned spectrogram is the input scaled by `M`. A bin with
/// no energy in either estimate gets a zero mask.
///
/// `time_kernel` and `freq_kernel` are the median window lengths; they should
/// be odd. The window shrinks symmetrically at the spectrogram edges (the
/// median is taken over whatever samples are in range), so no padding value is
/// invented. A kernel of `1` makes its estimate the bin itself (no filtering on
/// that axis).
///
/// # Errors
///
/// [`AnalysisError::InvalidInput`] if the spectrogram frames have inconsistent
/// bin counts.
pub fn emphasize_harmonic(
    spectrogram: &[Vec<f32>],
    time_kernel: usize,
    freq_kernel: usize,
) -> Result<Vec<Vec<f32>>, AnalysisError> {
    if spectrogram.is_empty() {
        return Ok(Vec::new());
    }
    let n_bins = spectrogram[0].len();
    for (i, frame) in spectrogram.iter().enumerate() {
        if frame.len() != n_bins {
            return Err(AnalysisError::InvalidInput(format!(
                "Inconsistent spectrogram frame lengths: frame 0 has {} bins, frame {} has {} bins",
                n_bins,
                i,
                frame.len()
            )));
        }
    }
    let n_frames = spectrogram.len();
    let t_half = time_kernel / 2;

    let mut out = vec![vec![0.0f32; n_bins]; n_frames];
    // Percussive estimate of the current frame (median across frequency),
    // filled per row by a sliding-window median. Reused across frames.
    let mut percussive = vec![0.0f32; n_bins];
    let mut window: Vec<f32> = Vec::with_capacity(freq_kernel.max(1));
    // Scratch for the (small) per-bin time-median.
    let mut time_col: Vec<f32> = Vec::with_capacity(time_kernel.max(1));

    for t in 0..n_frames {
        // Percussive estimate P[t][·]: median of this frame along frequency.
        median_filter_1d(&spectrogram[t], freq_kernel, &mut percussive, &mut window);

        for b in 0..n_bins {
            // Harmonic estimate H[t][b]: median of this bin along time. The
            // window is small (a few frames), so a direct selection is cheap.
            let t0 = t.saturating_sub(t_half);
            let t1 = (t + t_half + 1).min(n_frames);
            time_col.clear();
            time_col.extend((t0..t1).map(|tt| spectrogram[tt][b]));
            let mid = time_col.len() / 2;
            time_col.select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
            let h = time_col[mid];

            let p = percussive[b];
            let h2 = h * h;
            let p2 = p * p;
            let denom = h2 + p2;
            let mask = if denom > EPSILON { h2 / denom } else { 0.0 };
            out[t][b] = spectrogram[t][b] * mask;
        }
    }
    Ok(out)
}

/// Median-filter a 1-D signal with a sliding window of length `kernel`, writing
/// `out[i] = median(input[i - kernel/2 ..= i + kernel/2])`. The window shrinks
/// symmetrically at the edges (no padding value is invented). Because the
/// window advances one sample at a time, it is maintained as a sorted buffer
/// with one insert and at most one removal per step — O(kernel) work per output
/// rather than the O(kernel) *selection* a from-scratch median would repeat.
/// For an even window the upper-middle order statistic is returned (only the
/// shrinking edge windows are even; interior ones are odd).
///
/// `out` must be the same length as `input`; `window` is a reused scratch
/// buffer (cleared on entry).
fn median_filter_1d(input: &[f32], kernel: usize, out: &mut [f32], window: &mut Vec<f32>) {
    debug_assert_eq!(input.len(), out.len());
    let n = input.len();
    let half = kernel / 2;
    window.clear();
    let mut lo = 0usize; // inclusive: window currently holds input[lo..hi]
    let mut hi = 0usize; // exclusive
    for (i, slot) in out.iter_mut().enumerate() {
        let want_hi = (i + half + 1).min(n);
        let want_lo = i.saturating_sub(half);
        while hi < want_hi {
            sorted_insert(window, input[hi]);
            hi += 1;
        }
        while lo < want_lo {
            sorted_remove(window, input[lo]);
            lo += 1;
        }
        *slot = window[window.len() / 2];
    }
}

/// Insert `x` into the ascending sorted buffer `v`, keeping it sorted.
fn sorted_insert(v: &mut Vec<f32>, x: f32) {
    let pos = v.partition_point(|&y| y < x);
    v.insert(pos, x);
}

/// Remove one occurrence of `x` from the ascending sorted buffer `v`. `x` is
/// always present (it was inserted when it entered the window), so the
/// `partition_point` lands on a matching element.
fn sorted_remove(v: &mut Vec<f32>, x: f32) {
    let pos = v.partition_point(|&y| y < x);
    v.remove(pos);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_spectrogram_is_empty() {
        assert!(emphasize_harmonic(&[], 5, 37).unwrap().is_empty());
    }

    #[test]
    fn inconsistent_frame_lengths_error() {
        let spec = vec![vec![1.0f32; 8], vec![1.0f32; 7]];
        assert!(emphasize_harmonic(&spec, 3, 3).is_err());
    }

    #[test]
    fn output_preserves_shape_and_is_finite_nonnegative() {
        let n_frames = 12;
        let n_bins = 20;
        let spec: Vec<Vec<f32>> = (0..n_frames)
            .map(|t| {
                (0..n_bins)
                    .map(|b| ((t * 7 + b * 3) % 11) as f32 * 0.1)
                    .collect()
            })
            .collect();
        let out = emphasize_harmonic(&spec, 5, 7).unwrap();
        assert_eq!(out.len(), n_frames);
        for frame in &out {
            assert_eq!(frame.len(), n_bins);
            for &v in frame {
                assert!(v.is_finite() && v >= 0.0);
            }
        }
    }

    #[test]
    fn sustained_tone_is_preserved() {
        // One bin energetic across all frames, the rest silent: a horizontal
        // ridge. Its time-median (H) is the tone; its frequency-median (P) is
        // ~0 (the tone is one bin among many), so the mask is ~1.
        let n_frames = 16;
        let n_bins = 24;
        let tone_bin = 10;
        let mut spec = vec![vec![0.0f32; n_bins]; n_frames];
        for frame in &mut spec {
            frame[tone_bin] = 1.0;
        }
        let out = emphasize_harmonic(&spec, 5, 7).unwrap();
        assert!(
            out[8][tone_bin] > 0.9,
            "sustained tone attenuated: {}",
            out[8][tone_bin]
        );
    }

    #[test]
    fn percussive_transient_is_suppressed() {
        // One frame energetic across all bins, the rest silent: a vertical
        // ridge. Its time-median (H) is ~0 (one frame among many); its
        // frequency-median (P) is the broadband level, so the mask is ~0.
        let n_frames = 16;
        let n_bins = 24;
        let hit = 8;
        let mut spec = vec![vec![0.0f32; n_bins]; n_frames];
        spec[hit].fill(1.0);
        let out = emphasize_harmonic(&spec, 5, 7).unwrap();
        for (b, &v) in out[hit].iter().enumerate().take(n_bins - 5).skip(5) {
            assert!(v < 0.1, "transient bin {b} not suppressed: {v}");
        }
    }

    #[test]
    fn harmonic_kept_percussive_dropped_in_mixture() {
        // A sustained tone crossed by a single broadband transient frame.
        let n_frames = 16;
        let n_bins = 24;
        let tone_bin = 10;
        let hit = 8;
        let mut spec = vec![vec![0.0f32; n_bins]; n_frames];
        for frame in &mut spec {
            frame[tone_bin] = 1.0;
        }
        for slot in &mut spec[hit] {
            *slot += 1.0;
        }
        let out = emphasize_harmonic(&spec, 5, 7).unwrap();
        // Tone away from the hit stays strong; a non-tone bin on the hit frame
        // is suppressed.
        assert!(out[2][tone_bin] > 0.9, "tone lost: {}", out[2][tone_bin]);
        assert!(out[hit][3] < 0.3, "transient survived: {}", out[hit][3]);
    }
}
