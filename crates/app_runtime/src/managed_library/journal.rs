// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Crash-recovery journal for managed-library moves. Before any consolidation
//! or metadata-driven retarget touches the filesystem it records the intended
//! moves here; on the next launch [`recover_library_consolidation_journal`]
//! replays an interrupted batch so SQLite and the on-disk layout agree.

use std::{
    fs,
    io::Write,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rustix::fs::{CWD, RenameFlags, renameat_with};
use sustain_domain::{TrackId, TrackLocation, TrackRelativePath};

use crate::{ApplicationRuntimeError, ApplicationRuntimeResult};

use super::consolidation::PlannedLibraryConsolidationMove;
use super::file_ops::{
    FileIdentity, regular_file_identity, remove_file_and_sync_parent, sync_directory,
};

const CONSOLIDATION_JOURNAL_FILE_NAME: &str = ".sustain-consolidation-journal";
const CONSOLIDATION_JOURNAL_HEADER: &str = "# sustain managed library consolidation journal v2";

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConsolidationJournalEntry {
    track_id: TrackId,
    source_identity: FileIdentity,
    source_relative_path: TrackRelativePath,
    destination_relative_path: TrackRelativePath,
}

pub(crate) fn recover_library_consolidation_journal(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
) -> ApplicationRuntimeResult<()> {
    let journal_path = consolidation_journal_path(library_path);
    if !journal_path.exists() {
        return Ok(());
    }

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
    remove_consolidation_journal_if_present(library_path)
}

fn recover_consolidation_journal_entry(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    entry: &ConsolidationJournalEntry,
) -> ApplicationRuntimeResult<()> {
    let source_path = entry.source_relative_path.resolve(library_path);
    let destination_path = entry.destination_relative_path.resolve(library_path);
    let source = inspect_journal_path(&source_path, entry)?;
    let destination = inspect_journal_path(&destination_path, entry)?;

    match (source, destination) {
        // Destination identity proves publication completed. If the original
        // source link still exists, finish its unlink durably; an unexpected
        // source pathname is left untouched because it belongs to neither the
        // journal nor Sustain.
        (JournalPathState::Expected, JournalPathState::Expected) => {
            remove_file_and_sync_parent(&source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
            save_recovered_consolidation_track(
                library_store,
                entry,
                &entry.destination_relative_path,
            )?;
        }
        (JournalPathState::Missing | JournalPathState::Unexpected, JournalPathState::Expected) => {
            save_recovered_consolidation_track(
                library_store,
                entry,
                &entry.destination_relative_path,
            )?;
        }
        // The old filesystem state is also a valid recovery endpoint. Persist
        // it explicitly in case an interrupted rollback left SQLite uncertain.
        (JournalPathState::Expected, JournalPathState::Missing) => {
            save_recovered_consolidation_track(library_store, entry, &entry.source_relative_path)?;
        }
        // Neither pathname can prove where the managed inode lives. Preserve
        // the journal so startup reports an actionable failure instead of
        // silently discarding the only recovery record.
        _ => return Err(ApplicationRuntimeError::LibraryConsolidationFailed),
    }

    Ok(())
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

fn save_recovered_consolidation_track(
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
    library_store
        .update_track_location(
            entry.track_id,
            &TrackLocation::available(relative_path.clone()),
        )
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
}

pub(super) fn write_consolidation_journal(
    library_path: &Path,
    moves: &[PlannedLibraryConsolidationMove],
) -> ApplicationRuntimeResult<()> {
    let journal_path = consolidation_journal_path(library_path);
    if journal_path.exists() {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }

    let temporary_path = temporary_consolidation_journal_path(library_path);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;

        writeln!(file, "{CONSOLIDATION_JOURNAL_HEADER}")
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        for planned_move in moves {
            if regular_file_identity(&planned_move.source_path) != Ok(planned_move.source_identity)
            {
                return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
            }
            let source = encode_relative_path(&planned_move.source_relative_path);
            let destination = encode_relative_path(&planned_move.destination_relative_path);
            writeln!(
                file,
                "move\t{}\t{}\t{}\t{}\t{}",
                planned_move.track_id.get(),
                planned_move.source_identity.device,
                planned_move.source_identity.inode,
                source,
                destination
            )
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        }
        file.flush()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        file.sync_all()
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        publish_journal_without_overwrite(&temporary_path, &journal_path)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        sync_directory(library_path)
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)
    })();

    if result.is_err() {
        let _ = remove_file_and_sync_parent(&temporary_path);
    }
    result
}

fn publish_journal_without_overwrite(
    temporary_path: &Path,
    journal_path: &Path,
) -> std::io::Result<()> {
    renameat_with(
        CWD,
        temporary_path,
        CWD,
        journal_path,
        RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from)
}

fn read_consolidation_journal(
    library_path: &Path,
) -> ApplicationRuntimeResult<Vec<ConsolidationJournalEntry>> {
    let contents = fs::read_to_string(consolidation_journal_path(library_path))
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
        });
    }

    Ok(entries)
}

pub(super) fn remove_consolidation_journal_if_present(
    library_path: &Path,
) -> ApplicationRuntimeResult<()> {
    let journal_path = consolidation_journal_path(library_path);
    if !journal_path.exists() {
        return Ok(());
    }

    remove_file_and_sync_parent(&journal_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)
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
        CONSOLIDATION_JOURNAL_FILE_NAME, CONSOLIDATION_JOURNAL_HEADER, encode_relative_path,
        publish_journal_without_overwrite, recover_library_consolidation_journal,
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

            recover_library_consolidation_journal(&fixture.root, &fixture.store)
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

        assert!(recover_library_consolidation_journal(&fixture.root, &fixture.store).is_err());
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

        assert!(recover_library_consolidation_journal(&fixture.root, &fixture.store).is_err());
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

        assert!(recover_library_consolidation_journal(&fixture.root, &fixture.store).is_err());
        assert!(fixture.journal_path().exists());
        assert!(fixture.source_path.exists());
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
            fs::write(
                self.journal_path(),
                format!(
                    "{CONSOLIDATION_JOURNAL_HEADER}\nmove\t{}\t{}\t{}\t{}\t{}\n",
                    self.track_id.get(),
                    self.source_device,
                    self.source_inode,
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
