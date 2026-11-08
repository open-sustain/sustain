// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Learn the key detector's major and minor profiles from labelled audio.
//!
//! A profile is the pitch-class distribution a key leaves in the
//! analyzer's own chroma ([`Analyzer::key_chroma`]): each labelled track's
//! mean chroma, scaled to sum one so every track weighs the same, is
//! rotated so its tonic sits at index 0, then averaged with the other
//! tracks of its mode. Learned this way the profiles carry what the front
//! end really measures — overtones landing on the fifth, the weight of the
//! bass, the spread of each peak — which profiles from listening tests
//! (Krumhansl–Kessler) know nothing about.
//!
//! `analysis-bench learn-key-profiles` runs this over key manifests and
//! prints the profiles in the form `sustain_dsp`'s key templates hold.

use std::path::Path;
use std::time::Duration;

use sustain_analysis::{AnalysisOptions, Analyzer};

use crate::manifest::Manifest;
use crate::metrics::{KeyClass, parse_key};

/// Running sums of tonic-aligned chroma, one per mode.
#[derive(Clone, Debug, Default)]
pub struct ProfileLearner {
    major: ModeSum,
    minor: ModeSum,
}

#[derive(Clone, Debug, Default)]
struct ModeSum {
    sum: [f64; 12],
    tracks: usize,
}

/// Learned profiles, tonic at index 0, scaled so the tonic is 1.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyProfiles {
    pub major: [f64; 12],
    pub minor: [f64; 12],
    /// Tracks each profile was averaged over.
    pub major_tracks: usize,
    pub minor_tracks: usize,
}

impl ProfileLearner {
    /// Add one track's chroma frames under its labelled `key`. Returns
    /// whether the track counted: frames without any energy (or with a
    /// non-finite value) say nothing about a key.
    pub fn add(&mut self, frames: &[Vec<f32>], key: KeyClass) -> bool {
        let mut mean = [0.0_f64; 12];
        for frame in frames {
            for (bin, &value) in mean.iter_mut().zip(frame) {
                *bin += f64::from(value);
            }
        }
        let total: f64 = mean.iter().sum();
        if !(total.is_finite() && total > 0.0) {
            return false;
        }
        let mode = if key.major {
            &mut self.major
        } else {
            &mut self.minor
        };
        for (interval, bin) in mode.sum.iter_mut().enumerate() {
            *bin += mean[(usize::from(key.tonic) + interval) % 12] / total;
        }
        mode.tracks += 1;
        true
    }

    /// The learned profiles, or `None` until both modes have a track.
    pub fn profiles(&self) -> Option<KeyProfiles> {
        Some(KeyProfiles {
            major: self.major.profile()?,
            minor: self.minor.profile()?,
            major_tracks: self.major.tracks,
            minor_tracks: self.minor.tracks,
        })
    }
}

impl ModeSum {
    fn profile(&self) -> Option<[f64; 12]> {
        let tonic = self.sum[0];
        (self.tracks > 0 && tonic > 0.0).then(|| self.sum.map(|bin| bin / tonic))
    }
}

/// What [`learn`] went through, beside the profiles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LearnReport {
    /// Tracks whose chroma went into a profile.
    pub learned: usize,
    /// Tracks without a readable key label, a file path, or any chroma.
    pub skipped: usize,
}

/// Learn the profiles from every labelled audio file in `manifests`.
/// `progress` is told each track's id as it is analysed.
pub fn learn(
    manifests: &[Manifest],
    mut progress: impl FnMut(&str),
) -> (Option<KeyProfiles>, LearnReport) {
    let mut learner = ProfileLearner::default();
    let mut report = LearnReport::default();
    for entry in manifests.iter().flat_map(|manifest| &manifest.tracks) {
        progress(&entry.id);
        let key = entry.key.as_deref().and_then(parse_key);
        let counted = match (key, &entry.path) {
            (Some(key), Some(path)) => {
                chroma(path, entry.duration_secs).is_some_and(|frames| learner.add(&frames, key))
            }
            _ => false,
        };
        if counted {
            report.learned += 1;
        } else {
            report.skipped += 1;
        }
    }
    (learner.profiles(), report)
}

fn chroma(path: &Path, duration_secs: Option<f64>) -> Option<Vec<Vec<f32>>> {
    let hint = duration_secs
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(Duration::from_secs_f64);
    Analyzer::new(path.to_path_buf(), AnalysisOptions::default(), hint).key_chroma()
}

impl KeyProfiles {
    /// The profiles as the Rust arrays the key templates hold.
    pub fn to_rust(&self) -> String {
        format!(
            "// Major: {} tracks. Minor: {} tracks.\n\
             const MAJOR_PROFILE: [f32; 12] = {};\n\
             const MINOR_PROFILE: [f32; 12] = {};\n",
            self.major_tracks,
            self.minor_tracks,
            rust_array(&self.major),
            rust_array(&self.minor),
        )
    }
}

fn rust_array(profile: &[f64; 12]) -> String {
    let values: Vec<String> = profile.iter().map(|value| format!("{value:.3}")).collect();
    format!("[{}]", values.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(bins: &[(usize, f32)]) -> Vec<f32> {
        let mut frame = vec![0.0; 12];
        for &(bin, value) in bins {
            frame[bin] = value;
        }
        frame
    }

    #[test]
    fn tracks_are_rotated_to_their_tonic_before_averaging() {
        let mut learner = ProfileLearner::default();
        // A C major triad and a G major triad, louder: the same profile
        // once each sits on its tonic, whatever its level.
        let c_major = KeyClass {
            tonic: 0,
            major: true,
        };
        let g_major = KeyClass {
            tonic: 7,
            major: true,
        };
        assert!(learner.add(&[frame(&[(0, 2.0), (4, 1.0), (7, 1.0)])], c_major));
        assert!(learner.add(&[frame(&[(7, 8.0), (11, 4.0), (2, 4.0)])], g_major));
        assert!(learner.add(
            &[frame(&[(9, 1.0)]), frame(&[(0, 1.0)])],
            KeyClass {
                tonic: 9,
                major: false,
            }
        ));

        let profiles = learner.profiles().expect("both modes have a track");
        let mut major = [0.0; 12];
        major[0] = 1.0;
        major[4] = 0.5;
        major[7] = 0.5;
        assert_eq!(profiles.major, major);
        assert_eq!(profiles.major_tracks, 2);
        let mut minor = [0.0; 12];
        minor[0] = 1.0;
        minor[3] = 1.0;
        assert_eq!(profiles.minor, minor);
    }

    #[test]
    fn silence_teaches_nothing() {
        let mut learner = ProfileLearner::default();
        let key = KeyClass {
            tonic: 0,
            major: true,
        };
        assert!(!learner.add(&[vec![0.0; 12]], key));
        assert!(!learner.add(&[], key));
        assert_eq!(learner.profiles(), None);
    }

    #[test]
    fn profiles_print_as_template_arrays() {
        let profiles = KeyProfiles {
            major: [1.0, 0.25, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            minor: [1.0; 12],
            major_tracks: 3,
            minor_tracks: 4,
        };
        let rust = profiles.to_rust();
        assert!(rust.contains("// Major: 3 tracks. Minor: 4 tracks."));
        assert!(rust.contains(
            "const MAJOR_PROFILE: [f32; 12] = [1.000, 0.250, 0.500, 0.000, 0.000, \
             0.000, 0.000, 0.000, 0.000, 0.000, 0.000, 0.000];"
        ));
    }
}
