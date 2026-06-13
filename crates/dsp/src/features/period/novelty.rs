// SPDX-License-Identifier: MIT OR Apache-2.0
//! Novelty curve extraction for tempogram analysis
//!
//! Extracts novelty curves from magnitude spectrograms using multiple complementary methods:
//! - Spectral flux: Detects spectral changes (harmonic onsets)
//! - Energy flux: Detects energy changes (percussive onsets)
//! - High-frequency content (HFC): Detects high-frequency attacks
//!
//! These curves are combined with weighted voting to create a robust novelty curve
//! for tempogram-based BPM detection.
//!
//! # Reference
//!
//! Grosche, P., Müller, M., & Serrà, J. (2012). Robust Local Features for Remote Folk Music Identification.
//! *IEEE Transactions on Audio, Speech, and Language Processing*.
//!
//! Klapuri, A., Eronen, A., & Astola, J. (2006). Analysis of the Meter of Audio Signals.
//! *IEEE Transactions on Audio, Speech, and Language Processing*, 14(1), 342-355.
//!
//! # Example
//!
//! ```ignore
//! use stratum_dsp::features::period::novelty::{superflux_novelty, energy_flux_novelty, hfc_novelty, combined_novelty};
//!
//! // magnitude_spec_frames is a spectrogram: Vec<Vec<f32>> where each inner Vec is a frame
//! let magnitude_spec_frames = vec![vec![0.0f32; 1024]; 100];
//!
//! let spectral = superflux_novelty(&magnitude_spec_frames, 4)?;
//! let energy = energy_flux_novelty(&magnitude_spec_frames)?;
//! let hfc = hfc_novelty(&magnitude_spec_frames, 44100)?;
//! let combined = combined_novelty(&spectral, &energy, &hfc);
//! # Ok::<(), stratum_dsp::AnalysisError>(())
//! ```

use crate::error::AnalysisError;

/// Numerical stability epsilon
const EPSILON: f32 = 1e-10;

fn validate_spectrogram(magnitude_spec_frames: &[Vec<f32>]) -> Result<usize, AnalysisError> {
    if magnitude_spec_frames.is_empty() || magnitude_spec_frames.len() < 2 {
        return Ok(0);
    }
    let n_bins = magnitude_spec_frames[0].len();
    if n_bins == 0 {
        return Err(AnalysisError::InvalidInput(
            "Empty magnitude frames".to_string(),
        ));
    }
    for (i, frame) in magnitude_spec_frames.iter().enumerate() {
        if frame.len() != n_bins {
            return Err(AnalysisError::InvalidInput(format!(
                "Inconsistent frame lengths: frame 0 has {} bins, frame {} has {} bins",
                n_bins,
                i,
                frame.len()
            )));
        }
    }
    Ok(n_bins)
}

/// Extract SuperFlux novelty curve from magnitude spectrogram.
///
/// SuperFlux (Böck & Widmer, 2013) improves over plain spectral flux by using a
/// max-filtered reference (here: along frequency on the *previous* frame) before
/// differencing. This reduces false positives from vibrato/pitch modulation and
/// tends to yield a cleaner onset-strength function for rhythmic analysis.
///
/// This implementation is a lightweight variant suitable for our tempo pipeline:
/// - Log-compress magnitudes (`log1p`)
/// - For each bin, subtract the max in a small frequency neighborhood of the previous frame
/// - Half-wave rectify and sum energy
///
/// # Arguments
///
/// * `magnitude_spec_frames` - FFT magnitude spectrogram (n_frames × n_bins)
/// * `max_filter_bins` - Neighborhood radius in bins for max filtering (typical: 3–8)
///
/// # Returns
///
/// Novelty curve as `Vec<f32>` with length `n_frames - 1`, normalized to [0, 1]
pub fn superflux_novelty(
    magnitude_spec_frames: &[Vec<f32>],
    max_filter_bins: usize,
) -> Result<Vec<f32>, AnalysisError> {
    if magnitude_spec_frames.is_empty() || magnitude_spec_frames.len() < 2 {
        return Ok(Vec::new());
    }

    let n_bins = validate_spectrogram(magnitude_spec_frames)?;
    if n_bins == 0 {
        return Ok(Vec::new());
    }

    let k = max_filter_bins.max(1);

    // Precompute log-compressed frames
    let mut log_frames: Vec<Vec<f32>> = Vec::with_capacity(magnitude_spec_frames.len());
    for frame in magnitude_spec_frames {
        let lf: Vec<f32> = frame.iter().map(|&x| (1.0 + x.max(0.0)).ln()).collect();
        log_frames.push(lf);
    }

    let mut flux = Vec::with_capacity(log_frames.len().saturating_sub(1));
    for i in 1..log_frames.len() {
        let prev = &log_frames[i - 1];
        let curr = &log_frames[i];

        let mut sum = 0.0f32;
        #[allow(clippy::needless_range_loop)]
        for b in 0..n_bins {
            let start = b.saturating_sub(k);
            let end = (b + k + 1).min(n_bins);
            let mut prev_max = 0.0f32;
            for v in &prev[start..end] {
                prev_max = prev_max.max(*v);
            }
            let diff = (curr[b] - prev_max).max(0.0);
            sum += diff * diff;
        }
        flux.push(sum.sqrt());
    }

    if flux.is_empty() {
        return Ok(Vec::new());
    }
    let max_flux = flux.iter().copied().fold(0.0f32, f32::max);
    if max_flux > EPSILON {
        for v in &mut flux {
            *v /= max_flux;
        }
    }
    Ok(flux)
}

/// Extract energy flux novelty curve from magnitude spectrogram
///
/// Computes frame-to-frame energy changes by summing magnitude across all
/// frequency bins. This method is particularly effective for detecting
/// percussive onsets and energy-based changes.
///
/// # Reference
///
/// Bello, J. P., Daudet, L., Abdallah, S., Duxbury, C., Davies, M., & Sandler, M. B. (2005).
/// A Tutorial on Onset Detection in Music Signals.
/// *IEEE Transactions on Speech and Audio Processing*, 13(5), 1035-1047.
///
/// # Arguments
///
/// * `magnitude_spec_frames` - FFT magnitude spectrogram (n_frames × n_bins)
///
/// # Returns
///
/// Novelty curve as `Vec<f32>` with length `n_frames - 1` (one value per frame transition)
/// Values are normalized to [0, 1] range
pub fn energy_flux_novelty(magnitude_spec_frames: &[Vec<f32>]) -> Result<Vec<f32>, AnalysisError> {
    if magnitude_spec_frames.is_empty() {
        return Ok(Vec::new());
    }

    if magnitude_spec_frames.len() < 2 {
        return Ok(Vec::new());
    }

    // Check that all frames have the same length
    let n_bins = magnitude_spec_frames[0].len();
    if n_bins == 0 {
        return Err(AnalysisError::InvalidInput(
            "Empty magnitude frames".to_string(),
        ));
    }

    for (i, frame) in magnitude_spec_frames.iter().enumerate() {
        if frame.len() != n_bins {
            return Err(AnalysisError::InvalidInput(format!(
                "Inconsistent frame lengths: frame 0 has {} bins, frame {} has {} bins",
                n_bins,
                i,
                frame.len()
            )));
        }
    }

    log::debug!(
        "Computing energy flux novelty: {} frames, {} bins per frame",
        magnitude_spec_frames.len(),
        n_bins
    );

    // Compute energy per frame (sum of squared magnitudes)
    let energies: Vec<f32> = magnitude_spec_frames
        .iter()
        .map(|frame| frame.iter().map(|&x| x * x).sum())
        .collect();

    // Compute energy flux (positive differences only)
    let mut flux = Vec::with_capacity(energies.len().saturating_sub(1));

    for i in 1..energies.len() {
        let diff = energies[i] - energies[i - 1];
        // Only positive differences (energy increases = onsets)
        flux.push(diff.max(0.0));
    }

    if flux.is_empty() {
        return Ok(Vec::new());
    }

    // Normalize to [0, 1]
    let max_flux = flux.iter().copied().fold(0.0f32, f32::max);
    if max_flux > EPSILON {
        for val in &mut flux {
            *val /= max_flux;
        }
    }

    log::debug!(
        "Energy flux novelty: {} values, max={:.6}",
        flux.len(),
        max_flux
    );

    Ok(flux)
}

/// Extract high-frequency content (HFC) novelty curve from magnitude spectrogram
///
/// Computes weighted sum emphasizing high frequencies, making this method
/// particularly effective for detecting percussive attacks and sharp transients.
///
/// # Reference
///
/// Bello, J. P., Daudet, L., Abdallah, S., Duxbury, C., Davies, M., & Sandler, M. B. (2005).
/// A Tutorial on Onset Detection in Music Signals.
/// *IEEE Transactions on Speech and Audio Processing*, 13(5), 1035-1047.
///
/// # Arguments
///
/// * `magnitude_spec_frames` - FFT magnitude spectrogram (n_frames × n_bins)
/// * `sample_rate` - Sample rate in Hz (used for frequency bin calculation)
///
/// # Returns
///
/// Novelty curve as `Vec<f32>` with length `n_frames - 1` (one value per frame transition)
/// Values are normalized to [0, 1] range
pub fn hfc_novelty(
    magnitude_spec_frames: &[Vec<f32>],
    sample_rate: u32,
) -> Result<Vec<f32>, AnalysisError> {
    if magnitude_spec_frames.is_empty() {
        return Ok(Vec::new());
    }

    if sample_rate == 0 {
        return Err(AnalysisError::InvalidInput(
            "Sample rate must be > 0".to_string(),
        ));
    }

    if magnitude_spec_frames.len() < 2 {
        return Ok(Vec::new());
    }

    // Check that all frames have the same length
    let n_bins = magnitude_spec_frames[0].len();
    if n_bins == 0 {
        return Err(AnalysisError::InvalidInput(
            "Empty magnitude frames".to_string(),
        ));
    }

    for (i, frame) in magnitude_spec_frames.iter().enumerate() {
        if frame.len() != n_bins {
            return Err(AnalysisError::InvalidInput(format!(
                "Inconsistent frame lengths: frame 0 has {} bins, frame {} has {} bins",
                n_bins,
                i,
                frame.len()
            )));
        }
    }

    log::debug!(
        "Computing HFC novelty: {} frames, {} bins per frame, sample_rate={}",
        magnitude_spec_frames.len(),
        n_bins,
        sample_rate
    );

    // Compute HFC per frame: sum over k (k * |X[k]|^2)
    // where k is the frequency bin index (higher bins = higher frequencies)
    let hfc_values: Vec<f32> = magnitude_spec_frames
        .iter()
        .map(|frame| {
            frame
                .iter()
                .enumerate()
                .map(|(k, &mag)| (k as f32) * mag * mag)
                .sum()
        })
        .collect();

    // Compute HFC flux (positive differences only)
    let mut flux = Vec::with_capacity(hfc_values.len().saturating_sub(1));

    for i in 1..hfc_values.len() {
        let diff = hfc_values[i] - hfc_values[i - 1];
        // Only positive differences (HFC increases = high-frequency onsets)
        flux.push(diff.max(0.0));
    }

    if flux.is_empty() {
        return Ok(Vec::new());
    }

    // Normalize to [0, 1]
    let max_flux = flux.iter().copied().fold(0.0f32, f32::max);
    if max_flux > EPSILON {
        for val in &mut flux {
            *val /= max_flux;
        }
    }

    log::debug!("HFC novelty: {} values, max={:.6}", flux.len(), max_flux);

    Ok(flux)
}

/// Combine multiple novelty curves with weighted voting
///
/// Combines spectral flux, energy flux, and HFC novelty curves into a single
/// robust novelty curve. Each method captures different aspects of onsets:
/// - Spectral flux: Harmonic changes
/// - Energy flux: Energy changes
/// - HFC: High-frequency attacks
///
/// Consensus voting makes the combined curve more reliable than any single method.
///
/// # Arguments
///
/// * `spectral` - Spectral flux novelty curve
/// * `energy` - Energy flux novelty curve
/// * `hfc` - HFC novelty curve
///
/// # Returns
///
/// Combined novelty curve as `Vec<f32>`, normalized to [0, 1] range
/// Length matches the shortest input curve
///
/// # Weights
///
/// Default weights (can be adjusted based on empirical performance):
/// - Spectral flux: 0.5 (most important for BPM detection per Klapuri et al. 2006)
/// - Energy flux: 0.3
/// - HFC: 0.2
pub fn combined_novelty(spectral: &[f32], energy: &[f32], hfc: &[f32]) -> Vec<f32> {
    combined_novelty_with_params(spectral, energy, hfc, 0.5, 0.3, 0.2, 16, 5)
}

/// Combine multiple novelty curves with configurable weights and conditioning.
///
/// This is a tuning hook: novelty weighting and conditioning strongly affects whether the
/// novelty emphasizes the beat-level pulse vs subdivisions (hi-hats) vs harmonic motion.
#[allow(clippy::too_many_arguments)]
pub fn combined_novelty_with_params(
    spectral: &[f32],
    energy: &[f32],
    hfc: &[f32],
    w_spectral: f32,
    w_energy: f32,
    w_hfc: f32,
    local_mean_window: usize,
    smooth_window: usize,
) -> Vec<f32> {
    // Find minimum length (all curves should be same length, but be safe)
    let min_len = spectral.len().min(energy.len()).min(hfc.len());

    if min_len == 0 {
        return Vec::new();
    }

    let ws = w_spectral.max(0.0);
    let we = w_energy.max(0.0);
    let wh = w_hfc.max(0.0);
    let wsum = (ws + we + wh).max(EPSILON);

    let mut combined = Vec::with_capacity(min_len);
    for i in 0..min_len {
        let spectral_val = spectral.get(i).copied().unwrap_or(0.0);
        let energy_val = energy.get(i).copied().unwrap_or(0.0);
        let hfc_val = hfc.get(i).copied().unwrap_or(0.0);

        // Weighted average
        let weighted_sum = (spectral_val * ws + energy_val * we + hfc_val * wh) / wsum;
        combined.push(weighted_sum);
    }

    // Normalize to [0, 1] (should already be normalized, but ensure it)
    normalize_in_place(&mut combined);

    // Conditioning:
    // - local mean subtraction is effectively a high-pass in time, with half-wave rectification
    // - smoothing stabilizes the novelty and can reduce spurious sub-beat transients
    if local_mean_window > 1 {
        combined = local_mean_subtract(&combined, local_mean_window);
    }
    if smooth_window > 1 {
        smooth_moving_average_in_place(&mut combined, smooth_window);
    }
    normalize_in_place(&mut combined);

    log::debug!(
        "Combined novelty (conditioned): {} values (w=[{:.2},{:.2},{:.2}], local_mean={}, smooth={})",
        combined.len(),
        ws,
        we,
        wh,
        local_mean_window,
        smooth_window
    );

    combined
}

/// Normalize a curve in-place to [0, 1] by dividing by its maximum.
fn normalize_in_place(curve: &mut [f32]) {
    let max_val = curve.iter().copied().fold(0.0f32, f32::max);
    if max_val > EPSILON {
        for v in curve {
            *v /= max_val;
        }
    }
}

/// Subtract a moving-average local mean and half-wave rectify.
///
/// Output: `max(0, x[i] - local_mean[i])`.
fn local_mean_subtract(x: &[f32], window: usize) -> Vec<f32> {
    if x.is_empty() || window == 0 {
        return x.to_vec();
    }
    let w = window.max(1);
    let half = w / 2;
    let mut out = vec![0.0f32; x.len()];

    for i in 0..x.len() {
        let start = i.saturating_sub(half);
        let end = (i + half + 1).min(x.len());
        let mut sum = 0.0f32;
        for v in &x[start..end] {
            sum += *v;
        }
        let mean = sum / (end - start) as f32;
        out[i] = (x[i] - mean).max(0.0);
    }

    out
}

/// Simple moving-average smoothing in-place.
fn smooth_moving_average_in_place(x: &mut [f32], window: usize) {
    if x.len() < 3 || window <= 1 {
        return;
    }
    let w = window.max(1);
    let half = w / 2;
    let orig = x.to_vec();
    for i in 0..x.len() {
        let start = i.saturating_sub(half);
        let end = (i + half + 1).min(x.len());
        let mut sum = 0.0f32;
        for v in &orig[start..end] {
            sum += *v;
        }
        x[i] = sum / (end - start) as f32;
    }
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    #[test]
    fn test_energy_flux_novelty_basic() {
        let mut spectrogram = vec![vec![0.1f32; 1024]; 10];

        // Frame 5: higher energy
        for bin in 0..1024 {
            spectrogram[5][bin] = 1.0f32;
        }

        let novelty = energy_flux_novelty(&spectrogram).unwrap();

        assert_eq!(novelty.len(), 9);
        // Should detect energy increase at frame 5
        assert!(novelty[4] > 0.0 || novelty[5] > 0.0);
    }

    #[test]
    fn test_hfc_novelty_basic() {
        let mut spectrogram = vec![vec![0.1f32; 1024]; 10];

        // Frame 5: high-frequency content
        for bin in 512..1024 {
            spectrogram[5][bin] = 1.0f32;
        }

        let novelty = hfc_novelty(&spectrogram, 44100).unwrap();

        assert_eq!(novelty.len(), 9);
        // Should detect HFC increase at frame 5
        assert!(novelty[4] > 0.0 || novelty[5] > 0.0);
    }

    #[test]
    fn test_combined_novelty() {
        let spectral = vec![0.0, 0.5, 1.0, 0.5, 0.0];
        let energy = vec![0.0, 0.3, 0.8, 0.3, 0.0];
        let hfc = vec![0.0, 0.2, 0.6, 0.2, 0.0];

        let combined = combined_novelty(&spectral, &energy, &hfc);

        assert_eq!(combined.len(), 5);
        // Conditioning can reshape small synthetic examples; just validate normalization/range.
        assert!(combined.iter().all(|&v| (0.0..=1.0).contains(&v)));
        assert!(combined.iter().copied().fold(0.0f32, f32::max) > 0.0);
    }

    #[test]
    fn test_combined_novelty_different_lengths() {
        let spectral = vec![0.0, 0.5, 1.0];
        let energy = vec![0.0, 0.3, 0.8, 0.3];
        let hfc = vec![0.0, 0.2];

        let combined = combined_novelty(&spectral, &energy, &hfc);

        // Should use minimum length (2)
        assert_eq!(combined.len(), 2);
    }
}
