// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Managed-library entry points: the [`ApplicationRuntime`] methods that drive
//! import and consolidation, plus the metadata-edit retarget path. The heavy
//! lifting lives in the submodules — `import` (adding files), `consolidation`
//! (relocating to the canonical layout), `journal` (crash recovery), and
//! `file_ops` (verified copies and no-overwrite moves).

use std::{
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};

use sustain_domain::{FieldChange, LibraryManagementMode, MetadataChange, Track};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult,
    LibraryConsolidationResult, LibraryConsolidationTask, LibraryImportResult, LibraryImportTask,
    NotificationCategory, NotificationSeverity, notifications,
};

mod consolidation;
mod file_ops;
mod import;
mod journal;

pub use consolidation::run_library_consolidation_task;
pub use import::run_library_import_task;
pub(crate) use journal::recover_library_consolidation_journal;

use consolidation::plan_managed_track_retarget;
use file_ops::{move_file_without_copy_or_overwrite, rollback_file_move};
use journal::{remove_consolidation_journal_if_present, write_consolidation_journal};

impl ApplicationRuntime {
    pub(super) fn add_external_library_items(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> ApplicationRuntimeResult<()> {
        let task = self.prepare_library_import(paths)?;
        match run_library_import_task(task) {
            Ok(result) => {
                self.apply_library_import_result(result);
                Ok(())
            }
            Err(error) => {
                self.fail_library_import(error.clone());
                Err(error)
            }
        }
    }

    pub fn prepare_library_import(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> ApplicationRuntimeResult<LibraryImportTask> {
        if self.background_task_status.is_running() {
            return Err(ApplicationRuntimeError::BackgroundTaskRunning);
        }

        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;

        let cancellation_requested = Arc::new(AtomicBool::new(false));
        self.library_import_cancellation = Some(cancellation_requested.clone());
        self.background_task_status = crate::BackgroundTaskStatus::LibraryImportRunning;
        let notification_id = self.push_persistent_notification(
            NotificationCategory::LibraryImport,
            NotificationSeverity::Info,
            notifications::library_import_running_text(),
            true,
        );
        self.library_import_notification_id = Some(notification_id);

        Ok(LibraryImportTask {
            paths,
            settings: self.settings.clone(),
            existing_tracks: self.library_tracks.clone(),
            library_store,
            metadata_service,
            cancellation_requested,
        })
    }

    pub fn apply_library_import_result(&mut self, result: LibraryImportResult) {
        let summary = result.summary;
        self.last_library_import_summary = Some(summary.clone());
        self.library_tracks.extend(result.tracks);
        self.library_tracks.sort_by_key(|track| track.id);
        self.rebuild_search_index();
        self.refresh_playback_queue_track_ids();
        self.library_import_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.library_import_notification_id.take() {
            self.dismiss_notification(id);
        }
        self.push_ephemeral_notification(
            NotificationCategory::LibraryImport,
            NotificationSeverity::Info,
            notifications::library_import_outcome_text(&summary),
        );
    }

    pub fn fail_library_import(&mut self, error: ApplicationRuntimeError) {
        self.library_import_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.library_import_notification_id.take() {
            self.dismiss_notification(id);
        }
        self.push_ephemeral_notification(
            NotificationCategory::LibraryImport,
            NotificationSeverity::Error,
            notifications::runtime_error_text(&error).to_owned(),
        );
    }

    pub fn prepare_library_consolidation(
        &mut self,
    ) -> ApplicationRuntimeResult<LibraryConsolidationTask> {
        if self.background_task_status.is_running() {
            return Err(ApplicationRuntimeError::BackgroundTaskRunning);
        }
        if self.settings.library.management_mode != LibraryManagementMode::CopyAddedFilesIntoLibrary
        {
            return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
        }

        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let cancellation_requested = Arc::new(AtomicBool::new(false));
        self.library_consolidation_cancellation = Some(cancellation_requested.clone());
        self.background_task_status = crate::BackgroundTaskStatus::LibraryConsolidationRunning;
        let notification_id = self.push_persistent_notification(
            NotificationCategory::LibraryConsolidation,
            NotificationSeverity::Info,
            notifications::library_consolidation_running_text(),
            true,
        );
        self.library_consolidation_notification_id = Some(notification_id);

        Ok(LibraryConsolidationTask {
            settings: self.settings.clone(),
            existing_tracks: self.library_tracks.clone(),
            library_store,
            cancellation_requested,
        })
    }

    pub fn apply_library_consolidation_result(&mut self, result: LibraryConsolidationResult) {
        let summary = result.summary;
        self.last_library_consolidation_summary = Some(summary.clone());
        // `result.tracks` now carries both the relocated (still
        // available) tracks AND any rows whose `is_missing` flag the
        // planner flipped because the source file had vanished —
        // fire the availability observer so the UI repaints the
        // status column on those rows without the cost of a full
        // table rebuild.
        let flipped_availability = result.tracks.iter().any(|incoming| {
            self.library_tracks
                .iter()
                .find(|existing| existing.id == incoming.id)
                .is_some_and(|existing| {
                    existing.location.is_missing() != incoming.location.is_missing()
                })
        });
        replace_library_track_locations_by_id(&mut self.library_tracks, result.tracks);
        self.rebuild_search_index();
        self.refresh_playback_queue_track_ids();
        if flipped_availability {
            self.notify_track_availability_observer();
        }
        self.library_consolidation_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.library_consolidation_notification_id.take() {
            self.dismiss_notification(id);
        }
        // An auto-resume that found nothing to move and nothing
        // missing is silenced by the auto-dismiss timer just like any
        // ephemeral — no special "boring success" branch needed.
        self.push_ephemeral_notification(
            NotificationCategory::LibraryConsolidation,
            NotificationSeverity::Info,
            notifications::library_consolidation_outcome_text(&summary),
        );
    }

    pub fn fail_library_consolidation(&mut self, error: ApplicationRuntimeError) {
        self.library_consolidation_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.library_consolidation_notification_id.take() {
            self.dismiss_notification(id);
        }
        self.push_ephemeral_notification(
            NotificationCategory::LibraryConsolidation,
            NotificationSeverity::Error,
            notifications::runtime_error_text(&error).to_owned(),
        );
    }
}

pub(super) fn metadata_change_affects_managed_path(change: &MetadataChange) -> bool {
    !matches!(change.title, FieldChange::Unchanged)
        || !matches!(change.artist, FieldChange::Unchanged)
        || !matches!(change.album, FieldChange::Unchanged)
        || !matches!(change.album_artist, FieldChange::Unchanged)
        || !matches!(change.composer, FieldChange::Unchanged)
        || !matches!(change.track_number, FieldChange::Unchanged)
        || !matches!(change.disc_number, FieldChange::Unchanged)
        || !matches!(change.disc_total, FieldChange::Unchanged)
        || !matches!(change.compilation, FieldChange::Unchanged)
}

pub(super) fn save_managed_metadata_update(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    existing_tracks: &[Track],
    track: Track,
    change: &MetadataChange,
) -> ApplicationRuntimeResult<Track> {
    recover_library_consolidation_journal(library_path, library_store)?;

    let plan = plan_managed_track_retarget(library_path, existing_tracks, track.clone())?;
    let Some(planned_move) = plan else {
        library_store
            .apply_track_metadata_change(track.id, change)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        return Ok(track);
    };

    write_consolidation_journal(library_path, std::slice::from_ref(&planned_move))?;

    if move_file_without_copy_or_overwrite(
        &planned_move.source_path,
        &planned_move.destination_path,
    )
    .is_err()
    {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }

    let updated_track = planned_move.updated_track;
    if library_store
        .apply_track_metadata_change_and_location(updated_track.id, change, &updated_track.location)
        .is_err()
    {
        rollback_file_move(&planned_move.source_path, &planned_move.destination_path).ok();
        return Err(ApplicationRuntimeError::LibraryStoreFailed);
    }

    remove_consolidation_journal_if_present(library_path)?;
    Ok(updated_track)
}

fn replace_library_track_locations_by_id(library_tracks: &mut [Track], updated_tracks: Vec<Track>) {
    for updated_track in updated_tracks {
        if let Some(track) = library_tracks
            .iter_mut()
            .find(|track| track.id == updated_track.id)
        {
            track.location = updated_track.location;
        }
    }
    library_tracks.sort_by_key(|track| track.id);
}
