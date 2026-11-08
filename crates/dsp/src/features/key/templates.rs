// SPDX-License-Identifier: MIT OR Apache-2.0
//! Key templates for template-matching key detection
//!
//! The profiles are learned, not taken from listening tests: each is the
//! average tonic-aligned chroma that tracks of that mode leave in Sustain's
//! own key front end. Krumhansl–Kessler's probe-tone profiles, used until
//! analyzer version 15, describe what listeners hear; the learned ones
//! describe what the front end measures — the overtones of every note
//! landing on its fifth, the weight of the bass, the spread of each
//! spectral peak — so a key matches the chroma it really produces.
//!
//! # Provenance
//!
//! Learned by `analysis-bench learn-key-profiles` from 3,485 human-labelled
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

/// The major profile, tonic first (C major), scaled so the tonic is 1.
const MAJOR_PROFILE: [f32; 12] = [
    1.000, 0.246, 0.534, 0.275, 0.595, 0.578, 0.263, 0.810, 0.273, 0.514, 0.314, 0.393,
];

/// The minor profile, tonic first (C minor), scaled so the tonic is 1.
const MINOR_PROFILE: [f32; 12] = [
    1.000, 0.330, 0.456, 0.604, 0.343, 0.595, 0.311, 0.813, 0.465, 0.322, 0.541, 0.337,
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
    /// The 24 key templates: the learned major and minor profiles rotated
    /// onto each tonic, L2-normalized.
    pub fn new() -> Self {
        Self {
            major: std::array::from_fn(|tonic| rotated(&MAJOR_PROFILE, tonic)),
            minor: std::array::from_fn(|tonic| rotated(&MINOR_PROFILE, tonic)),
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

/// `profile` (tonic first) rotated so its tonic sits on pitch class
/// `tonic`, L2-normalized.
fn rotated(profile: &[f32; 12], tonic: usize) -> Vec<f32> {
    let mut template: Vec<f32> = (0..12)
        .map(|pitch_class| profile[(pitch_class + 12 - tonic) % 12])
        .collect();
    let norm = template.iter().map(|&x| x * x).sum::<f32>().sqrt();
    for x in &mut template {
        *x /= norm;
    }
    template
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_is_a_unit_vector_peaking_on_its_tonic() {
        let templates = KeyTemplates::new();
        for tonic in 0..12 {
            for template in [&templates.major[tonic], &templates.minor[tonic]] {
                assert_eq!(template.len(), 12);
                let norm = template.iter().map(|&x| x * x).sum::<f32>().sqrt();
                assert!((norm - 1.0).abs() < 1e-6);
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
    fn templates_are_rotations_of_one_profile() {
        let templates = KeyTemplates::new();
        let c_major = templates.get_major_template(0);
        let d_major = templates.get_major_template(2);
        for pitch_class in 0..12 {
            assert!((d_major[(pitch_class + 2) % 12] - c_major[pitch_class]).abs() < 1e-6);
        }
    }
}
