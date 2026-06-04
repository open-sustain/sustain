// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! User-confirmed duplicate consolidation with crash recovery.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use sustain_domain::{
    DuplicateConsolidationRequest, DuplicateMatchMode, Track, TrackId, TrackRelativePath,
    duplicate_groups, plan_duplicate_consolidation,
};
use sustain_library_store::LibraryStore;
use sustain_metadata::{MetadataService, hash_reader_content};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, BackgroundTaskStatus,
    NotificationCategory, NotificationSeverity,
    managed_library::{
        ManagedLibraryFilesystemValidator,
        file_ops::{
            FileIdentity, PinnedFilePath, RegularFileCapability, open_regular_file,
            path_refers_to_capability, publish_file_capability,
            publish_pinned_file_without_overwrite, regular_file_capability_from_file,
            regular_file_identity, remove_file_and_sync_parent,
            remove_pinned_regular_file_matching_capability,
            remove_regular_file_matching_capability,
        },
    },
    metadata_writer::full_metadata_mirror,
    notifications::runtime_error_text,
};

const JOURNAL_NAME: &str = ".sustain-duplicate-consolidation-journal";
const JOURNAL_HEADER: &str = "# sustain duplicate consolidation journal v1";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DuplicateConsolidationSummary {
    pub removed_tracks: usize,
    pub cleanup_failed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateConsolidationResult {
    pub survivor_id: TrackId,
    pub removed_track_ids: Vec<TrackId>,
    pub summary: DuplicateConsolidationSummary,
}

pub struct DuplicateGroupsTask {
    store: std::sync::Arc<dyn LibraryStore>,
    mode: DuplicateMatchMode,
}

impl DuplicateGroupsTask {
    pub fn run(self) -> ApplicationRuntimeResult<Vec<Vec<TrackId>>> {
        let tracks = self
            .store
            .tracks()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        Ok(duplicate_groups(&tracks, self.mode))
    }
}

impl ApplicationRuntime {
    pub fn duplicate_groups_task(
        &self,
        mode: DuplicateMatchMode,
    ) -> ApplicationRuntimeResult<DuplicateGroupsTask> {
        self.ensure_library_hydrated()?;
        let store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        Ok(DuplicateGroupsTask { store, mode })
    }

    pub fn consolidate_duplicate_tracks(
        &mut self,
        request: DuplicateConsolidationRequest,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_conflicting_library_mutation()?;
        let library_root = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        let store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;

        for track_id in &request.track_ids {
            self.stop_playback_if_playing(*track_id);
        }
        self.background_task_status = BackgroundTaskStatus::DuplicateConsolidationRunning;
        self.duplicate_consolidation_notification_id = Some(self.push_persistent_notification(
            NotificationCategory::DuplicateConsolidation,
            NotificationSeverity::Info,
            "Consolidating duplicate tracks...".to_owned(),
            false,
        ));

        if let Some(writer) = self.metadata_writer() {
            if writer.consolidate_duplicate_tracks(request) {
                return Ok(());
            }
            let error = ApplicationRuntimeError::DuplicateConsolidationFailed;
            self.apply_duplicate_consolidation_result(
                crate::metadata_writer::DuplicateConsolidationWriterResult {
                    outcome: Err(error.clone()),
                },
            );
            return Err(error);
        }

        let outcome = consolidate_duplicate_tracks(
            &library_root,
            store.as_ref(),
            metadata_service.as_ref(),
            &self.managed_library_filesystem_validator,
            &request,
        );
        let command_outcome = outcome.as_ref().map(|_| ()).map_err(Clone::clone);
        self.apply_duplicate_consolidation_result(
            crate::metadata_writer::DuplicateConsolidationWriterResult { outcome },
        );
        command_outcome
    }

    pub(crate) fn apply_duplicate_consolidation_result(
        &mut self,
        result: crate::metadata_writer::DuplicateConsolidationWriterResult,
    ) {
        if let Some(id) = self.duplicate_consolidation_notification_id.take() {
            self.dismiss_notification(id);
        }
        self.background_task_status = BackgroundTaskStatus::Idle;

        match result.outcome {
            Ok(result) => {
                let refresh = self.library_store.as_ref().map_or(
                    Err(ApplicationRuntimeError::LibraryServicesUnavailable),
                    |store| {
                        let tracks = crate::library_scan::load_library_tracks(store.as_ref())?;
                        let playlists = store
                            .playlists()
                            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
                        Ok((tracks, playlists))
                    },
                );
                let Ok((tracks, playlists)) = refresh else {
                    self.push_ephemeral_notification(
                        NotificationCategory::DuplicateConsolidation,
                        NotificationSeverity::Error,
                        runtime_error_text(&ApplicationRuntimeError::LibraryStoreFailed).to_owned(),
                    );
                    return;
                };
                self.library_tracks = tracks;
                self.playlists = playlists;
                self.rebuild_search_index();
                self.refresh_playback_queue_track_ids();
                self.request_smart_shuffle_rebuild();
                let (severity, body) = if result.summary.cleanup_failed {
                    (
                        NotificationSeverity::Warning,
                        format!(
                            "{} duplicate track(s) consolidated. Sustain retained recovery files because cleanup could not finish safely.",
                            result.summary.removed_tracks
                        ),
                    )
                } else {
                    (
                        NotificationSeverity::Info,
                        format!(
                            "{} duplicate track(s) consolidated.",
                            result.summary.removed_tracks
                        ),
                    )
                };
                self.push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    severity,
                    body,
                );
            }
            Err(error) => {
                self.push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    NotificationSeverity::Error,
                    runtime_error_text(&error).to_owned(),
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
struct JournalEntry {
    track_id: TrackId,
    source_identity: FileIdentity,
    source_capability: Option<RegularFileCapability>,
    source: TrackRelativePath,
    backup: TrackRelativePath,
}

#[derive(Clone, Debug)]
struct Journal {
    survivor_id: TrackId,
    stage: TrackRelativePath,
    entries: Vec<JournalEntry>,
}

pub(crate) fn consolidate_duplicate_tracks(
    library_root: &Path,
    store: &dyn LibraryStore,
    metadata_service: &dyn MetadataService,
    filesystem_validator: &ManagedLibraryFilesystemValidator,
    request: &DuplicateConsolidationRequest,
) -> ApplicationRuntimeResult<DuplicateConsolidationResult> {
    filesystem_validator
        .validate(library_root)
        .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;
    recover_duplicate_consolidation_journal(library_root, store)?;

    let tracks = store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let playlists = store
        .playlists()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    // Validate every user-controlled reference and the enforced audio-quality
    // policy before publishing a journal or touching any pathname.
    plan_duplicate_consolidation(&tracks, &playlists, request, 0, false)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let selected = selected_tracks(&tracks, request)?;
    // Every selected file is hard-linked, rewritten, and removed, so a single
    // file missing on disk would leave the merge half-done. Reject up front
    // with a distinct error (#126) rather than failing opaquely mid-journal.
    ensure_selected_files_present(library_root, &selected)?;
    let journal = plan_journal(library_root, request.audio_track_id, &selected)?;
    let artwork_reference = selected
        .iter()
        .find(|track| track.id == request.artwork_track_id)
        .expect("selected references validated");
    let artwork = metadata_service
        .read_artwork(&artwork_reference.location.absolute_path(library_root))
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;

    write_journal(library_root, &journal)?;
    let pre_commit = run_pre_commit(
        library_root,
        store,
        metadata_service,
        request,
        &tracks,
        &playlists,
        &journal,
        &artwork,
    );
    let plan = match pre_commit {
        Ok(plan) => plan,
        Err(error) => {
            rollback_pre_commit(library_root, &journal)?;
            return Err(error);
        }
    };

    store
        .flush_durable()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let cleanup_failed = cleanup_committed(library_root, &journal).is_err();
    Ok(DuplicateConsolidationResult {
        survivor_id: plan.survivor.id,
        removed_track_ids: plan.removed_track_ids,
        summary: DuplicateConsolidationSummary {
            removed_tracks: journal.entries.len().saturating_sub(1),
            cleanup_failed,
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn run_pre_commit(
    library_root: &Path,
    store: &dyn LibraryStore,
    metadata_service: &dyn MetadataService,
    request: &DuplicateConsolidationRequest,
    tracks: &[Track],
    playlists: &[sustain_domain::Playlist],
    journal: &Journal,
    artwork: &Option<Vec<u8>>,
) -> ApplicationRuntimeResult<sustain_domain::DuplicateConsolidationPlan> {
    for entry in &journal.entries {
        let source = entry
            .source_capability
            .as_ref()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        if !path_refers_to_capability(&entry.source.resolve(library_root), source).unwrap_or(false)
        {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        create_link(source, &entry.backup.resolve(library_root))?;
    }
    let survivor_entry = survivor_entry(journal);
    let survivor_source = survivor_entry.source.resolve(library_root);
    let stage_path = journal.stage.resolve(library_root);
    copy_to_stage(
        survivor_entry
            .source_capability
            .as_ref()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        &stage_path,
    )?;

    let preliminary = plan_duplicate_consolidation(
        tracks,
        playlists,
        request,
        file_size(&stage_path)?,
        artwork.is_some(),
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    metadata_service
        .write_metadata(
            &stage_path,
            full_metadata_mirror(&preliminary.survivor.metadata),
        )
        .and_then(|()| metadata_service.write_rating(&stage_path, preliminary.survivor.rating))
        .and_then(|()| metadata_service.write_artwork(&stage_path, artwork.clone()))
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    verify_staged_tags(
        metadata_service,
        &stage_path,
        &preliminary.survivor,
        artwork,
    )?;

    let stage = open_regular_file(&stage_path)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    replace_original_with_stage(
        &survivor_source,
        survivor_entry
            .source_capability
            .as_ref()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        &stage,
    )?;
    for entry in journal
        .entries
        .iter()
        .filter(|entry| entry.track_id != journal.survivor_id)
    {
        remove_matching_file(
            &entry.source.resolve(library_root),
            entry
                .source_capability
                .as_ref()
                .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        )?;
    }

    let plan = plan_duplicate_consolidation(
        tracks,
        playlists,
        request,
        file_size(&survivor_source)?,
        artwork.is_some(),
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    store
        .commit_duplicate_consolidation(&plan)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(plan)
}

pub fn recover_duplicate_consolidation_journal(
    library_root: &Path,
    store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<()> {
    if !journal_path(library_root).exists() {
        return Ok(());
    }
    let journal = read_journal(library_root)?;
    let survivor_exists = store
        .track(journal.survivor_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        .is_some();
    let removed_rows = journal
        .entries
        .iter()
        .filter(|entry| entry.track_id != journal.survivor_id)
        .map(|entry| {
            store
                .track(entry.track_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
        })
        .collect::<Result<Vec<_>, _>>()?;

    if survivor_exists && removed_rows.iter().all(Option::is_none) {
        store
            .flush_durable()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        cleanup_committed(library_root, &journal)
    } else if survivor_exists && removed_rows.iter().all(Option::is_some) {
        rollback_pre_commit(library_root, &journal)
    } else {
        Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
    }
}

fn selected_tracks<'a>(
    tracks: &'a [Track],
    request: &DuplicateConsolidationRequest,
) -> ApplicationRuntimeResult<Vec<&'a Track>> {
    let selected_ids = request.track_ids.iter().copied().collect::<BTreeSet<_>>();
    if selected_ids.len() < 2 || selected_ids.len() != request.track_ids.len() {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let selected = request
        .track_ids
        .iter()
        .map(|track_id| {
            tracks
                .iter()
                .find(|track| track.id == *track_id && !track.location.is_missing())
                .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for reference in [
        request.audio_track_id,
        request.artwork_track_id,
        request.rating_track_id,
    ] {
        if !selected.iter().any(|track| track.id == reference) {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
    }
    Ok(selected)
}

/// Reject the merge before it touches any pathname if a selected file is not a
/// readable regular file on disk (#126). The selection already screened the
/// stale `is_missing` flag; this catches a file deleted out from under a track
/// whose flag was never updated.
fn ensure_selected_files_present(
    library_root: &Path,
    selected: &[&Track],
) -> ApplicationRuntimeResult<()> {
    for track in selected {
        if regular_file_identity(&track.location.absolute_path(library_root)).is_err() {
            return Err(ApplicationRuntimeError::DuplicateConsolidationSourceMissing);
        }
    }
    Ok(())
}

fn plan_journal(
    library_root: &Path,
    survivor_id: TrackId,
    tracks: &[&Track],
) -> ApplicationRuntimeResult<Journal> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let recovery_directory = format!(
        ".sustain-duplicate-consolidation-{}-{unique}",
        std::process::id()
    );
    let survivor = tracks
        .iter()
        .find(|track| track.id == survivor_id)
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let extension = survivor
        .location
        .path()
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let stage = relative_path(format!("{recovery_directory}/stage.{extension}"))?;
    let entries = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let source = track.location.relative_path.clone();
            let source_capability = open_regular_file(&source.resolve(library_root))
                .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
            Ok(JournalEntry {
                track_id: track.id,
                source_identity: source_capability.identity(),
                source_capability: Some(source_capability),
                source,
                backup: relative_path(format!("{recovery_directory}/backup-{index}"))?,
            })
        })
        .collect::<ApplicationRuntimeResult<Vec<_>>>()?;
    Ok(Journal {
        survivor_id,
        stage,
        entries,
    })
}

fn survivor_entry(journal: &Journal) -> &JournalEntry {
    journal
        .entries
        .iter()
        .find(|entry| entry.track_id == journal.survivor_id)
        .expect("journal survivor entry")
}

fn create_link(source: &RegularFileCapability, destination: &Path) -> ApplicationRuntimeResult<()> {
    publish_file_capability(source, destination)
        .map(drop)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn copy_to_stage(
    source: &RegularFileCapability,
    destination: &Path,
) -> ApplicationRuntimeResult<()> {
    let destination = PinnedFilePath::creating_parent(destination)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    if destination
        .exists()
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?
    {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let mut destination_file = destination
        .create_new_file()
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let destination_capability = regular_file_capability_from_file(
        destination_file
            .try_clone()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?,
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let result = (|| {
        let mut source_file = source
            .try_clone_file()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        source_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let source_metadata = source_file
            .metadata()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let copied = io::copy(&mut source_file, &mut destination_file)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        if copied != source_metadata.len() {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        destination_file
            .set_permissions(source_metadata.permissions())
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        destination_file
            .flush()
            .and_then(|()| destination_file.sync_all())
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        source_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        destination_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let source_hash = hash_reader_content(&mut source_file)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let destination_hash = hash_reader_content(&mut destination_file)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        if source_hash != destination_hash
            || !destination
                .refers_to(&destination_capability)
                .unwrap_or(false)
            || destination.sync_parent().is_err()
        {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ =
            remove_pinned_regular_file_matching_capability(&destination, &destination_capability);
    }
    result
}

fn remove_matching_file(
    path: &Path,
    expected: &RegularFileCapability,
) -> ApplicationRuntimeResult<()> {
    remove_regular_file_matching_capability(path, expected)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn replace_original_with_stage(
    original: &Path,
    original_capability: &RegularFileCapability,
    stage: &RegularFileCapability,
) -> ApplicationRuntimeResult<()> {
    remove_matching_file(original, original_capability)?;
    create_link(stage, original)
}

fn verify_staged_tags(
    service: &dyn MetadataService,
    stage: &Path,
    survivor: &Track,
    artwork: &Option<Vec<u8>>,
) -> ApplicationRuntimeResult<()> {
    let written = service
        .read_persisted_tags(stage)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let written_artwork = service
        .read_artwork(stage)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    if !editable_metadata_matches(&written.metadata, &survivor.metadata)
        || written.rating != survivor.rating
        || &written_artwork != artwork
    {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    Ok(())
}

pub(crate) fn editable_metadata_matches(
    left: &sustain_domain::TrackMetadata,
    right: &sustain_domain::TrackMetadata,
) -> bool {
    left.title == right.title
        && left.artist == right.artist
        && left.album == right.album
        && left.album_artist == right.album_artist
        && left.composer == right.composer
        && left.grouping == right.grouping
        && left.genre == right.genre
        && left.track_number == right.track_number
        && left.track_total == right.track_total
        && left.disc_number == right.disc_number
        && left.disc_total == right.disc_total
        && left.year == right.year
        && left.compilation == right.compilation
        && left.bpm == right.bpm
        && left.key == right.key
        && left.comments == right.comments
        && left.lyrics == right.lyrics
}

fn rollback_pre_commit(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    let stage = open_regular_file(&journal.stage.resolve(library_root)).ok();
    for entry in &journal.entries {
        let source = entry.source.resolve(library_root);
        let backup = entry.backup.resolve(library_root);
        let backup = match open_regular_file(&backup) {
            Ok(backup) => backup,
            Err(_) if path_is_missing(&backup) => {
                if regular_file_identity(&source).is_err() {
                    return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
                }
                continue;
            }
            Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        };
        match path_refers_to_capability(&source, &backup) {
            Ok(true) => {}
            Err(_) if path_is_missing(&source) => create_link(&backup, &source)?,
            Ok(false)
                if entry.track_id == journal.survivor_id
                    && stage.as_ref().is_some_and(|stage| {
                        path_refers_to_capability(&source, stage).unwrap_or(false)
                    }) =>
            {
                remove_matching_file(
                    &source,
                    stage
                        .as_ref()
                        .expect("stage capability checked in match guard"),
                )?;
                create_link(&backup, &source)?;
            }
            _ => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        }
    }
    cleanup_recovery_files(library_root, journal)?;
    remove_journal(library_root)
}

fn cleanup_committed(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    let survivor = survivor_entry(journal).source.resolve(library_root);
    regular_file_identity(&survivor)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    for entry in journal
        .entries
        .iter()
        .filter(|entry| entry.track_id != journal.survivor_id)
    {
        let source = entry.source.resolve(library_root);
        let backup = open_regular_file(&entry.backup.resolve(library_root))
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        match path_refers_to_capability(&source, &backup) {
            Ok(true) => remove_matching_file(&source, &backup)?,
            Err(_) if path_is_missing(&source) => {}
            _ => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        }
    }
    cleanup_recovery_files(library_root, journal)?;
    remove_journal(library_root)
}

fn cleanup_recovery_files(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    let stage_path = journal.stage.resolve(library_root);
    let recovery_directory = stage_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let recovery_directory_path = PinnedFilePath::existing_parent(&recovery_directory)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let recovery_directory_capability = match recovery_directory_path.open_directory() {
        Ok(directory) => Arc::new(directory),
        Err(_) if path_is_missing(&recovery_directory) => return Ok(()),
        Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
    };

    let stage = pinned_recovery_child(
        &recovery_directory,
        recovery_directory_capability.clone(),
        &stage_path,
    )?;
    match stage.open_regular_file() {
        Ok(stage_capability) => {
            remove_pinned_regular_file_matching_capability(&stage, &stage_capability)
                .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        }
        Err(_) if !stage.exists().unwrap_or(true) => {}
        Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
    }
    for entry in &journal.entries {
        let backup = pinned_recovery_child(
            &recovery_directory,
            recovery_directory_capability.clone(),
            &entry.backup.resolve(library_root),
        )?;
        match backup.open_regular_file() {
            Ok(backup_capability) if backup_capability.identity() == entry.source_identity => {
                remove_pinned_regular_file_matching_capability(&backup, &backup_capability)
                    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
            }
            Err(_) if !backup.exists().unwrap_or(true) => {}
            _ => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        }
    }
    if !recovery_directory_path
        .refers_to_directory(recovery_directory_capability.as_ref())
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?
    {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    recovery_directory_path
        .remove_directory()
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn pinned_recovery_child(
    recovery_directory: &Path,
    recovery_directory_capability: Arc<fs::File>,
    path: &Path,
) -> ApplicationRuntimeResult<PinnedFilePath> {
    if path.parent() != Some(recovery_directory) {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    PinnedFilePath::in_open_parent(
        recovery_directory,
        recovery_directory_capability,
        path.file_name()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn path_is_missing(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn remove_journal(library_root: &Path) -> ApplicationRuntimeResult<()> {
    let path = journal_path(library_root);
    let journal = open_regular_file(&path)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    remove_regular_file_matching_capability(&path, &journal)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn write_journal(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    validate_journal(journal)?;
    let destination = journal_path(library_root);
    if destination.exists() {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = library_root.join(format!(
        ".sustain-duplicate-consolidation-journal-{}-{unique}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let temporary = PinnedFilePath::existing_parent(&temporary)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let mut file = temporary
            .create_new_file()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let temporary_capability = regular_file_capability_from_file(
            file.try_clone()
                .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        )
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        writeln!(file, "{JOURNAL_HEADER}")
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        writeln!(
            file,
            "survivor\t{}\t{}",
            journal.survivor_id.get(),
            encode_path(&journal.stage)
        )
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        for entry in &journal.entries {
            writeln!(
                file,
                "entry\t{}\t{}\t{}\t{}\t{}",
                entry.track_id.get(),
                entry.source_identity.device,
                entry.source_identity.inode,
                encode_path(&entry.source),
                encode_path(&entry.backup),
            )
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        }
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let destination = PinnedFilePath::existing_parent(&destination)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        publish_pinned_file_without_overwrite(&temporary, &destination, &temporary_capability)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
    })();
    if result.is_err() {
        let _ = remove_file_and_sync_parent(&temporary);
    }
    result
}

/*
 * Kept below this point: journal parsing and path encoding. The journal is
 * intentionally plain text so recovery can reject malformed state without
 * guessing.
 */

fn read_journal(library_root: &Path) -> ApplicationRuntimeResult<Journal> {
    let journal = open_regular_file(&journal_path(library_root))
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let mut contents = String::new();
    journal
        .try_clone_file()
        .and_then(|mut journal| journal.read_to_string(&mut contents))
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let mut lines = contents.lines();
    if lines.next() != Some(JOURNAL_HEADER) {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let mut survivor_parts = lines
        .next()
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?
        .split('\t');
    if survivor_parts.next() != Some("survivor") {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let survivor_id = parse_track_id(survivor_parts.next())?;
    let stage = decode_path(
        survivor_parts
            .next()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
    )?;
    if survivor_parts.next().is_some() {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let mut entries = Vec::new();
    for line in lines {
        let mut parts = line.split('\t');
        if parts.next() != Some("entry") {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        entries.push(JournalEntry {
            track_id: parse_track_id(parts.next())?,
            source_identity: FileIdentity {
                device: parse_u64(parts.next())?,
                inode: parse_u64(parts.next())?,
            },
            source_capability: None,
            source: decode_path(
                parts
                    .next()
                    .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
            )?,
            backup: decode_path(
                parts
                    .next()
                    .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
            )?,
        });
        if parts.next().is_some() {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
    }
    let journal = Journal {
        survivor_id,
        stage,
        entries,
    };
    validate_journal(&journal)?;
    Ok(journal)
}

fn validate_journal(journal: &Journal) -> ApplicationRuntimeResult<()> {
    let stage = journal.stage.as_path();
    let recovery_directory = stage
        .parent()
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let directory_name = recovery_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let recovery_prefix = ".sustain-duplicate-consolidation-";
    if recovery_directory.components().count() != 1
        || !directory_name.starts_with(recovery_prefix)
        || directory_name.len() == recovery_prefix.len()
        || !stage
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("stage.") && name.len() > "stage.".len())
        || journal.entries.len() < 2
    {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }

    let mut track_ids = BTreeSet::new();
    let mut sources = BTreeSet::new();
    for (index, entry) in journal.entries.iter().enumerate() {
        if entry.backup.as_path() != recovery_directory.join(format!("backup-{index}"))
            || entry.source.as_path().starts_with(recovery_directory)
            || !track_ids.insert(entry.track_id)
            || !sources.insert(entry.source.as_path())
        {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
    }
    if !track_ids.contains(&journal.survivor_id) {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    Ok(())
}

fn journal_path(library_root: &Path) -> PathBuf {
    library_root.join(JOURNAL_NAME)
}

fn file_size(path: &Path) -> ApplicationRuntimeResult<u64> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn relative_path(path: impl Into<PathBuf>) -> ApplicationRuntimeResult<TrackRelativePath> {
    TrackRelativePath::new(path.into()).ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn parse_track_id(value: Option<&str>) -> ApplicationRuntimeResult<TrackId> {
    value
        .and_then(|value| value.parse().ok())
        .and_then(TrackId::new)
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn parse_u64(value: Option<&str>) -> ApplicationRuntimeResult<u64> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn encode_path(path: &TrackRelativePath) -> String {
    use std::os::unix::ffi::OsStrExt;
    path.as_path()
        .as_os_str()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_path(value: &str) -> ApplicationRuntimeResult<TrackRelativePath> {
    use std::os::unix::ffi::OsStringExt;
    if value.len() % 2 != 0 {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let bytes = value
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| Some((hex(chunk[0])? << 4) | hex(chunk[1])?))
        .collect::<Option<Vec<_>>>()
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    relative_path(std::ffi::OsString::from_vec(bytes))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_in_result,
        reason = "test doubles use expect to state fixture mutation invariants"
    )]

    use std::{fs, path::Path, time::SystemTime};

    use sustain_domain::{PlayStatistics, Rating, TrackLocation, TrackMetadata, TrackRelativePath};
    use sustain_library_store::{InMemoryLibraryStore, LibraryStore};
    use sustain_metadata::{InitialTags, MetadataError, MetadataResult};

    use super::*;

    #[test]
    fn consolidation_publishes_verified_survivor_then_removes_duplicate() {
        let root = test_root("success");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let store = InMemoryLibraryStore::new();
        let survivor = track(1, "keep.flac", "Old");
        let metadata_reference = track(2, "remove.flac", "Chosen");
        store.save_track(survivor).expect("save survivor");
        store
            .save_track(metadata_reference.clone())
            .expect("save reference");
        let metadata = FixedMetadata {
            persisted: InitialTags {
                metadata: metadata_reference.metadata,
                rating: Rating::unrated(),
                has_embedded_artwork: false,
            },
        };

        let result = consolidate_duplicate_tracks(
            &root,
            &store,
            &metadata,
            &ManagedLibraryFilesystemValidator::default(),
            &DuplicateConsolidationRequest {
                track_ids: vec![track_id(1), track_id(2)],
                audio_track_id: track_id(1),
                metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(2)),
                artwork_track_id: track_id(2),
                rating_track_id: track_id(1),
            },
        )
        .expect("consolidate");

        assert_eq!(result.survivor_id, track_id(1));
        assert_eq!(result.removed_track_ids, vec![track_id(2)]);
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert!(!root.join("remove.flac").exists());
        assert_eq!(
            store
                .track(track_id(1))
                .expect("load survivor")
                .expect("survivor exists")
                .metadata
                .title
                .as_deref(),
            Some("Chosen")
        );
        assert_eq!(store.track(track_id(2)), Ok(None));
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rollback_refuses_to_remove_an_unexpected_survivor_replacement() {
        let root = test_root("unexpected");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let survivor = track(1, "keep.flac", "Old");
        let removed = track(2, "remove.flac", "Old");
        let tracks = [&survivor, &removed];
        let journal = plan_journal(&root, survivor.id, &tracks).expect("plan journal");
        write_journal(&root, &journal).expect("write journal");
        for entry in &journal.entries {
            create_link(
                entry.source_capability.as_ref().expect("source capability"),
                &entry.backup.resolve(&root),
            )
            .expect("backup");
        }
        let survivor_entry = survivor_entry(&journal);
        copy_to_stage(
            survivor_entry
                .source_capability
                .as_ref()
                .expect("source capability"),
            &journal.stage.resolve(&root),
        )
        .expect("stage");
        remove_matching_file(
            &survivor_entry.source.resolve(&root),
            survivor_entry
                .source_capability
                .as_ref()
                .expect("source capability"),
        )
        .expect("remove original");
        fs::write(root.join("keep.flac"), b"external replacement").expect("replace externally");
        let store = InMemoryLibraryStore::new();
        store.save_track(survivor).expect("save survivor");
        store.save_track(removed).expect("save removed");

        assert_eq!(
            recover_duplicate_consolidation_journal(&root, &store),
            Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
        );
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("replacement survives"),
            b"external replacement"
        );
        assert!(journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn staged_write_failure_leaves_original_files_unchanged() {
        let root = test_root("staged-write-failure");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let store = InMemoryLibraryStore::new();
        store
            .save_track(track(1, "keep.flac", "Old"))
            .expect("save survivor");
        store
            .save_track(track(2, "remove.flac", "Chosen"))
            .expect("save reference");

        assert_eq!(
            consolidate_duplicate_tracks(
                &root,
                &store,
                &FailingMetadata,
                &ManagedLibraryFilesystemValidator::default(),
                &DuplicateConsolidationRequest {
                    track_ids: vec![track_id(1), track_id(2)],
                    audio_track_id: track_id(1),
                    metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(2)),
                    artwork_track_id: track_id(2),
                    rating_track_id: track_id(1),
                },
            ),
            Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
        );

        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert_eq!(
            fs::read(root.join("remove.flac")).expect("duplicate"),
            b"duplicate"
        );
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_selected_file_missing_on_disk_is_rejected_before_any_change() {
        let root = test_root("missing-on-disk");
        fs::create_dir_all(&root).expect("create root");
        // The survivor exists; the duplicate's row claims it is present
        // (`is_missing` was never set) but its file is gone from disk (#126).
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        let store = InMemoryLibraryStore::new();
        store
            .save_track(track(1, "keep.flac", "Old"))
            .expect("save survivor");
        store
            .save_track(track(2, "gone.flac", "Chosen"))
            .expect("save vanished duplicate");

        assert_eq!(
            consolidate_duplicate_tracks(
                &root,
                &store,
                &FailingMetadata,
                &ManagedLibraryFilesystemValidator::default(),
                &DuplicateConsolidationRequest {
                    track_ids: vec![track_id(1), track_id(2)],
                    audio_track_id: track_id(1),
                    metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(1)),
                    artwork_track_id: track_id(1),
                    rating_track_id: track_id(1),
                },
            ),
            Err(ApplicationRuntimeError::DuplicateConsolidationSourceMissing)
        );

        // Nothing was staged, linked, or committed: the survivor is untouched,
        // both rows remain, and no journal was published.
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert!(store.track(track_id(1)).expect("load survivor").is_some());
        assert!(store.track(track_id(2)).expect("load duplicate").is_some());
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn journal_validation_rejects_a_stage_outside_the_recovery_directory() {
        let journal = Journal {
            survivor_id: track_id(1),
            stage: relative_path("victim.flac").expect("stage"),
            entries: vec![
                JournalEntry {
                    track_id: track_id(1),
                    source_identity: FileIdentity {
                        device: 1,
                        inode: 1,
                    },
                    source_capability: None,
                    source: relative_path("keep.flac").expect("source"),
                    backup: relative_path(".sustain-duplicate-consolidation-1/backup-0")
                        .expect("backup"),
                },
                JournalEntry {
                    track_id: track_id(2),
                    source_identity: FileIdentity {
                        device: 1,
                        inode: 2,
                    },
                    source_capability: None,
                    source: relative_path("remove.flac").expect("source"),
                    backup: relative_path(".sustain-duplicate-consolidation-1/backup-1")
                        .expect("backup"),
                },
            ],
        };

        assert_eq!(
            validate_journal(&journal),
            Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
        );
    }

    struct FixedMetadata {
        persisted: InitialTags,
    }

    impl MetadataService for FixedMetadata {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Ok(self.persisted.clone())
        }

        fn read_persisted_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Ok(self.persisted.clone())
        }

        fn write_metadata(
            &self,
            _path: &Path,
            _change: sustain_domain::MetadataChange,
        ) -> MetadataResult<()> {
            Ok(())
        }

        fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
            Ok(())
        }

        fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            Ok(())
        }
    }

    struct FailingMetadata;

    impl MetadataService for FailingMetadata {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Ok(InitialTags {
                metadata: TrackMetadata::default(),
                rating: Rating::unrated(),
                has_embedded_artwork: false,
            })
        }

        fn write_metadata(
            &self,
            path: &Path,
            _change: sustain_domain::MetadataChange,
        ) -> MetadataResult<()> {
            fs::write(path, b"partially rewritten").expect("mutate staged file");
            Err(MetadataError::WriteFailed)
        }

        fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
            Ok(())
        }

        fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            Ok(())
        }
    }

    fn track(id: i64, path: &str, title: &str) -> Track {
        Track {
            id: track_id(id),
            location: TrackLocation::available(
                TrackRelativePath::new(path).expect("relative path"),
            ),
            metadata: TrackMetadata {
                title: Some(title.to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn track_id(value: i64) -> TrackId {
        TrackId::new(value).expect("track id")
    }

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sustain_duplicate_consolidation_{label}_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }
}
