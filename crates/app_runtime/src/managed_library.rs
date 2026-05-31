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

use sustain_domain::{FieldChange, LibraryManagementMode, MetadataChange, Track, TrackId};
use sustain_library_store::StoreResult;

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult,
    LibraryConsolidationResult, LibraryConsolidationTask, LibraryImportResult, LibraryImportTask,
    NotificationCategory, NotificationSeverity, notifications,
};

mod capabilities;
mod consolidation;
mod file_ops;
mod import;
mod journal;

pub use capabilities::ManagedLibraryFilesystemError;
pub use consolidation::run_library_consolidation_task;
pub use import::run_library_import_task;
pub(crate) use journal::recover_library_consolidation_journal;

pub(crate) use capabilities::ManagedLibraryFilesystemValidator;
use consolidation::plan_managed_track_retarget;
use file_ops::{move_file_without_copy_or_overwrite_matching_identity, rollback_file_move};
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
        self.ensure_no_conflicting_library_mutation()?;
        if self.settings.library.management_mode == LibraryManagementMode::CopyAddedFilesIntoLibrary
        {
            self.ensure_managed_library_filesystem_supported()?;
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
            managed_library_filesystem_validator: self.managed_library_filesystem_validator.clone(),
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
        if !self.report_managed_library_filesystem_error(&error) {
            self.push_ephemeral_notification(
                NotificationCategory::LibraryImport,
                NotificationSeverity::Error,
                notifications::runtime_error_text(&error).to_owned(),
            );
        }
    }

    pub fn prepare_library_consolidation(
        &mut self,
    ) -> ApplicationRuntimeResult<LibraryConsolidationTask> {
        self.ensure_no_conflicting_library_mutation()?;
        if self.settings.library.management_mode != LibraryManagementMode::CopyAddedFilesIntoLibrary
        {
            return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
        }
        self.ensure_managed_library_filesystem_supported()?;

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
            managed_library_filesystem_validator: self.managed_library_filesystem_validator.clone(),
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
        if !self.report_managed_library_filesystem_error(&error) {
            self.push_ephemeral_notification(
                NotificationCategory::LibraryConsolidation,
                NotificationSeverity::Error,
                notifications::runtime_error_text(&error).to_owned(),
            );
        }
    }

    pub(crate) fn ensure_managed_library_filesystem_supported(
        &mut self,
    ) -> ApplicationRuntimeResult<()> {
        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        self.ensure_managed_library_filesystem_supported_at(&library_path)
    }

    pub(crate) fn ensure_managed_library_filesystem_supported_at(
        &mut self,
        library_path: &Path,
    ) -> ApplicationRuntimeResult<()> {
        match self
            .managed_library_filesystem_validator
            .validate(library_path)
        {
            Ok(()) => {
                self.dismiss_managed_library_filesystem_warning();
                Ok(())
            }
            Err(error) => {
                let error = ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(error);
                self.report_managed_library_filesystem_error(&error);
                Err(error)
            }
        }
    }

    pub(crate) fn report_managed_library_filesystem_error(
        &mut self,
        error: &ApplicationRuntimeError,
    ) -> bool {
        let ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(error) = error else {
            return false;
        };
        let body = error.user_message().to_owned();
        if let Some(id) = self.managed_library_filesystem_notification_id {
            self.update_notification_body(id, body);
        } else {
            let id = self.push_persistent_notification(
                NotificationCategory::ManagedLibraryFilesystem,
                NotificationSeverity::Error,
                body,
                false,
            );
            self.managed_library_filesystem_notification_id = Some(id);
        }
        true
    }

    pub(crate) fn dismiss_managed_library_filesystem_warning(&mut self) {
        if let Some(id) = self.managed_library_filesystem_notification_id.take() {
            self.dismiss_notification(id);
        }
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

pub(crate) fn retarget_managed_metadata(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    managed_library_filesystem_validator: &ManagedLibraryFilesystemValidator,
    track_id: TrackId,
    change: &MetadataChange,
) -> ApplicationRuntimeResult<()> {
    retarget_managed_metadata_with_persist(
        library_path,
        library_store,
        managed_library_filesystem_validator,
        track_id,
        change,
        |updated_track| {
            library_store.apply_track_metadata_change_and_location_and_enqueue_mirror(
                updated_track.id,
                change,
                &updated_track.location,
            )
        },
    )
}

fn retarget_managed_metadata_with_persist(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    managed_library_filesystem_validator: &ManagedLibraryFilesystemValidator,
    track_id: TrackId,
    change: &MetadataChange,
    persist_moved_track: impl FnOnce(&Track) -> StoreResult<()>,
) -> ApplicationRuntimeResult<()> {
    managed_library_filesystem_validator
        .validate(library_path)
        .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;
    recover_library_consolidation_journal(
        library_path,
        library_store,
        managed_library_filesystem_validator,
    )?;

    let existing_tracks = library_store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let mut track = library_store
        .track(track_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
    if track.location.is_missing() {
        return Err(ApplicationRuntimeError::TrackUnavailable);
    }
    track.metadata.apply_change(change);

    let plan = plan_managed_track_retarget(library_path, &existing_tracks, track)?;
    let Some(planned_move) = plan else {
        library_store
            .apply_track_metadata_change_and_enqueue_mirror(track_id, change)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        return Ok(());
    };

    write_consolidation_journal(library_path, std::slice::from_ref(&planned_move))?;

    if move_file_without_copy_or_overwrite_matching_identity(
        &planned_move.source_path,
        &planned_move.destination_path,
        planned_move.source_identity,
    )
    .is_err()
    {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }

    let updated_track = planned_move.updated_track;
    if persist_moved_track(&updated_track).is_err() {
        rollback_file_move(&planned_move.source_path, &planned_move.destination_path).ok();
        return Err(ApplicationRuntimeError::LibraryStoreFailed);
    }

    library_store
        .flush_durable()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    remove_consolidation_journal_if_present(library_path)?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::{
        cell::RefCell,
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use sustain_domain::{
        FieldChange, MetadataChange, PlayStatistics, Rating, Track, TrackLocation, TrackMetadata,
        TrackRelativePath,
    };
    use sustain_library_store::{InMemoryLibraryStore, LibraryStore, StoreError};

    use super::{
        ApplicationRuntimeError, ManagedLibraryFilesystemValidator,
        recover_library_consolidation_journal, retarget_managed_metadata_with_persist,
    };

    #[test]
    fn managed_metadata_retarget_rolls_back_file_move_when_store_commit_fails() {
        let root = unique_test_directory();
        let source_relative_path = relative_path("loose/song.flac");
        let source_path = source_relative_path.resolve(&root);
        fs::create_dir_all(source_path.parent().expect("source parent"))
            .expect("create source parent");
        fs::write(&source_path, b"audio").expect("write source");

        let store = InMemoryLibraryStore::new();
        let track = Track {
            id: track_id(1),
            location: TrackLocation::available(source_relative_path.clone()),
            metadata: TrackMetadata {
                title: Some("Old title".to_owned()),
                artist: Some("Artist".to_owned()),
                album: Some("Album".to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
        };
        store.save_track(track.clone()).expect("save track");

        let change = MetadataChange {
            title: FieldChange::Set("New title".to_owned()),
            ..MetadataChange::default()
        };
        let destination_path = RefCell::new(None);
        let validator = ManagedLibraryFilesystemValidator::default();
        let outcome = retarget_managed_metadata_with_persist(
            &root,
            &store,
            &validator,
            track.id,
            &change,
            |updated| {
                destination_path.replace(Some(updated.location.absolute_path(&root)));
                Err(StoreError::StoreUnavailable)
            },
        );

        assert_eq!(outcome, Err(ApplicationRuntimeError::LibraryStoreFailed));
        assert!(
            source_path.exists(),
            "rollback restores the original pathname"
        );
        assert!(
            !destination_path
                .borrow()
                .as_ref()
                .expect("planned destination")
                .exists(),
            "rollback removes the proposed canonical pathname"
        );
        assert_eq!(store.track(track.id).expect("reload track"), Some(track));
        assert!(
            root.join(".sustain-consolidation-journal").exists(),
            "the retained journal lets restart recovery prove the final state"
        );
        assert!(store.tag_mirrors_due(0, 10).expect("outbox").is_empty());

        recover_library_consolidation_journal(&root, &store, &validator)
            .expect("recover retained journal");
        assert!(!root.join(".sustain-consolidation-journal").exists());
        fs::remove_dir_all(root).expect("remove fixture root");
    }

    fn track_id(value: i64) -> sustain_domain::TrackId {
        sustain_domain::TrackId::new(value).expect("positive track id")
    }

    fn relative_path(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(path).expect("relative path")
    }

    fn unique_test_directory() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "sustain_managed_metadata_retarget_test_{}_{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
