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
//! 4. **Tuning estimation** — before binning, the recording's reference pitch
//!    is estimated over the whole spectrogram (the energy-weighted circular
//!    mean of every peak's deviation from the 440 Hz grid; see
//!    [`TuningEvidence`]) and subtracted from every pitch class, so audio
//!    mastered off 440 is not smeared across two bins. The evidence is
//!    returned with the frames, so a recording analysed in pieces can bring
//!    every piece onto the tuning of the whole ([`Hpcp::into_frames_tuned_to`]).
//!
//! Harmonic summation (crediting each peak's subharmonics), sub-semitone
//! resolution and per-frame max-normalization — the remaining elements of a
//! full HPCP — are intentionally *not* here. Harmonic summation was, until key
//! analyzer v17: it blurred every peak onto its fifth below and measured no
//! better without. The output contract matches the vendored path exactly: one
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

/// How far a recording's peaks sit from the equal-tempered 440 Hz grid: the
/// resultant of `energy · e^{i·2π·deviation}` over every spectral peak, where
/// `deviation` is the peak's distance in semitones from its nearest note.
///
/// A true vector sum, so it is robust to the ±0.5-semitone wrap (a deviation
/// of +0.49 and one of −0.49 are nearly the same direction, not opposite),
/// and evidence from several pieces of a recording adds up to the evidence of
/// the whole (`+`, [`Iterator::sum`]). It is parameter-free and self-zeroing:
/// an already-on-grid signal averages to ≈ 0.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TuningEvidence {
    re: f32,
    im: f32,
}

impl TuningEvidence {
    /// The tuning offset the evidence points to, in semitones ∈ [−0.5, 0.5]:
    /// the circular mean of the peaks' deviations. `0` when there is no peak
    /// energy to measure.
    pub fn semitones(self) -> f32 {
        if self.re * self.re + self.im * self.im < EPSILON {
            0.0
        } else {
            self.im.atan2(self.re) / std::f32::consts::TAU
        }
    }
}

impl std::ops::Add for TuningEvidence {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            re: self.re + other.re,
            im: self.im + other.im,
        }
    }
}

impl std::iter::Sum for TuningEvidence {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), std::ops::Add::add)
    }
}

/// The HPCP frames of a spectrogram, and the tuning they are binned on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Hpcp {
    /// One 12-element, L2-normalized pitch-class vector per spectrogram frame.
    pub frames: Vec<Vec<f32>>,
    /// The spectrogram's own tuning evidence, whose
    /// [`semitones`](TuningEvidence::semitones) every frame is binned on.
    pub tuning: TuningEvidence,
}

impl Hpcp {
    /// The frames, on the pitch-class grid of the tuning `reference` points
    /// to rather than their own.
    ///
    /// A tuning is a direction on a circle, so its offset is only defined up
    /// to a whole semitone: two pieces of one recording a little under a
    /// quarter-tone sharp can estimate +0.45 and −0.45 (≡ +0.55) and bin the
    /// same note one pitch class apart. Each piece keeps its own offset, but
    /// the one congruent to it nearest `reference`'s: the frames turn by the
    /// whole number of semitones between the two. Binning on an offset a
    /// whole `k` semitones lower is exactly the same profile turned by `k`
    /// bins, so nothing is recomputed. `reference` is typically the sum of
    /// every piece's evidence.
    pub fn into_frames_tuned_to(self, reference: TuningEvidence) -> Vec<Vec<f32>> {
        let semitones = (self.tuning.semitones() - reference.semitones()).round();
        let shift = semitones.rem_euclid(12.0) as usize;
        let mut frames = self.frames;
        if shift != 0 {
            for frame in &mut frames {
                let bins = frame.len().max(1);
                frame.rotate_right(shift % bins);
            }
        }
        frames
    }
}

/// Compute a Harmonic Pitch Class Profile for each frame of a magnitude
/// spectrogram.
///
/// Returns one 12-element, L2-normalized pitch-class vector per input frame —
/// the same shape and normalization as
/// [`extract_chroma_from_spectrogram_with_options`], so the frames are a
/// drop-in chroma source for [`detect_key`] — with the tuning evidence they
/// are binned on. See the module docs for the algorithm.
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
) -> Result<Hpcp, AnalysisError> {
    if spectrogram.is_empty() {
        return Ok(Hpcp::default());
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
    // bind every frame's pitch-class grid to it (see `TuningEvidence`).
    let tuning = tuning_evidence(spectrogram, freq_resolution, fmax);
    let semitones = tuning.semitones();
    let frames = spectrogram
        .iter()
        .map(|frame| frame_to_hpcp(frame, freq_resolution, fmax, semitones))
        .collect();
    Ok(Hpcp { frames, tuning })
}

/// Invoke `visit(freq, energy)` for every spectral peak of `magnitude_frame`.
///
/// A peak is an interior local maximum, refined to a sub-bin frequency and
/// amplitude by quadratic (QIFFT) interpolation and kept only if it falls inside
/// the `[`[CHROMA_FMIN_HZ]`, `fmax`]` band; `energy` is that amplitude squared.
/// This is the single source of peak picking shared by [`tuning_evidence`] (the
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

        // Interpolated peak amplitude, floored at zero against degenerate
        // inputs.
        let amplitude = (mid - 0.25 * (left - right) * offset).max(0.0);
        visit(freq, amplitude * amplitude);
    }
}

/// The [`TuningEvidence`] of every peak of `spectrogram`. Subtracting its
/// offset from each pitch class (in [`frame_to_hpcp`]) re-references the
/// profile to the recording's actual concert pitch instead of assuming 440.
fn tuning_evidence(spectrogram: &[Vec<f32>], freq_resolution: f32, fmax: f32) -> TuningEvidence {
    let mut evidence = TuningEvidence::default();
    for frame in spectrogram {
        for_each_peak(frame, freq_resolution, fmax, |freq, energy| {
            let position = 12.0 * (freq / A4_FREQ).log2() + SEMITONE_OFFSET;
            let deviation = position - position.round(); // nearest-semitone error
            let phase = std::f32::consts::TAU * deviation;
            evidence.re += energy * phase.cos();
            evidence.im += energy * phase.sin();
        });
    }
    evidence
}

/// Map one magnitude frame to a 12-element, L2-normalized HPCP vector, with the
/// pitch-class grid shifted by `tuning` semitones (see [`TuningEvidence`]).
fn frame_to_hpcp(
    magnitude_frame: &[f32],
    freq_resolution: f32,
    fmax: f32,
    tuning: f32,
) -> Vec<f32> {
    let mut hpcp = [0.0f32; 12];

    for_each_peak(magnitude_frame, freq_resolution, fmax, |freq, energy| {
        // Pitch class of the peak on the shared grid (index 0 = C),
        // re-referenced by the estimated `tuning`: the `+ SEMITONE_OFFSET`
        // places A4 at pitch class 9 (matching the key templates) and the
        // `- tuning` shifts the whole grid onto the recording's concert pitch.
        let pitch_class =
            (12.0 * (freq / A4_FREQ).log2() + SEMITONE_OFFSET - tuning).rem_euclid(12.0);

        // The squared-cosine window spans strictly less than one semitone, so
        // only the two bins straddling `pitch_class` can receive energy; every
        // other bin a full 0..12 scan would visit fails the distance test and
        // adds nothing. The two indices are always distinct (`floor ∈ 0..=11`),
        // so no bin is added twice.
        let lo = pitch_class.floor() as usize % 12;
        for bin in [lo, (lo + 1) % 12] {
            let raw = (pitch_class - bin as f32).abs();
            let distance = raw.min(12.0 - raw); // circular pitch-class distance
            if distance < PITCH_WINDOW_SEMITONES {
                let phase = std::f32::consts::FRAC_PI_2 * distance / PITCH_WINDOW_SEMITONES;
                let weight = phase.cos() * phase.cos();
                hpcp[bin] += energy * weight;
            }
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
        assert!(compute_hpcp(&[], SAMPLE_RATE, FFT_SIZE)
            .unwrap()
            .frames
            .is_empty());
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
        let out = compute_hpcp(&spec, SAMPLE_RATE, FFT_SIZE).unwrap().frames;
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
        let hpcp = &compute_hpcp(&spec, SAMPLE_RATE, FFT_SIZE).unwrap().frames[0];
        let top = hpcp
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0;
        assert_eq!(top, 9, "A4 peak resolved to pitch class {top}, not A");

        // Middle C (≈261.63 Hz) must put pitch class 0 (C) on top.
        let spec_c = vec![frame_with_peak(bin_of(261.63))];
        let hpcp_c = &compute_hpcp(&spec_c, SAMPLE_RATE, FFT_SIZE).unwrap().frames[0];
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
        let hpcp = &compute_hpcp(&[frame], SAMPLE_RATE, FFT_SIZE)
            .unwrap()
            .frames[0];
        assert!(
            hpcp.iter().all(|&x| x == 0.0),
            "ramp produced a non-empty profile: {hpcp:?}"
        );
    }

    #[test]
    fn peak_between_bins_splits_across_neighbors() {
        // A peak whose frequency sits a quarter-tone above A splits across
        // A (9) and A#/Bb (10), its two nearest bins. Exercised against
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
            "the peak's two nearest bins are not the two largest: {hpcp:?}"
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
        let tuning = tuning_evidence(&[frame.clone()], freq_resolution, fmax).semitones();
        assert!(
            (tuning - detune).abs() < 0.05,
            "tuning estimate {tuning} did not recover the +{detune} semitone detune"
        );
        let hpcp = &compute_hpcp(&[frame], SAMPLE_RATE, FFT_SIZE)
            .unwrap()
            .frames[0];
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
        let tuning = tuning_evidence(&[frame], freq_resolution, fmax).semitones();
        assert!(
            tuning.abs() < 0.05,
            "in-tune signal produced tuning {tuning}"
        );
    }

    /// A frame holding four octaves of C, detuned by `detune` semitones.
    fn detuned_c_frame(detune: f32) -> Vec<f32> {
        let note = 261.63 * 2.0f32.powf(detune / 12.0);
        let mut frame = vec![0.0f32; FFT_SIZE / 2 + 1];
        for octave in 0..4 {
            let b = bin_of(note * 2.0f32.powi(octave));
            frame[b - 1] += 0.5;
            frame[b] += 1.0;
            frame[b + 1] += 0.5;
        }
        frame
    }

    fn top_bin(frame: &[f32]) -> usize {
        frame
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap()
            .0
    }

    #[test]
    fn pieces_tuned_either_side_of_the_wrap_meet_on_one_grid() {
        // Two pieces of one recording about a quarter-tone sharp of C: one
        // estimates +0.4 and bins its note on C, the other estimates −0.4
        // (+0.6) and bins the same note on C#. Tuned to their summed evidence,
        // which the louder first piece pulls just above +0.4, both land on the
        // same pitch class.
        let first = compute_hpcp(&vec![detuned_c_frame(0.4); 2], SAMPLE_RATE, FFT_SIZE).unwrap();
        let second = compute_hpcp(&[detuned_c_frame(0.6)], SAMPLE_RATE, FFT_SIZE).unwrap();
        assert_eq!(top_bin(&first.frames[0]), 0);
        assert_eq!(top_bin(&second.frames[0]), 1);

        let track = first.tuning + second.tuning;
        let first = first.into_frames_tuned_to(track);
        let second = second.into_frames_tuned_to(track);
        assert_eq!(top_bin(&first[0]), 0);
        assert_eq!(top_bin(&second[0]), 0, "{:?}", second[0]);
    }

    #[test]
    fn a_piece_tuned_like_the_whole_is_unchanged() {
        let hpcp = compute_hpcp(&[detuned_c_frame(0.2)], SAMPLE_RATE, FFT_SIZE).unwrap();
        let tuning = hpcp.tuning;
        assert_eq!(hpcp.clone().into_frames_tuned_to(tuning), hpcp.frames);
    }
}
