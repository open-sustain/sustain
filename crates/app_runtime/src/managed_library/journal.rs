// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Crash-recovery journal for managed-library moves. Before any consolidation
//! or metadata-driven retarget touches the filesystem it records the intended
//! moves here; on the next launch [`recover_library_consolidation_journal`]
//! replays an interrupted batch so SQLite and the on-disk layout agree.

use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use rustix::fs::Dir;
use sustain_domain::{TrackId, TrackLocation, TrackRelativePath};

use crate::{ApplicationRuntimeError, ApplicationRuntimeResult};

use super::capabilities::ManagedLibraryFilesystemValidator;
use super::consolidation::{JournalTrackPersistence, PlannedLibraryConsolidationMove};
use super::file_ops::{
    FileIdentity, PRIVATE_DIRECTORY_MODE, PinnedFilePath, RegularFileCapability, open_regular_file,
    path_refers_to_capability, prune_empty_ancestor_directories_for_sources,
    publish_file_capability_to_pinned_path, publish_pinned_file_without_overwrite,
    regular_file_capability_from_file, remove_file_and_sync_parent,
    remove_pinned_regular_file_matching_capability, remove_regular_file_matching_capability,
    sync_directory,
};

const CONSOLIDATION_JOURNAL_FILE_NAME: &str = ".sustain-consolidation-journal";
const CONSOLIDATION_JOURNAL_HEADER: &str = "# sustain managed library consolidation journal v3";
const CONSOLIDATION_RECOVERY_DIRECTORY_NAME: &str = ".sustain-consolidation-recovery";

#[derive(Debug)]
pub(super) struct PreparedConsolidationRecovery {
    library_path: PathBuf,
    directory_path: PathBuf,
    directory: Arc<fs::File>,
}

impl PreparedConsolidationRecovery {
    pub(super) fn pin_source(
        &self,
        track_id: TrackId,
        source: &RegularFileCapability,
    ) -> ApplicationRuntimeResult<FileIdentity> {
        let backup = self.backup_path(track_id)?;
        publish_file_capability_to_pinned_path(source, &backup)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        Ok(source.identity())
    }

    fn open_source(
        &self,
        track_id: TrackId,
        source_identity: FileIdentity,
    ) -> ApplicationRuntimeResult<RegularFileCapability> {
        let source = self
            .backup_path(track_id)?
            .open_regular_file()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        if source.identity() != source_identity {
            return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
        }
        Ok(source)
    }

    fn verify_directory_path(&self) -> ApplicationRuntimeResult<()> {
        let directory_path = PinnedFilePath::existing_parent(&self.directory_path)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        if !directory_path
            .refers_to_directory(self.directory.as_ref())
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?
        {
            return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
        }
        Ok(())
    }

    fn backup_path(&self, track_id: TrackId) -> ApplicationRuntimeResult<PinnedFilePath> {
        let file_name = format!("track-{}.backup", track_id.get());
        PinnedFilePath::in_open_parent(
            &self.directory_path,
            self.directory.clone(),
            OsStr::new(&file_name),
        )
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)
    }
}

impl Drop for PreparedConsolidationRecovery {
    fn drop(&mut self) {
        let journal_path = consolidation_journal_path(&self.library_path);
        if fs::symlink_metadata(journal_path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
        {
            let _ = cleanup_open_consolidation_recovery_directory(
                &self.directory_path,
                self.directory.clone(),
            );
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConsolidationJournalEntry {
    track_id: TrackId,
    source_identity: FileIdentity,
    source_relative_path: TrackRelativePath,
    destination_relative_path: TrackRelativePath,
    persistence: JournalTrackPersistence,
}

pub(crate) fn recover_library_consolidation_journal(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    managed_library_filesystem_validator: &ManagedLibraryFilesystemValidator,
) -> ApplicationRuntimeResult<()> {
    let journal_path = consolidation_journal_path(library_path);
    if !journal_path.exists() {
        return cleanup_orphaned_consolidation_recovery_directory(library_path);
    }
    managed_library_filesystem_validator
        .validate(library_path)
        .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;

    let entries = read_consolidation_journal(library_path)?;
    for entry in &entries {
        recover_consolidation_journal_entry(library_path, library_store, entry)?;
    }

    // The external journal remains authoritative until every reconciled
    // SQLite location is power-loss durable. SQLite WAL mode with
    // synchronous=NORMAL does not sync ordinary commits; the store barrier
    // checkpoints and syncs before the journal namespace is removed.
    library_store
        .flush_durable()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    remove_consolidation_journal_if_present(library_path)?;
    let source_paths = entries
        .iter()
        .flat_map(|entry| {
            [
                entry.source_relative_path.resolve(library_path),
                entry.destination_relative_path.resolve(library_path),
            ]
        })
        .collect::<Vec<_>>();
    prune_empty_ancestor_directories_for_sources(library_path, &source_paths);
    Ok(())
}

fn recover_consolidation_journal_entry(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    entry: &ConsolidationJournalEntry,
) -> ApplicationRuntimeResult<()> {
    let source_path = entry.source_relative_path.resolve(library_path);
    let destination_path = entry.destination_relative_path.resolve(library_path);
    let backup_path = consolidation_recovery_backup_path(library_path, entry.track_id);
    if let Ok(backup) = open_regular_file(&backup_path) {
        let source = inspect_journal_path_against_capability(&source_path, &backup)?;
        let destination = inspect_journal_path_against_capability(&destination_path, &backup)?;
        return recover_consolidation_journal_entry_with_capability(
            library_path,
            library_store,
            entry,
            &source_path,
            &backup,
            source,
            destination,
        );
    }

    // Compatibility with journals written before descriptor-backed recovery
    // links existed. New writes always create a backup hard link first.
    let source = inspect_journal_path(&source_path, entry)?;
    let destination = inspect_journal_path(&destination_path, entry)?;

    match (source, destination) {
        // Destination identity proves publication completed. If the original
        // source link still exists, finish its unlink durably; an unexpected
        // source pathname is left untouched because it belongs to neither the
        // journal nor Sustain.
        (JournalPathState::Expected, JournalPathState::Expected) => {
            let destination = open_regular_file(&destination_path)
                .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
            remove_regular_file_matching_capability(&source_path, &destination)
                .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
            save_recovered_consolidation_track(
                library_path,
                library_store,
                entry,
                &entry.destination_relative_path,
            )?;
        }
        (JournalPathState::Missing | JournalPathState::Unexpected, JournalPathState::Expected) => {
            save_recovered_consolidation_track(
                library_path,
                library_store,
                entry,
                &entry.destination_relative_path,
            )?;
        }
        // The old filesystem state is also a valid recovery endpoint. Persist
        // it explicitly in case an interrupted rollback left SQLite uncertain.
        (JournalPathState::Expected, JournalPathState::Missing) => {
            save_recovered_consolidation_track(
                library_path,
                library_store,
                entry,
                &entry.source_relative_path,
            )?;
        }
        // Neither pathname can prove where the managed inode lives. Preserve
        // the journal so startup reports an actionable failure instead of
        // silently discarding the only recovery record.
        _ => return Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }

    Ok(())
}

fn recover_consolidation_journal_entry_with_capability(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    entry: &ConsolidationJournalEntry,
    source_path: &Path,
    backup: &super::file_ops::RegularFileCapability,
    source: JournalPathState,
    destination: JournalPathState,
) -> ApplicationRuntimeResult<()> {
    match (source, destination) {
        (JournalPathState::Expected, JournalPathState::Expected) => {
            remove_regular_file_matching_capability(source_path, backup)
                .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
            save_recovered_consolidation_track(
                library_path,
                library_store,
                entry,
                &entry.destination_relative_path,
            )
        }
        (JournalPathState::Missing | JournalPathState::Unexpected, JournalPathState::Expected) => {
            save_recovered_consolidation_track(
                library_path,
                library_store,
                entry,
                &entry.destination_relative_path,
            )
        }
        (JournalPathState::Expected, JournalPathState::Missing) => {
            save_recovered_consolidation_track(
                library_path,
                library_store,
                entry,
                &entry.source_relative_path,
            )
        }
        _ => Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JournalPathState {
    Missing,
    Expected,
    Unexpected,
}

fn inspect_journal_path(
    path: &Path,
    entry: &ConsolidationJournalEntry,
) -> ApplicationRuntimeResult<JournalPathState> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && metadata.dev() == entry.source_identity.device
                && metadata.ino() == entry.source_identity.inode =>
        {
            Ok(JournalPathState::Expected)
        }
        Ok(_) => Ok(JournalPathState::Unexpected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(JournalPathState::Missing),
        Err(_) => Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }
}

fn inspect_journal_path_against_capability(
    path: &Path,
    expected: &super::file_ops::RegularFileCapability,
) -> ApplicationRuntimeResult<JournalPathState> {
    match path_refers_to_capability(path, expected) {
        Ok(true) => Ok(JournalPathState::Expected),
        Ok(false) | Err(super::file_ops::FileMoveError::SourceIsNotFile) => {
            Ok(JournalPathState::Unexpected)
        }
        Err(super::file_ops::FileMoveError::SourceUnavailable)
            if fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            Ok(JournalPathState::Missing)
        }
        Err(_) => Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }
}

fn save_recovered_consolidation_track(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    entry: &ConsolidationJournalEntry,
    relative_path: &TrackRelativePath,
) -> ApplicationRuntimeResult<()> {
    if library_store
        .track(entry.track_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        .is_none()
    {
        return Err(ApplicationRuntimeError::LibraryStoreFailed);
    }
    let location = TrackLocation::available(relative_path.clone());
    match entry.persistence {
        JournalTrackPersistence::LocationOnly => library_store
            .update_track_location(entry.track_id, &location)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed),
        JournalTrackPersistence::Relocation => {
            let file_size_bytes = fs::metadata(relative_path.resolve(library_path))
                .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?
                .len();
            library_store
                .relocate_track_and_enqueue_mirror(entry.track_id, &location, file_size_bytes)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
        }
    }
}

pub(super) fn write_consolidation_journal(
    library_path: &Path,
    moves: &[PlannedLibraryConsolidationMove],
) -> ApplicationRuntimeResult<()> {
    let journal_path = consolidation_journal_path(library_path);
    if journal_path.exists() {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }
    let recovery = moves
        .first()
        .map(|planned_move| &planned_move.prepared_recovery)
        .ok_or(ApplicationRuntimeError::LibraryConsolidationFailed)?;
    if recovery.library_path != library_path
        || moves
            .iter()
            .any(|planned_move| !Arc::ptr_eq(recovery, &planned_move.prepared_recovery))
    {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }
    recovery.verify_directory_path()?;

    let temporary_path = temporary_consolidation_journal_path(library_path);
    let result = (|| {
        let temporary = PinnedFilePath::existing_parent(&temporary_path)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let mut file = temporary
            .create_new_file()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let temporary_capability = regular_file_capability_from_file(
            file.try_clone()
                .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?,
        )
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;

        writeln!(file, "{CONSOLIDATION_JOURNAL_HEADER}")
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        for planned_move in moves {
            let source = open_consolidation_recovery_source(planned_move)?;
            if !path_refers_to_capability(&planned_move.source_path, &source).unwrap_or(false) {
                return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
            }
            let source = encode_relative_path(&planned_move.source_relative_path);
            let destination = encode_relative_path(&planned_move.destination_relative_path);
            writeln!(
                file,
                "move\t{}\t{}\t{}\t{}\t{}\t{}",
                planned_move.track_id.get(),
                planned_move.source_identity.device,
                planned_move.source_identity.inode,
                persistence_name(planned_move.persistence),
                source,
                destination
            )
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        }
        file.flush()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        file.sync_all()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        recovery.verify_directory_path()?;
        publish_journal_without_overwrite_matching_capability(
            &temporary,
            &journal_path,
            &temporary_capability,
        )
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        sync_directory(library_path)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)
    })();

    if result.is_err() {
        let _ = remove_file_and_sync_parent(&temporary_path);
    }
    result
}

pub(super) fn open_consolidation_recovery_source(
    planned_move: &PlannedLibraryConsolidationMove,
) -> ApplicationRuntimeResult<RegularFileCapability> {
    planned_move
        .prepared_recovery
        .open_source(planned_move.track_id, planned_move.source_identity)
}

pub(super) fn prepare_consolidation_recovery(
    library_path: &Path,
) -> ApplicationRuntimeResult<Arc<PreparedConsolidationRecovery>> {
    match fs::symlink_metadata(consolidation_journal_path(library_path)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) | Err(_) => return Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }
    let directory_path = consolidation_recovery_directory(library_path);
    let directory_entry = PinnedFilePath::existing_parent(&directory_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    directory_entry
        .create_directory(PRIVATE_DIRECTORY_MODE)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    let directory = Arc::new(
        directory_entry
            .open_directory()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?,
    );
    if !directory_entry
        .refers_to_directory(directory.as_ref())
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?
    {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }
    Ok(Arc::new(PreparedConsolidationRecovery {
        library_path: library_path.to_path_buf(),
        directory_path,
        directory,
    }))
}

pub(super) fn publish_journal_without_overwrite(
    temporary_path: &Path,
    journal_path: &Path,
) -> std::io::Result<()> {
    let temporary = PinnedFilePath::existing_parent(temporary_path)?;
    let capability = temporary
        .open_regular_file()
        .map_err(|error| std::io::Error::other(format!("{error:?}")))?;
    publish_journal_without_overwrite_matching_capability(&temporary, journal_path, &capability)
}

fn publish_journal_without_overwrite_matching_capability(
    temporary: &PinnedFilePath,
    journal_path: &Path,
    capability: &super::file_ops::RegularFileCapability,
) -> std::io::Result<()> {
    let journal = PinnedFilePath::existing_parent(journal_path)?;
    publish_pinned_file_without_overwrite(temporary, &journal, capability)
}

fn read_consolidation_journal(
    library_path: &Path,
) -> ApplicationRuntimeResult<Vec<ConsolidationJournalEntry>> {
    let journal = open_regular_file(&consolidation_journal_path(library_path))
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    let mut contents = String::new();
    journal
        .try_clone_file()
        .and_then(|mut journal| journal.read_to_string(&mut contents))
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    let mut entries = Vec::new();
    let mut lines = contents.lines();
    if lines.next() != Some(CONSOLIDATION_JOURNAL_HEADER) {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }

        let mut parts = line.split('\t');
        let Some("move") = parts.next() else {
            return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
        };
        let track_id = parts
            .next()
            .and_then(|value| value.parse::<i64>().ok())
            .and_then(TrackId::new)
            .ok_or(ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let source_device = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let source_inode = parts
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let persistence = parts
            .next()
            .and_then(persistence_from_name)
            .ok_or(ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let source_relative_path = parts
            .next()
            .and_then(decode_relative_path)
            .ok_or(ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let destination_relative_path = parts
            .next()
            .and_then(decode_relative_path)
            .ok_or(ApplicationRuntimeError::LibraryConsolidationFailed)?;
        if parts.next().is_some() {
            return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
        }

        entries.push(ConsolidationJournalEntry {
            track_id,
            source_identity: FileIdentity {
                device: source_device,
                inode: source_inode,
            },
            source_relative_path,
            destination_relative_path,
            persistence,
        });
    }

    Ok(entries)
}

fn persistence_name(persistence: JournalTrackPersistence) -> &'static str {
    match persistence {
        JournalTrackPersistence::LocationOnly => "location",
        JournalTrackPersistence::Relocation => "relocation",
    }
}

fn persistence_from_name(value: &str) -> Option<JournalTrackPersistence> {
    match value {
        "location" => Some(JournalTrackPersistence::LocationOnly),
        "relocation" => Some(JournalTrackPersistence::Relocation),
        _ => None,
    }
}

pub(super) fn remove_consolidation_journal_if_present(
    library_path: &Path,
) -> ApplicationRuntimeResult<()> {
    let journal_path = consolidation_journal_path(library_path);
    if !journal_path.exists() {
        return cleanup_orphaned_consolidation_recovery_directory(library_path);
    }

    let entries = read_consolidation_journal(library_path)?;
    let journal = open_regular_file(&journal_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    remove_regular_file_matching_capability(&journal_path, &journal)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    cleanup_consolidation_recovery_links(library_path, entries.iter().map(|entry| entry.track_id));
    Ok(())
}

fn cleanup_orphaned_consolidation_recovery_directory(
    library_path: &Path,
) -> ApplicationRuntimeResult<()> {
    let directory = consolidation_recovery_directory(library_path);
    match fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(ApplicationRuntimeError::LibraryConsolidationFailed),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }

    let directory_path = PinnedFilePath::existing_parent(&directory)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    let directory_capability = Arc::new(
        directory_path
            .open_directory()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?,
    );
    cleanup_open_consolidation_recovery_directory(&directory, directory_capability)
}

fn cleanup_open_consolidation_recovery_directory(
    directory_path: &Path,
    directory_capability: Arc<fs::File>,
) -> ApplicationRuntimeResult<()> {
    let entries = Dir::read_from(directory_capability.as_ref())
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    let file_names = entries
        .map(|entry| {
            let entry = entry.map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
            Ok(OsStr::from_bytes(entry.file_name().to_bytes()).to_owned())
        })
        .collect::<ApplicationRuntimeResult<Vec<OsString>>>()?
        .into_iter()
        .filter(|file_name| file_name != "." && file_name != "..")
        .collect::<Vec<_>>();
    if file_names
        .iter()
        .any(|file_name| !is_consolidation_recovery_backup_name(file_name))
    {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }
    for file_name in file_names {
        let path = PinnedFilePath::in_open_parent(
            directory_path,
            directory_capability.clone(),
            &file_name,
        )
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        let backup = path
            .open_regular_file()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        remove_pinned_regular_file_matching_capability(&path, &backup)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    }
    let directory_entry = PinnedFilePath::existing_parent(directory_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    match directory_entry.refers_to_directory(directory_capability.as_ref()) {
        Ok(true) => directory_entry
            .remove_directory()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed),
        Ok(false) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }
}

fn is_consolidation_recovery_backup_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix("track-"))
        .and_then(|name| name.strip_suffix(".backup"))
        .and_then(|id| id.parse::<i64>().ok())
        .and_then(TrackId::new)
        .is_some()
}

fn cleanup_consolidation_recovery_links(
    library_path: &Path,
    track_ids: impl Iterator<Item = TrackId>,
) {
    let directory = consolidation_recovery_directory(library_path);
    let Ok(directory_path) = PinnedFilePath::existing_parent(&directory) else {
        return;
    };
    let Ok(directory_capability) = directory_path.open_directory().map(Arc::new) else {
        return;
    };
    for track_id in track_ids {
        let file_name = format!("track-{}.backup", track_id.get());
        let Ok(path) = PinnedFilePath::in_open_parent(
            &directory,
            directory_capability.clone(),
            OsStr::new(&file_name),
        ) else {
            continue;
        };
        if let Ok(backup) = path.open_regular_file() {
            let _ = remove_pinned_regular_file_matching_capability(&path, &backup);
        }
    }
    if directory_path
        .refers_to_directory(directory_capability.as_ref())
        .unwrap_or(false)
    {
        let _ = directory_path.remove_directory();
    }
}

fn consolidation_recovery_directory(library_path: &Path) -> PathBuf {
    library_path.join(CONSOLIDATION_RECOVERY_DIRECTORY_NAME)
}

fn consolidation_recovery_backup_path(library_path: &Path, track_id: TrackId) -> PathBuf {
    consolidation_recovery_directory(library_path).join(format!("track-{}.backup", track_id.get()))
}

fn consolidation_journal_path(library_path: &Path) -> PathBuf {
    library_path.join(CONSOLIDATION_JOURNAL_FILE_NAME)
}

fn temporary_consolidation_journal_path(library_path: &Path) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    library_path.join(format!(
        ".sustain-consolidation-journal-{}-{unique}.tmp",
        std::process::id()
    ))
}

fn encode_relative_path(relative_path: &TrackRelativePath) -> String {
    use std::os::unix::ffi::OsStrExt;

    relative_path
        .as_path()
        .as_os_str()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_relative_path(value: &str) -> Option<TrackRelativePath> {
    use std::os::unix::ffi::OsStringExt;

    if value.len() % 2 != 0 {
        return None;
    }

    let bytes = value
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| {
            let high = hex_value(chunk[0])?;
            let low = hex_value(chunk[1])?;
            Some((high << 4) | low)
        })
        .collect::<Option<Vec<_>>>()?;

    TrackRelativePath::new(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::MetadataExt,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use sustain_domain::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };
    use sustain_library_store::{InMemoryLibraryStore, LibraryStore};

    use super::{
        CONSOLIDATION_JOURNAL_FILE_NAME, CONSOLIDATION_JOURNAL_HEADER,
        CONSOLIDATION_RECOVERY_DIRECTORY_NAME, ManagedLibraryFilesystemValidator,
        encode_relative_path, publish_journal_without_overwrite,
        recover_library_consolidation_journal, remove_consolidation_journal_if_present,
    };

    #[derive(Clone, Copy, Debug)]
    enum InterruptedAfter {
        JournalPublication,
        DestinationPublication,
        SourceUnlink,
        SqliteCommit,
    }

    #[test]
    fn recovery_reconciles_every_interrupted_protocol_boundary() {
        for boundary in [
            InterruptedAfter::JournalPublication,
            InterruptedAfter::DestinationPublication,
            InterruptedAfter::SourceUnlink,
            InterruptedAfter::SqliteCommit,
        ] {
            let fixture = Fixture::new();
            fixture.publish_journal();

            match boundary {
                InterruptedAfter::JournalPublication => {}
                InterruptedAfter::DestinationPublication => fixture.publish_destination_link(),
                InterruptedAfter::SourceUnlink => {
                    fixture.publish_destination_link();
                    fixture.remove_source();
                }
                InterruptedAfter::SqliteCommit => {
                    fixture.publish_destination_link();
                    fixture.remove_source();
                    fixture.persist_destination();
                }
            }

            recover_library_consolidation_journal(
                &fixture.root,
                &fixture.store,
                &ManagedLibraryFilesystemValidator::default(),
            )
            .expect("recovery succeeds");

            let recovered_path = fixture.stored_relative_path();
            match boundary {
                InterruptedAfter::JournalPublication => {
                    assert_eq!(recovered_path, fixture.source_relative);
                    assert!(fixture.source_path.exists());
                    assert!(!fixture.destination_path.exists());
                }
                InterruptedAfter::DestinationPublication
                | InterruptedAfter::SourceUnlink
                | InterruptedAfter::SqliteCommit => {
                    assert_eq!(recovered_path, fixture.destination_relative);
                    assert!(!fixture.source_path.exists());
                    assert!(fixture.destination_path.exists());
                }
            }
            assert!(!fixture.journal_path().exists());
        }
    }

    #[test]
    fn recovery_retains_journal_when_both_managed_names_are_missing() {
        let fixture = Fixture::new();
        fixture.publish_journal();
        fixture.remove_source();

        assert!(
            recover_library_consolidation_journal(
                &fixture.root,
                &fixture.store,
                &ManagedLibraryFilesystemValidator::default(),
            )
            .is_err()
        );
        assert!(fixture.journal_path().exists());
        assert_eq!(fixture.stored_relative_path(), fixture.source_relative);
    }

    #[test]
    fn recovery_retains_journal_when_destination_has_unexpected_inode() {
        let fixture = Fixture::new();
        fixture.publish_journal();
        fs::create_dir_all(
            fixture
                .destination_path
                .parent()
                .expect("destination parent"),
        )
        .expect("create destination directory");
        fs::write(&fixture.destination_path, b"unrelated bytes").expect("write unrelated file");

        assert!(
            recover_library_consolidation_journal(
                &fixture.root,
                &fixture.store,
                &ManagedLibraryFilesystemValidator::default(),
            )
            .is_err()
        );
        assert!(fixture.journal_path().exists());
        assert_eq!(fixture.stored_relative_path(), fixture.source_relative);
        assert_eq!(
            fs::read(&fixture.destination_path).expect("read unrelated file"),
            b"unrelated bytes"
        );
    }

    #[test]
    fn recovery_retains_journal_when_authoritative_track_row_is_missing() {
        let fixture = Fixture::new();
        fixture.publish_journal();
        fixture
            .store
            .delete_track(fixture.track_id)
            .expect("delete track row");

        assert!(
            recover_library_consolidation_journal(
                &fixture.root,
                &fixture.store,
                &ManagedLibraryFilesystemValidator::default(),
            )
            .is_err()
        );
        assert!(fixture.journal_path().exists());
        assert!(fixture.source_path.exists());
    }

    #[test]
    fn relocation_recovery_resets_source_observations_and_queues_canonical_mirrors() {
        let fixture = Fixture::new();
        fixture
            .store
            .update_track_location(
                fixture.track_id,
                &TrackLocation::missing(fixture.source_relative.clone()),
            )
            .expect("mark missing");
        fixture.publish_relocation_journal();

        recover_library_consolidation_journal(
            &fixture.root,
            &fixture.store,
            &ManagedLibraryFilesystemValidator::default(),
        )
        .expect("recover relocation");

        let stored = fixture
            .store
            .track(fixture.track_id)
            .expect("load relocated track")
            .expect("track exists");
        assert_eq!(
            stored.location,
            TrackLocation::available(fixture.source_relative.clone())
        );
        assert_eq!(stored.file_size_bytes, Some(b"audio bytes".len() as u64));
        assert_eq!(stored.has_embedded_artwork, None);
        assert_eq!(stored.file_modified_at, None);
        let mirrors = fixture.store.tag_mirrors_due(0, 10).expect("load mirrors");
        assert_eq!(mirrors.len(), 1);
        assert!(mirrors[0].kinds.metadata);
        assert!(mirrors[0].kinds.rating);
    }

    #[test]
    fn journal_publication_refuses_to_overwrite_existing_recovery_record() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let temporary_path = root.join(".journal.tmp");
        let journal_path = root.join(CONSOLIDATION_JOURNAL_FILE_NAME);
        fs::write(&temporary_path, b"new journal").expect("write temporary journal");
        fs::write(&journal_path, b"existing journal").expect("write existing journal");

        assert!(publish_journal_without_overwrite(&temporary_path, &journal_path).is_err());
        assert_eq!(
            fs::read(&journal_path).expect("read journal"),
            b"existing journal"
        );
        assert_eq!(
            fs::read(&temporary_path).expect("read temporary"),
            b"new journal"
        );

        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn absent_journal_cleans_prepublication_recovery_links() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let source = root.join("source.flac");
        fs::write(&source, b"audio bytes").expect("write source");
        let recovery = root.join(CONSOLIDATION_RECOVERY_DIRECTORY_NAME);
        fs::create_dir(&recovery).expect("create recovery directory");
        fs::hard_link(&source, recovery.join("track-1.backup")).expect("create recovery link");

        remove_consolidation_journal_if_present(&root).expect("clean orphan recovery links");

        assert!(source.exists());
        assert!(!recovery.exists());
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn absent_journal_refuses_unknown_recovery_entries() {
        let root = unique_test_directory();
        let recovery = root.join(CONSOLIDATION_RECOVERY_DIRECTORY_NAME);
        fs::create_dir_all(&recovery).expect("create recovery directory");
        fs::write(recovery.join("unknown"), b"unexpected bytes").expect("write unknown entry");

        assert!(remove_consolidation_journal_if_present(&root).is_err());
        assert!(recovery.join("unknown").exists());
        fs::remove_dir_all(root).expect("remove root");
    }

    struct Fixture {
        root: PathBuf,
        store: InMemoryLibraryStore,
        track_id: TrackId,
        source_relative: TrackRelativePath,
        destination_relative: TrackRelativePath,
        source_path: PathBuf,
        destination_path: PathBuf,
        source_device: u64,
        source_inode: u64,
    }

    impl Fixture {
        fn new() -> Self {
            let root = unique_test_directory();
            fs::create_dir_all(&root).expect("create fixture root");
            let source_relative = relative_path("loose.flac");
            let destination_relative = relative_path("Artist/Album/01 Song.flac");
            let source_path = source_relative.resolve(&root);
            let destination_path = destination_relative.resolve(&root);
            fs::write(&source_path, b"audio bytes").expect("write source");
            let source_metadata = fs::metadata(&source_path).expect("source metadata");
            let track_id = TrackId::new(1).expect("track id");
            let store = InMemoryLibraryStore::new();
            store
                .save_track(test_track(track_id, source_relative.clone()))
                .expect("seed track");
            Self {
                root,
                store,
                track_id,
                source_relative,
                destination_relative,
                source_path,
                destination_path,
                source_device: source_metadata.dev(),
                source_inode: source_metadata.ino(),
            }
        }

        fn publish_journal(&self) {
            self.publish_journal_with_persistence("location");
        }

        fn publish_relocation_journal(&self) {
            self.publish_journal_with_persistence("relocation");
        }

        fn publish_journal_with_persistence(&self, persistence: &str) {
            fs::write(
                self.journal_path(),
                format!(
                    "{CONSOLIDATION_JOURNAL_HEADER}\nmove\t{}\t{}\t{}\t{}\t{}\t{}\n",
                    self.track_id.get(),
                    self.source_device,
                    self.source_inode,
                    persistence,
                    encode_relative_path(&self.source_relative),
                    encode_relative_path(&self.destination_relative),
                ),
            )
            .expect("write journal");
        }

        fn publish_destination_link(&self) {
            fs::create_dir_all(self.destination_path.parent().expect("destination parent"))
                .expect("create destination directory");
            fs::hard_link(&self.source_path, &self.destination_path).expect("publish hard link");
        }

        fn remove_source(&self) {
            fs::remove_file(&self.source_path).expect("remove source");
        }

        fn persist_destination(&self) {
            self.store
                .update_track_location(
                    self.track_id,
                    &TrackLocation::available(self.destination_relative.clone()),
                )
                .expect("persist destination");
        }

        fn stored_relative_path(&self) -> TrackRelativePath {
            self.store
                .track(self.track_id)
                .expect("load stored track")
                .expect("track exists")
                .location
                .relative_path
        }

        fn journal_path(&self) -> PathBuf {
            self.root.join(CONSOLIDATION_JOURNAL_FILE_NAME)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).expect("remove fixture root");
        }
    }

    fn test_track(track_id: TrackId, relative_path: TrackRelativePath) -> Track {
        Track {
            id: track_id,
            location: TrackLocation::available(relative_path),
            metadata: TrackMetadata::default(),
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn relative_path(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(path).expect("relative path")
    }

    fn unique_test_directory() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "sustain_consolidation_journal_test_{}_{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
