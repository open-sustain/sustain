// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Smart Shuffle for Sustain — a local, deterministic,
//! seed-conditioned *perceptual transition scorer*.
//!
//! Given the track playing now, Smart Shuffle chooses a next track
//! from the library that feels like a *continuation* of it — the same
//! mood, flow, and thread the listener is already inside. It is a
//! **sequencer**, not a recommender: the affinity at the heart of every
//! score is a function of the *pair* (X, Y). In a hand-curated library
//! every track is already liked, so there is nothing to learn about
//! whether the user likes Y; what remains is the largely-objective,
//! perceptual question of whether Y *follows* X well. The only terms
//! about Y alone are the user's own rating, as a small prior, and
//! penalties against repeating what was just heard.
//!
//! ## No learning, by design
//!
//! There is no model and no training. Continuation is scored by a
//! fixed, interpretable, hand-weighted perceptual metric over track
//! pairs. This is deliberate: the only behavioural labels available
//! (play/skip counts, album order, playlist adjacency) are *dishonest*
//! for this task — they measure track-level engagement or teach
//! "stay on the album," the inverse of a library-wide shuffle. Fixed
//! weights here do not mean "there is nothing to learn"; they mean
//! "we refuse to learn from dishonest labels." The architecture
//! reserves a seam to learn later from *explicit* user transition
//! feedback — the one label source that actually scores the pair —
//! but builds no learning machinery now.
//!
//! ## Shape
//!
//! * [`index`] — the prepared, library-dependent state ([`SmartShuffleIndex`]):
//!   genre-token IDF, the cached per-track acoustic features, and the
//!   robust normalization ranges the timbral terms need. Rebuilt on a
//!   cadence; persisted as an opaque, schema-versioned blob. *Not* a
//!   fitted model.
//! * [`similarity`] — the per-feature, seed-conditioned similarity
//!   functions, each masked (`None`) when its feature is absent.
//! * [`affinity`] — the masked weighted sum with the coverage
//!   correction for thin evidence.
//! * [`picker`] — the four-term pipeline (guards → affinity → priors →
//!   penalties), bounded-pool temperature sampling, and the debug log.
//!
//! This crate has no Sustain-runtime dependency: it borrows `&[Track]`
//! slices and returns plain data. The runtime (`sustain_app_runtime`)
//! schedules the index rebuild, persists the blob, and feeds pick
//! context to [`pick_next_track`].

#![forbid(unsafe_code)]

pub mod affinity;
pub mod index;
pub mod picker;
mod rng;
pub mod similarity;

pub use affinity::{AffinityBreakdown, AffinityFeature, NEUTRAL_PRIOR, compute_affinity};
pub use index::{INDEX_SCHEMA_VERSION, SmartShuffleIndex, genre_tokens};
pub use picker::{
    PickContext, PickDebug, PickDebugEntry, PickMode, PickedTrack, format_debug, pick_next_track,
};

/// Errors surfaced by the Smart Shuffle index blob round-trip. The
/// runtime treats either as "discard the stored blob and rebuild from
/// scratch" — there is no migration path (pre-release).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartShuffleError {
    /// The prepared index could not be serialised to its persisted
    /// blob form.
    IndexSerialisationFailed,
    /// A stored blob could not be decoded — a different
    /// [`INDEX_SCHEMA_VERSION`], or a corrupt row.
    IndexDeserialisationFailed,
}
