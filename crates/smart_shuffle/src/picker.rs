// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The next-track picker — the four-term, fully transparent pipeline
//! the design brief (§4) specifies:
//!
//! ```text
//! 1. eligibility guards   → can Y follow X at all? (hard prune)
//! 2. perceptual affinity  → how well does Y continue X? (the metric)
//! 3. candidate priors     → small standalone nudges (rating)
//! 4. candidate penalties  → fatigue / anti-repetition / anti-clump
//!                ↓
//!     bounded pool → temperature sampling → full debug log
//! ```
//!
//! Keeping the four terms apart is exactly what the discarded design
//! got wrong (it folded a standalone "do I like Y" prior into the pair
//! score at 60%). Here the affinity is *only* the pairwise metric;
//! rating is a small after-the-fact nudge; fatigue and anti-clump are
//! separate subtractions; and catastrophic transitions are pruned by
//! guards *before* the pool is formed, so the Exploration slider can
//! never "rescue" them.
//!
//! The picker is deterministic by construction (§14): it seeds its
//! sampler from the inputs — seed track, recent history, schema
//! version, exploration mode — never the wall clock, so the
//! `SUSTAIN_LOG_SMART_SHUFFLE=1` trace is reproducible after the fact.
//! Folding recent history into the seed means the same seed yields
//! different (but reproducible) picks as the session evolves.

use std::collections::HashSet;
use std::time::SystemTime;

use sustain_domain::{SmartShuffleEntropy, Track, TrackId};

use crate::affinity::{self, AffinityBreakdown, AffinityFeature, NEUTRAL_PRIOR};
use crate::index::{INDEX_SCHEMA_VERSION, SmartShuffleIndex};
use crate::rng::SplitMix64;

/// Read-only inputs the runtime hands the picker for one transition.
/// `candidates` is the library-wide pool (the runtime has already
/// dropped unavailable files); `played_history` is this session's
/// played tracks, **most-recent-last**, used for anti-repetition and
/// the same-artist streak.
pub struct PickContext<'a> {
    pub seed: &'a Track,
    pub candidates: &'a [&'a Track],
    pub played_history: &'a [&'a Track],
    pub entropy: SmartShuffleEntropy,
    pub now: SystemTime,
}

/// The chosen track plus how it was scored.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PickedTrack {
    pub track_id: TrackId,
    pub mode: PickMode,
}

/// Whether the winning candidate was scored by the perceptual metric
/// (it shared at least one feature with the seed) or fell back to the
/// neutral prior (it shared none, so it was drawn essentially at
/// random for this pick — §5's degenerate policy).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PickMode {
    Perceptual,
    Fallback,
}

// --- Candidate-side prior / penalty constants (§6.4, §10) -------------------

/// Recency half-life: a track played this long ago carries half of its
/// peak fatigue penalty. The penalty decays smoothly toward zero so
/// every track eventually becomes fully eligible again — without this,
/// a long session's tail goes random once the unplayed set drains.
const RECENCY_HALF_LIFE_SECS: f32 = 45.0 * 60.0;

/// Score weight of the recency/fatigue penalty. Strong: a just-played
/// track is pushed well below fresh candidates.
const RECENCY_PENALTY_WEIGHT: f32 = 1.0;

/// A recent *skip* counts as fatigue too, but more softly than a play.
const SKIP_FATIGUE_FACTOR: f32 = 0.5;

/// Penalty for a candidate already played earlier in this session.
/// Distinct from recency (which is time-decayed and survives
/// restarts): this is the "do not loop within the session" guard, and
/// it does not wait for the play-count threshold to register.
const SESSION_REPEAT_PENALTY: f32 = 0.6;

/// Per-step escalation of the same-artist-streak penalty, applied to a
/// candidate by the same artist as the current trailing run. Mild, so
/// Smart feels *less* artist-clumpy than Pure without banning an
/// artist outright.
const SAME_ARTIST_STREAK_STEP: f32 = 0.15;

/// Cap on the streak length the penalty escalates over.
const SAME_ARTIST_STREAK_CAP: u32 = 6;

/// Anti-album-walk-back: a small penalty for a candidate on the seed's
/// own album. Same-album affinity is deliberately neutral (§6.3) and
/// here mildly negative — mechanically wandering back into the seed's
/// album during a *library-wide* shuffle is the failure mode, not the
/// goal. If the listener wanted the album, they would play the album.
const SAME_ALBUM_PENALTY: f32 = 0.10;

/// Asymmetric loudness guard, ascent half (§7): a candidate whose
/// short-term loudness *peak* sits more than this many LUFS above the
/// seed's is excluded outright — going from a quiet master into a
/// brickwalled one *startles*, and no genre/tempo match should rescue
/// it (a guard, not a weight, so the Exploration slider cannot override
/// it). Keyed off short-term max, not integrated LUFS, because the
/// jarring boundary is the loud peak, not the whole-track average.
const LOUDNESS_GUARD_ASCENT_LUFS: f32 = 8.0;

/// Asymmetric loudness guard, descent half (§7): going *down* in
/// loudness is a natural breakdown the ear forgives, so the quiet
/// direction is tolerated far further than the loud one before the
/// transition is excluded.
const LOUDNESS_GUARD_DESCENT_LUFS: f32 = 14.0;

/// Hard cap on how many top candidates the debug logger prints.
const DEBUG_TOP_K: usize = 6;

/// How the Exploration slider widens the search. Two dials, not one
/// (§11): a bounded candidate pool *and* a softmax temperature. Guards
/// prune before the pool, so even Adventurous stays coherent — a wider
/// net with flatter sampling inside it, never a reach into the
/// catastrophic tail.
struct Exploration {
    /// Pool size as a fraction of the eligible candidate count…
    pool_fraction: f32,
    /// …clamped to at least this many…
    pool_floor: usize,
    /// …and at most this many.
    pool_cap: usize,
    /// Softmax temperature: higher = flatter sampling within the pool.
    temperature: f32,
}

impl Exploration {
    fn for_entropy(entropy: SmartShuffleEntropy) -> Self {
        // The temperature comes from the domain enum (shared with the
        // settings layer); the pool dial lives here because it is a
        // picker concern. Fractions/floors/caps are starting points to
        // tune against a real library (§11).
        let temperature = entropy.temperature();
        match entropy {
            SmartShuffleEntropy::Focused => Self {
                pool_fraction: 0.01,
                pool_floor: 8,
                pool_cap: 50,
                temperature,
            },
            SmartShuffleEntropy::Balanced => Self {
                pool_fraction: 0.05,
                pool_floor: 20,
                pool_cap: 200,
                temperature,
            },
            SmartShuffleEntropy::Adventurous => Self {
                pool_fraction: 0.15,
                pool_floor: 40,
                pool_cap: 600,
                temperature,
            },
        }
    }

    /// Pool size for `eligible` survivors, clamped to the floor/cap and
    /// never larger than the eligible set itself.
    fn pool_size(&self, eligible: usize) -> usize {
        let target = (self.pool_fraction * eligible as f32).round() as usize;
        target
            .clamp(self.pool_floor, self.pool_cap)
            .min(eligible)
            .max(1)
    }
}

/// A fully-scored candidate, retained so the debug log can show the
/// whole decomposition.
struct Scored<'a> {
    track: &'a Track,
    affinity: Option<AffinityBreakdown>,
    affinity_value: f32,
    rating_prior: f32,
    recency_penalty: f32,
    streak_penalty: f32,
    album_penalty: f32,
    session_penalty: f32,
    score: f32,
}

/// Choose the next track to follow `context.seed`. Returns `None` only
/// when there is genuinely nothing to pick (no eligible candidate).
/// The optional [`PickDebug`] is populated only when
/// `SUSTAIN_LOG_SMART_SHUFFLE=1` is set.
pub fn pick_next_track(
    index: Option<&SmartShuffleIndex>,
    context: PickContext<'_>,
) -> Option<(PickedTrack, Option<PickDebug>)> {
    // Step 1 — eligibility guards. The seed cannot follow itself; the
    // runtime has already dropped unavailable files; and the asymmetric
    // loudness guard (§7) prunes catastrophic level jumps *before* the
    // pool is formed, so no Exploration setting can rescue them. The
    // guard masks gracefully — a candidate (or seed) without acoustics
    // is never excluded, only un-guarded.
    let played_ids: HashSet<TrackId> = context.played_history.iter().map(|t| t.id).collect();
    let streak = same_artist_streak(context.seed, context.played_history);

    let mut scored: Vec<Scored<'_>> = context
        .candidates
        .iter()
        .filter(|cand| cand.id != context.seed.id)
        .filter(|cand| passes_loudness_guard(index, context.seed, cand))
        .map(|cand| score_candidate(index, &context, cand, &played_ids, &streak))
        .collect();

    if scored.is_empty() {
        return None;
    }

    // Deterministic ordering: by score desc, ties broken by track id so
    // the pool boundary and the debug log are reproducible.
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.track.id.get().cmp(&b.track.id.get()))
    });

    // Step 7 — bounded pool from the survivors.
    let exploration = Exploration::for_entropy(context.entropy);
    let pool_size = exploration.pool_size(scored.len());
    let pool = &scored[..pool_size];

    // Step 8 — temperature sampling within the pool.
    let mut rng = SplitMix64::new(sampler_seed(&context, context.entropy));
    let (chosen_index, probabilities) = softmax_sample(pool, exploration.temperature, &mut rng);
    let chosen = &pool[chosen_index];

    let mode = match &chosen.affinity {
        Some(_) => PickMode::Perceptual,
        None => PickMode::Fallback,
    };

    let debug = std::env::var_os("SUSTAIN_LOG_SMART_SHUFFLE")
        .filter(|value| value == "1")
        .map(|_| {
            build_debug(
                context.seed.id,
                &scored,
                pool_size,
                &probabilities,
                chosen.track.id,
            )
        });

    Some((
        PickedTrack {
            track_id: chosen.track.id,
            mode,
        },
        debug,
    ))
}

fn score_candidate<'a>(
    index: Option<&SmartShuffleIndex>,
    context: &PickContext<'a>,
    cand: &'a Track,
    played_ids: &HashSet<TrackId>,
    streak: &ArtistStreak,
) -> Scored<'a> {
    // Step 2 — perceptual affinity. A candidate sharing no feature with
    // the seed has no affinity; it sits at the neutral prior so it can
    // still be drawn (especially in Adventurous) without dominating.
    let affinity = affinity::compute_affinity(index, context.seed, cand);
    let affinity_value = affinity
        .as_ref()
        .map(|b| b.final_affinity)
        .unwrap_or(NEUTRAL_PRIOR);

    // Step 3 — candidate prior.
    let rating_prior = rating_prior(cand);

    // Step 4 — candidate penalties.
    let recency_penalty = RECENCY_PENALTY_WEIGHT * recency_fatigue(cand, context.now);
    let session_penalty = if played_ids.contains(&cand.id) {
        SESSION_REPEAT_PENALTY
    } else {
        0.0
    };
    let streak_penalty = streak.penalty_for(cand);
    let album_penalty = same_album_penalty(context.seed, cand);

    let score = affinity_value + rating_prior
        - recency_penalty
        - session_penalty
        - streak_penalty
        - album_penalty;

    Scored {
        track: cand,
        affinity,
        affinity_value,
        rating_prior,
        recency_penalty,
        streak_penalty,
        album_penalty,
        session_penalty,
        score,
    }
}

/// Small additive nudge toward higher-rated candidates, applied after
/// the pairwise distance (§6.4). "Same rating as the seed" is
/// meaningless — a 5★ punk track is not a good follow to a 5★ ambient
/// one *because* they share a rating — so rating nudges, never
/// decides. Unrated (0★) is "not judged," not "bad", so it is never
/// penalised.
fn rating_prior(cand: &Track) -> f32 {
    match cand.rating.stars() {
        1 => -0.04,
        2 => -0.02,
        4 => 0.03,
        5 => 0.06,
        // 0 (unrated) and 3 (neutral) → no nudge.
        _ => 0.0,
    }
}

/// Smooth recency/fatigue penalty in `[0, 1]`. Driven by whichever is
/// more recent of `last_played_at` / `last_skipped_at`, with skips
/// counting more softly than plays. Exponential decay so a track
/// played five minutes ago is strongly demoted, an hour ago partly,
/// three hours ago essentially neutral.
fn recency_fatigue(cand: &Track, now: SystemTime) -> f32 {
    let played = cand
        .statistics
        .last_played_at
        .map(|at| decay(at, now))
        .unwrap_or(0.0);
    let skipped = cand
        .statistics
        .last_skipped_at
        .map(|at| SKIP_FATIGUE_FACTOR * decay(at, now))
        .unwrap_or(0.0);
    played.max(skipped)
}

fn decay(event: SystemTime, now: SystemTime) -> f32 {
    let Ok(elapsed) = now.duration_since(event) else {
        // Clock went backwards (suspend/resume) — treat as "just now".
        return 1.0;
    };
    let time_constant = RECENCY_HALF_LIFE_SECS / std::f32::consts::LN_2;
    (-elapsed.as_secs_f32() / time_constant).exp()
}

/// The trailing run of same-artist picks at the end of the session.
struct ArtistStreak {
    /// Trimmed, lower-cased artist currently clumping, if any.
    streak_artist: Option<String>,
    /// How many consecutive most-recent picks share it.
    length: u32,
}

impl ArtistStreak {
    fn penalty_for(&self, cand: &Track) -> f32 {
        let Some(streak_artist) = self.streak_artist.as_deref() else {
            return 0.0;
        };
        let Some(cand_artist) = normalized_artist(cand) else {
            return 0.0;
        };
        if cand_artist == streak_artist {
            SAME_ARTIST_STREAK_STEP * self.length.min(SAME_ARTIST_STREAK_CAP) as f32
        } else {
            0.0
        }
    }
}

fn normalized_artist(track: &Track) -> Option<String> {
    track
        .metadata
        .artist
        .as_deref()
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_ascii_lowercase)
}

/// Count the trailing run of tracks (most-recent-last) sharing the
/// seed's artist. A run of length `n` means the picker has landed on
/// this artist `n` times in a row; a candidate by the same artist is
/// then penalised proportionally.
fn same_artist_streak(seed: &Track, played_history: &[&Track]) -> ArtistStreak {
    let Some(seed_artist) = normalized_artist(seed) else {
        return ArtistStreak {
            streak_artist: None,
            length: 0,
        };
    };
    let mut length = 0_u32;
    for track in played_history.iter().rev() {
        match normalized_artist(track) {
            Some(artist) if artist == seed_artist => length += 1,
            _ => break,
        }
    }
    ArtistStreak {
        streak_artist: Some(seed_artist),
        length,
    }
}

fn same_album_penalty(seed: &Track, cand: &Track) -> f32 {
    match (
        seed.metadata.album.as_deref(),
        cand.metadata.album.as_deref(),
    ) {
        (Some(seed_album), Some(cand_album))
            if !seed_album.trim().is_empty()
                && seed_album.trim().eq_ignore_ascii_case(cand_album.trim()) =>
        {
            SAME_ALBUM_PENALTY
        }
        _ => 0.0,
    }
}

/// The asymmetric loudness guard (§7): may `cand` follow `seed` at all?
/// Compares the two tracks' short-term loudness *peaks* — the boundary
/// the ear actually hits — and excludes a jump that is too loud upward
/// (the startle case) or implausibly far downward. Degrades gracefully:
/// with no index, or when either track lacks acoustics, the guard
/// cannot evaluate and so never excludes (a hole is not a signal, §5).
fn passes_loudness_guard(index: Option<&SmartShuffleIndex>, seed: &Track, cand: &Track) -> bool {
    let Some(index) = index else {
        return true;
    };
    let (Some(seed_a), Some(cand_a)) = (index.acoustics(seed.id), index.acoustics(cand.id)) else {
        return true;
    };
    let delta = cand_a.short_term_lufs_max - seed_a.short_term_lufs_max;
    (-LOUDNESS_GUARD_DESCENT_LUFS..=LOUDNESS_GUARD_ASCENT_LUFS).contains(&delta)
}

/// Softmax-sample one candidate from the pool, returning its index and
/// the full probability vector (for the debug log). Numerically stable
/// (subtract the max before `exp`). Falls back to a uniform draw if the
/// distribution degenerates.
fn softmax_sample(
    pool: &[Scored<'_>],
    temperature: f32,
    rng: &mut SplitMix64,
) -> (usize, Vec<f32>) {
    let temperature = temperature.max(1e-3);
    let max_score = pool
        .iter()
        .map(|c| c.score)
        .fold(f32::NEG_INFINITY, f32::max);
    let weights: Vec<f32> = pool
        .iter()
        .map(|c| ((c.score - max_score) / temperature).exp())
        .collect();
    let total: f32 = weights.iter().sum();
    if !total.is_finite() || total <= 0.0 {
        let index = rng.next_bounded(pool.len());
        let uniform = 1.0 / pool.len() as f32;
        return (index, vec![uniform; pool.len()]);
    }
    let probabilities: Vec<f32> = weights.iter().map(|w| w / total).collect();
    let pick = rng.next_unit_interval();
    let mut cumulative = 0.0;
    for (index, probability) in probabilities.iter().enumerate() {
        cumulative += probability;
        if pick <= cumulative {
            return (index, probabilities);
        }
    }
    (pool.len() - 1, probabilities)
}

/// Deterministic sampler seed: folds the seed track id, the recent
/// history (length plus the trailing ids), the exploration mode, and
/// the index schema version. Reproducible, but evolves with the
/// session so the same seed does not play the same successor forever.
fn sampler_seed(context: &PickContext<'_>, entropy: SmartShuffleEntropy) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(context.seed.id.get().unsigned_abs());
    mix(context.played_history.len() as u64);
    // The trailing few history ids — enough to evolve the seed without
    // walking the whole (possibly long) history each pick.
    for track in context.played_history.iter().rev().take(8) {
        mix(track.id.get().unsigned_abs());
    }
    mix(entropy as u64);
    mix(u64::from(INDEX_SCHEMA_VERSION));
    hash
}

// --- Debug trace (§14) ------------------------------------------------------

/// Per-candidate breakdown retained for the `SUSTAIN_LOG_SMART_SHUFFLE`
/// trace. Mirrors the picker's internal `Scored`, plus the pool rank
/// and sample probability.
#[derive(Clone, Debug)]
pub struct PickDebugEntry {
    pub track_id: TrackId,
    pub contributions: Vec<(AffinityFeature, Option<f32>)>,
    pub affinity: Option<f32>,
    pub coverage: Option<f32>,
    pub final_affinity: f32,
    pub rating_prior: f32,
    pub recency_penalty: f32,
    pub streak_penalty: f32,
    pub album_penalty: f32,
    pub session_penalty: f32,
    pub total_score: f32,
    /// 1-based rank within the bounded pool, or `None` if outside it.
    pub pool_rank: Option<usize>,
    pub sample_prob: Option<f32>,
    /// Whether this is the candidate that was ultimately chosen.
    pub chosen: bool,
}

/// Top candidates with their score breakdown, ordered by score.
#[derive(Clone, Debug)]
pub struct PickDebug {
    pub seed_track_id: TrackId,
    pub entries: Vec<PickDebugEntry>,
}

fn build_debug(
    seed_track_id: TrackId,
    scored: &[Scored<'_>],
    pool_size: usize,
    probabilities: &[f32],
    chosen_id: TrackId,
) -> PickDebug {
    let entries = scored
        .iter()
        .take(DEBUG_TOP_K)
        .enumerate()
        .map(|(rank, entry)| {
            let in_pool = rank < pool_size;
            PickDebugEntry {
                track_id: entry.track.id,
                contributions: entry
                    .affinity
                    .as_ref()
                    .map(|b| {
                        b.contributions
                            .iter()
                            .map(|c| (c.feature, c.similarity))
                            .collect()
                    })
                    .unwrap_or_default(),
                affinity: entry.affinity.as_ref().map(|b| b.affinity),
                coverage: entry.affinity.as_ref().map(|b| b.coverage),
                final_affinity: entry.affinity_value,
                rating_prior: entry.rating_prior,
                recency_penalty: entry.recency_penalty,
                streak_penalty: entry.streak_penalty,
                album_penalty: entry.album_penalty,
                session_penalty: entry.session_penalty,
                total_score: entry.score,
                pool_rank: in_pool.then_some(rank + 1),
                sample_prob: probabilities.get(rank).copied().filter(|_| in_pool),
                chosen: entry.track.id == chosen_id,
            }
        })
        .collect();
    PickDebug {
        seed_track_id,
        entries,
    }
}

/// Format a [`PickDebug`] for stderr. The runtime supplies a
/// `track_label` closure so the picker stays ignorant of how a track
/// id renders as a human-readable name.
pub fn format_debug(debug: &PickDebug, track_label: impl Fn(TrackId) -> String) -> String {
    let mut lines = vec![format!(
        "[smart-shuffle] seed: {}",
        track_label(debug.seed_track_id)
    )];
    for entry in &debug.entries {
        let marker = if entry.chosen { "→" } else { " " };
        lines.push(format!(
            "{marker} candidate: {}",
            track_label(entry.track_id)
        ));
        for (feature, similarity) in &entry.contributions {
            match similarity {
                Some(value) => lines.push(format!(
                    "      {:<16} {:.2} · {:.2} = {:.3}",
                    feature.label(),
                    value,
                    feature.weight(),
                    value * feature.weight(),
                )),
                None => lines.push(format!("      {:<16} masked", feature.label())),
            }
        }
        match (entry.affinity, entry.coverage) {
            (Some(affinity), Some(coverage)) => lines.push(format!(
                "      affinity {:.3}  coverage {:.2}  → final {:.3}",
                affinity, coverage, entry.final_affinity
            )),
            _ => lines.push(format!(
                "      no shared feature → neutral prior {:.3}",
                entry.final_affinity
            )),
        }
        lines.push(format!(
            "      rating {:+.3}  recency {:-.3}  streak {:-.3}  album {:-.3}  session {:-.3}",
            entry.rating_prior,
            entry.recency_penalty,
            entry.streak_penalty,
            entry.album_penalty,
            entry.session_penalty,
        ));
        let pool = match (entry.pool_rank, entry.sample_prob) {
            (Some(rank), Some(prob)) => format!("pool #{rank}  p={prob:.3}"),
            _ => "outside pool".to_owned(),
        };
        lines.push(format!("      score {:+.3}  {pool}", entry.total_score));
        if entry.chosen {
            if let Some(reason) = leading_reason(entry) {
                lines.push(format!("      picked — {reason}"));
            }
        }
    }
    lines.join("\n")
}

/// Name the single largest positive affinity contributor for the
/// one-line "why this track" summary.
fn leading_reason(entry: &PickDebugEntry) -> Option<String> {
    entry
        .contributions
        .iter()
        .filter_map(|(feature, similarity)| {
            similarity.map(|value| (feature, value * feature.weight()))
        })
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(feature, contribution)| format!("{} match (+{:.2})", feature.label(), contribution))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use sustain_domain::{
        PlayStatistics, Rating, SmartShuffleEntropy, Track, TrackId, TrackLocation, TrackMetadata,
        TrackRelativePath,
    };

    use super::{PickContext, PickMode, pick_next_track};

    fn track(id: i64, metadata: TrackMetadata) -> Track {
        Track {
            id: TrackId::new(id).expect("valid id"),
            location: TrackLocation::available(
                TrackRelativePath::new(format!("t/{id}.flac")).expect("relative path"),
            ),
            metadata,
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn ctx<'a>(
        seed: &'a Track,
        candidates: &'a [&'a Track],
        history: &'a [&'a Track],
    ) -> PickContext<'a> {
        PickContext {
            seed,
            candidates,
            played_history: history,
            entropy: SmartShuffleEntropy::Balanced,
            now: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn returns_none_without_candidates() {
        let seed = track(1, TrackMetadata::default());
        assert!(pick_next_track(None, ctx(&seed, &[], &[&seed])).is_none());
    }

    #[test]
    fn never_picks_the_seed() {
        let seed = track(
            1,
            TrackMetadata {
                genre: Some("Rock".to_owned()),
                ..TrackMetadata::default()
            },
        );
        let other = track(
            2,
            TrackMetadata {
                genre: Some("Rock".to_owned()),
                ..TrackMetadata::default()
            },
        );
        let candidates = [&seed, &other];
        let (pick, _) = pick_next_track(None, ctx(&seed, &candidates, &[&seed])).expect("pick");
        assert_eq!(pick.track_id, other.id);
    }

    #[test]
    fn is_deterministic_for_identical_inputs() {
        let seed = track(
            7,
            TrackMetadata {
                genre: Some("House".to_owned()),
                bpm: Some(124),
                ..TrackMetadata::default()
            },
        );
        let pool: Vec<Track> = (1..=40)
            .map(|i| {
                track(
                    i,
                    TrackMetadata {
                        genre: Some(if i % 2 == 0 { "House" } else { "Folk" }.to_owned()),
                        bpm: Some(if i % 2 == 0 { 126 } else { 90 }),
                        ..TrackMetadata::default()
                    },
                )
            })
            .collect();
        let refs: Vec<&Track> = pool.iter().collect();
        let history = [&seed];
        let first = pick_next_track(None, ctx(&seed, &refs, &history))
            .expect("first")
            .0;
        let second = pick_next_track(None, ctx(&seed, &refs, &history))
            .expect("second")
            .0;
        assert_eq!(first.track_id, second.track_id);
    }

    #[test]
    fn focused_mode_prefers_the_strongest_continuation() {
        // A clearly-best match (same genre + tempo) among many poor
        // ones must win under Focused (near-argmax) sampling.
        let seed = track(
            100,
            TrackMetadata {
                genre: Some("Ambient".to_owned()),
                bpm: Some(80),
                ..TrackMetadata::default()
            },
        );
        let best = track(
            200,
            TrackMetadata {
                genre: Some("Ambient".to_owned()),
                bpm: Some(80),
                ..TrackMetadata::default()
            },
        );
        let mut candidates: Vec<Track> = vec![best.clone()];
        for i in 0..30 {
            candidates.push(track(
                i + 1,
                TrackMetadata {
                    genre: Some("Speedcore".to_owned()),
                    bpm: Some(220),
                    ..TrackMetadata::default()
                },
            ));
        }
        let refs: Vec<&Track> = candidates.iter().collect();
        let history = [&seed];
        let context = PickContext {
            entropy: SmartShuffleEntropy::Focused,
            ..ctx(&seed, &refs, &history)
        };
        let (pick, _) = pick_next_track(None, context).expect("pick");
        assert_eq!(pick.track_id, best.id);
        assert_eq!(pick.mode, PickMode::Perceptual);
    }

    #[test]
    fn recently_played_tracks_are_demoted() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(3 * 60 * 60);
        let seed = track(
            1,
            TrackMetadata {
                genre: Some("Rock".to_owned()),
                ..TrackMetadata::default()
            },
        );
        // Two identical candidates; one was played one minute ago.
        let fresh = track(
            2,
            TrackMetadata {
                genre: Some("Rock".to_owned()),
                ..TrackMetadata::default()
            },
        );
        let mut stale = track(
            3,
            TrackMetadata {
                genre: Some("Rock".to_owned()),
                ..TrackMetadata::default()
            },
        );
        stale.statistics.last_played_at = Some(now - Duration::from_secs(60));
        let candidates = [&fresh, &stale];
        let history = [&seed];
        let context = PickContext {
            entropy: SmartShuffleEntropy::Focused,
            now,
            ..ctx(&seed, &candidates, &history)
        };
        let (pick, _) = pick_next_track(None, context).expect("pick");
        assert_eq!(
            pick.track_id, fresh.id,
            "the just-played track should be demoted"
        );
    }
}
