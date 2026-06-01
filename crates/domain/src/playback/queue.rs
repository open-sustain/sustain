// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use crate::{PlaylistId, TrackId};

use super::{PlaybackOptions, RepeatMode, ShuffleMode, shuffle::shuffled_track_ids};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum PlaybackQueueSource {
    #[default]
    Library,
    Album,
    Playlist(PlaylistId),
    SearchResults,
    Selection,
}

impl PlaybackQueueSource {
    /// True for queue sources where Smart Shuffle is meaningful — a
    /// stable library-scale corpus where engagement signals carry across
    /// sessions. Smart is silently downgraded to pure random for ad-hoc
    /// sources (Album / SearchResults / Selection) where the candidate
    /// pool is intentionally narrow and the user is signalling an
    /// explicit listening context, not asking for discovery.
    pub fn supports_smart_shuffle(&self) -> bool {
        matches!(self, Self::Library | Self::Playlist(_))
    }
}

/// Describes the queue the runtime should build when starting playback at a
/// specific track. The activation source (UI view, MPRIS, ...) decides:
/// the runtime never reaches for "all library tracks" by default; it does
/// only what the request asks for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackQueueRequest {
    /// Build the queue from every playable library track. Used by Songs view
    /// and other surfaces that don't pin the queue to a narrower context.
    Library,
    /// Build the queue from this explicit ordered list, labelled with the
    /// given source for downstream UI / MPRIS reporting. Track ids that
    /// don't resolve to a playable library track are silently dropped so
    /// the queue never tries to play missing entries.
    Explicit {
        source: PlaybackQueueSource,
        ordered_track_ids: Vec<TrackId>,
    },
}

/// Snapshot of the queue's internal layout — Eager precomputes the
/// full play order at construction (pure shuffle's Fisher-Yates, or
/// the identity ordering when shuffle is off); Lazy keeps an
/// append-only `played_history` stack with a cursor, with new tracks
/// chosen on demand by an externally-supplied Smart Shuffle picker.
///
/// Both variants share `ordered_track_ids` (the source-of-truth pool)
/// and `current_track_id`; their `next_track_id` / `previous_track_id`
/// implementations diverge because Lazy has browser-style
/// back/forward semantics over `played_history` rather than a fixed
/// total ordering.
#[derive(Clone, Debug, Eq, PartialEq)]
enum PlaybackQueueLayout {
    Eager {
        play_order_track_ids: Vec<TrackId>,
    },
    Lazy {
        /// Tracks chosen so far by the smart-shuffle picker (or
        /// seeded by an explicit play, or spliced in by Enqueue
        /// Next / Last), in the order they will be played.
        played_history: Vec<TrackId>,
        /// Index into `played_history` of the currently-playing
        /// track. Stepping back via Previous decrements `cursor`
        /// (no new pick); stepping past the tail triggers a new
        /// pick which is appended.
        cursor: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackQueue {
    source: PlaybackQueueSource,
    ordered_track_ids: Vec<TrackId>,
    layout: PlaybackQueueLayout,
    current_track_id: Option<TrackId>,
    options: PlaybackOptions,
}

/// Read-only view onto a Lazy queue's pick context. Returned by
/// [`PlaybackQueue::lazy_pick_context`]; the caller (the runtime)
/// hands this to its Smart Shuffle picker, which scores the
/// candidate pool against the seed and the in-session history,
/// then writes the chosen track back via
/// [`PlaybackQueue::lazy_append_pick`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LazyPickContext<'a> {
    pub seed_track_id: TrackId,
    pub candidate_pool: &'a [TrackId],
    pub played_history: &'a [TrackId],
}

impl PlaybackQueue {
    pub fn new(
        source: PlaybackQueueSource,
        ordered_track_ids: Vec<TrackId>,
        current_track_id: TrackId,
        options: PlaybackOptions,
        shuffle_seed: u64,
    ) -> Self {
        let current_track_id = ordered_track_ids
            .contains(&current_track_id)
            .then_some(current_track_id);
        let layout = build_layout(
            &ordered_track_ids,
            current_track_id,
            effective_shuffle_mode(options.shuffle_mode, &source),
            shuffle_seed,
        );

        Self {
            source,
            ordered_track_ids,
            layout,
            current_track_id,
            options,
        }
    }

    pub fn empty(options: PlaybackOptions) -> Self {
        Self {
            source: PlaybackQueueSource::Library,
            ordered_track_ids: Vec::new(),
            layout: PlaybackQueueLayout::Eager {
                play_order_track_ids: Vec::new(),
            },
            current_track_id: None,
            options,
        }
    }

    pub fn source(&self) -> &PlaybackQueueSource {
        &self.source
    }

    pub fn ordered_track_ids(&self) -> &[TrackId] {
        &self.ordered_track_ids
    }

    /// The realised playback sequence — for Eager layouts this is the
    /// precomputed Fisher-Yates order (or the identity order when
    /// shuffle is off); for Lazy layouts it is the prefix of tracks the
    /// smart-shuffle picker has selected so far (`played_history`),
    /// which grows as the user advances.
    pub fn play_order_track_ids(&self) -> &[TrackId] {
        match &self.layout {
            PlaybackQueueLayout::Eager {
                play_order_track_ids,
            } => play_order_track_ids,
            PlaybackQueueLayout::Lazy { played_history, .. } => played_history,
        }
    }

    pub fn current_track_id(&self) -> Option<TrackId> {
        self.current_track_id
    }

    pub fn options(&self) -> PlaybackOptions {
        self.options
    }

    /// Advance to the next mode in the tri-state cycle
    /// (`Off → Pure → Smart → Off`), preserving the current track and
    /// rebuilding the layout to match the new mode. The seed is only
    /// consulted when the new mode is Pure; Lazy layouts derive their
    /// per-pick randomness inside the picker, not from this seed.
    pub fn cycle_shuffle_mode(&mut self, shuffle_seed: u64) {
        self.options = self.options.with_shuffle_cycled();
        self.rebuild_layout(shuffle_seed);
    }

    /// Explicitly set the shuffle mode (used by source-specific
    /// Play / Shuffle controls that don't want to consult the
    /// transport's current state). No-op when the requested mode is
    /// already active.
    pub fn set_shuffle_mode(&mut self, shuffle_mode: ShuffleMode, shuffle_seed: u64) {
        if self.options.shuffle_mode == shuffle_mode {
            return;
        }
        self.options = self.options.with_shuffle_mode(shuffle_mode);
        self.rebuild_layout(shuffle_seed);
    }

    pub fn toggle_repeat_mode(&mut self) {
        self.options = self.options.with_repeat_toggled();
    }

    pub fn set_repeat_mode(&mut self, repeat_mode: RepeatMode) {
        self.options.repeat_mode = repeat_mode;
    }

    /// The next track to play after the current one. Eager layouts
    /// return the precomputed neighbour; Lazy layouts return the
    /// already-picked-but-not-yet-played track at `cursor + 1`, or
    /// `None` when the picker has not been consulted yet — in which
    /// case the caller checks [`Self::needs_lazy_pick`] and calls the
    /// picker to extend the history.
    pub fn next_track_id(&self) -> Option<TrackId> {
        self.adjacent_track_id(TrackStep::Next)
    }

    pub fn previous_track_id(&self) -> Option<TrackId> {
        self.adjacent_track_id(TrackStep::Previous)
    }

    /// True when the queue is in Lazy layout, has no already-picked
    /// successor for the current track, and has at least one
    /// candidate to pick from. Eager layouts always return `false`.
    pub fn needs_lazy_pick(&self) -> bool {
        match &self.layout {
            PlaybackQueueLayout::Eager { .. } => false,
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                // Already-picked successor available — no fresh pick needed.
                if cursor + 1 < played_history.len() {
                    return false;
                }
                // A pick can only happen if we have a seed (current track)
                // and at least one candidate in the underlying pool.
                self.current_track_id.is_some() && !self.ordered_track_ids.is_empty()
            }
        }
    }

    /// Build the read-only context the runtime's Smart Shuffle picker
    /// consults to choose a track. `None` for Eager layouts or when
    /// there is no seed to anchor a pick.
    pub fn lazy_pick_context(&self) -> Option<LazyPickContext<'_>> {
        let PlaybackQueueLayout::Lazy { played_history, .. } = &self.layout else {
            return None;
        };
        let seed_track_id = self.current_track_id?;
        Some(LazyPickContext {
            seed_track_id,
            candidate_pool: &self.ordered_track_ids,
            played_history,
        })
    }

    /// Append the picker's chosen track to the Lazy queue's history,
    /// directly after the current cursor position. `move_to_track`
    /// then advances the cursor onto the appended entry when playback
    /// of it actually begins. Returns `false` when the layout is not
    /// Lazy, the track is not in `ordered_track_ids`, or there is no
    /// current track to anchor against — every one of those is a
    /// programming error in the caller, not a runtime condition.
    pub fn lazy_append_pick(&mut self, track_id: TrackId) -> bool {
        if !self.ordered_track_ids.contains(&track_id) {
            return false;
        }
        let PlaybackQueueLayout::Lazy {
            played_history,
            cursor,
        } = &mut self.layout
        else {
            return false;
        };
        // Splice immediately after the cursor — Enqueue Next / Last
        // may have pushed tracks past it; the picker's choice always
        // takes the cursor+1 slot.
        let insertion = (*cursor).saturating_add(1).min(played_history.len());
        played_history.insert(insertion, track_id);
        true
    }

    pub fn move_to_track(&mut self, track_id: TrackId) -> bool {
        if !self.ordered_track_ids.contains(&track_id) {
            return false;
        }

        self.current_track_id = Some(track_id);
        match &mut self.layout {
            PlaybackQueueLayout::Eager { .. } => {}
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                // Walk the history for the target. Found → cursor jumps
                // to it (covers Previous, repeated Next replays). Not
                // found → the user clicked a track outside the picked
                // sequence (explicit library activation); fold it in by
                // truncating any speculative future picks and pushing
                // the new selection as the head of a fresh sub-sequence.
                if let Some(index) = played_history.iter().position(|id| *id == track_id) {
                    *cursor = index;
                } else {
                    played_history.truncate(cursor.saturating_add(1));
                    played_history.push(track_id);
                    *cursor = played_history.len() - 1;
                }
            }
        }
        true
    }

    pub fn replace_ordered_track_ids(
        &mut self,
        ordered_track_ids: Vec<TrackId>,
        shuffle_seed: u64,
    ) {
        let current_track_id = self
            .current_track_id
            .filter(|track_id| ordered_track_ids.contains(track_id));

        self.ordered_track_ids = ordered_track_ids;
        self.current_track_id = current_track_id;
        self.rebuild_layout(shuffle_seed);
    }

    pub fn remove_track(&mut self, track_id: TrackId, shuffle_seed: u64) {
        let ordered_track_ids = self
            .ordered_track_ids
            .iter()
            .copied()
            .filter(|candidate| *candidate != track_id)
            .collect();
        self.replace_ordered_track_ids(ordered_track_ids, shuffle_seed);
    }

    /// Inserts the given tracks so they play immediately after the currently
    /// playing track, in both the ordered queue and the realised play order.
    /// Tracks already present elsewhere in the queue are moved to the new
    /// position; the currently playing track is left in place and skipped if
    /// present in `track_ids`. Returns `false` when there is no current track
    /// to anchor against or when the candidate list reduces to nothing.
    pub fn enqueue_after_current(&mut self, track_ids: &[TrackId]) -> bool {
        let Some(current_track_id) = self.current_track_id else {
            return false;
        };

        let mut to_insert: Vec<TrackId> = Vec::with_capacity(track_ids.len());
        for candidate in track_ids {
            if *candidate != current_track_id && !to_insert.contains(candidate) {
                to_insert.push(*candidate);
            }
        }
        if to_insert.is_empty() {
            return false;
        }

        self.ordered_track_ids.retain(|id| !to_insert.contains(id));

        if let Some(index) = self
            .ordered_track_ids
            .iter()
            .position(|id| *id == current_track_id)
        {
            for (offset, track_id) in to_insert.iter().enumerate() {
                self.ordered_track_ids.insert(index + 1 + offset, *track_id);
            }
        }

        match &mut self.layout {
            PlaybackQueueLayout::Eager {
                play_order_track_ids,
            } => {
                play_order_track_ids.retain(|id| !to_insert.contains(id));
                if let Some(index) = play_order_track_ids
                    .iter()
                    .position(|id| *id == current_track_id)
                {
                    for (offset, track_id) in to_insert.iter().enumerate() {
                        play_order_track_ids.insert(index + 1 + offset, *track_id);
                    }
                }
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                // Lazy semantics: forcing tracks after current means
                // splicing them between cursor and the next picked
                // track. The user's queued-up order wins over the
                // picker's tentative successor (which, if present at
                // cursor+1, gets pushed back).
                played_history.retain(|id| !to_insert.contains(id));
                // Truncate stale references that no longer make sense after
                // the retain above may have shifted cursor's position.
                let safe_cursor = (*cursor).min(played_history.len().saturating_sub(1));
                let insertion = safe_cursor.saturating_add(1).min(played_history.len());
                for (offset, track_id) in to_insert.iter().enumerate() {
                    played_history.insert(insertion + offset, *track_id);
                }
                *cursor = safe_cursor;
            }
        }

        true
    }

    /// Appends the given tracks at the tail of the play queue, behind every
    /// already-queued track in both the ordered queue and the realised play
    /// order. Tracks already present elsewhere in the queue are moved to the
    /// new position; the currently playing track is left in place and skipped
    /// if present in `track_ids`. Returns `false` when there is no current
    /// track to anchor against or when the candidate list reduces to nothing.
    pub fn enqueue_at_end(&mut self, track_ids: &[TrackId]) -> bool {
        let Some(current_track_id) = self.current_track_id else {
            return false;
        };

        let mut to_append: Vec<TrackId> = Vec::with_capacity(track_ids.len());
        for candidate in track_ids {
            if *candidate != current_track_id && !to_append.contains(candidate) {
                to_append.push(*candidate);
            }
        }
        if to_append.is_empty() {
            return false;
        }

        self.ordered_track_ids.retain(|id| !to_append.contains(id));
        self.ordered_track_ids.extend(to_append.iter().copied());

        match &mut self.layout {
            PlaybackQueueLayout::Eager {
                play_order_track_ids,
            } => {
                play_order_track_ids.retain(|id| !to_append.contains(id));
                play_order_track_ids.extend(to_append.iter().copied());
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                played_history.retain(|id| !to_append.contains(id));
                *cursor = (*cursor).min(played_history.len().saturating_sub(1));
                played_history.extend(to_append.iter().copied());
            }
        }

        true
    }

    /// The tracks queued to play after the current one, in play order.
    ///
    /// For Eager layouts this is the realised play order after the
    /// current track's position; for Lazy (Smart Shuffle) layouts it is
    /// the already-picked / explicitly-enqueued tail after the cursor,
    /// which is usually empty because Smart Shuffle decides successors on
    /// demand. Empty when nothing is playing. The slice is the queue's
    /// *content* — repeat-mode wrapping is a playback behaviour layered on
    /// top by [`Self::next_track_id`], not part of the upcoming list.
    pub fn upcoming_track_ids(&self) -> &[TrackId] {
        let Some(current_track_id) = self.current_track_id else {
            return &[];
        };
        match &self.layout {
            PlaybackQueueLayout::Eager {
                play_order_track_ids,
            } => match play_order_track_ids
                .iter()
                .position(|id| *id == current_track_id)
            {
                Some(index) => &play_order_track_ids[index + 1..],
                None => &[],
            },
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                let start = cursor.saturating_add(1).min(played_history.len());
                &played_history[start..]
            }
        }
    }

    /// Remove a single upcoming track from the queue entirely — both the
    /// source pool and the realised play order — preserving every other
    /// track's position. Unlike [`Self::remove_track`] (which rebuilds the
    /// layout from the pool, re-rolling the shuffle, and is used when a
    /// track leaves the library), this is a surgical excision with no
    /// shuffle re-roll: the semantics the queue view's per-track evict
    /// needs.
    ///
    /// Refuses to remove the currently playing track, and only acts on a
    /// track that is actually upcoming (not already-played history, not
    /// absent), so it can never corrupt the cursor or the played prefix.
    /// Returns `true` when a track was removed.
    pub fn remove_from_queue(&mut self, track_id: TrackId) -> bool {
        if self.current_track_id == Some(track_id) {
            return false;
        }
        if !self.upcoming_track_ids().contains(&track_id) {
            return false;
        }

        self.ordered_track_ids.retain(|id| *id != track_id);
        match &mut self.layout {
            PlaybackQueueLayout::Eager {
                play_order_track_ids,
            } => {
                play_order_track_ids.retain(|id| *id != track_id);
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                // The track is strictly after the cursor (it was upcoming),
                // so removing it never shifts the cursor's target; the
                // clamp below only defends the in-range invariant.
                played_history.retain(|id| *id != track_id);
                *cursor = (*cursor).min(played_history.len().saturating_sub(1));
            }
        }
        true
    }

    /// Reorder one upcoming track within the queue, moving it so it plays
    /// immediately before (`place_after == false`) or after
    /// (`place_after == true`) another upcoming track. Edits only the
    /// realised play order — the source pool's membership is unchanged —
    /// so no shuffle re-roll happens and the set held by
    /// `ordered_track_ids` stays equal to the play order's set.
    ///
    /// No-op (returns `false`) when the two tracks are the same, when
    /// either is the currently playing track, or when either is not
    /// currently upcoming. Backs the queue view's drag-to-reorder.
    pub fn move_within_queue(
        &mut self,
        track_id: TrackId,
        target_track_id: TrackId,
        place_after: bool,
    ) -> bool {
        if track_id == target_track_id {
            return false;
        }
        if self.current_track_id == Some(track_id) || self.current_track_id == Some(target_track_id)
        {
            return false;
        }
        {
            let upcoming = self.upcoming_track_ids();
            if !upcoming.contains(&track_id) || !upcoming.contains(&target_track_id) {
                return false;
            }
        }

        match &mut self.layout {
            PlaybackQueueLayout::Eager {
                play_order_track_ids,
            } => reposition_track(play_order_track_ids, track_id, target_track_id, place_after),
            PlaybackQueueLayout::Lazy { played_history, .. } => {
                reposition_track(played_history, track_id, target_track_id, place_after)
            }
        }
    }

    fn adjacent_track_id(&self, step: TrackStep) -> Option<TrackId> {
        let current_track_id = self.current_track_id?;
        if self.options.repeat_mode == RepeatMode::One {
            return Some(current_track_id);
        }

        match &self.layout {
            PlaybackQueueLayout::Eager {
                play_order_track_ids,
            } => {
                let current_index = play_order_track_ids
                    .iter()
                    .position(|track_id| *track_id == current_track_id)?;
                let adjacent_index = match step {
                    TrackStep::Previous => current_index.checked_sub(1),
                    TrackStep::Next => current_index.checked_add(1),
                };

                match adjacent_index.and_then(|index| play_order_track_ids.get(index).copied()) {
                    Some(track_id) => Some(track_id),
                    None if self.options.repeat_mode == RepeatMode::All => match step {
                        TrackStep::Previous => play_order_track_ids.last().copied(),
                        TrackStep::Next => play_order_track_ids.first().copied(),
                    },
                    None => None,
                }
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                let adjacent_index = match step {
                    TrackStep::Previous => cursor.checked_sub(1),
                    TrackStep::Next => cursor.checked_add(1),
                };
                match adjacent_index.and_then(|index| played_history.get(index).copied()) {
                    Some(track_id) => Some(track_id),
                    None if self.options.repeat_mode == RepeatMode::All => match step {
                        // Lazy + RepeatAll wraps to the ends of the
                        // *already-played* history. A fresh forward
                        // pick triggered by Next at the tail goes
                        // through `needs_lazy_pick` instead — Repeat
                        // All is only reached here when no candidate
                        // remains to pick, which is the natural wrap
                        // condition.
                        TrackStep::Previous => played_history.last().copied(),
                        TrackStep::Next => played_history.first().copied(),
                    },
                    None => None,
                }
            }
        }
    }

    fn rebuild_layout(&mut self, shuffle_seed: u64) {
        self.layout = build_layout(
            &self.ordered_track_ids,
            self.current_track_id,
            effective_shuffle_mode(self.options.shuffle_mode, &self.source),
            shuffle_seed,
        );
    }
}

impl Default for PlaybackQueue {
    fn default() -> Self {
        Self::empty(PlaybackOptions::default())
    }
}

#[derive(Clone, Copy)]
enum TrackStep {
    Previous,
    Next,
}

/// The actual shuffle mode the layout should honour, which downgrades
/// Smart to Pure for queue sources that do not support it (Album,
/// SearchResults, Selection). The user's stored intent — the
/// `ShuffleMode` on `PlaybackOptions` — is preserved as-is; this is
/// only the projection used when laying out the playback sequence.
fn effective_shuffle_mode(mode: ShuffleMode, source: &PlaybackQueueSource) -> ShuffleMode {
    if matches!(mode, ShuffleMode::Smart) && !source.supports_smart_shuffle() {
        ShuffleMode::Pure
    } else {
        mode
    }
}

fn build_layout(
    ordered_track_ids: &[TrackId],
    current_track_id: Option<TrackId>,
    effective_mode: ShuffleMode,
    shuffle_seed: u64,
) -> PlaybackQueueLayout {
    match effective_mode {
        ShuffleMode::Off => PlaybackQueueLayout::Eager {
            play_order_track_ids: ordered_track_ids.to_vec(),
        },
        ShuffleMode::Pure => PlaybackQueueLayout::Eager {
            play_order_track_ids: build_pure_play_order(
                ordered_track_ids,
                current_track_id,
                shuffle_seed,
            ),
        },
        ShuffleMode::Smart => PlaybackQueueLayout::Lazy {
            played_history: current_track_id.map(|id| vec![id]).unwrap_or_default(),
            cursor: 0,
        },
    }
}

/// Move `moved` within `order` so it sits immediately before or after
/// `target`. Both ids are expected to be present (the caller validates
/// against the upcoming slice); if `target` somehow vanished after the
/// removal the move is rolled back and `false` returned.
fn reposition_track(
    order: &mut Vec<TrackId>,
    moved: TrackId,
    target: TrackId,
    place_after: bool,
) -> bool {
    let Some(from) = order.iter().position(|id| *id == moved) else {
        return false;
    };
    order.remove(from);
    let Some(target_index) = order.iter().position(|id| *id == target) else {
        order.insert(from, moved);
        return false;
    };
    let insert_at = if place_after {
        target_index + 1
    } else {
        target_index
    };
    order.insert(insert_at, moved);
    true
}

fn build_pure_play_order(
    ordered_track_ids: &[TrackId],
    current_track_id: Option<TrackId>,
    shuffle_seed: u64,
) -> Vec<TrackId> {
    let mut track_ids = shuffled_track_ids(ordered_track_ids, shuffle_seed);
    if let Some(current_track_id) = current_track_id
        && let Some(current_index) = track_ids
            .iter()
            .position(|track_id| *track_id == current_track_id)
    {
        track_ids.rotate_left(current_index);
    }
    track_ids
}

#[cfg(test)]
#[path = "queue_tests.rs"]
mod tests;
