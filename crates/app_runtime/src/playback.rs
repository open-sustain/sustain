// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Duration, SystemTime};

use sustain_domain::{
    PlaybackCommand, PlaybackQueue, PlaybackQueueRequest, PlaybackQueueSource, PlaybackSession,
    PlaybackState, Track, TrackAvailability, TrackId, TrackPlaybackSource,
};
use sustain_playback::PlaybackService;
use sustain_smart_shuffle::{PickContext, format_debug, pick_next_track};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, NotificationCategory,
    NotificationSeverity,
    file_presence::{FilePresence, probe_file_presence},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingPlayRegistration {
    session_id: u64,
    track_id: TrackId,
    registered_at: SystemTime,
}

impl ApplicationRuntime {
    pub(super) fn handle_playback_command(
        &mut self,
        command: PlaybackCommand,
    ) -> ApplicationRuntimeResult<()> {
        match command {
            PlaybackCommand::CycleShuffleMode => {
                self.playback_queue
                    .cycle_shuffle_mode(playback_shuffle_seed());
                self.on_shuffle_mode_changed();
                self.persist_playback_shuffle_mode()
            }
            PlaybackCommand::SetShuffleMode(mode) => {
                self.playback_queue
                    .set_shuffle_mode(mode, playback_shuffle_seed());
                self.on_shuffle_mode_changed();
                self.persist_playback_shuffle_mode()
            }
            PlaybackCommand::ToggleRepeat => {
                self.playback_queue.toggle_repeat_mode();
                Ok(())
            }
            PlaybackCommand::PlayTrack { track_id, queue } => {
                let new_queue = self.build_playback_queue(track_id, queue)?;
                self.play_track(track_id)?;
                self.playback_queue = new_queue;
                Ok(())
            }
            PlaybackCommand::PlayPreviousTrack => self.play_previous_track(),
            PlaybackCommand::PlayNextTrack => self.play_next_track(),
            PlaybackCommand::SkipCurrentTrack => self.skip_current_track(),
            PlaybackCommand::EnqueueNext(track_ids) => {
                self.playback_queue.enqueue_after_current(&track_ids);
                Ok(())
            }
            PlaybackCommand::EnqueueLast(track_ids) => {
                self.playback_queue.enqueue_at_end(&track_ids);
                Ok(())
            }
            PlaybackCommand::RemoveFromQueue(track_id) => {
                self.playback_queue.remove_from_queue(track_id);
                Ok(())
            }
            PlaybackCommand::ReorderQueue {
                track_id,
                target_track_id,
                place_after,
            } => {
                self.playback_queue
                    .move_within_queue(track_id, target_track_id, place_after);
                Ok(())
            }
            PlaybackCommand::RepopulateQueue(request) => {
                self.repopulate_queue(request);
                Ok(())
            }
            PlaybackCommand::Pause => self.pause_playback(),
            PlaybackCommand::Resume => self.resume_playback(),
            PlaybackCommand::TogglePlayPause => match self.playback_service()?.state() {
                PlaybackState::Playing { .. } => self.pause_playback(),
                PlaybackState::Paused { .. } => self.resume_playback(),
                PlaybackState::Stopped | PlaybackState::Loading { .. } => Ok(()),
            },
            PlaybackCommand::Stop => self.stop_playback(),
            PlaybackCommand::Seek(position) => self.seek_playback(position),
            PlaybackCommand::SetVolume(volume) => self
                .playback_service()?
                .set_volume(volume)
                .map_err(|_| ApplicationRuntimeError::PlaybackFailed),
        }
    }

    fn playback_service(&self) -> ApplicationRuntimeResult<&dyn PlaybackService> {
        self.playback_service
            .as_deref()
            .ok_or(ApplicationRuntimeError::PlaybackServiceUnavailable)
    }

    fn pause_playback(&mut self) -> ApplicationRuntimeResult<()> {
        self.settle_current_playing_session();
        self.playback_service()?
            .pause()
            .map_err(|_| ApplicationRuntimeError::PlaybackFailed)?;
        self.freeze_current_playback_session();
        Ok(())
    }

    fn resume_playback(&mut self) -> ApplicationRuntimeResult<()> {
        self.playback_service()?
            .resume()
            .map_err(|_| ApplicationRuntimeError::PlaybackFailed)?;
        if matches!(self.playback_state(), PlaybackState::Playing { .. }) {
            let now = self.monotonic_clock.now();
            if let Some(session) = self.playback_session.as_mut() {
                session.resume_accounting(now);
            }
        }
        Ok(())
    }

    fn stop_playback(&mut self) -> ApplicationRuntimeResult<()> {
        self.settle_current_playing_session();
        self.playback_service()?
            .stop()
            .map_err(|_| ApplicationRuntimeError::PlaybackFailed)?;
        self.playback_session = None;
        Ok(())
    }

    fn seek_playback(&mut self, position: Duration) -> ApplicationRuntimeResult<()> {
        // Settle real listening before changing transport position. The
        // baseline remains anchored at the seek instant, so jumping forward
        // contributes no artificial listening time.
        self.settle_current_playing_session();
        self.playback_service()?
            .seek(position)
            .map_err(|_| ApplicationRuntimeError::PlaybackFailed)
    }

    fn build_playback_queue(
        &self,
        track_id: TrackId,
        request: PlaybackQueueRequest,
    ) -> ApplicationRuntimeResult<PlaybackQueue> {
        // Resolving the track here also serves as the "track exists and is
        // playable" precondition for the whole command — same role
        // `library_playback_queue` played before. If it fails we never get
        // to play_track.
        let _source = self.track_playback_source(track_id)?;
        let (source, ordered_track_ids) = match request {
            PlaybackQueueRequest::Library => {
                (PlaybackQueueSource::Library, self.playable_track_ids())
            }
            PlaybackQueueRequest::Explicit {
                source,
                ordered_track_ids,
            } => {
                let playable: HashSet<TrackId> = self.playable_track_ids().into_iter().collect();
                let filtered: Vec<TrackId> = ordered_track_ids
                    .into_iter()
                    .filter(|id| playable.contains(id))
                    .collect();
                (source, filtered)
            }
        };
        Ok(PlaybackQueue::new(
            source,
            ordered_track_ids,
            track_id,
            self.playback_queue.options(),
            playback_shuffle_seed(),
        ))
    }

    /// Re-derive the play queue from `request`, keeping the currently
    /// playing track and the transport untouched (the audio is not
    /// reloaded). Backs [`PlaybackCommand::RepopulateQueue`].
    ///
    /// No-op in four cases, each of which would otherwise corrupt the
    /// queue or surprise the user:
    /// - Nothing is playing — there is no anchor track to preserve.
    /// - The queue is an album — album playback "always queues the album,
    ///   nothing more, nothing less" (#78); an album is never built from a
    ///   search filter, so there is nothing to widen and widening it would
    ///   break that contract no matter which view the user cleared from.
    /// - The widened request does not contain the playing track — the
    ///   browsing context changed (e.g. the user switched views before
    ///   clearing the search); widening to it would orphan the queue's
    ///   current track and break auto-advance.
    /// - The resolved track pool is identical to the current one — the
    ///   queue was never actually narrowed, so rebuilding would needlessly
    ///   re-roll a shuffle order.
    fn repopulate_queue(&mut self, request: PlaybackQueueRequest) {
        let Some(current_track_id) = self.playback_queue.current_track_id() else {
            return;
        };
        if matches!(self.playback_queue.source(), PlaybackQueueSource::Album) {
            return;
        }
        let Ok(new_queue) = self.build_playback_queue(current_track_id, request) else {
            return;
        };
        if new_queue.current_track_id() != Some(current_track_id) {
            return;
        }
        if new_queue.ordered_track_ids() == self.playback_queue.ordered_track_ids() {
            return;
        }
        self.playback_queue = new_queue;
    }

    fn play_track(&mut self, track_id: TrackId) -> ApplicationRuntimeResult<()> {
        self.play_track_with(track_id, probe_file_presence)
    }

    pub(super) fn play_track_with(
        &mut self,
        track_id: TrackId,
        probe: impl Fn(&Path) -> FilePresence,
    ) -> ApplicationRuntimeResult<()> {
        let source = self.track_playback_source(track_id)?;
        // Lazy availability reconciliation: every play attempt
        // re-stats the resolved path and brings the persisted
        // `is_missing` flag into agreement with what is actually on
        // disk right now. The flag is therefore a *cache* of the
        // last observed availability — never a gate that prevents
        // future plays. This is how a track recovers after the user
        // renames its file back into place: the click flows through
        // here, the fallible filesystem probe sees the file again, the
        // flag flips back to Available, and playback proceeds.
        let recorded_missing = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id)
            .map(|track| track.location.is_missing())
            .unwrap_or(false);
        match (probe(&source.path), recorded_missing) {
            (FilePresence::Absent, true) => {
                return Err(ApplicationRuntimeError::TrackUnavailable);
            }
            (FilePresence::Absent, false) => {
                self.mark_track_missing(track_id)?;
                return Err(ApplicationRuntimeError::TrackUnavailable);
            }
            (FilePresence::Present, true) => self.mark_track_available(track_id)?,
            (FilePresence::Present, false) => {}
            (FilePresence::ProbeFailed, _) => {
                return Err(ApplicationRuntimeError::TrackUnavailable);
            }
        }
        self.settle_current_playing_session();
        self.playback_service()?
            .play_track(source)
            .map_err(|_| ApplicationRuntimeError::PlaybackFailed)?;
        // Every new playback starts a fresh session: any unfinished
        // listening on the previous track ends here without registering
        // either a play or a skip (unless the caller — see
        // `skip_current_track` — committed one first). Capturing the
        // duration up front means immediate Next clicks still see a
        // session and can decide skip eligibility correctly, instead
        // of racing the 1 Hz tick that would otherwise create it.
        let duration = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.metadata.duration)
            .unwrap_or(Duration::ZERO);
        self.start_playback_session(track_id, duration);
        Ok(())
    }

    /// Resolves the absolute on-disk path for `track_id`. Does NOT
    /// consult the persisted `is_missing` flag — that flag is a
    /// cache of the last observed availability, and the caller
    /// ([`Self::play_track`]) reconciles it against the live filesystem on
    /// every play. Returning `TrackUnavailable` here therefore means
    /// the runtime cannot even form a candidate path (track id not
    /// in the library, or no library root configured), not that the
    /// file is necessarily gone.
    fn track_playback_source(
        &self,
        track_id: TrackId,
    ) -> ApplicationRuntimeResult<TrackPlaybackSource> {
        let track = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id)
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        let path = self
            .absolute_track_path(track)
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        Ok(TrackPlaybackSource::new(track_id, path))
    }

    /// Flip a track's persisted availability to `Missing` after a live
    /// playback attempt has proven the file is gone. Persists the
    /// updated row and rebuilds the playback queue so the missing
    /// track stops appearing in next/previous navigation. No-op when
    /// the track is already flagged missing or no library store is
    /// installed.
    fn mark_track_missing(&mut self, track_id: TrackId) -> ApplicationRuntimeResult<()> {
        self.set_track_availability(track_id, TrackAvailability::Missing)
    }

    /// Counterpart to [`Self::mark_track_missing`]: flip a previously-missing
    /// track back to `Available` once a live playback attempt has
    /// proven the file is reachable again (e.g. the user renamed it
    /// back, restored from trash, or remounted the volume). Same
    /// persistence and observer plumbing.
    fn mark_track_available(&mut self, track_id: TrackId) -> ApplicationRuntimeResult<()> {
        self.set_track_availability(track_id, TrackAvailability::Available)
    }

    fn set_track_availability(
        &mut self,
        track_id: TrackId,
        availability: TrackAvailability,
    ) -> ApplicationRuntimeResult<()> {
        let Some(index) = self
            .library_tracks
            .iter()
            .position(|track| track.id == track_id)
        else {
            return Ok(());
        };
        if self.library_tracks[index].location.availability == availability {
            return Ok(());
        }
        let mut updated = self.library_tracks[index].clone();
        updated.location = updated.location.with_availability(availability);
        if let Some(store) = self.library_store.as_ref() {
            store
                .update_track_location(track_id, &updated.location)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }
        self.store_library_track(index, updated);
        self.refresh_playback_queue_track_ids();
        self.notify_track_availability_observer();
        Ok(())
    }

    fn play_previous_track(&mut self) -> ApplicationRuntimeResult<()> {
        self.play_adjacent_track(self.playback_queue.previous_track_id())
    }

    fn play_next_track(&mut self) -> ApplicationRuntimeResult<()> {
        // Smart Shuffle path: the lazy queue hasn't decided a next
        // track yet (the picker is consulted on demand). Resolve a
        // pick first, append it to the queue's history, and feed it
        // through the same auto-advance machinery used by Eager
        // playback. Pure / Off shuffle queues short-circuit here
        // because `needs_lazy_pick` returns false for them.
        if self.playback_queue.needs_lazy_pick() {
            if let Some(picked_track_id) = self.pick_smart_shuffle_next() {
                return self.play_adjacent_track(Some(picked_track_id));
            }
        }
        self.play_adjacent_track(self.playback_queue.next_track_id())
    }

    /// Consult the Smart Shuffle picker for the next track to play,
    /// then append it to the queue's history. Returns `None` when
    /// no candidates remain or the queue is not in lazy layout.
    /// Emits the `SUSTAIN_LOG_SMART_SHUFFLE=1` trace line on stderr
    /// when the env var is set.
    fn pick_smart_shuffle_next(&mut self) -> Option<TrackId> {
        let context = self.playback_queue.lazy_pick_context()?;

        // Resolve pool / history ids to live `&Track`s through a map
        // built once per transition. The candidate pool is the whole
        // library, so a linear `find` per id would be quadratic at the
        // 10 000-track scale the performance budget targets.
        let by_id: HashMap<TrackId, &Track> = self
            .library_tracks
            .iter()
            .map(|track| (track.id, track))
            .collect();

        let seed = by_id.get(&context.seed_track_id).copied()?;
        // Candidate pool: every library track, minus missing files so
        // the picker never proposes something we cannot play. The
        // picker itself drops the seed and applies the guards.
        let candidate_refs: Vec<&Track> = context
            .candidate_pool
            .iter()
            .filter_map(|track_id| by_id.get(track_id).copied())
            .filter(|track| !track.location.is_missing())
            .collect();
        // Played history, most-recent-last, resolved to tracks for the
        // anti-repeat set and the same-artist streak.
        let history_refs: Vec<&Track> = context
            .played_history
            .iter()
            .filter_map(|track_id| by_id.get(track_id).copied())
            .collect();

        let pick_context = PickContext {
            seed,
            candidates: &candidate_refs,
            played_history: &history_refs,
            entropy: self.settings.playback.smart_shuffle_entropy,
            now: self.clock.now(),
        };
        let (picked, debug) = pick_next_track(self.smart_shuffle_index.as_ref(), pick_context)?;

        if let Some(debug) = debug {
            let label = |track_id: TrackId| -> String {
                by_id
                    .get(&track_id)
                    .and_then(|track| track.metadata.title.clone())
                    .unwrap_or_else(|| format!("#{}", track_id.get()))
            };
            eprintln!("{}", format_debug(&debug, label));
        }

        self.playback_queue.lazy_append_pick(picked.track_id);
        Some(picked.track_id)
    }

    fn play_adjacent_track(&mut self, track_id: Option<TrackId>) -> ApplicationRuntimeResult<()> {
        let Some(track_id) = track_id else {
            // End of queue (or no neighbour in the current direction). Stop
            // the backend so its state stops reporting the previous track as
            // still playing — otherwise the auto-advance triggered by EOS
            // would leave the UI showing the last track at a stale position.
            self.settle_current_playing_session();
            if let Some(service) = self.playback_service.as_deref() {
                let _ = service.stop();
            }
            self.playback_session = None;
            return Ok(());
        };

        self.play_track(track_id)?;
        let _moved = self.playback_queue.move_to_track(track_id);
        Ok(())
    }

    fn skip_current_track(&mut self) -> ApplicationRuntimeResult<()> {
        self.settle_current_playing_session();
        // Register a skip on the current session only when one exists
        // AND the play threshold has not already been reached. After
        // the threshold there is no skip — the play is already counted
        // or retained for retry.
        // This is the only entry point that ever increments skip_count;
        // EOS auto-advance and Previous never do.
        let pending_skip = self.playback_session.as_ref().and_then(|session| {
            (!session.is_play_registered() && !session.is_play_registration_pending())
                .then_some(session.track_id())
        });
        if let Some(track_id) = pending_skip {
            let now = self.clock.now();
            self.commit_skip_increment(track_id, now)?;
        }
        self.play_next_track()
    }

    /// Drive play-statistics accounting from the injected monotonic clock.
    /// The UI calls this on a nominal cadence, but delayed or coalesced
    /// callbacks still account the real elapsed interval. Accumulation only
    /// happens while the playback service reports [`PlaybackState::Playing`]
    /// and only against the track currently associated with the session.
    ///
    /// When the cumulative listened time crosses the play threshold
    /// (see [`PlaybackSession::play_threshold`]), the play count is
    /// incremented exactly once, `last_played_at` is updated, and the
    /// new statistics are flushed to SQLite. No file-tag write is
    /// emitted — listening statistics live exclusively in the
    /// library, per the persistence policy in AGENTS.md.
    pub fn on_playback_tick(&mut self) -> ApplicationRuntimeResult<()> {
        let now = self.monotonic_clock.now();
        let state = self.playback_state();
        match state {
            PlaybackState::Playing { track_id, .. } => {
                self.ensure_session_for_track(track_id, now);
                if let Some(session) = self.playback_session.as_mut() {
                    session.account_playing_until(now);
                }
                self.enqueue_play_registration_if_needed();
            }
            PlaybackState::Paused { .. }
            | PlaybackState::Stopped
            | PlaybackState::Loading { .. } => self.freeze_current_playback_session_at(now),
        }

        self.retry_pending_play_registrations()
    }

    fn ensure_session_for_track(&mut self, track_id: TrackId, now: Duration) {
        if let Some(session) = self.playback_session.as_ref()
            && session.track_id() == track_id
        {
            return;
        }
        let duration = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.metadata.duration)
            .unwrap_or(Duration::ZERO);
        self.start_playback_session_at(track_id, duration, now);
    }

    fn start_playback_session(&mut self, track_id: TrackId, duration: Duration) {
        self.start_playback_session_at(track_id, duration, self.monotonic_clock.now());
    }

    fn start_playback_session_at(&mut self, track_id: TrackId, duration: Duration, now: Duration) {
        let session_id = self.next_playback_session_id;
        self.next_playback_session_id = self
            .next_playback_session_id
            .checked_add(1)
            .expect("playback session id exhausted");
        let mut session = PlaybackSession::new(session_id, track_id, duration);
        session.resume_accounting(now);
        self.playback_session = Some(session);
    }

    fn settle_current_playing_session(&mut self) {
        if matches!(self.playback_state(), PlaybackState::Playing { .. }) {
            let now = self.monotonic_clock.now();
            if let Some(session) = self.playback_session.as_mut() {
                session.account_playing_until(now);
            }
            self.enqueue_play_registration_if_needed();
        }
        let _ = self.retry_pending_play_registrations();
    }

    fn freeze_current_playback_session(&mut self) {
        self.freeze_current_playback_session_at(self.monotonic_clock.now());
    }

    fn freeze_current_playback_session_at(&mut self, now: Duration) {
        if let Some(session) = self.playback_session.as_mut() {
            session.freeze_accounting(now);
        }
        self.enqueue_play_registration_if_needed();
    }

    fn enqueue_play_registration_if_needed(&mut self) {
        let Some(session) = self.playback_session.as_mut() else {
            return;
        };
        if !session.begin_play_registration() {
            return;
        }
        self.pending_play_registrations
            .push_back(PendingPlayRegistration {
                session_id: session.session_id(),
                track_id: session.track_id(),
                registered_at: self.clock.now(),
            });
    }

    fn retry_pending_play_registrations(&mut self) -> ApplicationRuntimeResult<()> {
        while let Some(pending) = self.pending_play_registrations.front().copied() {
            if let Err(error) = self.commit_play_increment(pending.track_id, pending.registered_at)
            {
                self.push_or_update_playback_statistics_warning();
                return Err(error);
            }
            self.pending_play_registrations.pop_front();
            if let Some(session) = self.playback_session.as_mut()
                && session.session_id() == pending.session_id
            {
                session.confirm_play_registration();
            }
        }

        self.dismiss_playback_statistics_warning();
        Ok(())
    }

    fn push_or_update_playback_statistics_warning(&mut self) {
        let body = "Sustain could not save listening statistics. The pending play count is retained in memory and will retry automatically."
            .to_owned();
        if let Some(id) = self.playback_statistics_notification_id {
            self.update_notification_body(id, body);
        } else {
            let id = self.push_persistent_notification(
                NotificationCategory::PlaybackStatistics,
                NotificationSeverity::Error,
                body,
                false,
            );
            self.playback_statistics_notification_id = Some(id);
        }
    }

    fn dismiss_playback_statistics_warning(&mut self) {
        if let Some(id) = self.playback_statistics_notification_id.take() {
            self.dismiss_notification(id);
        }
    }

    fn commit_play_increment(
        &mut self,
        track_id: TrackId,
        at: SystemTime,
    ) -> ApplicationRuntimeResult<()> {
        self.mutate_track_statistics(track_id, |statistics| {
            statistics.play_count = statistics.play_count.saturating_add(1);
            statistics.last_played_at = Some(at);
        })
    }

    fn commit_skip_increment(
        &mut self,
        track_id: TrackId,
        at: SystemTime,
    ) -> ApplicationRuntimeResult<()> {
        self.mutate_track_statistics(track_id, |statistics| {
            statistics.skip_count = statistics.skip_count.saturating_add(1);
            statistics.last_skipped_at = Some(at);
        })
    }

    // Applies the given mutation to a track's in-memory statistics,
    // persists the updated track row, and notifies the UI so the
    // affected table row repaints its play-count / last-played /
    // skip columns live (issue #46). When no library store is
    // installed — for instance in headless tests — only the in-memory
    // copy is updated; the SQLite write is a no-op so the same code
    // path stays callable.
    fn mutate_track_statistics<F>(
        &mut self,
        track_id: TrackId,
        mutate: F,
    ) -> ApplicationRuntimeResult<()>
    where
        F: FnOnce(&mut sustain_domain::PlayStatistics),
    {
        let Some(track_index) = self
            .library_tracks
            .iter()
            .position(|track| track.id == track_id)
        else {
            return Ok(());
        };
        let mut updated = self.library_tracks[track_index].clone();
        mutate(&mut updated.statistics);
        if let Some(store) = self.library_store.as_ref() {
            store
                .update_track_statistics(track_id, &updated.statistics)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }
        self.store_library_track(track_index, updated);
        self.fire_track_data_observer(track_id);
        Ok(())
    }

    fn playable_track_ids(&self) -> Vec<TrackId> {
        self.library_tracks
            .iter()
            .filter(|track| !track.location.is_missing())
            .map(|track| track.id)
            .collect()
    }

    /// Re-derive the queue's ordered track ids from the current library
    /// state, preserving the queue's source. Called after library-level
    /// mutations (scan, library move/import, settings update, track
    /// removal) so a track that just disappeared is dropped from the
    /// queue without stomping the user's selected queue context.
    ///
    /// When the source is `Library`, the queue is rebuilt from every
    /// playable track. When it's `Playlist(id)`, the queue is rebuilt
    /// from that playlist's authoritative entry order, intersected with
    /// the currently-playable tracks. Other sources (Album,
    /// SmartPlaylist, SearchResults, Selection) are ad-hoc lists the
    /// runtime cannot re-derive without UI context; for those we re-run
    /// the same filter against the queue's existing ids so missing
    /// tracks fall out, leaving everything else untouched.
    pub(super) fn refresh_playback_queue_track_ids(&mut self) {
        let playable: HashSet<TrackId> = self.playable_track_ids().into_iter().collect();
        let refreshed: Vec<TrackId> = match self.playback_queue.source().clone() {
            PlaybackQueueSource::Library => self.playable_track_ids(),
            PlaybackQueueSource::Playlist(playlist_id) => {
                match self.playlists().iter().find(|p| p.id == playlist_id) {
                    Some(playlist) => {
                        let mut entries: Vec<_> = playlist.entries.iter().collect();
                        entries.sort_by_key(|entry| entry.position);
                        entries
                            .into_iter()
                            .map(|entry| entry.track_id)
                            .filter(|id| playable.contains(id))
                            .collect()
                    }
                    None => Vec::new(),
                }
            }
            PlaybackQueueSource::Album
            | PlaybackQueueSource::SearchResults
            | PlaybackQueueSource::Selection => self
                .playback_queue
                .ordered_track_ids()
                .iter()
                .copied()
                .filter(|id| playable.contains(id))
                .collect(),
        };
        self.playback_queue
            .replace_ordered_track_ids(refreshed, playback_shuffle_seed());
    }
}

pub(super) fn playback_track_id(state: &PlaybackState) -> Option<TrackId> {
    match state {
        PlaybackState::Loading { track_id }
        | PlaybackState::Playing { track_id, .. }
        | PlaybackState::Paused { track_id, .. } => Some(*track_id),
        PlaybackState::Stopped => None,
    }
}

pub(super) fn playback_shuffle_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
}
