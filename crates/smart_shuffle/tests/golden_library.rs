// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Golden-library counterfactual tests (§15 of the design brief) — the
//! automated regression guards that must hold on every weight or
//! normalization change. They exercise the whole public pipeline (index
//! build → affinity → loudness guard → pick) against hand-built
//! libraries with known structure, asserting *relative* outcomes rather
//! than absolute scores so they stay robust to constant tuning.

use std::time::SystemTime;

use sustain_domain::{
    AcousticFeatures, PlayStatistics, Rating, SmartShuffleEntropy, Track, TrackId, TrackLocation,
    TrackMetadata, TrackRelativePath,
};
use sustain_smart_shuffle::affinity::AffinityFeature;
use sustain_smart_shuffle::{PickContext, SmartShuffleIndex, compute_affinity, pick_next_track};

/// A library track with a genre and a BPM (the metadata the affinity
/// terms read directly).
fn track(id: i64, genre: &str, bpm: u32) -> Track {
    Track {
        id: TrackId::new(id).expect("valid id"),
        location: TrackLocation::available(
            TrackRelativePath::new(format!("t/{id}.flac")).expect("relative path"),
        ),
        metadata: TrackMetadata {
            genre: Some(genre.to_owned()),
            bpm: Some(bpm),
            ..TrackMetadata::default()
        },
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    }
}

/// Quiet, sparse, dark, dynamic — the "ambient" cluster.
fn ambient_acoustics(integrated: f32, onset: f32) -> AcousticFeatures {
    AcousticFeatures {
        integrated_lufs: integrated,
        short_term_lufs_max: integrated + 3.0,
        loudness_range_lu: 12.0,
        onset_rate_hz: onset,
        low_band_ratio: 0.65,
        mid_band_ratio: 0.30,
        high_band_ratio: 0.05,
        low_band_variation: 0.20,
        tonalness: 0.80,
    }
}

/// Loud, busy, bright, compressed — the "techno" cluster.
fn techno_acoustics(integrated: f32, onset: f32) -> AcousticFeatures {
    AcousticFeatures {
        integrated_lufs: integrated,
        short_term_lufs_max: integrated + 2.0,
        loudness_range_lu: 3.0,
        onset_rate_hz: onset,
        low_band_ratio: 0.35,
        mid_band_ratio: 0.35,
        high_band_ratio: 0.30,
        low_band_variation: 0.85,
        tonalness: 0.40,
    }
}

/// §15 — *bimodal library*: in-cluster pairs must out-score
/// cross-cluster pairs by a wide margin. Ambient (−23 LUFS, 80 BPM,
/// sparse, dark) vs techno (−6 LUFS, 130 BPM, busy, bright).
#[test]
fn in_cluster_pairs_outscore_cross_cluster_by_a_wide_margin() {
    let tracks = [
        track(1, "Ambient", 80),
        track(2, "Ambient", 82),
        track(3, "Techno", 130),
        track(4, "Techno", 128),
    ];
    let acoustics = vec![
        (tracks[0].id, ambient_acoustics(-23.0, 0.5)),
        (tracks[1].id, ambient_acoustics(-22.0, 0.6)),
        (tracks[2].id, techno_acoustics(-6.0, 6.0)),
        (tracks[3].id, techno_acoustics(-6.5, 6.2)),
    ];
    let index = SmartShuffleIndex::build(&tracks, &acoustics, 0);

    let in_cluster = compute_affinity(Some(&index), &tracks[0], &tracks[1])
        .expect("ambient pair shares features")
        .final_affinity;
    let cross_cluster = compute_affinity(Some(&index), &tracks[0], &tracks[2])
        .expect("ambient↔techno still share some features")
        .final_affinity;

    assert!(
        in_cluster > cross_cluster + 0.3,
        "in-cluster {in_cluster} must beat cross-cluster {cross_cluster} by a wide margin"
    );
}

/// §15 — *guard test*: a candidate with otherwise-perfect affinity but a
/// loudness jump beyond the ascent threshold is excluded **regardless of
/// the exploration setting** — guards prune before the pool, so a high
/// temperature can never rescue it.
#[test]
fn loudness_guard_excludes_a_loud_jump_under_every_exploration_mode() {
    // Seed and candidate are acoustically/metadata-identical except the
    // candidate's short-term peak is +15 LUFS — a brickwall after a
    // quiet master. A clone at a safe level is the only valid follow.
    let seed = track(1, "Ambient", 90);
    let loud_jump = track(2, "Ambient", 90);
    let safe = track(3, "Ambient", 90);
    let tracks = [seed.clone(), loud_jump.clone(), safe.clone()];
    let acoustics = vec![
        (seed.id, ambient_acoustics(-20.0, 0.5)),
        // Same everything, but a startling short-term peak.
        (
            loud_jump.id,
            AcousticFeatures {
                short_term_lufs_max: -2.0, // +15 LUFS over the seed's −17
                ..ambient_acoustics(-20.0, 0.5)
            },
        ),
        (safe.id, ambient_acoustics(-19.0, 0.5)),
    ];
    let index = SmartShuffleIndex::build(&tracks, &acoustics, 0);
    let candidates = [&loud_jump, &safe];
    let history = [&seed];

    for entropy in [
        SmartShuffleEntropy::Focused,
        SmartShuffleEntropy::Balanced,
        SmartShuffleEntropy::Adventurous,
    ] {
        let context = PickContext {
            seed: &seed,
            candidates: &candidates,
            played_history: &history,
            entropy,
            now: SystemTime::UNIX_EPOCH,
        };
        let (pick, _) = pick_next_track(Some(&index), context).expect("the safe follow survives");
        assert_eq!(
            pick.track_id, safe.id,
            "the +15 LUFS jump must be guarded out under {entropy:?}"
        );
    }
}

/// §15 — *guard test, sole-candidate case*: when the only candidate is a
/// catastrophic loudness jump, the guard prunes it and the pick yields
/// nothing rather than serving the jump.
#[test]
fn loudness_guard_yields_no_pick_when_the_only_candidate_is_a_loud_jump() {
    let seed = track(1, "Ambient", 90);
    let loud_jump = track(2, "Ambient", 90);
    let tracks = [seed.clone(), loud_jump.clone()];
    let acoustics = vec![
        (seed.id, ambient_acoustics(-20.0, 0.5)),
        (
            loud_jump.id,
            AcousticFeatures {
                short_term_lufs_max: -2.0,
                ..ambient_acoustics(-20.0, 0.5)
            },
        ),
    ];
    let index = SmartShuffleIndex::build(&tracks, &acoustics, 0);
    let candidates = [&loud_jump];
    let history = [&seed];
    let context = PickContext {
        seed: &seed,
        candidates: &candidates,
        played_history: &history,
        entropy: SmartShuffleEntropy::Adventurous,
        now: SystemTime::UNIX_EPOCH,
    };
    assert!(
        pick_next_track(Some(&index), context).is_none(),
        "a guarded-out sole candidate must not be served"
    );
}

/// §15 — *guard descent asymmetry*: the same-magnitude jump that is
/// fatal upward is tolerated downward — going into a quieter track is a
/// natural breakdown, so a −15 LUFS descent stays eligible where a +15
/// ascent is pruned.
#[test]
fn loudness_guard_tolerates_the_descent_it_forbids_on_ascent() {
    let seed = track(1, "Ambient", 90);
    let quieter = track(2, "Ambient", 90);
    let tracks = [seed.clone(), quieter.clone()];
    let acoustics = vec![
        // Seed peaks loud.
        (
            seed.id,
            AcousticFeatures {
                short_term_lufs_max: -3.0,
                ..ambient_acoustics(-8.0, 0.5)
            },
        ),
        // Candidate peaks ~13 LUFS quieter — within the descent tolerance.
        (
            quieter.id,
            AcousticFeatures {
                short_term_lufs_max: -16.0,
                ..ambient_acoustics(-20.0, 0.5)
            },
        ),
    ];
    let index = SmartShuffleIndex::build(&tracks, &acoustics, 0);
    let candidates = [&quieter];
    let history = [&seed];
    let context = PickContext {
        seed: &seed,
        candidates: &candidates,
        played_history: &history,
        entropy: SmartShuffleEntropy::Focused,
        now: SystemTime::UNIX_EPOCH,
    };
    let (pick, _) = pick_next_track(Some(&index), context).expect("the quieter follow is eligible");
    assert_eq!(pick.track_id, quieter.id);
}

/// §15 — *missing-feature test*: a candidate the user has not run audio
/// analysis on is masked on every acoustic term — neither penalized nor
/// bonused — so it is scored honestly on the metadata it does have.
#[test]
fn unanalysed_candidate_is_masked_on_acoustic_terms_not_penalized() {
    let seed = track(1, "Ambient", 80);
    let analysed = track(2, "Ambient", 80);
    let unanalysed = track(3, "Ambient", 80);
    let tracks = [seed.clone(), analysed.clone(), unanalysed.clone()];
    // Only the seed and one candidate carry acoustics.
    let acoustics = vec![
        (seed.id, ambient_acoustics(-20.0, 0.5)),
        (analysed.id, ambient_acoustics(-20.0, 0.5)),
    ];
    let index = SmartShuffleIndex::build(&tracks, &acoustics, 0);

    let analysed_breakdown =
        compute_affinity(Some(&index), &seed, &analysed).expect("shares features");
    let unanalysed_breakdown =
        compute_affinity(Some(&index), &seed, &unanalysed).expect("shares metadata features");

    // The acoustic terms are present for the analysed candidate and
    // masked (None) for the un-analysed one.
    let acoustic_features = [
        AffinityFeature::Loudness,
        AffinityFeature::OnsetDensity,
        AffinityFeature::Brightness,
        AffinityFeature::Tonalness,
        AffinityFeature::LowBandVariation,
        AffinityFeature::DynamicRange,
    ];
    for feature in acoustic_features {
        assert!(
            similarity_of(&analysed_breakdown, feature).is_some(),
            "{feature:?} should be present for the analysed candidate"
        );
        assert!(
            similarity_of(&unanalysed_breakdown, feature).is_none(),
            "{feature:?} must be masked for the un-analysed candidate"
        );
    }

    // Masking is not a penalty: the un-analysed candidate keeps a
    // healthy score (it agrees on every metadata feature it has). The
    // analysed one merely has *more* evidence (higher coverage).
    assert!(
        unanalysed_breakdown.final_affinity > 0.5,
        "masked acoustic terms must not drag the score down: {}",
        unanalysed_breakdown.final_affinity
    );
    assert!(
        analysed_breakdown.coverage > unanalysed_breakdown.coverage,
        "the analysed candidate votes with more weight"
    );
}

/// Read one feature's similarity out of a breakdown (the per-feature
/// contributions are exposed for exactly this kind of assertion).
fn similarity_of(
    breakdown: &sustain_smart_shuffle::AffinityBreakdown,
    feature: AffinityFeature,
) -> Option<f32> {
    breakdown
        .contributions
        .iter()
        .find(|c| c.feature == feature)
        .and_then(|c| c.similarity)
}
