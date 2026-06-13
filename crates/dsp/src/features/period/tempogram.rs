// SPDX-License-Identifier: MIT OR Apache-2.0
//! Tempogram-based BPM detection (main entry point)
//!
//! Combines FFT and autocorrelation tempogram methods for maximum accuracy.
//! Compares both methods and selects the best estimate or uses ensemble approach.
//!
//! # Reference
//!
//! Grosche, P., Müller, M., & Serrà, J. (2012). Robust Local Features for Remote Folk Music Identification.
//! *IEEE Transactions on Audio, Speech, and Language Processing*.
//!
//! # Algorithm
//!
//! 1. Extract novelty curve from magnitude spectrogram
//! 2. Compute FFT tempogram (frequency-domain analysis)
//! 3. Compute autocorrelation tempogram (direct hypothesis testing)
//! 4. Compare results and select best estimate or use ensemble
//!
//! # Example
//!
//! ```ignore
//! use stratum_dsp::features::period::tempogram::estimate_bpm_tempogram;
//!
//! let magnitude_spec_frames = vec![vec![0.0f32; 1024]; 100];
//! let result = estimate_bpm_tempogram(&magnitude_spec_frames, 44100, 512, 40.0, 240.0, 0.5)?;
//! println!("BPM: {:.2} (confidence: {:.3})", result.bpm, result.confidence);
//! # Ok::<(), stratum_dsp::AnalysisError>(())
//! ```
//!
//! ## Sustain note
//!
//! Vendored from `stratum-dsp@5f4b416`, trimmed to the **full-band** path.
//! Upstream `estimate_bpm_tempogram_impl` carried optional multi-band-fusion
//! and log-mel-novelty candidate generators behind a `TempogramBandFusionConfig`;
//! Sustain only ever called the plain `estimate_bpm_tempogram` entry point
//! (which passed `None`), so this copy folds out that machinery and the
//! diagnostics-candidate buffer the band-fusion variants returned. The
//! full-band scoring — superflux+energy+HFC novelty → FFT + autocorrelation
//! tempograms → tempo-folded candidate scoring with the metrical prior and the
//! >180 BPM octave-fold correction — is unchanged.

use super::novelty::{combined_novelty, energy_flux_novelty, hfc_novelty, superflux_novelty};
use super::tempogram_autocorr::{autocorrelation_tempogram, find_best_bpm_autocorr};
use super::tempogram_fft::{fft_tempogram, find_best_bpm_fft};
use super::BpmEstimate;
use crate::error::AnalysisError;

/// SuperFlux max-filter neighborhood radius (bins), upstream's full-band default.
const SUPERFLUX_MAX_FILTER_BINS: usize = 4;

/// Estimate BPM using tempogram approach (dual method: FFT + Autocorrelation)
///
/// This is the main entry point for tempogram-based BPM detection. It combines
/// both FFT and autocorrelation tempogram methods for maximum accuracy.
///
/// # Reference
///
/// Grosche, P., Müller, M., & Serrà, J. (2012). Robust Local Features for Remote Folk Music Identification.
/// *IEEE Transactions on Audio, Speech, and Language Processing*.
///
/// # Arguments
///
/// * `magnitude_spec_frames` - FFT magnitude spectrogram (n_frames × n_bins)
/// * `sample_rate` - Sample rate in Hz
/// * `hop_size` - Hop size used for STFT (samples per frame)
/// * `min_bpm` - Minimum BPM to consider (default: 40.0)
/// * `max_bpm` - Maximum BPM to consider (default: 240.0)
/// * `bpm_resolution` - BPM resolution for autocorrelation tempogram (default: 0.5)
///
/// # Returns
///
/// Best BPM estimate with confidence and method agreement
///
/// # Errors
///
/// Returns `AnalysisError` if:
/// - Spectrogram is empty
/// - Invalid parameters
/// - Tempogram computation fails
///
/// # Strategy
///
/// 1. **Extract novelty curve**: Combine spectral flux, energy flux, and HFC
/// 2. **FFT tempogram**: Fast frequency-domain analysis (more consistent)
/// 3. **Autocorrelation tempogram**: Direct hypothesis testing (better resolution)
/// 4. **Selection logic**: tempo-folded candidate scoring with a mild metrical
///    prior toward common DJ tempo ranges and a >180 BPM octave-fold correction.
pub fn estimate_bpm_tempogram(
    magnitude_spec_frames: &[Vec<f32>],
    sample_rate: u32,
    hop_size: u32,
    min_bpm: f32,
    max_bpm: f32,
    bpm_resolution: f32,
) -> Result<BpmEstimate, AnalysisError> {
    log::debug!(
        "Estimating BPM using tempogram: {} frames, sample_rate={}, hop_size={}, BPM range=[{:.1}, {:.1}]",
        magnitude_spec_frames.len(),
        sample_rate,
        hop_size,
        min_bpm,
        max_bpm
    );

    // --- Step 1: full-band novelty extraction ---
    let n_bins = magnitude_spec_frames.first().map(|f| f.len()).unwrap_or(0);
    if n_bins == 0 {
        return Err(AnalysisError::InvalidInput(
            "Empty magnitude frames".to_string(),
        ));
    }

    let spectral_full = superflux_novelty(magnitude_spec_frames, SUPERFLUX_MAX_FILTER_BINS)?;
    let energy_full = energy_flux_novelty(magnitude_spec_frames)?;
    let hfc_full = hfc_novelty(magnitude_spec_frames, sample_rate)?;
    let novelty_full = combined_novelty(&spectral_full, &energy_full, &hfc_full);
    if novelty_full.is_empty() {
        return Err(AnalysisError::ProcessingError(
            "Novelty curve is empty after extraction".to_string(),
        ));
    }

    let fft_full = fft_tempogram(&novelty_full, sample_rate, hop_size, min_bpm, max_bpm)?;
    let autocorr_full = autocorrelation_tempogram(
        &novelty_full,
        sample_rate,
        hop_size,
        min_bpm,
        max_bpm,
        bpm_resolution,
    )?;

    let fft_best = find_best_bpm_fft(&fft_full);
    let autocorr_best = find_best_bpm_autocorr(&autocorr_full);

    let max_fft = fft_full.first().map(|(_, p)| *p).unwrap_or(1.0).max(1e-12);
    let max_autocorr = autocorr_full
        .first()
        .map(|(_, s)| *s)
        .unwrap_or(1.0)
        .max(1e-12);

    // Step 4: Metrical-level selection (tempo folding) + scoring
    //
    // Empirically, the dominant failure mode is tempo octave / metrical-level selection (e.g., 2×).
    // Instead of preferring the higher BPM when harmonic relationships exist, we explicitly score
    // tempo-related candidates {T, T/2, 2T, T/3, 3T, 2T/3, 3T/2} using both tempograms and a
    // mild prior toward common DJ/tempo ranges (60–180 BPM) while preserving support for 40–240.
    let (fft_primary_bpm, fft_primary_conf) = fft_best
        .map(|r| (r.bpm, r.confidence))
        .unwrap_or((0.0, 0.0));
    let (autocorr_primary_bpm, autocorr_primary_conf) = autocorr_best
        .map(|r| (r.bpm, r.confidence))
        .unwrap_or((0.0, 0.0));

    // "Empty" guard: if both representations are empty, we failed.
    if fft_full.is_empty() && autocorr_full.is_empty() {
        return Err(AnalysisError::ProcessingError(
            "Both FFT and autocorrelation tempograms are empty".to_string(),
        ));
    }

    fn lookup_nearest(tempogram: &[(f32, f32)], bpm: f32, tol_bpm: f32) -> f32 {
        let mut best_d = f32::INFINITY;
        let mut best_v = 0.0f32;
        for (b, v) in tempogram.iter() {
            let d = (*b - bpm).abs();
            if d <= tol_bpm && d < best_d {
                best_d = d;
                best_v = *v;
            }
        }
        best_v
    }

    fn push_candidate(cands: &mut Vec<f32>, bpm: f32, min_bpm: f32, max_bpm: f32) {
        if bpm.is_finite() && bpm >= min_bpm && bpm <= max_bpm {
            cands.push(bpm);
        }
    }

    // Seed candidates: top-N peaks from both methods + primary picks
    let mut seed_bpms: Vec<f32> = Vec::new();
    seed_bpms.extend(fft_full.iter().take(8).map(|(b, _)| *b));
    seed_bpms.extend(autocorr_full.iter().take(8).map(|(b, _)| *b));
    if fft_primary_bpm > 0.0 {
        seed_bpms.push(fft_primary_bpm);
    }
    if autocorr_primary_bpm > 0.0 {
        seed_bpms.push(autocorr_primary_bpm);
    }

    // Tempo folding factors to resolve metrical-level ambiguity
    const FACTORS: [f32; 7] = [1.0, 0.5, 2.0, 1.0 / 3.0, 3.0, 2.0 / 3.0, 3.0 / 2.0];

    let mut candidates: Vec<f32> = Vec::new();
    for base in seed_bpms {
        for f in FACTORS {
            push_candidate(&mut candidates, base * f, min_bpm, max_bpm);
        }
    }

    // Deduplicate by clustering within ~0.75 BPM
    candidates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut uniq: Vec<f32> = Vec::new();
    for bpm in candidates {
        if let Some(last) = uniq.last().copied() {
            if (bpm - last).abs() < 0.75 {
                continue;
            }
        }
        uniq.push(bpm);
    }

    // Preferable (but not exclusive) tempo range.
    //
    // Note: FMA ground truth includes many >180 BPM tracks; keep the prior mild.
    const PREFERRED_MIN: f32 = 60.0;
    const PREFERRED_MAX: f32 = 180.0;

    #[derive(Debug, Clone)]
    struct ScoredCandidate {
        bpm: f32,
        score: f32,
        fft_norm: f32,
        autocorr_norm: f32,
    }

    let mut scored: Vec<ScoredCandidate> = Vec::with_capacity(uniq.len());
    for bpm in uniq {
        // Use a nearest-neighbor lookup (not max-over-window) to avoid score plateaus where
        // adjacent BPMs all inherit the same peak strength (which collapses confidence to 0.0).
        let fft_val = lookup_nearest(&fft_full, bpm, 0.75);
        let autocorr_val = lookup_nearest(&autocorr_full, bpm, bpm_resolution.max(0.5));
        let fft_norm = (fft_val / max_fft).clamp(0.0, 1.0);
        let autocorr_norm = (autocorr_val / max_autocorr).clamp(0.0, 1.0);

        // Weighted combination:
        // Autocorr gives finer BPM resolution and tends to be strong on rhythmic material,
        // while FFT provides global robustness. Use a balanced blend.
        let mut score = 0.55 * autocorr_norm + 0.45 * fft_norm;

        // Mild metrical prior: penalize extreme tempos unless strongly supported
        if bpm > PREFERRED_MAX {
            score *= 0.80;
        } else if bpm < PREFERRED_MIN {
            score *= 0.90;
        }

        scored.push(ScoredCandidate {
            bpm,
            score,
            fft_norm,
            autocorr_norm,
        });
    }

    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut best = scored.first().cloned().ok_or_else(|| {
        AnalysisError::ProcessingError("No BPM candidates could be scored".to_string())
    })?;

    // Tempo-octave correction (DJ-centric metrical-level selection):
    // If the best candidate is above the preferred range (>180), check whether its /2 fold
    // is competitively scored. If so, prefer the folded value.
    //
    // This directly targets the dominant empirical error mode: ~2× predictions.
    if best.bpm > PREFERRED_MAX {
        let folded = best.bpm / 2.0;
        if folded >= min_bpm && folded <= max_bpm {
            if let Some(folded_scored) = scored
                .iter()
                .find(|c| (c.bpm - folded).abs() < 0.75)
                .cloned()
            {
                // Default behavior: tempo-octave ambiguity is common above ~180.
                // Keep the high BPM only if it is substantially stronger in *both* representations.
                let eps = 1e-6;
                let autocorr_ratio =
                    (best.autocorr_norm + eps) / (folded_scored.autocorr_norm + eps);
                let fft_ratio = (best.fft_norm + eps) / (folded_scored.fft_norm + eps);

                // Fold unless high BPM is convincingly stronger.
                if !(autocorr_ratio > 2.0 && fft_ratio > 2.0) {
                    log::debug!(
                        "Tempo-octave fold applied: {:.2} -> {:.2} (scores: high={:.3}, half={:.3}, ratios: autocorr={:.2}, fft={:.2})",
                        best.bpm,
                        folded_scored.bpm,
                        best.score,
                        folded_scored.score,
                        autocorr_ratio,
                        fft_ratio
                    );
                    best = folded_scored;
                }
            }
        }
    }

    let second = scored.get(1).cloned();

    // Confidence from separation between best and second-best scores
    let confidence = if best.score > 1e-12 {
        let second_score = second.as_ref().map(|s| s.score).unwrap_or(0.0);
        ((best.score - second_score).max(0.0) / best.score).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Method agreement: count methods whose primary pick matches the chosen BPM
    let mut method_agreement = 0u32;
    if fft_primary_bpm > 0.0 && (fft_primary_bpm - best.bpm).abs() < 2.0 {
        method_agreement += 1;
    }
    if autocorr_primary_bpm > 0.0 && (autocorr_primary_bpm - best.bpm).abs() < 2.0 {
        method_agreement += 1;
    }

    log::debug!(
        "Tempogram metrical selection: chosen {:.2} BPM (score={:.3}, conf={:.3}, fft_norm={:.3}, autocorr_norm={:.3}), primary FFT={:.2} (conf={:.3}), primary Autocorr={:.2} (conf={:.3})",
        best.bpm,
        best.score,
        confidence,
        best.fft_norm,
        best.autocorr_norm,
        fft_primary_bpm,
        fft_primary_conf,
        autocorr_primary_bpm,
        autocorr_primary_conf
    );

    Ok(BpmEstimate {
        bpm: best.bpm,
        confidence,
        method_agreement,
    })
}

#[cfg(test)]
#[allow(clippy::needless_range_loop)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_bpm_tempogram_basic() {
        // Create spectrogram with periodic pattern
        let mut spectrogram = vec![vec![0.1f32; 1024]; 500];

        // Add periodic pattern (simulating 120 BPM)
        let period = 43; // Approximate for 120 BPM at 44.1kHz, 512 hop
        for (i, frame) in spectrogram.iter_mut().enumerate() {
            if i % period == 0 {
                for bin in 0..512 {
                    frame[bin] = 1.0;
                }
            }
        }

        let result = estimate_bpm_tempogram(&spectrogram, 44100, 512, 100.0, 140.0, 0.5).unwrap();

        // Should find BPM around 120
        assert!(
            result.bpm >= 115.0 && result.bpm <= 125.0,
            "Expected BPM around 120, got {:.1}",
            result.bpm
        );
        assert!(result.confidence >= 0.0 && result.confidence <= 1.0);
    }

    #[test]
    fn test_estimate_bpm_tempogram_empty() {
        let spectrogram = vec![];
        let result = estimate_bpm_tempogram(&spectrogram, 44100, 512, 40.0, 240.0, 0.5);
        assert!(result.is_err());
    }

    #[test]
    fn test_estimate_bpm_tempogram_agreement() {
        // This test would require mocking the tempogram methods to ensure agreement
        // For now, we test that the function runs without error
        let spectrogram = vec![vec![0.5f32; 1024]; 200];
        let result = estimate_bpm_tempogram(&spectrogram, 44100, 512, 40.0, 240.0, 0.5);

        // Should either succeed or fail gracefully
        match result {
            Ok(est) => {
                assert!(est.bpm >= 40.0 && est.bpm <= 240.0);
                assert!(est.confidence >= 0.0 && est.confidence <= 1.0);
            }
            Err(_) => {
                // Failure is acceptable for random input
            }
        }
    }
}
