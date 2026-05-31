// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::Path;

use sustain_artwork::validate_encoded_artwork;
use sustain_domain::{LibraryManagementMode, MetadataChange, PlaybackCommand, Rating, TrackId};
use sustain_library_store::TagMirrorArtwork;

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, ArtworkFetchResult,
    ManagedMetadataRetargetResult,
    artwork_fetcher::{ArtworkFetchRequest, query_from_metadata},
    file_presence::{FilePresence, probe_path_entry_presence},
    managed_library::{metadata_change_affects_managed_path, retarget_managed_metadata},
    playback::{playback_shuffle_seed, playback_track_id},
};

impl ApplicationRuntime {
    pub(super) fn set_rating(
        &mut self,
        track_id: TrackId,
        rating: Rating,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_background_library_task()?;
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let track_index = self
            .library_tracks
            .iter()
            .position(|track| track.id == track_id && !track.location.is_missing())
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        // Apply the authoritative row and durable mirror intent in one
        // transaction so a crash cannot lose the courtesy file-tag write.
        let mut track = self.library_tracks[track_index].clone();
        track.rating = rating;
        library_store
            .update_track_rating_and_enqueue_mirror(track_id, rating)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.store_library_track(track_index, track);
        self.nudge_metadata_writer();

        Ok(())
    }

    pub(super) fn update_metadata(
        &mut self,
        track_id: TrackId,
        change: MetadataChange,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_background_library_task()?;
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let track_index = self
            .library_tracks
            .iter()
            .position(|track| track.id == track_id && !track.location.is_missing())
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        let managed_rename_needed = self.settings.library.management_mode
            == LibraryManagementMode::CopyAddedFilesIntoLibrary
            && metadata_change_affects_managed_path(&change);

        if managed_rename_needed {
            // Serialize retargets with every courtesy tag rewrite. The actor
            // reloads current SQLite state by id, durably moves first, then
            // commits metadata/path plus outbox intent atomically. It never
            // mirrors tags against an obsolete pathname.
            if let Some(writer) = self.metadata_writer() {
                if !writer.retarget_managed_metadata(track_id, change) {
                    return Err(ApplicationRuntimeError::LibraryServicesUnavailable);
                }
                self.register_pending_managed_metadata_retarget(track_id);
                return Ok(());
            }

            // Tests and headless callers may omit the actor. With no worker
            // there is no concurrent tag rewrite, so run the same retarget
            // operation synchronously and drain its durable mirror intent
            // through the common fallback below.
            let library_path = self
                .settings
                .library_path()
                .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?;
            let outcome = retarget_managed_metadata(
                library_path,
                library_store.as_ref(),
                &self.managed_library_filesystem_validator,
                track_id,
                &change,
            );
            self.apply_managed_metadata_retarget_result(ManagedMetadataRetargetResult {
                track_id,
                outcome: outcome.clone(),
            });
            outcome?;
            self.nudge_metadata_writer();
            return Ok(());
        }

        // Optimistic path: apply in-memory + SQLite synchronously; ship
        // the durable tag-mirror intent to the async writer so the UI
        // returns immediately.
        let mut track = self.library_tracks[track_index].clone();
        track.metadata.apply_change(&change);
        library_store
            .apply_track_metadata_change_and_enqueue_mirror(track_id, &change)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.store_library_track(track_index, track);
        self.nudge_metadata_writer();

        Ok(())
    }

    pub(super) fn set_artwork(
        &mut self,
        track_id: TrackId,
        artwork: Option<Vec<u8>>,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_background_library_task()?;
        if let Some(bytes) = artwork.as_deref() {
            validate_encoded_artwork(bytes)
                .map_err(|_| ApplicationRuntimeError::ArtworkRejected)?;
        }
        let _track = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id && !track.location.is_missing())
            .cloned()
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let artwork = match artwork {
            Some(bytes) => TagMirrorArtwork::Set(
                library_store
                    .publish_tag_mirror_artwork(&bytes)
                    .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?,
            ),
            None => TagMirrorArtwork::Clear,
        };
        library_store
            .enqueue_tag_mirror_artwork(track_id, artwork)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.nudge_metadata_writer();

        Ok(())
    }

    /// Submit a remote artwork fetch for `track_id`. Returns
    /// `Err(ArtworkFetchingUnavailable)` if no remote service is
    /// installed or the fetcher worker was never started — both are
    /// build-time conditions, not runtime ones, so the UI can decide
    /// up front whether to expose the click-to-fetch affordance.
    ///
    /// The fetch itself is asynchronous: the worker runs the network
    /// roundtrip and posts an [`ArtworkFetchResult`] through the
    /// runtime's result sink. The UI consumer dispatches a follow-up
    /// `SetArtwork` command on success to persist via the existing
    /// tag-writing path.
    pub(super) fn fetch_artwork(&self, track_id: TrackId) -> ApplicationRuntimeResult<()> {
        self.ensure_no_background_library_task()?;
        let track = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id && !track.location.is_missing())
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        let fetcher = self
            .artwork_fetcher()
            .ok_or(ApplicationRuntimeError::ArtworkFetchingUnavailable)?;

        let query = query_from_metadata(&track.metadata);
        let sink = self.artwork_fetch_result_sink();
        let completion: crate::artwork_fetcher::ArtworkFetchCompletionCallback =
            Box::new(move |outcome| {
                if let Some(sink) = sink {
                    let _ = sink.try_send(ArtworkFetchResult { track_id, outcome });
                }
            });
        fetcher.submit(ArtworkFetchRequest { query, completion });
        Ok(())
    }

    pub(super) fn reset_play_count(&mut self, track_id: TrackId) -> ApplicationRuntimeResult<()> {
        self.ensure_no_background_library_task()?;
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let track_index = self
            .library_tracks
            .iter()
            .position(|track| track.id == track_id)
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;

        let mut track = self.library_tracks[track_index].clone();
        track.statistics.play_count = 0;
        track.statistics.skip_count = 0;
        track.statistics.last_played_at = None;
        track.statistics.last_skipped_at = None;
        library_store
            .update_track_statistics(track_id, &track.statistics)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.store_library_track(track_index, track);

        Ok(())
    }

    pub(super) fn remove_track_from_library(
        &mut self,
        track_id: TrackId,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_conflicting_library_mutation()?;
        self.stop_playback_if_playing(track_id);
        let library_store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        library_store
            .delete_track(track_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.library_tracks.retain(|track| track.id != track_id);
        self.search_index.remove(track_id);
        self.playback_queue
            .remove_track(track_id, playback_shuffle_seed());
        Ok(())
    }

    pub(super) fn move_track_to_trash(
        &mut self,
        track_id: TrackId,
    ) -> ApplicationRuntimeResult<()> {
        self.move_track_to_trash_with(track_id, probe_path_entry_presence, |path| {
            trash::delete(path).map_err(|_| ())
        })
    }

    /// Fail-closed core of [`Self::move_track_to_trash`], parameterised over
    /// the file-presence `probe` and the `trash` operation so the success,
    /// confirmed-absence, probe-error, and trash-backend-failure paths can
    /// be exercised deterministically.
    ///
    /// The library row is removed only after the file has entered the trash
    /// or its absence has been *proven*. An unresolved library root or a
    /// probe error (permission, transient I/O) leaves the row in place: the
    /// user asked to trash the file, so deleting the record while the file
    /// may still be live on disk would be the worst outcome.
    pub(super) fn move_track_to_trash_with(
        &mut self,
        track_id: TrackId,
        probe: impl Fn(&Path) -> FilePresence,
        trash: impl Fn(&Path) -> Result<(), ()>,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_conflicting_library_mutation()?;
        let track = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id)
            .cloned()
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;

        self.stop_playback_if_playing(track_id);

        // An unresolved library root means the file cannot be located at
        // all — fail closed rather than treating it as permission to drop
        // the library row.
        let path = self
            .absolute_track_path(&track)
            .ok_or(ApplicationRuntimeError::TrackTrashFailed)?;

        match probe(&path) {
            FilePresence::Present => {
                trash(&path).map_err(|()| ApplicationRuntimeError::TrackTrashFailed)?;
            }
            // Confirmed gone: there is nothing to trash and removing the
            // stale row is exactly what the user wanted.
            FilePresence::Absent => {}
            // A permission or transient I/O error means we cannot tell
            // whether the file is still there. Fail closed.
            FilePresence::ProbeFailed => {
                return Err(ApplicationRuntimeError::TrackTrashFailed);
            }
        }

        self.remove_track_from_library(track_id)
    }

    fn stop_playback_if_playing(&mut self, track_id: TrackId) {
        let Some(service) = self.playback_service.as_deref() else {
            return;
        };
        if playback_track_id(&service.state()) == Some(track_id) {
            let _ = self.handle_playback_command(PlaybackCommand::Stop);
        }
    }

    fn ensure_no_background_library_task(&self) -> ApplicationRuntimeResult<()> {
        self.ensure_library_hydrated()?;
        if self.background_task_status.is_running() {
            return Err(ApplicationRuntimeError::BackgroundTaskRunning);
        }

        Ok(())
    }

    pub(crate) fn ensure_no_conflicting_library_mutation(&self) -> ApplicationRuntimeResult<()> {
        self.ensure_no_background_library_task()?;
        if self.has_pending_managed_metadata_retarget() {
            return Err(ApplicationRuntimeError::BackgroundTaskRunning);
        }

        Ok(())
    }

    /// Wake the durable outbox worker. Tests and headless callers that do not
    /// install the worker drain eligible rows synchronously through the same
    /// implementation so behavior remains deterministic.
    fn nudge_metadata_writer(&self) {
        match self.metadata_writer() {
            Some(writer) => writer.nudge(),
            None => {
                let (Some(metadata_service), Some(library_store)) =
                    (self.metadata_service.as_ref(), self.library_store.as_ref())
                else {
                    return;
                };
                crate::metadata_writer::drain_due_synchronously(
                    metadata_service.as_ref(),
                    library_store.as_ref(),
                    self.settings.library.path.as_ref(),
                    self.metadata_writer_event_sink.as_ref(),
                );
            }
        }
    }
}
