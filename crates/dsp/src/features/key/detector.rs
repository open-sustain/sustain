// SPDX-License-Identifier: MIT OR Apache-2.0
//! Key detection algorithm
//!
//! Reads the musical key of an audio track from its chroma: the share each
//! pitch class takes of the mean chroma, on a log scale, weighed by each of
//! the 24 key templates (weights learned from labelled tracks, see
//! [`super::templates`]).

use super::{sort_key_scores_desc, templates::KeyTemplates, KeyDetectionResult};
use crate::analysis::result::Key;
use crate::error::AnalysisError;

/// The least share a pitch class is taken to hold of the chroma. A pitch
/// class a track never sounds would otherwise weigh minus infinity on the
/// log scale; a thousandth of the chroma is far below what any sounded
/// pitch class holds, and the learned weights are fitted with this floor.
const PITCH_CLASS_SHARE_FLOOR: f32 = 1e-3;

/// What [`detect_key`] reads the key from: the natural log of the share
/// each pitch class takes of the mean chroma across `chroma_vectors`,
/// floored at a thousandth (`PITCH_CLASS_SHARE_FLOOR`). The key weights
/// are learned from exactly this (`analysis-bench learn-key-weights`).
///
/// `None` when the chroma carries no tonal evidence — silence, or a
/// profile flat across the pitch classes — since no key is better than
/// any other.
///
/// # Errors
///
/// Returns `AnalysisError` if the chroma vectors are empty, are not 12
/// elements each, or hold a negative or non-finite value.
pub fn pitch_class_log_shares(
    chroma_vectors: &[Vec<f32>],
) -> Result<Option<[f32; 12]>, AnalysisError> {
    if chroma_vectors.is_empty() {
        return Err(AnalysisError::InvalidInput(
            "Empty chroma vectors".to_string(),
        ));
    }
    for (i, chroma) in chroma_vectors.iter().enumerate() {
        if chroma.len() != 12 {
            return Err(AnalysisError::InvalidInput(format!(
                "Chroma vector at index {} has {} elements, expected 12",
                i,
                chroma.len()
            )));
        }
        if !chroma
            .iter()
            .all(|value| value.is_finite() && *value >= 0.0)
        {
            return Err(AnalysisError::InvalidInput(format!(
                "Chroma vector at index {} holds a negative or non-finite value",
                i
            )));
        }
    }

    let mut sum = [0.0f32; 12];
    for chroma in chroma_vectors {
        for (bin, &value) in sum.iter_mut().zip(chroma) {
            *bin += value;
        }
    }
    // A flat profile, silence included, weighs every key of a mode alike:
    // any winner would be the tie-break order's.
    if !has_tonal_variance(&sum) {
        return Ok(None);
    }
    let total: f32 = sum.iter().sum();
    Ok(Some(
        sum.map(|bin| (bin / total + PITCH_CLASS_SHARE_FLOOR).ln()),
    ))
}

/// Detect musical key from chroma vectors
///
/// Weighs the [`pitch_class_log_shares`] of the chroma by each of the 24
/// key templates; the highest-scoring key wins. Scores are the keys'
/// probabilities under the learned model — the softmax of the weighted
/// sums — so they sum to one, and the confidence is the winner's.
///
/// # Returns
///
/// `None` when the chroma carries no tonal evidence — silence, or a
/// profile flat across the pitch classes — since no key is better than
/// any other. Otherwise a key detection result with:
/// - Detected key (major or minor, 0-11)
/// - Confidence: the detected key's probability (0.0-1.0)
/// - All 24 key probabilities (ranked)
///
/// # Errors
///
/// As for [`pitch_class_log_shares`].
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
/// if let Some(result) = detect_key(&chroma_vectors, &templates)? {
///     println!("Detected key: {:?}, confidence: {:.2}", result.key, result.confidence);
/// }
/// # Ok::<(), stratum_dsp::AnalysisError>(())
/// ```
pub fn detect_key(
    chroma_vectors: &[Vec<f32>],
    templates: &KeyTemplates,
) -> Result<Option<KeyDetectionResult>, AnalysisError> {
    log::debug!("Detecting key from {} chroma vectors", chroma_vectors.len());
    let Some(log_shares) = pitch_class_log_shares(chroma_vectors)? else {
        return Ok(None);
    };

    let weighted = |template: &[f32]| -> f32 {
        template
            .iter()
            .zip(&log_shares)
            .map(|(weight, log_share)| weight * log_share)
            .sum()
    };
    let mut scores: Vec<(Key, f32)> = (0..12)
        .map(|tonic| {
            (
                Key::Major(tonic),
                weighted(templates.get_major_template(tonic)),
            )
        })
        .chain((0..12).map(|tonic| {
            (
                Key::Minor(tonic),
                weighted(templates.get_minor_template(tonic)),
            )
        }))
        .collect();

    // Softmax, shifted by the highest score so no exponential overflows.
    let highest = scores
        .iter()
        .map(|(_, score)| *score)
        .fold(f32::NEG_INFINITY, f32::max);
    for (_, score) in scores.iter_mut() {
        *score = (*score - highest).exp();
    }
    let total: f32 = scores.iter().map(|(_, score)| score).sum();
    for (_, score) in scores.iter_mut() {
        *score /= total;
    }

    sort_key_scores_desc(&mut scores);
    let (key, confidence) = scores[0];
    let top_keys: Vec<(Key, f32)> = scores.iter().take(3).cloned().collect();

    log::debug!("Detected key: {:?}, probability: {:.4}", key, confidence);

    Ok(Some(KeyDetectionResult {
        key,
        confidence,
        all_scores: scores,
        top_keys,
    }))
}

/// Whether a chroma profile carries enough variance to hold tonal
/// information. A near-uniform profile differs between keys only through
/// floating-point noise, which would pick a spurious winner; this lets the
/// caller treat such a profile as "no evidence" instead. The test is
/// *relative* to the profile's own energy (variance as a fraction of mean
/// square), so it carries no absolute magnitude tuned to any signal level
/// or corpus — it only separates a genuinely structured profile (ratio
/// ~0.1–1) from float-flat noise (ratio ~1e-13), and changes no decision
/// on real tonal audio.
fn has_tonal_variance(chroma: &[f32]) -> bool {
    let n = chroma.len() as f32;
    let mean = chroma.iter().sum::<f32>() / n;
    let variance = chroma.iter().map(|&x| (x - mean) * (x - mean)).sum::<f32>() / n;
    let mean_square = chroma.iter().map(|&x| x * x).sum::<f32>() / n;
    variance > mean_square * 1e-6
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mean tonic-aligned chroma of the major tracks in the corpora the
    /// key weights are learned from (the templates of analyzer version 15):
    /// the chroma a typical major track leaves, tonic first.
    const TYPICAL_MAJOR_CHROMA: [f32; 12] = [
        1.000, 0.246, 0.534, 0.275, 0.595, 0.578, 0.263, 0.810, 0.273, 0.514, 0.314, 0.393,
    ];

    /// The same for minor tracks.
    const TYPICAL_MINOR_CHROMA: [f32; 12] = [
        1.000, 0.330, 0.456, 0.604, 0.343, 0.595, 0.311, 0.813, 0.465, 0.322, 0.541, 0.337,
    ];

    /// Build `count` identical chroma frames from a 12-element profile.
    fn frames(profile: &[f32], count: usize) -> Vec<Vec<f32>> {
        vec![profile.to_vec(); count]
    }

    /// `chroma` (tonic first) moved onto `tonic`.
    fn on_tonic(chroma: &[f32; 12], tonic: u32) -> Vec<f32> {
        (0..12)
            .map(|pitch_class| chroma[(pitch_class + 12 - tonic as usize) % 12])
            .collect()
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
    fn negative_or_non_finite_chroma_is_refused() {
        let templates = KeyTemplates::new();
        for bad in [-0.1, f32::NAN, f32::INFINITY] {
            let mut chroma = TYPICAL_MAJOR_CHROMA.to_vec();
            chroma[3] = bad;
            assert!(detect_key(&frames(&chroma, 4), &templates).is_err());
        }
    }

    #[test]
    fn detects_c_major_from_tonic_triad() {
        // A clean C-major triad (C, E, G) must resolve to C major, not to
        // the minor keys sharing two of its notes.
        let templates = KeyTemplates::new();
        let mut chroma = vec![0.0f32; 12];
        chroma[0] = 1.0; // C
        chroma[4] = 1.0; // E
        chroma[7] = 1.0; // G
        let detection = detect_key(&frames(&chroma, 10), &templates)
            .unwrap()
            .expect("a key");
        assert_eq!(detection.key, Key::Major(0), "got {:?}", detection.key);
        assert!(!detection.top_keys.is_empty() && detection.top_keys.len() <= 3);
        assert_eq!(detection.all_scores.len(), 24);
    }

    #[test]
    fn typical_chroma_of_each_key_detects_that_key() {
        // Both modes reachable on every tonic: a regression guard for the
        // #192 mode bias that once called every track minor, and for its
        // mirror image.
        let templates = KeyTemplates::new();
        for tonic in 0..12 {
            for (chroma, key) in [
                (&TYPICAL_MAJOR_CHROMA, Key::Major(tonic)),
                (&TYPICAL_MINOR_CHROMA, Key::Minor(tonic)),
            ] {
                let detection = detect_key(&frames(&on_tonic(chroma, tonic), 8), &templates)
                    .unwrap()
                    .expect("a key");
                assert_eq!(detection.key, key, "detected as {:?}", detection.key);
            }
        }
    }

    #[test]
    fn flat_chroma_has_no_key() {
        // A perfectly flat distribution carries no tonal evidence: the
        // detector must not invent a winner from float noise.
        let templates = KeyTemplates::new();
        assert!(matches!(
            detect_key(&frames(&[0.1f32; 12], 10), &templates),
            Ok(None)
        ));
    }

    #[test]
    fn silence_has_no_key() {
        // Silence leaves every pitch class empty, which is no more C major
        // than any other key.
        let templates = KeyTemplates::new();
        assert!(matches!(
            detect_key(&frames(&[0.0f32; 12], 10), &templates),
            Ok(None)
        ));
    }

    #[test]
    fn scores_are_the_keys_probabilities() {
        // A lopsided chroma with silent pitch classes: every score finite,
        // the 24 summing to one, ranked, the confidence the winner's.
        let templates = KeyTemplates::new();
        let chroma = vec![
            0.4, 0.0, 0.2, 0.02, 0.3, 0.1, 0.0, 0.35, 0.04, 0.15, 0.0, 0.08,
        ];
        let detection = detect_key(&frames(&chroma, 12), &templates)
            .unwrap()
            .expect("a key");
        assert_eq!(detection.all_scores.len(), 24);
        assert!(detection
            .all_scores
            .iter()
            .all(|(_, p)| p.is_finite() && (0.0..=1.0).contains(p)));
        let total: f32 = detection.all_scores.iter().map(|(_, p)| p).sum();
        assert!((total - 1.0).abs() < 1e-5, "probabilities sum to {total}");
        assert!(detection
            .all_scores
            .windows(2)
            .all(|pair| pair[0].1 >= pair[1].1));
        assert_eq!(
            detection.all_scores[0],
            (detection.key, detection.confidence)
        );
        assert_eq!(detection.top_keys, detection.all_scores[..3].to_vec());
    }

    #[test]
    fn the_chroma_level_does_not_matter() {
        // Only each pitch class's share counts, not how loud the track is.
        let templates = KeyTemplates::new();
        let quiet = on_tonic(&TYPICAL_MINOR_CHROMA, 4);
        let loud: Vec<f32> = quiet.iter().map(|value| value * 250.0).collect();
        let quiet = detect_key(&frames(&quiet, 3), &templates)
            .unwrap()
            .expect("a key");
        let loud = detect_key(&frames(&loud, 3), &templates)
            .unwrap()
            .expect("a key");
        assert_eq!(quiet.key, loud.key);
        assert!((quiet.confidence - loud.confidence).abs() < 1e-4);
    }
}
