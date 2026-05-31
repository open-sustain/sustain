// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::Path;

use sustain_domain::{LibraryManagementMode, MetadataChange, Rating, TrackId};

use crate::MetadataWriteKind;
use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, ArtworkFetchResult,
    artwork_fetcher::{ArtworkFetchRequest, query_from_metadata},
    managed_library::{metadata_change_affects_managed_path, save_managed_metadata_update},
    metadata_writer::{
        MetadataWriteJob, MetadataWriteOutcome, MetadataWriteRequest, MetadataWriteResult,
    },
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
        let path = self
            .absolute_track_path(&self.library_tracks[track_index])
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;

        // Optimistic update: apply in-memory + SQLite synchronously so
        // the UI sees the new rating immediately. The tag write to the
        // audio file goes through the async writer below — the GTK main
        // thread is never blocked on disk I/O for a star click.
        let mut track = self.library_tracks[track_index].clone();
        track.rating = rating;
        library_store
            .update_track_rating(track_id, rating)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.store_library_track(track_index, track);

        self.submit_metadata_write(
            track_id,
            MetadataWriteKind::Rating,
            MetadataWriteJob::Rating { path, rating },
        );

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
        let path = self
            .absolute_track_path(&self.library_tracks[track_index])
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;

        let managed_rename_needed = self.settings.library.management_mode
            == LibraryManagementMode::CopyAddedFilesIntoLibrary
            && metadata_change_affects_managed_path(&change);

        if managed_rename_needed {
            // The managed-rename branch is a transactional sequence —
            // write the tag, move the file to its new computed path,
            // persist with the new relative path, rollback on failure —
            // so we keep it synchronous. Async-ifying it would require
            // moving the journal/rollback dance into the worker, which
            // is a separate, careful piece of work.
            let metadata_service = self
                .metadata_service
                .clone()
                .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
            metadata_service
                .write_metadata(&path, change.clone())
                .map_err(|_| ApplicationRuntimeError::MetadataWriteFailed)?;
            let mut track = self.library_tracks[track_index].clone();
            track.metadata.apply_change(&change);
            let library_path = self
                .settings
                .library_path()
                .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?;
            let track = save_managed_metadata_update(
                library_path,
                library_store.as_ref(),
                &self.library_tracks,
                track,
                &change,
            )?;
            self.store_library_track(track_index, track);
            return Ok(());
        }

        // Optimistic path: apply in-memory + SQLite synchronously; ship
        // the tag write off to the async writer so the UI returns
        // immediately.
        let mut track = self.library_tracks[track_index].clone();
        track.metadata.apply_change(&change);
        library_store
            .apply_track_metadata_change(track_id, &change)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.store_library_track(track_index, track);

        self.submit_metadata_write(
            track_id,
            MetadataWriteKind::Metadata,
            MetadataWriteJob::Metadata {
                path,
                change: Box::new(change),
            },
        );

        Ok(())
    }

    pub(super) fn set_artwork(
        &mut self,
        track_id: TrackId,
        artwork: Option<Vec<u8>>,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_background_library_task()?;
        let track = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id && !track.location.is_missing())
            .cloned()
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        let path = self
            .absolute_track_path(&track)
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;

        // Artwork is not cached in the Track model, so there is no
        // in-memory or SQLite optimistic state to apply — only the disk
        // tag write. Ship it to the async writer so the UI returns
        // immediately even for the worst case (large embedded cover
        // rewriting a ~100 MB FLAC).
        self.submit_metadata_write(
            track_id,
            MetadataWriteKind::Artwork,
            MetadataWriteJob::Artwork { path, artwork },
        );

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
        self.ensure_no_background_library_task()?;
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
        self.move_track_to_trash_with(track_id, probe_file_presence, |path| {
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
        self.ensure_no_background_library_task()?;
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

    fn stop_playback_if_playing(&self, track_id: TrackId) {
        let Some(service) = self.playback_service.as_deref() else {
            return;
        };
        if playback_track_id(&service.state()) == Some(track_id) {
            let _ = service.stop();
        }
    }

    fn ensure_no_background_library_task(&self) -> ApplicationRuntimeResult<()> {
        if self.background_task_status.is_running() {
            return Err(ApplicationRuntimeError::BackgroundTaskRunning);
        }

        Ok(())
    }

    /// Submits a tag write to the async writer, attaching a completion
    /// callback that forwards the per-write outcome through the result
    /// sink (typically consumed by the UI's main loop). If no writer is
    /// installed — common in tests — falls back to running the write
    /// synchronously on the calling thread so behaviour stays
    /// deterministic.
    fn submit_metadata_write(
        &self,
        track_id: TrackId,
        kind: MetadataWriteKind,
        job: MetadataWriteJob,
    ) {
        let sink = self.metadata_write_result_sink();
        let completion: crate::metadata_writer::WriteCompletionCallback =
            Box::new(move |outcome: MetadataWriteOutcome| {
                if let Some(sink) = sink {
                    // `try_send` only fails on a closed channel, which
                    // means the UI has torn down its receiver. Dropping
                    // the result silently is correct at shutdown.
                    let _ = sink.try_send(MetadataWriteResult {
                        track_id,
                        kind,
                        outcome,
                    });
                }
            });

        match self.metadata_writer() {
            Some(writer) => writer.submit(MetadataWriteRequest { job, completion }),
            None => {
                // No async writer installed: run synchronously so tests
                // and headless callers still see the disk-side effect.
                let metadata_service = match self.metadata_service.clone() {
                    Some(service) => service,
                    None => {
                        completion(MetadataWriteOutcome::Failed);
                        return;
                    }
                };
                let result = match job {
                    MetadataWriteJob::Rating { path, rating } => {
                        metadata_service.write_rating(&path, rating)
                    }
                    MetadataWriteJob::Metadata { path, change } => {
                        metadata_service.write_metadata(&path, *change)
                    }
                    MetadataWriteJob::Artwork { path, artwork } => {
                        metadata_service.write_artwork(&path, artwork)
                    }
                };
                let outcome = match result {
                    Ok(()) => MetadataWriteOutcome::Succeeded,
                    Err(_) => MetadataWriteOutcome::Failed,
                };
                completion(outcome);
            }
        }
    }
}

/// Three-state result of probing whether a file exists, distinguishing a
/// *proven* absence from a probe that could not answer. `Path::exists`
/// collapses the latter two into `false`, which is unsafe for destructive
/// actions: a permission or transient-I/O error must not read as "the file
/// is gone, drop its record".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilePresence {
    /// The path resolves to an existing file, directory, or symlink.
    Present,
    /// The path is confirmed not to exist (`ErrorKind::NotFound`).
    Absent,
    /// The presence could not be determined (permission denied, transient
    /// I/O error). Callers must fail closed.
    ProbeFailed,
}

/// Probe a path's presence without following symlinks, so a dangling
/// symlink (a real on-disk entry the user may want trashed) reads as
/// [`FilePresence::Present`] rather than as its missing target.
pub(crate) fn probe_file_presence(path: &Path) -> FilePresence {
    match std::fs::symlink_metadata(path) {
        Ok(_) => FilePresence::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FilePresence::Absent,
        Err(_) => FilePresence::ProbeFailed,
    }
}
