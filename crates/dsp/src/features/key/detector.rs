// SPDX-License-Identifier: MIT OR Apache-2.0
//! Key detection algorithm
//!
//! Matches chroma distribution against Krumhansl-Kessler templates to detect
//! the musical key of an audio track.
//!
//! # Reference
//!
//! Krumhansl, C. L., & Kessler, E. J. (1982). Tracing the Dynamic Changes in Perceived
//! Tonal Organization in a Spatial Representation of Musical Keys. *Psychological Review*,
//! 89(4), 334-368.

use super::{
    compare_key_scores_desc, sort_key_scores_desc, templates::KeyTemplates, KeyDetectionResult,
};
use crate::analysis::result::Key;
use crate::error::AnalysisError;

/// Detect musical key from chroma vectors
///
/// Aggregates the chroma across all frames into a mean profile and correlates
/// it with each of the 24 Krumhansl-Kessler key templates using the
/// Krumhansl-Schmuckler metric (mean-centered Pearson correlation). The
/// highest-scoring key is selected.
///
/// # Arguments
///
/// * `chroma_vectors` - Vector of 12-element chroma vectors (one per frame)
/// * `templates` - Key templates (Krumhansl-Kessler profiles)
///
/// # Returns
///
/// Key detection result with:
/// - Detected key (major or minor, 0-11)
/// - Confidence score (0.0-1.0)
/// - All 24 key scores (ranked)
///
/// # Errors
///
/// Returns `AnalysisError` if:
/// - Chroma vectors are empty
/// - Chroma vectors have incorrect dimensions
///
/// # Example
///
/// ```ignore
/// use stratum_dsp::features::key::{detector::detect_key, templates::KeyTemplates};
/// use stratum_dsp::features::chroma::extractor::extract_chroma;
///
/// let samples = vec![0.0f32; 44100 * 5];
/// let chroma_vectors = extract_chroma(&samples, 44100, 2048, 512)?;
/// let templates = KeyTemplates::new();
/// let result = detect_key(&chroma_vectors, &templates)?;
///
/// println!("Detected key: {:?}, confidence: {:.2}", result.key, result.confidence);
/// # Ok::<(), stratum_dsp::AnalysisError>(())
/// ```
pub fn detect_key(
    chroma_vectors: &[Vec<f32>],
    templates: &KeyTemplates,
) -> Result<KeyDetectionResult, AnalysisError> {
    detect_key_weighted(chroma_vectors, templates, None)
}

/// Detect musical key from chroma vectors with optional per-frame weights.
///
/// This is a small but important upgrade over simple averaging: weighting allows the caller to
/// emphasize frames with stronger/cleaner tonality (e.g., suppress percussive/noisy frames).
pub fn detect_key_weighted(
    chroma_vectors: &[Vec<f32>],
    templates: &KeyTemplates,
    frame_weights: Option<&[f32]>,
) -> Result<KeyDetectionResult, AnalysisError> {
    log::debug!(
        "Detecting key from {} chroma vectors (weighted={})",
        chroma_vectors.len(),
        frame_weights.is_some()
    );

    if chroma_vectors.is_empty() {
        return Err(AnalysisError::InvalidInput(
            "Empty chroma vectors".to_string(),
        ));
    }

    // Validate chroma vector dimensions
    let n_semitones = chroma_vectors[0].len();
    if n_semitones != 12 {
        return Err(AnalysisError::InvalidInput(format!(
            "Chroma vectors must have 12 elements, got {}",
            n_semitones
        )));
    }

    for (i, chroma) in chroma_vectors.iter().enumerate() {
        if chroma.len() != 12 {
            return Err(AnalysisError::InvalidInput(format!(
                "Chroma vector at index {} has {} elements, expected 12",
                i,
                chroma.len()
            )));
        }
    }

    if let Some(w) = frame_weights {
        if w.len() != chroma_vectors.len() {
            return Err(AnalysisError::InvalidInput(format!(
                "frame_weights length mismatch: got {}, expected {}",
                w.len(),
                chroma_vectors.len()
            )));
        }
    }

    // Step 1: Aggregate the frames into a single (optionally weighted) mean
    // chroma and correlate it against each of the 24 key profiles with the
    // Krumhansl-Schmuckler metric — Pearson correlation, i.e. a mean-centered
    // match rather than a raw dot product. Centering both the chroma and the
    // profile removes the shared baseline energy that real polyphonic audio
    // spreads across every pitch class; a raw dot product against that
    // baseline systematically favors the denser minor profile, which is the
    // generic major/minor confusion #192 tracks. Pearson is invariant to each
    // profile's scale and offset, so the L2-normalized templates need no
    // change. Frame weights, when given, shape the mean profile (emphasizing
    // cleaner frames) rather than weighting independent per-frame matches.
    //
    // With no usable signal — no frame carries weight, or the mean chroma is
    // perfectly flat — there is no tonal evidence: every score is zero, the
    // ranking falls back to the deterministic key order, and the confidence
    // below is zero. We never fabricate a tonal decision from noise.
    let mean = mean_chroma(chroma_vectors, frame_weights).filter(|m| has_tonal_variance(m));
    let mut scores: Vec<(Key, f32)> = Vec::with_capacity(24);
    for key_idx in 0..12 {
        let score = mean.as_ref().map_or(0.0, |m| {
            pearson_correlation(m, templates.get_major_template(key_idx))
        });
        scores.push((Key::Major(key_idx), score));
    }
    for key_idx in 0..12 {
        let score = mean.as_ref().map_or(0.0, |m| {
            pearson_correlation(m, templates.get_minor_template(key_idx))
        });
        scores.push((Key::Minor(key_idx), score));
    }

    // Step 1.5: Map all 24 raw correlations (each in [-1, 1]) onto a shared
    // [0, 1] scale with one min-max normalization. The transform is monotonic
    // across the whole set, so it preserves both the ranking and the
    // *cross-mode* magnitude that decides major vs. minor — unlike an earlier
    // per-mode rescale that pinned the top major and top minor to an exact tie
    // and, via the deterministic tie-break, made minor unreachable on real
    // audio (#192). It also lets the downstream circle-of-fifths bonus and the
    // confidence calculation, which assume non-negative scores, stay sign-safe
    // without ever clamping a negative correlation to zero (which would throw
    // away genuine anti-correlation signal).
    let max_raw = scores
        .iter()
        .map(|(_, s)| *s)
        .fold(f32::NEG_INFINITY, f32::max);
    let min_raw = scores.iter().map(|(_, s)| *s).fold(f32::INFINITY, f32::min);
    let range = max_raw - min_raw;
    if range > 1e-9 {
        for (_, s) in scores.iter_mut() {
            *s = (*s - min_raw) / range;
        }
    } else {
        // A flat field — no key is meaningfully better than any other. Zero
        // every score so confidence collapses to zero rather than inventing a
        // winner from float noise; the deterministic sort still yields a
        // stable key.
        for (_, s) in scores.iter_mut() {
            *s = 0.0;
        }
    }

    // Step 1.75: Circle-of-fifths distance weighting (optional refinement)
    // Keys close on the circle of fifths (e.g., C-G, C-F) are harmonically related
    // and often confused. We can boost scores of keys that are close to high-scoring keys.
    // This helps when the true key is a neighbor on the circle of fifths.
    let circle_of_fifths = [0, 7, 2, 9, 4, 11, 6, 1, 8, 3, 10, 5]; // C, G, D, A, E, B, F#, C#, G#, D#, A#, F
    let mut refined_scores = scores.clone();

    // Find the top-scoring key for each mode
    let (top_major_key, top_major_score) = scores
        .iter()
        .filter_map(|(k, s)| {
            if matches!(k, Key::Major(_)) {
                Some((*k, *s))
            } else {
                None
            }
        })
        .min_by(compare_key_scores_desc)
        .unwrap_or((Key::Major(0), 0.0));
    let (top_minor_key, top_minor_score) = scores
        .iter()
        .filter_map(|(k, s)| {
            if matches!(k, Key::Minor(_)) {
                Some((*k, *s))
            } else {
                None
            }
        })
        .min_by(compare_key_scores_desc)
        .unwrap_or((Key::Minor(0), 0.0));

    // Apply circle-of-fifths bonus to keys near the top-scoring keys
    let circle_bonus_weight = 0.20; // 20% bonus for adjacent keys (increased from 15%)
    for (k, s) in refined_scores.iter_mut() {
        let (ref_key, ref_score) = match k {
            Key::Major(_) => (&top_major_key, &top_major_score),
            Key::Minor(_) => (&top_minor_key, &top_minor_score),
        };

        if *ref_score > 1e-9 {
            let tonic = match k {
                Key::Major(i) => *i as usize,
                Key::Minor(i) => *i as usize,
            };
            let ref_tonic = match ref_key {
                Key::Major(i) => *i as usize,
                Key::Minor(i) => *i as usize,
            };

            // Find positions on circle of fifths
            let tonic_pos = circle_of_fifths
                .iter()
                .position(|&x| x == tonic)
                .unwrap_or(12);
            let ref_pos = circle_of_fifths
                .iter()
                .position(|&x| x == ref_tonic)
                .unwrap_or(12);

            if tonic_pos < 12 && ref_pos < 12 {
                // Compute circular distance (min of forward and backward)
                let dist = (tonic_pos as i32 - ref_pos as i32)
                    .abs()
                    .min(12 - (tonic_pos as i32 - ref_pos as i32).abs());

                // Apply bonus: adjacent keys (dist=1) get full bonus, distance 2 gets half bonus
                if dist <= 2 {
                    let bonus = circle_bonus_weight * (1.0 - (dist as f32) * 0.5);
                    *s += *ref_score * bonus;
                }
            }
        }
    }

    scores = refined_scores;

    // Step 2: Sort by score (highest first); the best-scoring key wins.
    sort_key_scores_desc(&mut scores);
    let (final_key, final_score) = scores[0];

    // Confidence is the top key's margin over the next-best key, on the shared
    // [0, 1] scale. It is zero when there is no tonal evidence (the flat field
    // above), so a flat or signal-less track never reports spurious certainty.
    let best_other = scores
        .iter()
        .find(|(k, _)| *k != final_key)
        .map(|(_, s)| *s)
        .unwrap_or(0.0);

    let confidence = if final_score > 0.0 {
        ((final_score - best_other) / final_score).clamp(0.0, 1.0)
    } else {
        0.0
    };

    // Step 4: Extract top N keys (default: top 3)
    let top_n = 3;
    let top_keys: Vec<(Key, f32)> = scores.iter().take(top_n).cloned().collect();

    log::debug!(
        "Detected key: {:?}, score: {:.4}, confidence: {:.4}",
        final_key,
        final_score,
        confidence
    );

    Ok(KeyDetectionResult {
        key: final_key,
        confidence,
        all_scores: scores,
        top_keys,
    })
}

/// Mean chroma across all frames, optionally frame-weighted. Returns `None`
/// when no frame carries usable weight (all-zero weights, or no frames), so
/// the caller treats it as "no tonal evidence" instead of dividing by zero.
fn mean_chroma(chroma_vectors: &[Vec<f32>], weights: Option<&[f32]>) -> Option<[f32; 12]> {
    let mut acc = [0.0f32; 12];
    let total: f32 = match weights {
        None => {
            for chroma in chroma_vectors {
                for (a, &c) in acc.iter_mut().zip(chroma) {
                    *a += c;
                }
            }
            chroma_vectors.len() as f32
        }
        Some(w) => {
            let mut total = 0.0f32;
            for (chroma, &wt) in chroma_vectors.iter().zip(w.iter()) {
                if wt > 0.0 {
                    for (a, &c) in acc.iter_mut().zip(chroma) {
                        *a += wt * c;
                    }
                    total += wt;
                }
            }
            total
        }
    };
    if total <= 1e-12 {
        return None;
    }
    for a in acc.iter_mut() {
        *a /= total;
    }
    Some(acc)
}

/// Whether a chroma profile carries enough variance to hold tonal
/// information. A near-uniform profile correlates with every key template
/// only through floating-point noise, which the downstream min-max rescale
/// would amplify into a spurious winner; this lets the caller treat such a
/// profile as "no evidence" instead. The test is *relative* to the profile's
/// own energy (variance as a fraction of mean square), so it carries no
/// absolute magnitude tuned to any signal level or corpus — it only separates
/// a genuinely structured profile (ratio ~0.1–1) from float-flat noise
/// (ratio ~1e-13), and changes no decision on real tonal audio.
fn has_tonal_variance(chroma: &[f32]) -> bool {
    let n = chroma.len() as f32;
    let mean = chroma.iter().sum::<f32>() / n;
    let variance = chroma.iter().map(|&x| (x - mean) * (x - mean)).sum::<f32>() / n;
    let mean_square = chroma.iter().map(|&x| x * x).sum::<f32>() / n;
    variance > mean_square * 1e-6
}

/// Pearson correlation between two equal-length vectors — the
/// Krumhansl-Schmuckler key-finding metric. Returns `0.0` when either vector
/// has no variance (a flat distribution carries no tonal information to
/// correlate), so the result is always finite and in `[-1, 1]`.
fn pearson_correlation(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len() as f32;
    let mean_a = a.iter().sum::<f32>() / n;
    let mean_b = b.iter().sum::<f32>() / n;
    let mut cov = 0.0f32;
    let mut var_a = 0.0f32;
    let mut var_b = 0.0f32;
    for (&x, &y) in a.iter().zip(b.iter()) {
        let da = x - mean_a;
        let db = y - mean_b;
        cov += da * db;
        var_a += da * da;
        var_b += db * db;
    }
    let denom = (var_a * var_b).sqrt();
    if denom > 1e-12 {
        cov / denom
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build `count` identical chroma frames from a 12-element profile.
    fn frames(profile: &[f32], count: usize) -> Vec<Vec<f32>> {
        vec![profile.to_vec(); count]
    }

    #[test]
    fn test_detect_key_empty() {
        let templates = KeyTemplates::new();
        let result = detect_key(&[], &templates);
        assert!(result.is_err());
    }

    #[test]
    fn test_detect_key_wrong_dimensions() {
        let templates = KeyTemplates::new();
        let chroma_vectors = vec![vec![0.0f32; 10]]; // Wrong size
        let result = detect_key(&chroma_vectors, &templates);
        assert!(result.is_err());
    }

    #[test]
    fn detects_c_major_from_tonic_triad() {
        // A clean C-major triad (C, E, G) must resolve to a *major* key, and
        // specifically C major: the mean-centered correlation has to prefer
        // the major profile over its parallel/relative minors.
        let templates = KeyTemplates::new();
        let mut chroma = vec![0.0f32; 12];
        chroma[0] = 1.0; // C
        chroma[4] = 1.0; // E
        chroma[7] = 1.0; // G
        let detection = detect_key(&frames(&chroma, 10), &templates).unwrap();
        assert_eq!(detection.key, Key::Major(0), "got {:?}", detection.key);
        assert!(!detection.top_keys.is_empty() && detection.top_keys.len() <= 3);
        assert_eq!(detection.all_scores.len(), 24);
    }

    #[test]
    fn major_template_chroma_detects_that_major_key() {
        // Feeding a major profile as the chroma is the cleanest possible major
        // signal; the detector must return that major key. Regression guard
        // for the #192 mode bias that called every track minor.
        let templates = KeyTemplates::new();
        for key_idx in 0..12 {
            let profile = templates.get_major_template(key_idx).to_vec();
            let detection = detect_key(&frames(&profile, 8), &templates).unwrap();
            assert_eq!(
                detection.key,
                Key::Major(key_idx),
                "major template {key_idx} detected as {:?}",
                detection.key
            );
        }
    }

    #[test]
    fn minor_template_chroma_detects_that_minor_key() {
        // The companion guard: a minor profile must resolve to that minor key,
        // so both modes stay reachable now that scoring is mean-centered.
        let templates = KeyTemplates::new();
        for key_idx in 0..12 {
            let profile = templates.get_minor_template(key_idx).to_vec();
            let detection = detect_key(&frames(&profile, 8), &templates).unwrap();
            assert_eq!(
                detection.key,
                Key::Minor(key_idx),
                "minor template {key_idx} detected as {:?}",
                detection.key
            );
        }
    }

    #[test]
    fn flat_chroma_has_zero_confidence_and_finite_scores() {
        // A perfectly flat distribution carries no tonal evidence: the
        // detector must not invent a winner from float noise. Scores stay
        // finite, confidence is zero, and the key is the deterministic
        // fallback (C major sorts first).
        let templates = KeyTemplates::new();
        let detection = detect_key(&frames(&[0.1f32; 12], 10), &templates).unwrap();
        assert_eq!(detection.confidence, 0.0);
        assert_eq!(detection.key, Key::Major(0));
        assert_eq!(detection.all_scores.len(), 24);
        assert!(detection.all_scores.iter().all(|(_, s)| s.is_finite()));
    }

    #[test]
    fn all_scores_and_confidence_are_finite() {
        // A realistic, lopsided chroma must never produce NaN/inf anywhere.
        let templates = KeyTemplates::new();
        let chroma = vec![
            0.4, 0.05, 0.2, 0.02, 0.3, 0.1, 0.03, 0.35, 0.04, 0.15, 0.02, 0.08,
        ];
        let detection = detect_key(&frames(&chroma, 12), &templates).unwrap();
        assert!(detection.confidence.is_finite());
        assert!((0.0..=1.0).contains(&detection.confidence));
        assert_eq!(detection.all_scores.len(), 24);
        assert!(detection.all_scores.iter().all(|(_, s)| s.is_finite()));
        assert!(!detection.top_keys.is_empty() && detection.top_keys.len() <= 3);
    }

    #[test]
    fn zero_weights_yield_finite_zero_confidence() {
        // All-zero frame weights leave no usable signal; the detector must
        // return a finite, zero-confidence result rather than dividing by zero
        // or producing NaN.
        let templates = KeyTemplates::new();
        let chroma_vectors = frames(&[0.2f32; 12], 10);
        let weights = vec![0.0f32; 10];
        let detection = detect_key_weighted(&chroma_vectors, &templates, Some(&weights)).unwrap();
        assert_eq!(detection.confidence, 0.0);
        assert!(detection.all_scores.iter().all(|(_, s)| s.is_finite()));
    }

    #[test]
    fn frame_weights_shape_the_mean_profile() {
        // Three frames, but only the C-major frame carries weight — the result
        // must follow the weighted frame, confirming weights shape the mean
        // profile rather than every frame contributing equally.
        let templates = KeyTemplates::new();
        let c_major = templates.get_major_template(0).to_vec();
        let a_minor = templates.get_minor_template(9).to_vec();
        let chroma_vectors = vec![c_major, a_minor.clone(), a_minor];
        let weights = vec![1.0f32, 0.0, 0.0];
        let detection = detect_key_weighted(&chroma_vectors, &templates, Some(&weights)).unwrap();
        assert_eq!(detection.key, Key::Major(0));
    }
}
