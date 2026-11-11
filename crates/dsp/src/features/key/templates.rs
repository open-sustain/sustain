// SPDX-License-Identifier: MIT OR Apache-2.0
//! Key templates for template-matching key detection
//!
//! A key's template weighs the log share each pitch class takes of a
//! track's chroma (see [`super::detector`]); the key whose weighted sum is
//! highest wins. The weights are learned, not taken from listening tests:
//! they are the multinomial logistic regression that best tells the 24 keys
//! apart on labelled tracks, as Sustain's own key front end measures them —
//! the overtones of every note landing on its fifth, the weight of the
//! bass, the spread of each spectral peak. One weight vector per mode is
//! learned, tonic first, and rotated onto each tonic, so every key of a mode
//! is told apart by the same rule.
//!
//! Until analyzer version 15 the templates were profiles correlated with the
//! mean chroma: Krumhansl–Kessler's probe-tone ratings, then the mean
//! tonic-aligned chroma of each mode. A correlation only asks which profile
//! the chroma resembles; the learned weights also know which pitch classes
//! tell one key from its neighbours.
//!
//! # Provenance
//!
//! Learned by `analysis-bench learn-key-weights` from 3,485 human-labelled
//! tracks: GiantSteps Key (604, Beatport dance music), GiantSteps MTG Key
//! (1,159 confident single-key labels, Beatport dance music) and FMAKv2's
//! `fma_medium` subset (1,722, seventeen genres). See
//! `docs/analysis-benchmark-corpora.md`. They must be learned again
//! whenever the key front end (HPSS, HPCP, tuning) changes.
//!
//! # References
//!
//! - Krumhansl, C. L., & Kessler, E. J. (1982). Tracing the Dynamic Changes in Perceived
//!   Tonal Organization in a Spatial Representation of Musical Keys. *Psychological Review*,
//!   89(4), 334-368.

/// The major weights, tonic first (C major).
const MAJOR_WEIGHTS: [f32; 12] = [
    2.758, -1.247, -0.216, -1.026, 1.264, 0.423, -0.850, 1.254, -0.593, -0.366, -0.836, -0.334,
];

/// The minor weights, tonic first (C minor).
const MINOR_WEIGHTS: [f32; 12] = [
    2.911, -0.952, -0.412, 1.033, -0.314, -0.751, -0.541, 1.214, -0.266, -1.348, -0.135, -0.669,
];

/// Key templates for all 24 keys
#[derive(Debug, Clone)]
pub struct KeyTemplates {
    /// Major key templates (12 keys: C, C#, D, ..., B)
    pub major: [Vec<f32>; 12],

    /// Minor key templates (12 keys: C, C#, D, ..., B)
    pub minor: [Vec<f32>; 12],
}

impl KeyTemplates {
    /// The 24 key templates: the learned major and minor weights rotated
    /// onto each tonic.
    pub fn new() -> Self {
        Self {
            major: std::array::from_fn(|tonic| rotated(&MAJOR_WEIGHTS, tonic)),
            minor: std::array::from_fn(|tonic| rotated(&MINOR_WEIGHTS, tonic)),
        }
    }

    /// Get template for major key
    ///
    /// # Arguments
    ///
    /// * `key_idx` - Major key index (0-11: C, C#, D, ..., B)
    ///
    /// # Returns
    ///
    /// 12-element template vector for the specified major key
    pub fn get_major_template(&self, key_idx: u32) -> &[f32] {
        &self.major[key_idx as usize % 12]
    }

    /// Get template for minor key
    ///
    /// # Arguments
    ///
    /// * `key_idx` - Minor key index (0-11: C, C#, D, ..., B)
    ///
    /// # Returns
    ///
    /// 12-element template vector for the specified minor key
    pub fn get_minor_template(&self, key_idx: u32) -> &[f32] {
        &self.minor[key_idx as usize % 12]
    }
}

impl Default for KeyTemplates {
    fn default() -> Self {
        Self::new()
    }
}

/// `weights` (tonic first) rotated so the tonic's weight sits on pitch
/// class `tonic`.
fn rotated(weights: &[f32; 12], tonic: usize) -> Vec<f32> {
    (0..12)
        .map(|pitch_class| weights[(pitch_class + 12 - tonic) % 12])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_weighs_its_tonic_most() {
        let templates = KeyTemplates::new();
        for tonic in 0..12 {
            for template in [&templates.major[tonic], &templates.minor[tonic]] {
                assert_eq!(template.len(), 12);
                let peak = (0..12)
                    .max_by(|&a, &b| template[a].total_cmp(&template[b]))
                    .expect("twelve bins");
                assert_eq!(peak, tonic);
            }
        }
    }

    #[test]
    fn the_third_tells_the_modes_apart() {
        let templates = KeyTemplates::new();
        // A major: C# (major third) over C (minor third); A minor the
        // other way round.
        let a_major = templates.get_major_template(9);
        let a_minor = templates.get_minor_template(9);
        assert!(a_major[1] > a_major[0]);
        assert!(a_minor[0] > a_minor[1]);
    }

    #[test]
    fn templates_are_rotations_of_one_weight_vector() {
        let templates = KeyTemplates::new();
        let c_major = templates.get_major_template(0);
        let d_major = templates.get_major_template(2);
        for pitch_class in 0..12 {
            assert!((d_major[(pitch_class + 2) % 12] - c_major[pitch_class]).abs() < 1e-6);
        }
    }
}
