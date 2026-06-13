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
//! 4. **Harmonic summation** — each peak also credits the pitch classes of its
//!    first [`N_HARMONICS`] *subharmonics* (`freq / h`), decayed by
//!    [`HARMONIC_DECAY`] per harmonic, folding overtone energy back onto the
//!    fundamentals it could belong to. This concentrates each played note on
//!    its true pitch class and counters the systematic leak whereby a tonic's
//!    5th harmonic (its major third) inflates the major-third bin.
//! 5. **Global tuning estimation** — before binning, the recording's reference
//!    pitch is estimated once (the energy-weighted circular mean of every
//!    peak's deviation from the 440 Hz grid; see [`estimate_tuning`]) and
//!    subtracted from every pitch class, so audio mastered off 440 is not
//!    smeared across two bins.
//!
//! Sub-semitone resolution and per-frame max-normalization — the remaining
//! elements of a full HPCP — are intentionally *not* here; each is a separable
//! change to be measured on its own. The output contract matches the vendored
//! path exactly: one
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

/// Number of harmonics folded by the harmonic-summation step. Each spectral
/// peak credits the pitch classes of its first `N_HARMONICS` *subharmonics*
/// (`freq / h`, h = 1..=`N_HARMONICS`), attributing the peak's energy back to
/// the fundamentals it could be an overtone of. Eight is the canonical
/// HPCP / harmonic-salience depth (Gómez 2006); with [`HARMONIC_DECAY`] the
/// eighth term carries `0.6^7 ≈ 0.028` of the energy, so deeper folding is
/// negligible. A fixed literature value, not tuned to any corpus.
const N_HARMONICS: usize = 8;

/// Geometric decay of the harmonic-summation weight: the h-th subharmonic
/// contributes `HARMONIC_DECAY^(h-1)` of the peak's energy, so `h = 1` (the
/// peak's own pitch class) keeps full weight and reproduces the core term
/// exactly. `0.6` is the standard harmonic-weighting decay (Gómez 2006); like
/// [`N_HARMONICS`] it is a fixed literature value, not fitted to any corpus.
const HARMONIC_DECAY: f32 = 0.6;

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

    // Estimate the recording's tuning once over the whole spectrogram, then
    // bind every frame's pitch-class grid to it (see `estimate_tuning`).
    let tuning = estimate_tuning(spectrogram, freq_resolution, fmax);

    let mut out = Vec::with_capacity(spectrogram.len());
    for frame in spectrogram {
        out.push(frame_to_hpcp(frame, freq_resolution, fmax, tuning));
    }
    Ok(out)
}

/// Invoke `visit(freq, energy)` for every spectral peak of `magnitude_frame`.
///
/// A peak is an interior local maximum, refined to a sub-bin frequency and
/// amplitude by quadratic (QIFFT) interpolation and kept only if it falls inside
/// the `[`[CHROMA_FMIN_HZ]`, `fmax`]` band; `energy` is that amplitude squared.
/// This is the single source of peak picking shared by [`estimate_tuning`] (the
/// tuning pass) and [`frame_to_hpcp`] (the binning pass), so the two can never
/// disagree on what counts as a peak.
fn for_each_peak(
    magnitude_frame: &[f32],
    freq_resolution: f32,
    fmax: f32,
    mut visit: impl FnMut(f32, f32),
) {
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
        visit(freq, amplitude * amplitude);
    }
}

/// Estimate a global tuning offset, in semitones ∈ [−0.5, 0.5], as the
/// energy-weighted **circular mean** of every peak's deviation from the
/// equal-tempered 440 Hz grid. Subtracting it from each pitch class (in
/// [`frame_to_hpcp`]) re-references the profile to the recording's actual
/// concert pitch instead of assuming 440 — so audio mastered or recorded off
/// 440 no longer smears each note across two bins and blunts the template match.
///
/// The estimate is the resultant of `energy · e^{i·2π·dev}`: a true vector sum,
/// so it is robust to the ±0.5-semitone wrap (a deviation of +0.49 and one of
/// −0.49 are nearly the same direction, not opposite). It is **parameter-free**
/// and self-zeroing — an already-on-grid signal averages to ≈ 0, leaving such
/// material untouched — and returns 0 when there is no peak energy to measure.
fn estimate_tuning(spectrogram: &[Vec<f32>], freq_resolution: f32, fmax: f32) -> f32 {
    let mut re = 0.0f32;
    let mut im = 0.0f32;
    for frame in spectrogram {
        for_each_peak(frame, freq_resolution, fmax, |freq, energy| {
            let position = 12.0 * (freq / A4_FREQ).log2() + SEMITONE_OFFSET;
            let deviation = position - position.round(); // nearest-semitone error
            let phase = std::f32::consts::TAU * deviation;
            re += energy * phase.cos();
            im += energy * phase.sin();
        });
    }
    if re * re + im * im < EPSILON {
        0.0
    } else {
        im.atan2(re) / std::f32::consts::TAU
    }
}

/// Map one magnitude frame to a 12-element, L2-normalized HPCP vector, with the
/// pitch-class grid shifted by `tuning` semitones (see [`estimate_tuning`]).
fn frame_to_hpcp(
    magnitude_frame: &[f32],
    freq_resolution: f32,
    fmax: f32,
    tuning: f32,
) -> Vec<f32> {
    let mut hpcp = [0.0f32; 12];

    for_each_peak(magnitude_frame, freq_resolution, fmax, |freq, energy| {
        // Harmonic summation. A peak at `freq` is, physically, a candidate h-th
        // harmonic of a fundamental at `freq / h`, so we credit each
        // subharmonic's pitch class with the peak's energy scaled by a
        // geometric decay — folding overtone energy back onto the fundamentals
        // it could belong to. `h = 1` is the peak's own pitch class at full
        // weight (the core contribution); the `h ≥ 2` terms pull, e.g., a
        // tonic's 5th-harmonic (major-third) and 3rd-harmonic (fifth) leakage
        // back toward the tonic, raising the contrast between played scale
        // degrees and spurious overtone bins. `freq / h` stays positive and its
        // pitch class is octave-invariant, so no band re-check is needed: the
        // band filter above already decided this *peak* is trustworthy.
        let mut harmonic_weight = 1.0;
        for h in 1..=N_HARMONICS {
            // Pitch class of the subharmonic on the shared grid (index 0 = C),
            // re-referenced by the estimated `tuning`: the `+ SEMITONE_OFFSET`
            // places A4 at pitch class 9 (matching the key templates) and the
            // `- tuning` shifts the whole grid onto the recording's concert
            // pitch. The shift is uniform, so it applies to every subharmonic.
            let sub_freq = freq / h as f32;
            let pitch_class =
                (12.0 * (sub_freq / A4_FREQ).log2() + SEMITONE_OFFSET - tuning).rem_euclid(12.0);
            let contribution = energy * harmonic_weight;

            // The squared-cosine window spans strictly less than one semitone,
            // so only the two bins straddling `pitch_class` can receive energy;
            // every other bin a full 0..12 scan would visit fails the distance
            // test and adds nothing. Touching just those two — `floor` and its
            // circular successor — is the identical sum on the identical bins,
            // and keeps the 8× harmonic fan-out affordable. The two indices are
            // always distinct (`floor ∈ 0..=11`), so no bin is added twice.
            let lo = pitch_class.floor() as usize % 12;
            for bin in [lo, (lo + 1) % 12] {
                let raw = (pitch_class - bin as f32).abs();
                let distance = raw.min(12.0 - raw); // circular pitch-class distance
                if distance < PITCH_WINDOW_SEMITONES {
                    let phase = std::f32::consts::FRAC_PI_2 * distance / PITCH_WINDOW_SEMITONES;
                    let weight = phase.cos() * phase.cos();
                    hpcp[bin] += contribution * weight;
                }
            }

            harmonic_weight *= HARMONIC_DECAY;
        }
    });

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
        // A peak whose frequency sits a quarter-tone above A splits across
        // A (9) and A#/Bb (10): octave-related subharmonics (`freq/2`, `/4`,
        // `/8`) share that same off-grid pitch class, so the fundamental's two
        // nearest bins stay the two largest of the profile. Exercised against
        // `frame_to_hpcp` with `tuning = 0` so the window is isolated from the
        // tuning estimator (which would otherwise re-center a lone off-grid
        // peak — that behavior is covered by `tuning_recenters_off_grid_peaks`).
        let between = 440.0 * 2.0f32.powf(0.5 / 12.0); // +0.5 semitone above A4
        let frame = frame_with_peak(bin_of(between));
        let freq_resolution = SAMPLE_RATE as f32 / FFT_SIZE as f32;
        let fmax = CHROMA_FMAX_HZ.min(SAMPLE_RATE as f32 / 2.0);
        let hpcp = frame_to_hpcp(&frame, freq_resolution, fmax, 0.0);
        assert!(hpcp[9] > 0.1, "A bin not energized: {}", hpcp[9]);
        assert!(hpcp[10] > 0.1, "A# bin not energized: {}", hpcp[10]);
        let mut ranked: Vec<usize> = (0..12).collect();
        ranked.sort_by(|&a, &b| hpcp[b].total_cmp(&hpcp[a]));
        assert_eq!(
            [ranked[0], ranked[1]]
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            [9usize, 10].into_iter().collect(),
            "fundamental's two nearest bins are not the two largest: {hpcp:?}"
        );
    }

    #[test]
    fn tuning_recenters_off_grid_peaks() {
        // A spectrum a third of a semitone sharp of C (so every partial is off
        // the 440 grid by the same amount) must, after tuning estimation, still
        // resolve to C (pitch class 0): the estimator recovers the offset and
        // the binning subtracts it. Without the correction the energy would
        // split between C (0) and C#/Db (1).
        let detune = 1.0 / 3.0; // +1/3 semitone
        let sharp_c = 261.63 * 2.0f32.powf(detune / 12.0);
        // A few octaves of the same detuned note — a realistic, multi-peak
        // signal the circular mean can lock onto (a single peak's deviation is
        // self-referencing and trivially zeroed).
        let mut frame = vec![0.0f32; FFT_SIZE / 2 + 1];
        for octave in 0..4 {
            let b = bin_of(sharp_c * 2.0f32.powi(octave));
            frame[b - 1] += 0.5;
            frame[b] += 1.0;
            frame[b + 1] += 0.5;
        }
        let freq_resolution = SAMPLE_RATE as f32 / FFT_SIZE as f32;
        let fmax = CHROMA_FMAX_HZ.min(SAMPLE_RATE as f32 / 2.0);
        let tuning = estimate_tuning(&[frame.clone()], freq_resolution, fmax);
        assert!(
            (tuning - detune).abs() < 0.05,
            "tuning estimate {tuning} did not recover the +{detune} semitone detune"
        );
        let hpcp = &compute_hpcp(&[frame], SAMPLE_RATE, FFT_SIZE).unwrap()[0];
        let top = hpcp
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(
            top, 0,
            "detuned C did not resolve to C after tuning: {hpcp:?}"
        );
    }

    #[test]
    fn in_tune_signal_needs_no_correction() {
        // An on-grid signal (A4 and octaves) must estimate ≈ 0 tuning, so
        // already-tuned material is left untouched. The peaks land on FFT bin
        // centers, themselves up to ~half a bin (≈ 0.02 semitone at this
        // frequency) off the exact note, which floors how close to zero a
        // synthetic test can get; 0.05 stays an order of magnitude below the
        // 1/3-semitone detune `tuning_recenters_off_grid_peaks` recovers.
        let mut frame = vec![0.0f32; FFT_SIZE / 2 + 1];
        for octave in -1..=1 {
            let b = bin_of(440.0 * 2.0f32.powi(octave));
            frame[b - 1] += 0.5;
            frame[b] += 1.0;
            frame[b + 1] += 0.5;
        }
        let freq_resolution = SAMPLE_RATE as f32 / FFT_SIZE as f32;
        let fmax = CHROMA_FMAX_HZ.min(SAMPLE_RATE as f32 / 2.0);
        let tuning = estimate_tuning(&[frame], freq_resolution, fmax);
        assert!(
            tuning.abs() < 0.05,
            "in-tune signal produced tuning {tuning}"
        );
    }

    #[test]
    fn harmonic_summation_credits_subharmonics() {
        // A single A4 peak must, under harmonic summation, energize the pitch
        // class of its 3rd subharmonic (A4/3 ≈ 146.7 Hz = D) — which is silent
        // under core-only HPCP. The fundamental A (9) still dominates.
        let spec = vec![frame_with_peak(bin_of(440.0))];
        let hpcp = &compute_hpcp(&spec, SAMPLE_RATE, FFT_SIZE).unwrap()[0];
        assert!(
            hpcp[2] > 0.0,
            "3rd subharmonic (D) not credited — harmonic summation inactive: {hpcp:?}"
        );
        let top = hpcp
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(
            top, 9,
            "fundamental A no longer dominant: pitch class {top}"
        );
        assert!(
            hpcp[9] > hpcp[2],
            "fundamental A not above its subharmonic D"
        );
    }
}
