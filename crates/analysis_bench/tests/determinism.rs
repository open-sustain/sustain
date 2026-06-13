// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! End-to-end determinism of the analysis pipeline over synthetic
//! fixtures: the same inputs and config must produce identical outputs.
//! This is the CI-safe backbone of the decoder/seek/mono/waveform
//! determinism guarantee — it exercises decode, mono collapse, the
//! windowed seek, the waveform tiers, and the BPM/acoustics DSP, all
//! without committing any audio (fixtures are generated on the fly and
//! discarded).
//!
//! # Key is now deterministic (resolved by the #192 vendoring)
//!
//! The harness surfaced — on its first run — that key detection was **not**
//! deterministic in the published `stratum-dsp 1.0` the analyzer used to
//! link: `detect_key` selected the winning key with a `max_by` over a
//! `std::collections::HashMap`, so on ambiguous chroma (a tie in
//! count/confidence) the result depended on hash-map iteration order and
//! flipped between runs. The #192 ingest replaced that crate with the
//! vendored `sustain_dsp`, whose `detect_key` ranks with a total-order sort
//! and a fixed `key_sort_index` tiebreak — deterministic by construction. So
//! key is asserted here alongside BPM, acoustics, and the waveform tiers;
//! all of the analyzer's stored outputs are now byte-stable across runs.

use std::path::Path;

use sustain_analysis_bench::manifest::Manifest;
use sustain_analysis_bench::run::{Report, run_manifest};
use tempfile::TempDir;

/// A small synthetic corpus covering silence, a stereo tone (mono
/// collapse), a ramp (seek positioning), a metronome (BPM), and a triad
/// (the key code path, for crash-coverage only). Kept short so the test
/// stays fast in an unoptimized build.
const MANIFEST: &str = r#"
[meta]
corpus_id = "determinism_smoke"

[options]
tasks = ["waveform", "acoustics", "bpm", "key"]

[[track]]
id = "silence_stereo"
synthetic = { kind = "silence", secs = 4.0, channels = 2 }

[[track]]
id = "tone_440_stereo"
synthetic = { kind = "tone", freq_hz = 440.0, secs = 4.0, channels = 2 }

[[track]]
id = "ramp"
synthetic = { kind = "ramp", secs = 4.0 }

[[track]]
id = "click_120"
synthetic = { kind = "click_train", bpm = 120.0, secs = 4.0 }
bpm = 120.0

[[track]]
id = "triad_c_major"
synthetic = { kind = "triad", root_pc = 0, secs = 4.0 }
key = "C"
"#;

fn run_once(manifest: &Manifest, fixtures: &Path) -> Report {
    // Reproducible mode drops env + timings, leaving only outputs/scores.
    run_manifest(manifest, fixtures, true)
}

#[test]
fn deterministic_outputs_are_stable_across_runs() {
    let manifest: Manifest = toml::from_str(MANIFEST).expect("parse manifest");

    let first_dir = TempDir::new().expect("temp dir");
    let second_dir = TempDir::new().expect("temp dir");
    let first = run_once(&manifest, first_dir.path());
    let second = run_once(&manifest, second_dir.path());

    assert_eq!(first.tracks.len(), second.tracks.len());
    for (a, b) in first.tracks.iter().zip(&second.tracks) {
        assert_eq!(a.id, b.id);
        // Decode → mono → waveform → BPM → key → acoustics are all
        // deterministic with the vendored DSP (see the module docs).
        assert_eq!(a.bpm, b.bpm, "{}: BPM not deterministic", a.id);
        assert_eq!(a.key, b.key, "{}: key not deterministic", a.id);
        assert_eq!(
            a.acoustics, b.acoustics,
            "{}: acoustics not deterministic",
            a.id
        );
        assert_eq!(
            a.waveform, b.waveform,
            "{}: waveform not deterministic",
            a.id
        );
    }
}

#[test]
fn every_synthetic_fixture_decodes_without_error() {
    let manifest: Manifest = toml::from_str(MANIFEST).expect("parse manifest");
    let dir = TempDir::new().expect("temp dir");
    let report = run_manifest(&manifest, dir.path(), false);

    assert_eq!(
        report.summary.error_count, 0,
        "no fixture should fail to decode"
    );
    for track in &report.tracks {
        assert!(track.error.is_none(), "track {} errored", track.id);
        // A waveform tier is always produced for a decodable track, even
        // silence (an empty-but-valid pair).
        assert!(
            track.waveform.is_some(),
            "track {} produced no waveform",
            track.id
        );
    }
}
