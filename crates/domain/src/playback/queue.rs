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
/// `played_history` stack with a cursor, with new continuation tracks
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
        play_order: Vec<PlaybackQueueEntry>,
    },
    Lazy {
        /// Tracks chosen so far by the smart-shuffle picker, seeded by
        /// an explicit play, or spliced in by Enqueue Next / Last, in
        /// the order they will be played.
        played_history: Vec<PlaybackQueueEntry>,
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

/// Why a realised queue entry will play. Curated entries were explicitly
/// requested through Play Next / Add to Queue; continuation entries come
/// from the source playthrough (library, album, playlist, ...).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackQueueEntryKind {
    Curated,
    Continuation,
}

/// One track in the realised play order. The origin is part of the queue
/// model rather than a UI guess: Add to Queue, bounded continuation previews,
/// drag-to-reorder and eviction all need the same distinction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackQueueEntry {
    track_id: TrackId,
    kind: PlaybackQueueEntryKind,
}

impl PlaybackQueueEntry {
    fn continuation(track_id: TrackId) -> Self {
        Self {
            track_id,
            kind: PlaybackQueueEntryKind::Continuation,
        }
    }

    fn curated(track_id: TrackId) -> Self {
        Self {
            track_id,
            kind: PlaybackQueueEntryKind::Curated,
        }
    }

    pub fn track_id(self) -> TrackId {
        self.track_id
    }

    pub fn kind(self) -> PlaybackQueueEntryKind {
        self.kind
    }

    pub fn is_curated(self) -> bool {
        self.kind == PlaybackQueueEntryKind::Curated
    }
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
    pub played_history: Vec<TrackId>,
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
            &[],
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
                play_order: Vec::new(),
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
    ///
    /// This allocates because the internal sequence also records whether
    /// each entry is curated or source continuation. It is intended for
    /// diagnostics and tests; UI code should use [`Self::upcoming_preview`]
    /// so a library-scale continuation never becomes a widget model.
    pub fn play_order_track_ids(&self) -> Vec<TrackId> {
        match &self.layout {
            PlaybackQueueLayout::Eager { play_order } => {
                play_order.iter().map(|entry| entry.track_id).collect()
            }
            PlaybackQueueLayout::Lazy { played_history, .. } => {
                played_history.iter().map(|entry| entry.track_id).collect()
            }
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

    /// Whether continuation tracks are selected on demand by Smart Shuffle
    /// rather than materialised ahead of time. UI surfaces use this semantic
    /// query instead of inspecting the saved shuffle preference: ad-hoc
    /// sources can preserve `ShuffleMode::Smart` while intentionally using
    /// an eager Pure layout.
    pub fn uses_lazy_continuation(&self) -> bool {
        matches!(&self.layout, PlaybackQueueLayout::Lazy { .. })
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
            played_history: played_history.iter().map(|entry| entry.track_id).collect(),
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
        // The runtime only asks for a lazy pick at the realised tail:
        // explicitly queued tracks always drain first.
        let insertion = (*cursor).saturating_add(1).min(played_history.len());
        played_history.insert(insertion, PlaybackQueueEntry::continuation(track_id));
        true
    }

    pub fn move_to_track(&mut self, track_id: TrackId) -> bool {
        if !self.ordered_track_ids.contains(&track_id) && !self.contains_realised_track(track_id) {
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
                let adjacent = played_history
                    .get(cursor.saturating_add(1))
                    .filter(|entry| entry.track_id == track_id)
                    .map(|_| cursor.saturating_add(1))
                    .or_else(|| {
                        cursor
                            .checked_sub(1)
                            .filter(|index| played_history[*index].track_id == track_id)
                    });
                if let Some(index) = adjacent.or_else(|| {
                    played_history
                        .iter()
                        .rposition(|entry| entry.track_id == track_id)
                }) {
                    *cursor = index;
                } else {
                    played_history.truncate(cursor.saturating_add(1));
                    played_history.push(PlaybackQueueEntry::continuation(track_id));
                    *cursor = played_history.len() - 1;
                }
            }
        }
        true
    }

    pub fn replace_ordered_track_ids(
        &mut self,
        ordered_track_ids: Vec<TrackId>,
        available_track_ids: &[TrackId],
        shuffle_seed: u64,
    ) {
        let current_track_id = self
            .current_track_id
            .filter(|track_id| available_track_ids.contains(track_id));
        let curated = self
            .upcoming_entries()
            .iter()
            .filter(|entry| entry.is_curated() && available_track_ids.contains(&entry.track_id))
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>();
        self.ordered_track_ids = ordered_track_ids;
        self.current_track_id = current_track_id;
        self.layout = build_layout(
            &self.ordered_track_ids,
            self.current_track_id,
            &curated,
            effective_shuffle_mode(self.options.shuffle_mode, &self.source),
            shuffle_seed,
        );
    }

    /// Replace the source playthrough while preserving still-playable
    /// curated Up Next entries. Used when a browsing context widens, such
    /// as clearing a search filter during playback.
    pub fn replace_source(
        &mut self,
        source: PlaybackQueueSource,
        ordered_track_ids: Vec<TrackId>,
        available_track_ids: &[TrackId],
        shuffle_seed: u64,
    ) {
        self.source = source;
        self.replace_ordered_track_ids(ordered_track_ids, available_track_ids, shuffle_seed);
    }

    pub fn remove_track(&mut self, track_id: TrackId) {
        self.ordered_track_ids
            .retain(|candidate| *candidate != track_id);
        if self.current_track_id == Some(track_id) {
            self.current_track_id = None;
        }
        match &mut self.layout {
            PlaybackQueueLayout::Eager { play_order } => {
                play_order.retain(|entry| entry.track_id != track_id);
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                remove_entries_and_reanchor_cursor(played_history, cursor, &[track_id]);
            }
        }
    }

    /// Inserts the given tracks at the head of the curated Up Next region:
    /// immediately after the currently playing track and before every
    /// previously curated or source-continuation entry. Existing occurrences
    /// are moved rather than duplicated. The source pool remains intact.
    pub fn enqueue_after_current(&mut self, track_ids: &[TrackId]) -> bool {
        let Some(current_track_id) = self.current_track_id else {
            return false;
        };

        let to_insert = queue_candidates(track_ids, current_track_id);
        if to_insert.is_empty() {
            return false;
        }

        match &mut self.layout {
            PlaybackQueueLayout::Eager { play_order } => {
                remove_entries(play_order, &to_insert);
                let Some(index) = entry_position(play_order, current_track_id) else {
                    return false;
                };
                insert_curated(play_order, index + 1, &to_insert);
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                remove_entries_and_reanchor_cursor(played_history, cursor, &to_insert);
                let insertion = cursor.saturating_add(1).min(played_history.len());
                insert_curated(played_history, insertion, &to_insert);
            }
        }

        true
    }

    /// Appends the given tracks to the curated Up Next region: after every
    /// explicitly queued track, but before source continuation. Existing
    /// occurrences are moved rather than duplicated. The source pool remains
    /// intact, so a library-scale continuation never swallows Add to Queue.
    pub fn enqueue_at_end(&mut self, track_ids: &[TrackId]) -> bool {
        let Some(current_track_id) = self.current_track_id else {
            return false;
        };

        let to_append = queue_candidates(track_ids, current_track_id);
        if to_append.is_empty() {
            return false;
        }

        match &mut self.layout {
            PlaybackQueueLayout::Eager { play_order } => {
                remove_entries(play_order, &to_append);
                let Some(current) = entry_position(play_order, current_track_id) else {
                    return false;
                };
                let insertion = curated_tail(play_order, current + 1);
                insert_curated(play_order, insertion, &to_append);
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                remove_entries_and_reanchor_cursor(played_history, cursor, &to_append);
                let insertion = curated_tail(played_history, cursor.saturating_add(1));
                insert_curated(played_history, insertion, &to_append);
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
    /// demand. Empty when nothing is playing. The returned list is the queue's
    /// *content* — repeat-mode wrapping is a playback behaviour layered on
    /// top by [`Self::next_track_id`], not part of the upcoming list.
    pub fn upcoming_track_ids(&self) -> Vec<TrackId> {
        self.upcoming_entries()
            .iter()
            .map(|entry| entry.track_id)
            .collect()
    }

    /// Whether `track_id` is currently scheduled after the playing track.
    /// Used by queue-popover activation to reject stale row clicks without
    /// rebuilding or otherwise mutating the queue.
    pub fn contains_upcoming_track(&self, track_id: TrackId) -> bool {
        self.upcoming_entries()
            .iter()
            .any(|entry| entry.track_id == track_id)
    }

    /// Bounded UI projection: every explicit Up Next entry followed by at
    /// most `continuation_limit` source-playthrough entries. The internal
    /// playback order may contain the whole library; popovers never need it.
    pub fn upcoming_preview(&self, continuation_limit: usize) -> Vec<PlaybackQueueEntry> {
        let mut remaining_continuation = continuation_limit;
        self.upcoming_entries()
            .iter()
            .copied()
            .filter(|entry| {
                if entry.is_curated() {
                    return true;
                }
                let include = remaining_continuation > 0;
                remaining_continuation = remaining_continuation.saturating_sub(1);
                include
            })
            .collect()
    }

    /// Remove a single explicitly queued Up Next track from the realised
    /// play order, preserving source continuation and the shuffle seed.
    ///
    /// Refuses to remove the currently playing track, and only acts on a
    /// track that is actually upcoming (not already-played history, not
    /// absent), so it can never corrupt the cursor or the played prefix.
    /// Returns `true` when a track was removed.
    pub fn remove_from_queue(&mut self, track_id: TrackId) -> bool {
        if !self
            .upcoming_entries()
            .iter()
            .any(|entry| entry.track_id == track_id && entry.is_curated())
        {
            return false;
        }

        match &mut self.layout {
            PlaybackQueueLayout::Eager { play_order } => {
                play_order.retain(|entry| entry.track_id != track_id);
            }
            PlaybackQueueLayout::Lazy {
                played_history,
                cursor,
            } => {
                // The track is strictly after the cursor (it was upcoming),
                // so removing it never shifts the cursor's target; the
                // clamp below only defends the in-range invariant.
                remove_entries_and_reanchor_cursor(played_history, cursor, &[track_id]);
            }
        }
        true
    }

    /// Reorder one curated Up Next track within the queue, moving it so it plays
    /// immediately before (`place_after == false`) or after
    /// (`place_after == true`) another upcoming track. Edits only the
    /// realised play order — the source pool's membership is unchanged —
    /// so no shuffle re-roll happens.
    ///
    /// No-op (returns `false`) when the two tracks are the same, when
    /// either is not a curated upcoming entry. Backs the queue view's
    /// drag-to-reorder.
    pub fn move_within_queue(
        &mut self,
        track_id: TrackId,
        target_track_id: TrackId,
        place_after: bool,
    ) -> bool {
        if track_id == target_track_id {
            return false;
        }
        {
            let upcoming = self.upcoming_entries();
            if !contains_curated(upcoming, track_id) || !contains_curated(upcoming, target_track_id)
            {
                return false;
            }
        }

        match &mut self.layout {
            PlaybackQueueLayout::Eager { play_order } => {
                reposition_track(play_order, track_id, target_track_id, place_after)
            }
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
            PlaybackQueueLayout::Eager { play_order } => {
                let current_index = play_order
                    .iter()
                    .position(|entry| entry.track_id == current_track_id)?;
                let adjacent_index = match step {
                    TrackStep::Previous => current_index.checked_sub(1),
                    TrackStep::Next => current_index.checked_add(1),
                };

                match adjacent_index.and_then(|index| play_order.get(index).copied()) {
                    Some(entry) => Some(entry.track_id),
                    None if self.options.repeat_mode == RepeatMode::All => match step {
                        TrackStep::Previous => play_order.last().map(|entry| entry.track_id),
                        TrackStep::Next => play_order.first().map(|entry| entry.track_id),
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
                    Some(entry) => Some(entry.track_id),
                    None if self.options.repeat_mode == RepeatMode::All => match step {
                        // Lazy + RepeatAll wraps to the ends of the
                        // *already-played* history. A fresh forward
                        // pick triggered by Next at the tail goes
                        // through `needs_lazy_pick` instead — Repeat
                        // All is only reached here when no candidate
                        // remains to pick, which is the natural wrap
                        // condition.
                        TrackStep::Previous => played_history.last().map(|entry| entry.track_id),
                        TrackStep::Next => played_history.first().map(|entry| entry.track_id),
                    },
                    None => None,
                }
            }
        }
    }

    fn rebuild_layout(&mut self, shuffle_seed: u64) {
        let curated = self
            .upcoming_entries()
            .iter()
            .filter(|entry| entry.is_curated())
            .map(|entry| entry.track_id)
            .collect::<Vec<_>>();
        self.layout = build_layout(
            &self.ordered_track_ids,
            self.current_track_id,
            &curated,
            effective_shuffle_mode(self.options.shuffle_mode, &self.source),
            shuffle_seed,
        );
    }

    fn contains_realised_track(&self, track_id: TrackId) -> bool {
        match &self.layout {
            PlaybackQueueLayout::Eager { play_order } => {
                play_order.iter().any(|entry| entry.track_id == track_id)
            }
            PlaybackQueueLayout::Lazy { played_history, .. } => played_history
                .iter()
                .any(|entry| entry.track_id == track_id),
        }
    }

    fn upcoming_entries(&self) -> &[PlaybackQueueEntry] {
        let Some(current_track_id) = self.current_track_id else {
            return &[];
        };
        match &self.layout {
            PlaybackQueueLayout::Eager { play_order } => match play_order
                .iter()
                .position(|entry| entry.track_id == current_track_id)
            {
                Some(index) => &play_order[index + 1..],
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
    curated_track_ids: &[TrackId],
    effective_mode: ShuffleMode,
    shuffle_seed: u64,
) -> PlaybackQueueLayout {
    match effective_mode {
        ShuffleMode::Off => PlaybackQueueLayout::Eager {
            play_order: eager_play_order(
                ordered_track_ids.to_vec(),
                current_track_id,
                curated_track_ids,
            ),
        },
        ShuffleMode::Pure => PlaybackQueueLayout::Eager {
            play_order: eager_play_order(
                build_pure_play_order(ordered_track_ids, current_track_id, shuffle_seed),
                current_track_id,
                curated_track_ids,
            ),
        },
        ShuffleMode::Smart => PlaybackQueueLayout::Lazy {
            played_history: current_track_id
                .map(PlaybackQueueEntry::continuation)
                .into_iter()
                .chain(
                    curated_track_ids
                        .iter()
                        .copied()
                        .filter(|id| Some(*id) != current_track_id)
                        .map(PlaybackQueueEntry::curated),
                )
                .collect(),
            cursor: 0,
        },
    }
}

fn eager_play_order(
    mut continuation_track_ids: Vec<TrackId>,
    current_track_id: Option<TrackId>,
    curated_track_ids: &[TrackId],
) -> Vec<PlaybackQueueEntry> {
    continuation_track_ids
        .retain(|id| Some(*id) == current_track_id || !curated_track_ids.contains(id));
    let mut play_order: Vec<_> = continuation_track_ids
        .into_iter()
        .map(PlaybackQueueEntry::continuation)
        .collect();
    if let Some(current_track_id) = current_track_id {
        let current = match entry_position(&play_order, current_track_id) {
            Some(index) => index,
            None => {
                play_order.insert(0, PlaybackQueueEntry::continuation(current_track_id));
                0
            }
        };
        insert_curated(&mut play_order, current + 1, curated_track_ids);
    }
    play_order
}

fn queue_candidates(track_ids: &[TrackId], current_track_id: TrackId) -> Vec<TrackId> {
    let mut candidates = Vec::with_capacity(track_ids.len());
    for track_id in track_ids {
        if *track_id != current_track_id && !candidates.contains(track_id) {
            candidates.push(*track_id);
        }
    }
    candidates
}

fn insert_curated(order: &mut Vec<PlaybackQueueEntry>, insertion: usize, track_ids: &[TrackId]) {
    for (offset, track_id) in track_ids.iter().enumerate() {
        order.insert(insertion + offset, PlaybackQueueEntry::curated(*track_id));
    }
}

fn remove_entries(order: &mut Vec<PlaybackQueueEntry>, track_ids: &[TrackId]) {
    order.retain(|entry| !track_ids.contains(&entry.track_id));
}

fn remove_entries_and_reanchor_cursor(
    order: &mut Vec<PlaybackQueueEntry>,
    cursor: &mut usize,
    track_ids: &[TrackId],
) {
    let removed_before_cursor = order
        .iter()
        .take(*cursor)
        .filter(|entry| track_ids.contains(&entry.track_id))
        .count();
    remove_entries(order, track_ids);
    *cursor = cursor
        .saturating_sub(removed_before_cursor)
        .min(order.len().saturating_sub(1));
}

fn entry_position(order: &[PlaybackQueueEntry], track_id: TrackId) -> Option<usize> {
    order.iter().position(|entry| entry.track_id == track_id)
}

fn curated_tail(order: &[PlaybackQueueEntry], start: usize) -> usize {
    order
        .iter()
        .skip(start)
        .take_while(|entry| entry.is_curated())
        .count()
        + start
}

fn contains_curated(entries: &[PlaybackQueueEntry], track_id: TrackId) -> bool {
    entries
        .iter()
        .any(|entry| entry.track_id == track_id && entry.is_curated())
}

/// Move `moved` within `order` so it sits immediately before or after
/// `target`. Both ids are expected to be present (the caller validates
/// against the upcoming slice); if `target` somehow vanished after the
/// removal the move is rolled back and `false` returned.
fn reposition_track(
    order: &mut Vec<PlaybackQueueEntry>,
    moved: TrackId,
    target: TrackId,
    place_after: bool,
) -> bool {
    let Some(from) = entry_position(order, moved) else {
        return false;
    };
    let entry = order.remove(from);
    let Some(target_index) = entry_position(order, target) else {
        order.insert(from, entry);
        return false;
    };
    let insert_at = if place_after {
        target_index + 1
    } else {
        target_index
    };
    order.insert(insert_at, entry);
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
