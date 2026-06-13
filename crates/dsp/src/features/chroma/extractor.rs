// SPDX-License-Identifier: MIT OR Apache-2.0
//! Chroma vector extraction
//!
//! Converts FFT magnitude spectrogram to 12-element chroma vectors.
//!
//! Algorithm:
//! 1. Compute STFT (Short-Time Fourier Transform)
//! 2. Convert frequency bins → semitone classes: `semitone = 12 * log2(freq / 440.0) + 57.0`
//! 3. Sum magnitude across octaves for each semitone class
//! 4. Normalize to L2 unit norm
//!
//! # Reference
//!
//! Müller, M., & Ewert, S. (2010). Chroma Toolbox: MATLAB Implementations for Extracting
//! Variants of Chroma-Based Audio Features. *Proceedings of the International Society for
//! Music Information Retrieval Conference*.
//!
//! # Example
//!
//! ```ignore
//! use stratum_dsp::features::chroma::extractor::{compute_stft, extract_chroma_from_spectrogram_with_options};
//!
//! let samples = vec![0.0f32; 44100 * 5]; // 5 seconds of audio
//! let stft = compute_stft(&samples, 2048, 512)?;
//! let chroma_vectors = extract_chroma_from_spectrogram_with_options(&stft, 44100, 2048, true, 0.5)?;
//! // chroma_vectors is Vec<Vec<f32>> where each inner Vec is a 12-element chroma vector
//! # Ok::<(), stratum_dsp::AnalysisError>(())
//! ```
//!
//! ## Sustain note
//!
//! Vendored from `stratum-dsp@5f4b416`, trimmed to the STFT front-end and the
//! straight (non-tuned, non-HPCP) chroma path Sustain's key detector uses:
//! [`compute_stft`] and [`extract_chroma_from_spectrogram_with_options`]
//! (which fans out to the private `frame_to_chroma` / `frame_to_chroma_tuned`).
//! The upstream tuning-estimation, HPCP, log-frequency, beat-synchronous,
//! spectrogram-conditioning, and raw-sample `extract_chroma*` helpers belong to
//! the full `analyze_audio` key pipeline Sustain does not call and are not
//! vendored.

use crate::error::AnalysisError;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;

/// Numerical stability epsilon
const EPSILON: f32 = 1e-10;

/// Reference frequency for semitone calculation (A4)
const A4_FREQ: f32 = 440.0;

/// Semitone offset to map A4 to semitone 57 (middle of piano range)
const SEMITONE_OFFSET: f32 = 57.0;

/// Default band-pass limits for chroma extraction.
///
/// Rationale: Very low frequencies are dominated by bass/kick energy (often weakly pitched),
/// and very high frequencies are dominated by broadband/percussive content. Band-limiting
/// typically improves tonal features like chroma/HPCP in real-world mixes.
const DEFAULT_CHROMA_FMIN_HZ: f32 = 100.0;
const DEFAULT_CHROMA_FMAX_HZ: f32 = 5000.0;

/// Upper edge of the band [`extract_chroma_from_spectrogram_with_options`]
/// reads (Hz); bins above this are ignored. Exposed so a caller that
/// preprocesses the spectrogram for chroma (e.g. HPSS harmonic emphasis) can
/// scope that work to the band chroma consumes instead of the full transform.
pub const CHROMA_FMAX_HZ: f32 = DEFAULT_CHROMA_FMAX_HZ;

/// Compute the magnitude STFT of a mono signal.
///
/// Returns one magnitude spectrum (`frame_size / 2 + 1` real-FFT bins) per
/// frame, Hann-windowed, hopped by `hop_size`. This is the spectral
/// front-end both BPM (tempogram) and key (chroma) detection build on —
/// each calling it at its own frame size, not sharing one transform.
///
/// # Errors
///
/// Returns `AnalysisError::InvalidInput` if `frame_size` or `hop_size` is 0.
pub fn compute_stft(
    samples: &[f32],
    frame_size: usize,
    hop_size: usize,
) -> Result<Vec<Vec<f32>>, AnalysisError> {
    if frame_size == 0 {
        return Err(AnalysisError::InvalidInput(
            "Frame size must be > 0".to_string(),
        ));
    }

    if hop_size == 0 {
        return Err(AnalysisError::InvalidInput(
            "Hop size must be > 0".to_string(),
        ));
    }

    let n_samples = samples.len();

    if n_samples < frame_size {
        // Not enough samples for even one frame
        return Ok(vec![]);
    }

    // Compute number of frames
    let n_frames = (n_samples - frame_size) / hop_size + 1;
    let mut magnitudes = Vec::with_capacity(n_frames);

    // Create Hann window
    let window: Vec<f32> = if frame_size == 1 {
        vec![1.0]
    } else {
        (0..frame_size)
            .map(|i| {
                let x = 2.0 * std::f32::consts::PI * i as f32 / (frame_size - 1) as f32;
                0.5 * (1.0 - x.cos())
            })
            .collect()
    };

    // FFT planner
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(frame_size);
    let mut fft_input = vec![Complex::new(0.0f32, 0.0f32); frame_size];
    let n_bins = frame_size / 2 + 1;

    // Process each frame
    for frame_idx in 0..n_frames {
        let start = frame_idx * hop_size;
        let end = start + frame_size;

        if end > n_samples {
            break;
        }

        // Window the frame into a reusable FFT buffer.
        for ((slot, &sample), &window_value) in fft_input
            .iter_mut()
            .zip(samples[start..end].iter())
            .zip(window.iter())
        {
            *slot = Complex::new(sample * window_value, 0.0);
        }

        // Forward FFT
        fft.process(&mut fft_input);

        // Compute magnitude spectrum (only need first frame_size/2 + 1 bins for real FFT)
        let mut magnitude = Vec::with_capacity(n_bins);
        magnitude.extend(
            fft_input[..n_bins]
                .iter()
                .map(|x| (x.re * x.re + x.im * x.im).sqrt()),
        );

        magnitudes.push(magnitude);
    }

    Ok(magnitudes)
}

/// Convert a single FFT magnitude frame to chroma vector
///
/// Maps frequency bins to semitone classes and sums across octaves.
///
/// # Arguments
///
/// * `magnitude_frame` - FFT magnitude spectrum for one frame
/// * `sample_rate` - Sample rate in Hz
/// * `fft_size` - FFT size (same as frame_size)
/// * `soft_mapping` - Enable soft mapping (spread to neighboring semitones)
/// * `soft_mapping_sigma` - Standard deviation for soft mapping (in semitones)
///
/// # Returns
///
/// 12-element chroma vector (one per semitone class)
fn frame_to_chroma(
    magnitude_frame: &[f32],
    sample_rate: u32,
    fft_size: usize,
    soft_mapping: bool,
    soft_mapping_sigma: f32,
) -> Result<Vec<f32>, AnalysisError> {
    frame_to_chroma_tuned(
        magnitude_frame,
        sample_rate,
        fft_size,
        soft_mapping,
        soft_mapping_sigma,
        0.0,
    )
}

fn frame_to_chroma_tuned(
    magnitude_frame: &[f32],
    sample_rate: u32,
    fft_size: usize,
    soft_mapping: bool,
    soft_mapping_sigma: f32,
    tuning_offset_semitones: f32,
) -> Result<Vec<f32>, AnalysisError> {
    // Initialize chroma vector (12 semitone classes)
    let mut chroma = vec![0.0f32; 12];

    // Frequency resolution: sample_rate / fft_size
    let freq_resolution = sample_rate as f32 / fft_size as f32;

    // Process each frequency bin
    for (bin_idx, &magnitude) in magnitude_frame.iter().enumerate() {
        // Convert bin index to frequency
        let freq = bin_idx as f32 * freq_resolution;

        // Band-limit for tonal/chroma extraction.
        if freq < DEFAULT_CHROMA_FMIN_HZ {
            continue;
        }
        if freq > DEFAULT_CHROMA_FMAX_HZ.min(sample_rate as f32 / 2.0) {
            break;
        }

        // Skip Nyquist and above
        if freq >= sample_rate as f32 / 2.0 {
            break;
        }

        // Convert frequency to semitone (and apply optional tuning compensation).
        // Formula: semitone = 12 * log2(freq / 440.0) + 57.0
        let semitone = 12.0 * (freq / A4_FREQ).log2() + SEMITONE_OFFSET - tuning_offset_semitones;

        // Light magnitude compression to reduce dominance of broadband energy.
        // (Key/chroma benefits from compressing dynamic range in real-world mixes.)
        //
        // Note: We intentionally do **not** apply additional frequency-dependent weighting here.
        // In early real-world tests on DJ tracks, even mild inverse-frequency weighting increased
        // low-frequency bias and reduced key accuracy.
        let magnitude = magnitude.max(0.0).powf(0.6);
        let contrib = magnitude;

        if soft_mapping {
            // Soft mapping: spread magnitude to neighboring semitone classes using a Gaussian kernel
            // in **circular** pitch-class space.
            //
            // NOTE: We must treat distance on the 12-tone circle (wrap-around between 11 and 0).
            // A linear distance would incorrectly zero out energy near the boundary (e.g., B↔C),
            // biasing chroma statistics and downstream key detection.
            let semitone_pc = semitone.rem_euclid(12.0); // [0, 12)
            let primary_pc = semitone_pc.round().rem_euclid(12.0); // [0, 12)
            let primary_class = primary_pc as i32;

            // Compute weights for the nearest pitch class and its immediate neighbors.
            for offset in -1..=1 {
                let target_class = (primary_class + offset).rem_euclid(12);
                let target_pc = target_class as f32; // [0, 11]
                let mut distance = (semitone_pc - target_pc).abs();
                distance = distance.min(12.0 - distance); // circular distance

                // Gaussian weight in semitone units.
                let sigma = soft_mapping_sigma.max(1e-6);
                let weight = (-distance * distance / (2.0 * sigma * sigma)).exp();

                chroma[target_class as usize] += contrib * weight;
            }
        } else {
            // Hard assignment: assign to nearest semitone class
            let semitone_class = (semitone.round() as i32) % 12;
            // Handle negative modulo
            let semitone_class = if semitone_class < 0 {
                semitone_class + 12
            } else {
                semitone_class
            } as usize;

            // Sum magnitude across octaves for this semitone class
            chroma[semitone_class] += contrib;
        }
    }

    // L2 normalize chroma vector
    let norm: f32 = chroma.iter().map(|&x| x * x).sum::<f32>().sqrt();

    if norm > EPSILON {
        for x in &mut chroma {
            *x /= norm;
        }
    }

    Ok(chroma)
}

/// Extract chroma vectors from a precomputed magnitude spectrogram.
///
/// One 12-element, L2-normalized chroma vector per spectrogram frame. This is
/// the entry point Sustain's key detector uses: it takes a spectrogram from
/// [`compute_stft`] (the key detector gives chroma its own larger-frame STFT,
/// distinct from the tempogram's) and maps each frame to a pitch class profile.
///
/// # Arguments
///
/// * `magnitude_spec_frames` - magnitude spectrogram (n_frames × n_bins)
/// * `sample_rate` - Sample rate in Hz
/// * `fft_size` - FFT size the spectrogram was computed with
/// * `soft_mapping` - spread energy to neighboring pitch classes (Gaussian)
/// * `soft_mapping_sigma` - soft-mapping standard deviation in semitones
///
/// # Errors
///
/// Returns `AnalysisError::InvalidInput` if the spectrogram frames are empty
/// or have inconsistent bin counts.
pub fn extract_chroma_from_spectrogram_with_options(
    magnitude_spec_frames: &[Vec<f32>],
    sample_rate: u32,
    fft_size: usize,
    soft_mapping: bool,
    soft_mapping_sigma: f32,
) -> Result<Vec<Vec<f32>>, AnalysisError> {
    if magnitude_spec_frames.is_empty() {
        return Ok(Vec::new());
    }
    let n_bins = magnitude_spec_frames[0].len();
    if n_bins == 0 {
        return Err(AnalysisError::InvalidInput(
            "Empty spectrogram frames".to_string(),
        ));
    }
    for (i, f) in magnitude_spec_frames.iter().enumerate() {
        if f.len() != n_bins {
            return Err(AnalysisError::InvalidInput(format!(
                "Inconsistent spectrogram frame lengths: frame 0 has {} bins, frame {} has {} bins",
                n_bins,
                i,
                f.len()
            )));
        }
    }

    let mut chroma_vectors = Vec::with_capacity(magnitude_spec_frames.len());
    for frame in magnitude_spec_frames {
        chroma_vectors.push(frame_to_chroma(
            frame,
            sample_rate,
            fft_size,
            soft_mapping,
            soft_mapping_sigma,
        )?);
    }
    Ok(chroma_vectors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_to_chroma() {
        let sample_rate = 44100;
        let fft_size = 2048;

        // Create a magnitude spectrum with energy at A4 (440 Hz)
        let mut magnitude = vec![0.0f32; fft_size / 2 + 1];
        let bin_a4 = (440.0 * fft_size as f32 / sample_rate as f32) as usize;
        if bin_a4 < magnitude.len() {
            magnitude[bin_a4] = 1.0;
        }

        let chroma = frame_to_chroma(&magnitude, sample_rate, fft_size, false, 0.5).unwrap();
        assert_eq!(chroma.len(), 12);

        // Check normalization
        let norm: f32 = chroma.iter().map(|&x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 0.01 || norm < EPSILON);
    }

    #[test]
    fn test_compute_stft_rejects_zero_sizes() {
        let samples = vec![0.0f32; 10000];

        assert!(compute_stft(&samples, 0, 512).is_err());
        assert!(compute_stft(&samples, 2048, 0).is_err());
    }

    #[test]
    fn test_compute_stft_single_sample_frame_is_finite() {
        let samples = vec![0.25f32, -0.5, 0.75];

        let frames = compute_stft(&samples, 1, 1).unwrap();

        assert_eq!(frames.len(), samples.len());
        for (frame, expected) in frames.iter().zip(samples.iter()) {
            assert_eq!(frame.len(), 1);
            assert!(frame[0].is_finite());
            assert!((frame[0] - expected.abs()).abs() < 1e-6);
        }
    }

    #[test]
    fn test_extract_chroma_from_spectrogram_basic() {
        // Generate a 2 s sine at A4 (440 Hz), take its STFT, and map to chroma
        // through the public path Sustain uses. A (pitch class 9) must dominate.
        let sample_rate = 44100u32;
        let duration_samples = (sample_rate * 2) as usize;
        let samples: Vec<f32> = (0..duration_samples)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin()
            })
            .collect();

        let stft = compute_stft(&samples, 2048, 512).unwrap();
        let chroma_vectors =
            extract_chroma_from_spectrogram_with_options(&stft, sample_rate, 2048, true, 0.5)
                .unwrap();
        assert!(!chroma_vectors.is_empty());

        for chroma in &chroma_vectors {
            assert_eq!(chroma.len(), 12);
            let norm: f32 = chroma.iter().map(|&x| x * x).sum::<f32>().sqrt();
            assert!((norm - 1.0).abs() < 0.01 || norm < EPSILON);
        }

        let avg_chroma: Vec<f32> = (0..12)
            .map(|i| chroma_vectors.iter().map(|v| v[i]).sum::<f32>() / chroma_vectors.len() as f32)
            .collect();
        assert!(
            avg_chroma[9] > 0.1,
            "A semitone class should be prominent for an A4 tone"
        );
    }

    #[test]
    fn test_extract_chroma_from_spectrogram_empty() {
        // No frames -> empty result (not an error).
        assert!(
            extract_chroma_from_spectrogram_with_options(&[], 44100, 2048, true, 0.5)
                .unwrap()
                .is_empty()
        );
        // A frame with zero bins is rejected.
        assert!(
            extract_chroma_from_spectrogram_with_options(&[vec![]], 44100, 2048, true, 0.5)
                .is_err()
        );
    }
}
