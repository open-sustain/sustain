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
/// Averages chroma vectors across all frames, then computes dot product with
/// each of the 24 key templates. The key with the highest score is selected.
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

    // Step 1: Compute weighted scores for all 24 keys.
    //
    // We keep the scoring frame-based (sum of dot products) so we can apply frame weights
    // without constructing a potentially biased global histogram.
    let mut scores = Vec::with_capacity(24);
    let weights = frame_weights;

    // Major keys (0-11)
    for key_idx in 0..12 {
        let template = templates.get_major_template(key_idx);
        let score = weighted_sum_dot(chroma_vectors, weights, template);
        scores.push((Key::Major(key_idx), score));
    }

    // Minor keys (0-11)
    for key_idx in 0..12 {
        let template = templates.get_minor_template(key_idx);
        let score = weighted_sum_dot(chroma_vectors, weights, template);
        scores.push((Key::Minor(key_idx), score));
    }

    // Step 1.5: Normalize major and minor scores separately to address scale differences.
    // This helps when major and minor template scores have different ranges, which can
    // bias mode selection. We normalize by the max score in each mode category.
    let max_major = scores
        .iter()
        .filter_map(|(k, s)| {
            if matches!(k, Key::Major(_)) {
                Some(*s)
            } else {
                None
            }
        })
        .fold(0.0f32, f32::max);
    let max_minor = scores
        .iter()
        .filter_map(|(k, s)| {
            if matches!(k, Key::Minor(_)) {
                Some(*s)
            } else {
                None
            }
        })
        .fold(0.0f32, f32::max);

    // Normalize scores if max values are non-zero
    if max_major > 1e-9 && max_minor > 1e-9 {
        for (k, s) in scores.iter_mut() {
            match k {
                Key::Major(_) => *s /= max_major,
                Key::Minor(_) => *s /= max_minor,
            }
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

    // Step 2: Sort by score (highest first)
    sort_key_scores_desc(&mut scores);

    // Step 3: Select best key using weighted top-N voting (if top keys are close)
    // This helps when the best key is only slightly better than alternatives
    let (best_key, best_score) = scores[0];
    let second_score = if scores.len() > 1 { scores[1].1 } else { 0.0 };
    let third_score = if scores.len() > 2 { scores[2].1 } else { 0.0 };

    // If top 3 keys are within 5% of each other, use weighted voting
    let score_threshold = best_score * 0.95;
    let use_weighted_voting =
        second_score >= score_threshold && third_score >= score_threshold * 0.90;

    let final_key = best_key;

    // Compute confidence for final key
    let final_score = scores
        .iter()
        .find(|(k, _)| *k == final_key)
        .map(|(_, s)| *s)
        .unwrap_or(best_score);
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
        "Detected key: {:?}, score: {:.4}, confidence: {:.4} (weighted voting: {})",
        final_key,
        final_score,
        confidence,
        use_weighted_voting
    );

    Ok(KeyDetectionResult {
        key: final_key,
        confidence,
        all_scores: scores,
        top_keys,
    })
}

/// Compute dot product between two vectors.
fn dot_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// Sum dot products across frames, optionally applying per-frame weights.
fn weighted_sum_dot(chroma_vectors: &[Vec<f32>], weights: Option<&[f32]>, template: &[f32]) -> f32 {
    let mut acc = 0.0f32;
    match weights {
        None => {
            for chroma in chroma_vectors {
                acc += dot_product(chroma, template);
            }
        }
        Some(w) => {
            for (chroma, &wt) in chroma_vectors.iter().zip(w.iter()) {
                if wt > 0.0 {
                    acc += wt * dot_product(chroma, template);
                }
            }
        }
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_key_empty() {
        let templates = KeyTemplates::new();
        let result = detect_key(&[], &templates);
        assert!(result.is_err());
    }

    #[test]
    fn test_detect_key_basic() {
        let templates = KeyTemplates::new();

        // Create chroma vectors that match C major
        // C major template has high values at indices 0 (C), 4 (E), 7 (G)
        let mut chroma_vectors = Vec::new();
        for _ in 0..10 {
            let mut chroma = vec![0.0f32; 12];
            chroma[0] = 0.3; // C
            chroma[4] = 0.3; // E
            chroma[7] = 0.3; // G
                             // Normalize
            let norm: f32 = chroma.iter().map(|&x| x * x).sum::<f32>().sqrt();
            for x in &mut chroma {
                *x /= norm;
            }
            chroma_vectors.push(chroma);
        }

        let result = detect_key(&chroma_vectors, &templates);
        assert!(result.is_ok());

        let detection = result.unwrap();
        assert!(detection.confidence >= 0.0 && detection.confidence <= 1.0);
        assert_eq!(detection.all_scores.len(), 24);

        // Best key should be C major (index 0)
        assert_eq!(detection.key, Key::Major(0));

        // Check top_keys is populated
        assert!(!detection.top_keys.is_empty());
        assert!(detection.top_keys.len() <= 3);
        assert_eq!(detection.top_keys[0].0, Key::Major(0));
    }

    #[test]
    fn test_detect_key_wrong_dimensions() {
        let templates = KeyTemplates::new();
        let chroma_vectors = vec![vec![0.0f32; 10]]; // Wrong size
        let result = detect_key(&chroma_vectors, &templates);
        assert!(result.is_err());
    }

    #[test]
    fn test_average_chroma() {
        // Historical test placeholder: key detector no longer exposes average-chroma directly.
        // Keep a sanity check that weighted scoring behaves sensibly.
        let templates = KeyTemplates::new();
        let chroma_vectors = vec![vec![0.0f32; 12]; 10];
        let weights = vec![0.0f32; 10];
        let result = detect_key_weighted(&chroma_vectors, &templates, Some(&weights));
        assert!(result.is_ok());
    }

    #[test]
    fn test_dot_product() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        let result = dot_product(&a, &b);
        assert_eq!(result, 32.0); // 1*4 + 2*5 + 3*6 = 4 + 10 + 18 = 32
    }
}
