// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    fs,
    path::{Path, PathBuf},
};

use sustain_artwork::validate_encoded_artwork;
use sustain_domain::{
    FieldChange, LibraryManagementMode, MetadataChange, PlaybackCommand, Rating, TrackId,
    TrackLocation, TrackRelativePath, valid_bpm,
};
use sustain_library_store::TagMirrorArtwork;
use sustain_metadata::audio_format_from_path;

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, ArtworkFetchResult,
    ManagedMetadataRetargetResult, MissingTrackRelocationResult,
    artwork_fetcher::{ArtworkFetchRequest, query_from_metadata},
    freedesktop_trash,
    managed_library::{
        file_ops::open_regular_file_if_present, metadata_change_affects_managed_path,
        prune_empty_ancestor_directories_for_sources, relocate_managed_missing_track,
        retarget_managed_metadata,
    },
    playback::playback_track_id,
};

impl ApplicationRuntime {
    pub(super) fn relocate_missing_track(
        &mut self,
        track_id: TrackId,
        replacement_path: &Path,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_conflicting_library_mutation()?;
        if !self
            .library_tracks
            .iter()
            .any(|track| track.id == track_id && track.location.is_missing())
        {
            return Err(ApplicationRuntimeError::TrackUnavailable);
        }
        if let Some(writer) = self.metadata_writer() {
            if !writer.relocate_missing_track(
                track_id,
                replacement_path.to_path_buf(),
                self.settings.library.management_mode,
            ) {
                return Err(ApplicationRuntimeError::LibraryServicesUnavailable);
            }
            self.register_pending_missing_track_relocation(track_id);
            return Ok(());
        }

        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let outcome = relocate_missing_track_with_store(
            &library_path,
            self.settings.library.management_mode,
            library_store.as_ref(),
            &self.managed_library_filesystem_validator,
            track_id,
            replacement_path,
        )?;
        self.apply_missing_track_relocation(track_id, outcome.empty_directory_cleanup_failed);
        Ok(())
    }

    pub(crate) fn apply_missing_track_relocation_result(
        &mut self,
        result: MissingTrackRelocationResult,
    ) {
        self.finish_pending_missing_track_relocation(result.track_id);
        match result.outcome {
            Ok(()) => {
                self.apply_missing_track_relocation(
                    result.track_id,
                    result.empty_directory_cleanup_failed,
                );
            }
            Err(error) => {
                if !self.report_managed_library_filesystem_error(&error) {
                    self.push_ephemeral_notification(
                        crate::NotificationCategory::Command,
                        crate::NotificationSeverity::Error,
                        crate::runtime_error_text(&error).to_owned(),
                    );
                }
            }
        }
    }

    fn apply_missing_track_relocation(
        &mut self,
        track_id: TrackId,
        empty_directory_cleanup_failed: bool,
    ) {
        // The actor may have run concurrently with optimistic rating or
        // metadata edits. Reload SQLite by id rather than applying the
        // relocation planner's older snapshot over newer authoritative data.
        self.apply_track_updated(track_id);
        self.refresh_playback_queue_track_ids();
        self.notify_track_availability_observer();
        self.nudge_metadata_writer();
        if empty_directory_cleanup_failed {
            self.push_ephemeral_notification(
                crate::NotificationCategory::LibraryImport,
                crate::NotificationSeverity::Warning,
                "The track was located, but empty library folders could not be removed.".to_owned(),
            );
        }
    }

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
        validate_metadata_change(&change)?;
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
            let empty_directory_cleanup_failed = outcome
                .as_ref()
                .is_ok_and(|outcome| outcome.empty_directory_cleanup_failed);
            let outcome = outcome.map(|_| ());
            self.apply_managed_metadata_retarget_result(ManagedMetadataRetargetResult {
                track_id,
                outcome: outcome.clone(),
                empty_directory_cleanup_failed,
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
        self.playback_queue.remove_track(track_id);
        self.dismiss_all_metadata_write_warnings(track_id);
        Ok(())
    }

    pub(super) fn move_track_to_trash(
        &mut self,
        track_id: TrackId,
    ) -> ApplicationRuntimeResult<()> {
        self.move_track_to_trash_with_token(
            track_id,
            |path| open_regular_file_if_present(path).map_err(|_| ()),
            |path, source| freedesktop_trash::trash_regular_file(path, &source).map_err(|_| ()),
        )
    }

    /// Fail-closed core of [`Self::move_track_to_trash`], parameterised over
    /// the file-identity `probe` and the `trash` operation so the success,
    /// confirmed-absence, probe-error, and trash-backend-failure paths can
    /// be exercised deterministically.
    ///
    /// The library row is removed only after the file has entered the trash
    /// or its absence has been *proven*. An unresolved library root or a
    /// probe error (permission, transient I/O) leaves the row in place: the
    /// user asked to trash the file, so deleting the record while the file
    /// may still be live on disk would be the worst outcome.
    #[cfg(test)]
    pub(super) fn move_track_to_trash_with(
        &mut self,
        track_id: TrackId,
        probe: impl Fn(&Path) -> Result<Option<crate::managed_library::file_ops::FileIdentity>, ()>,
        trash: impl Fn(&Path, crate::managed_library::file_ops::FileIdentity) -> Result<(), ()>,
    ) -> ApplicationRuntimeResult<()> {
        self.move_track_to_trash_with_token(track_id, probe, trash)
    }

    fn move_track_to_trash_with_token<T>(
        &mut self,
        track_id: TrackId,
        probe: impl Fn(&Path) -> Result<Option<T>, ()>,
        trash: impl Fn(&Path, T) -> Result<(), ()>,
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
            Ok(Some(source)) => {
                trash(&path, source).map_err(|()| ApplicationRuntimeError::TrackTrashFailed)?;
            }
            // Confirmed gone: there is nothing to trash and removing the
            // stale row is exactly what the user wanted.
            Ok(None) => {}
            // A permission or transient I/O error means we cannot tell
            // whether the file is still there. Fail closed.
            Err(()) => {
                return Err(ApplicationRuntimeError::TrackTrashFailed);
            }
        }

        if self.settings.library.management_mode == LibraryManagementMode::CopyAddedFilesIntoLibrary
            && let Some(library_root) = self.settings.library_path()
        {
            let prune_outcome = prune_empty_ancestor_directories_for_sources(
                library_root,
                std::slice::from_ref(&path),
            );
            if prune_outcome.failed {
                self.push_managed_library_cleanup_warning();
            }
        }

        self.remove_track_from_library(track_id)
    }

    pub(crate) fn stop_playback_if_playing(&mut self, track_id: TrackId) {
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
        if self.has_pending_managed_metadata_retarget()
            || self.has_pending_missing_track_relocation()
            || self.has_pending_youtube_audio_replacement()
        {
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

fn validate_metadata_change(change: &MetadataChange) -> ApplicationRuntimeResult<()> {
    if let FieldChange::Set(bpm) = change.bpm
        && !valid_bpm(bpm)
    {
        return Err(ApplicationRuntimeError::InvalidBpm);
    }
    Ok(())
}

pub(crate) struct MissingTrackRelocationOutcome {
    pub(crate) empty_directory_cleanup_failed: bool,
}

pub(crate) fn relocate_missing_track_with_store(
    library_path: &Path,
    management_mode: LibraryManagementMode,
    library_store: &dyn sustain_library_store::LibraryStore,
    managed_library_filesystem_validator: &crate::managed_library::ManagedLibraryFilesystemValidator,
    track_id: TrackId,
    replacement_path: &Path,
) -> ApplicationRuntimeResult<MissingTrackRelocationOutcome> {
    let (replacement_path, replacement_file_size_bytes) =
        validate_missing_track_replacement(replacement_path)?;
    let existing_tracks = library_store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let track = existing_tracks
        .iter()
        .find(|track| track.id == track_id && track.location.is_missing())
        .cloned()
        .ok_or(ApplicationRuntimeError::TrackUnavailable)?;

    match management_mode {
        LibraryManagementMode::ReferenceFilesInPlace => {
            let relative_path = relative_replacement_path(&replacement_path, library_path)?;
            if existing_tracks.iter().any(|existing_track| {
                existing_track.id != track_id
                    && existing_track.location.relative_path == relative_path
            }) {
                return Err(ApplicationRuntimeError::TrackReplacementAlreadyInLibrary);
            }
            let mut updated_track = track;
            updated_track.location = TrackLocation::available(relative_path);
            updated_track.file_size_bytes = Some(replacement_file_size_bytes);
            updated_track.has_embedded_artwork = None;
            updated_track.file_modified_at = None;
            library_store
                .relocate_track_and_enqueue_mirror(
                    track_id,
                    &updated_track.location,
                    replacement_file_size_bytes,
                )
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
            Ok(MissingTrackRelocationOutcome {
                empty_directory_cleanup_failed: false,
            })
        }
        LibraryManagementMode::CopyAddedFilesIntoLibrary => {
            let outcome = relocate_managed_missing_track(
                library_path,
                library_store,
                managed_library_filesystem_validator,
                track_id,
                &replacement_path,
                replacement_file_size_bytes,
            )?;
            Ok(MissingTrackRelocationOutcome {
                empty_directory_cleanup_failed: outcome.empty_directory_cleanup_failed,
            })
        }
    }
}

fn validate_missing_track_replacement(path: &Path) -> ApplicationRuntimeResult<(PathBuf, u64)> {
    let canonical_path =
        fs::canonicalize(path).map_err(|_| ApplicationRuntimeError::TrackRelocationFailed)?;
    let metadata = fs::metadata(&canonical_path)
        .map_err(|_| ApplicationRuntimeError::TrackRelocationFailed)?;
    if !metadata.is_file() {
        return Err(ApplicationRuntimeError::TrackRelocationFailed);
    }
    audio_format_from_path(&canonical_path)
        .map_err(|_| ApplicationRuntimeError::TrackReplacementUnsupported)?;
    Ok((canonical_path, metadata.len()))
}

fn relative_replacement_path(
    replacement_path: &Path,
    library_path: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    let canonical_library_path = fs::canonicalize(library_path)
        .map_err(|_| ApplicationRuntimeError::LibraryPathUnavailable)?;
    replacement_path
        .strip_prefix(canonical_library_path)
        .ok()
        .and_then(|relative_path| TrackRelativePath::new(relative_path.to_path_buf()))
        .ok_or(ApplicationRuntimeError::TrackReplacementOutsideLibrary)
}
