// SPDX-License-Identifier: MIT OR Apache-2.0
//! Harmonic Pitch Class Profile (HPCP) chroma for tonal analysis.
//!
//! **Sustain-authored — not vendored from stratum-dsp.** Re-derived from
//! Gómez (2006), *Tonal Description of Music Audio Signals* (PhD thesis, UPF),
//! which introduced the HPCP feature; no code from MTG's Essentia — its
//! AGPL-3.0 reference implementation — was read or copied. Like every file in
//! this crate it is licensed `MIT OR Apache-2.0`, not the workspace's
//! GPL-3.0-or-later; `PROVENANCE.md` records why, so this header is intentional
//! and must not be "corrected" to GPL.
//!
//! # What HPCP adds over the band-summed chroma in [`super::extractor`]
//!
//! The vendored [`extract_chroma_from_spectrogram_with_options`] sums *every*
//! magnitude bin in the chroma band into pitch classes. HPCP instead derives
//! the profile from the spectrum's **peaks**:
//!
//! 1. **Spectral peak picking** with quadratic (parabolic) interpolation — only
//!    local maxima contribute, each refined to a sub-bin frequency and
//!    amplitude, so the noise floor *between* partials is discarded rather than
//!    averaged into the profile.
//! 2. **Energy weighting** — a peak contributes its interpolated amplitude
//!    *squared*.
//! 3. **Squared-cosine pitch-class window** — a peak spreads to the pitch-class
//!    bins within ±[`PITCH_WINDOW_SEMITONES`] of it, weighted by `cos²(·)`, so a
//!    peak landing between two bins splits smoothly across them and one landing
//!    on a bin lands fully on it.
//!
//! This module deliberately implements only that **core** front-end. Harmonic
//! summation, reference-frequency (tuning) estimation, sub-semitone resolution,
//! and per-frame max-normalization — the remaining elements of a full HPCP —
//! are intentionally *not* here; each is a separable change to be measured on
//! its own. The output contract matches the vendored path exactly: one
//! 12-element, L2-normalized pitch-class vector per frame, on the same
//! pitch-class grid (shared [`A4_FREQ`]/[`SEMITONE_OFFSET`]) and the same
//! `[`[CHROMA_FMIN_HZ]`, `[CHROMA_FMAX_HZ]`]` band, so [`detect_key`] consumes
//! it unchanged.
//!
//! [`extract_chroma_from_spectrogram_with_options`]:
//!     super::extractor::extract_chroma_from_spectrogram_with_options
//! [`detect_key`]: crate::detect_key
//!
//! # References
//!
//! - Gómez, E. (2006). *Tonal Description of Music Audio Signals*. PhD thesis,
//!   Universitat Pompeu Fabra. (HPCP definition.)
//! - Smith, J. O. *Spectral Audio Signal Processing* — quadratic interpolation
//!   of spectral peaks (QIFFT).

use super::extractor::{A4_FREQ, CHROMA_FMAX_HZ, CHROMA_FMIN_HZ, SEMITONE_OFFSET};
use crate::error::AnalysisError;

/// Numerical floor for the parabolic-interpolation denominator and the final
/// L2 normalization.
const EPSILON: f32 = 1e-10;

/// Half-width, in semitones, of the squared-cosine window a peak spreads over:
/// a peak contributes to every pitch-class bin strictly within this distance,
/// with weight `cos²(π/2 · δ / PITCH_WINDOW_SEMITONES)`. At `1.0` the weight is
/// `1` on a bin a peak lands exactly on and tapers to `0` one semitone away, so
/// an off-grid peak splits across its two nearest bins — the 12-bin analog of
/// the ±1-bin spread the band-summed path applies, derived from the bin spacing
/// rather than fitted to any corpus.
const PITCH_WINDOW_SEMITONES: f32 = 1.0;

/// Compute a Harmonic Pitch Class Profile for each frame of a magnitude
/// spectrogram.
///
/// Returns one 12-element, L2-normalized pitch-class vector per input frame —
/// the same shape and normalization as
/// [`extract_chroma_from_spectrogram_with_options`], so it is a drop-in chroma
/// source for [`detect_key`]. See the module docs for the algorithm.
///
/// `sample_rate` and `fft_size` are the parameters the spectrogram was computed
/// with; they set the bin→frequency mapping (`bin · sample_rate / fft_size`).
///
/// [`extract_chroma_from_spectrogram_with_options`]:
///     super::extractor::extract_chroma_from_spectrogram_with_options
/// [`detect_key`]: crate::detect_key
///
/// # Errors
///
/// [`AnalysisError::InvalidInput`] if the spectrogram frames are empty (zero
/// bins) or have inconsistent bin counts.
pub fn compute_hpcp(
    spectrogram: &[Vec<f32>],
    sample_rate: u32,
    fft_size: usize,
) -> Result<Vec<Vec<f32>>, AnalysisError> {
    if spectrogram.is_empty() {
        return Ok(Vec::new());
    }
    let n_bins = spectrogram[0].len();
    if n_bins == 0 {
        return Err(AnalysisError::InvalidInput(
            "Empty spectrogram frames".to_string(),
        ));
    }
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

    let freq_resolution = sample_rate as f32 / fft_size as f32;
    let fmax = CHROMA_FMAX_HZ.min(sample_rate as f32 / 2.0);

    let mut out = Vec::with_capacity(spectrogram.len());
    for frame in spectrogram {
        out.push(frame_to_hpcp(frame, freq_resolution, fmax));
    }
    Ok(out)
}

/// Map one magnitude frame to a 12-element, L2-normalized HPCP vector.
fn frame_to_hpcp(magnitude_frame: &[f32], freq_resolution: f32, fmax: f32) -> Vec<f32> {
    let mut hpcp = [0.0f32; 12];
    let n = magnitude_frame.len();

    // Only interior bins can be local maxima (a peak needs both neighbors for
    // the local-max test and the parabolic interpolation).
    for i in 1..n.saturating_sub(1) {
        let left = magnitude_frame[i - 1];
        let mid = magnitude_frame[i];
        let right = magnitude_frame[i + 1];

        // Local maximum: strictly above the left neighbor, at least the right
        // (so the left edge of a flat top is taken once, not every bin of it).
        if !(mid > left && mid >= right) || mid <= 0.0 {
            continue;
        }

        // Quadratic (parabolic) interpolation of the peak from the three
        // magnitude samples (Smith, QIFFT). `denom < 0` for a genuine local
        // maximum; the vertex offset is in [-0.5, 0.5], clamped defensively
        // against degenerate inputs. Done on linear magnitude — a small,
        // documented departure from the log-magnitude QIFFT, taken to avoid
        // `log(0)` on silent bins; the sub-bin frequency it yields is what
        // matters for pitch-class assignment.
        let denom = left - 2.0 * mid + right;
        let offset = if denom.abs() > EPSILON {
            (0.5 * (left - right) / denom).clamp(-0.5, 0.5)
        } else {
            0.0
        };

        let freq = (i as f32 + offset) * freq_resolution;
        if freq < CHROMA_FMIN_HZ || freq > fmax {
            continue;
        }

        // Interpolated peak amplitude; floored at the sampled magnitude so the
        // parabolic correction can only refine, never invent a larger peak.
        let amplitude = (mid - 0.25 * (left - right) * offset).max(0.0);
        let energy = amplitude * amplitude;

        // Pitch class of the peak on the shared grid (index 0 = C): the
        // `+ SEMITONE_OFFSET` is what places A4 at pitch class 9, matching the
        // key templates.
        let pitch_class = (12.0 * (freq / A4_FREQ).log2() + SEMITONE_OFFSET).rem_euclid(12.0);

        for (bin, slot) in hpcp.iter_mut().enumerate() {
            let raw = (pitch_class - bin as f32).abs();
            let distance = raw.min(12.0 - raw); // circular pitch-class distance
            if distance < PITCH_WINDOW_SEMITONES {
                let phase = std::f32::consts::FRAC_PI_2 * distance / PITCH_WINDOW_SEMITONES;
                let weight = phase.cos() * phase.cos();
                *slot += energy * weight;
            }
        }
    }

    let norm = hpcp.iter().map(|&x| x * x).sum::<f32>().sqrt();
    if norm > EPSILON {
        for x in &mut hpcp {
            *x /= norm;
        }
    }
    hpcp.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: u32 = 44_100;
    const FFT_SIZE: usize = 8_192;

    /// Bin index whose center frequency is nearest `freq`.
    fn bin_of(freq: f32) -> usize {
        (freq * FFT_SIZE as f32 / SAMPLE_RATE as f32).round() as usize
    }

    /// A magnitude frame (FFT_SIZE/2 + 1 bins) with a single triangular peak at
    /// `bin`: `bin` carries `1.0`, its two neighbors `0.5`, the rest silent.
    fn frame_with_peak(bin: usize) -> Vec<f32> {
        let mut frame = vec![0.0f32; FFT_SIZE / 2 + 1];
        frame[bin - 1] = 0.5;
        frame[bin] = 1.0;
        frame[bin + 1] = 0.5;
        frame
    }

    #[test]
    fn empty_spectrogram_is_empty() {
        assert!(compute_hpcp(&[], SAMPLE_RATE, FFT_SIZE).unwrap().is_empty());
    }

    #[test]
    fn zero_bin_frames_error() {
        assert!(compute_hpcp(&[vec![]], SAMPLE_RATE, FFT_SIZE).is_err());
    }

    #[test]
    fn inconsistent_frame_lengths_error() {
        let spec = vec![vec![0.0f32; 8], vec![0.0f32; 7]];
        assert!(compute_hpcp(&spec, SAMPLE_RATE, FFT_SIZE).is_err());
    }

    #[test]
    fn output_preserves_shape_and_is_l2_normalized() {
        let spec = vec![frame_with_peak(bin_of(440.0)); 5];
        let out = compute_hpcp(&spec, SAMPLE_RATE, FFT_SIZE).unwrap();
        assert_eq!(out.len(), 5);
        for vector in &out {
            assert_eq!(vector.len(), 12);
            let norm: f32 = vector.iter().map(|&x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 1e-4, "not L2-normalized: {norm}");
        }
    }

    #[test]
    fn pure_tone_resolves_to_its_pitch_class() {
        // A single peak at A4 (440 Hz) must put pitch class 9 (A) on top.
        let spec = vec![frame_with_peak(bin_of(440.0))];
        let hpcp = &compute_hpcp(&spec, SAMPLE_RATE, FFT_SIZE).unwrap()[0];
        let top = hpcp
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(top, 9, "A4 peak resolved to pitch class {top}, not A");

        // Middle C (≈261.63 Hz) must put pitch class 0 (C) on top.
        let spec_c = vec![frame_with_peak(bin_of(261.63))];
        let hpcp_c = &compute_hpcp(&spec_c, SAMPLE_RATE, FFT_SIZE).unwrap()[0];
        let top_c = hpcp_c
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(top_c, 0, "C4 peak resolved to pitch class {top_c}, not C");
    }

    #[test]
    fn monotonic_ramp_has_no_peaks() {
        // A strictly increasing spectrum has no interior local maximum, so an
        // HPCP built only from peaks is all-zero — confirming the noise floor
        // between partials never contributes (the core HPCP distinction).
        let frame: Vec<f32> = (0..FFT_SIZE / 2 + 1).map(|i| i as f32).collect();
        let hpcp = &compute_hpcp(&[frame], SAMPLE_RATE, FFT_SIZE).unwrap()[0];
        assert!(
            hpcp.iter().all(|&x| x == 0.0),
            "ramp produced a non-empty profile: {hpcp:?}"
        );
    }

    #[test]
    fn peak_between_bins_splits_across_neighbors() {
        // A peak whose frequency sits roughly a quarter-tone above A spreads
        // across A (9) and A#/Bb (10) and leaves the rest near zero.
        let between = 440.0 * 2.0f32.powf(0.5 / 12.0); // +0.5 semitone above A4
        let spec = vec![frame_with_peak(bin_of(between))];
        let hpcp = &compute_hpcp(&spec, SAMPLE_RATE, FFT_SIZE).unwrap()[0];
        assert!(hpcp[9] > 0.1, "A bin not energized: {}", hpcp[9]);
        assert!(hpcp[10] > 0.1, "A# bin not energized: {}", hpcp[10]);
        for (bin, &v) in hpcp.iter().enumerate() {
            if bin != 9 && bin != 10 {
                assert!(v < 0.05, "off-target bin {bin} carries energy: {v}");
            }
        }
    }
}
