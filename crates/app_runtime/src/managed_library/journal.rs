// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Crash-recovery journal for managed-library moves. Before any consolidation
//! or metadata-driven retarget touches the filesystem it records the intended
//! moves here; on the next launch [`recover_library_consolidation_journal`]
//! replays an interrupted batch so SQLite and the on-disk layout agree.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use sustain_domain::{TrackId, TrackLocation, TrackRelativePath};

use crate::{ApplicationRuntimeError, ApplicationRuntimeResult};

use super::consolidation::PlannedLibraryConsolidationMove;
use super::file_ops::{path_is_regular_file, paths_refer_to_same_file};

const CONSOLIDATION_JOURNAL_FILE_NAME: &str = ".sustain-consolidation-journal";

#[derive(Clone, Debug, Eq, PartialEq)]
struct ConsolidationJournalEntry {
    track_id: TrackId,
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

    remove_consolidation_journal_if_present(library_path)
}

fn recover_consolidation_journal_entry(
    library_path: &Path,
    library_store: &dyn sustain_library_store::LibraryStore,
    entry: &ConsolidationJournalEntry,
) -> ApplicationRuntimeResult<()> {
    let source_path = entry.source_relative_path.resolve(library_path);
    let destination_path = entry.destination_relative_path.resolve(library_path);
    let source_is_file = path_is_regular_file(&source_path);
    let destination_is_file = path_is_regular_file(&destination_path);

    match (source_is_file, destination_is_file) {
        (false, true) => {
            save_recovered_consolidation_track(library_store, entry)?;
        }
        (true, true) if paths_refer_to_same_file(&source_path, &destination_path) => {
            fs::remove_file(&source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
            save_recovered_consolidation_track(library_store, entry)?;
        }
        (true, false) | (false, false) | (true, true) => {}
    }

    Ok(())
}

fn save_recovered_consolidation_track(
    library_store: &dyn sustain_library_store::LibraryStore,
    entry: &ConsolidationJournalEntry,
) -> ApplicationRuntimeResult<()> {
    library_store
        .update_track_location(
            entry.track_id,
            &TrackLocation::available(entry.destination_relative_path.clone()),
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
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;

    writeln!(file, "# sustain managed library consolidation journal v1")
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    for planned_move in moves {
        let source = encode_relative_path(&planned_move.source_relative_path);
        let destination = encode_relative_path(&planned_move.destination_relative_path);
        writeln!(
            file,
            "move\t{}\t{}\t{}",
            planned_move.track_id.get(),
            source,
            destination
        )
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    }
    file.flush()
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    file.sync_all()
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    fs::rename(&temporary_path, &journal_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;

    Ok(())
}

fn read_consolidation_journal(
    library_path: &Path,
) -> ApplicationRuntimeResult<Vec<ConsolidationJournalEntry>> {
    let contents = fs::read_to_string(consolidation_journal_path(library_path))
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    let mut entries = Vec::new();

    for line in contents.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
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

    fs::remove_file(journal_path).map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)
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
