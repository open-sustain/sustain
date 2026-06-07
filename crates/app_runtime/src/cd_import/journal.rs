// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Crash-recovery journal for the per-track CD-import publish/row transition.
//!
//! The filesystem (publishing the encoded file) and SQLite (inserting the
//! authoritative row) cannot commit atomically. The worker records the
//! in-flight transition here *before* publishing and clears it only after
//! the row is durably flushed, so at most one entry can survive a crash.
//! [`recover`] then deterministically finishes or rolls it back:
//!
//! * destination present **and** a library row references it → the import
//!   completed in the window between the flush and the journal clear; keep
//!   the file and drop the entry (finish);
//! * destination present with **no** row → an orphan published file; delete
//!   it (roll back);
//!
//! and in both cases any leftover staging file is removed.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sustain_domain::TrackRelativePath;
use sustain_library_store::LibraryStore;

use crate::managed_library::prune_empty_ancestor_directories_for_sources;
use crate::{ApplicationRuntimeError, ApplicationRuntimeResult};

const JOURNAL_FILE_NAME: &str = ".sustain-cd-import-journal";
/// 7 ASCII bytes + a 1-byte format version.
const MAGIC: &[u8; 8] = b"SUSCDI\x00\x01";

struct PendingTransition {
    destination: PathBuf,
    staging: PathBuf,
}

fn journal_path(library_root: &Path) -> PathBuf {
    library_root.join(JOURNAL_FILE_NAME)
}

/// Record the in-flight publish before it happens. Durable on return: the
/// bytes and the directory entry are both fsynced.
pub(crate) fn write_pending(
    library_root: &Path,
    destination: &Path,
    staging: &Path,
) -> ApplicationRuntimeResult<()> {
    let mut bytes = Vec::with_capacity(64);
    bytes.extend_from_slice(MAGIC);
    append_path(&mut bytes, destination);
    append_path(&mut bytes, staging);
    atomic_write(&journal_path(library_root), &bytes)
}

/// Remove the journal once the row is durably committed.
pub(crate) fn clear_pending(library_root: &Path) -> ApplicationRuntimeResult<()> {
    let path = journal_path(library_root);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent_dir(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ApplicationRuntimeError::CdImportFailed),
    }
}

/// Finish or roll back any interrupted transition before CD importing
/// resumes. A no-op when no journal is present.
pub(crate) fn recover(
    library_root: &Path,
    library_store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<()> {
    let path = journal_path(library_root);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(ApplicationRuntimeError::CdImportFailed),
    };

    if let Some(transition) = parse(&bytes) {
        // A leftover staging file is always disposable.
        let _ = fs::remove_file(&transition.staging);
        if transition.destination.exists()
            && !destination_has_row(library_root, &transition.destination, library_store)?
        {
            // Orphan published file: no row points at it, so removing it is
            // the rollback. Tidy the now-possibly-empty managed folders too.
            let _ = fs::remove_file(&transition.destination);
            prune_empty_ancestor_directories_for_sources(
                library_root,
                std::slice::from_ref(&transition.destination),
            );
        }
    }

    // Corrupt or fully reconciled — either way the journal has served its
    // purpose and must not gate the next import.
    clear_pending(library_root)
}

fn destination_has_row(
    library_root: &Path,
    destination: &Path,
    library_store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<bool> {
    let Some(relative) = destination
        .strip_prefix(library_root)
        .ok()
        .and_then(|relative| TrackRelativePath::new(relative.to_path_buf()))
    else {
        return Ok(false);
    };
    let tracks = library_store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(tracks
        .iter()
        .any(|track| track.location.relative_path == relative))
}

fn append_path(buffer: &mut Vec<u8>, path: &Path) {
    let raw = path.as_os_str().as_bytes();
    buffer.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    buffer.extend_from_slice(raw);
}

fn parse(bytes: &[u8]) -> Option<PendingTransition> {
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let (destination, rest) = take_path(rest)?;
    let (staging, _rest) = take_path(rest)?;
    Some(PendingTransition {
        destination,
        staging,
    })
}

fn take_path(bytes: &[u8]) -> Option<(PathBuf, &[u8])> {
    let (length_bytes, rest) = bytes.split_at_checked(4)?;
    let length = u32::from_le_bytes(length_bytes.try_into().ok()?) as usize;
    let (raw, rest) = rest.split_at_checked(length)?;
    Some((PathBuf::from(std::ffi::OsStr::from_bytes(raw)), rest))
}

fn atomic_write(path: &Path, bytes: &[u8]) -> ApplicationRuntimeResult<()> {
    let parent = path
        .parent()
        .ok_or(ApplicationRuntimeError::CdImportFailed)?;
    let temp = parent.join(format!(
        ".sustain-cd-import-journal.{}.tmp",
        std::process::id()
    ));
    let write = || -> std::io::Result<()> {
        let mut file = File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if write().is_err() {
        let _ = fs::remove_file(&temp);
        return Err(ApplicationRuntimeError::CdImportFailed);
    }
    if fs::rename(&temp, path).is_err() {
        let _ = fs::remove_file(&temp);
        return Err(ApplicationRuntimeError::CdImportFailed);
    }
    sync_parent_dir(path)
}

fn sync_parent_dir(path: &Path) -> ApplicationRuntimeResult<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| ApplicationRuntimeError::CdImportFailed)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use sustain_domain::{PlayStatistics, Rating, Track, TrackLocation, TrackMetadata};
    use sustain_library_store::{InMemoryLibraryStore, LibraryStore};

    use super::*;

    fn unique_root() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "sustain_cd_journal_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).expect("create journal test root");
        root
    }

    fn track_at(relative: &str) -> Track {
        Track {
            id: sustain_domain::TrackId::new(1).expect("track id"),
            location: TrackLocation::available(
                TrackRelativePath::new(relative).expect("relative path"),
            ),
            metadata: TrackMetadata::default(),
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    #[test]
    fn recover_rolls_back_an_orphan_published_file() {
        let root = unique_root();
        let destination = root.join("Artist/Album/01 Song.flac");
        let staging = root.join(".staging.flac");
        fs::create_dir_all(destination.parent().expect("dest parent")).expect("dirs");
        fs::write(&destination, b"audio").expect("write dest");
        fs::write(&staging, b"staged").expect("write staging");
        write_pending(&root, &destination, &staging).expect("write journal");

        let store = InMemoryLibraryStore::new();
        recover(&root, &store).expect("recover");

        assert!(!destination.exists(), "orphan published file is removed");
        assert!(!staging.exists(), "staging is removed");
        assert!(!root.join("Artist").exists(), "empty folders pruned");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recover_keeps_a_published_file_that_has_a_row() {
        let root = unique_root();
        let destination = root.join("Artist/Album/01 Song.flac");
        let staging = root.join(".staging.flac");
        fs::create_dir_all(destination.parent().expect("dest parent")).expect("dirs");
        fs::write(&destination, b"audio").expect("write dest");
        write_pending(&root, &destination, &staging).expect("write journal");

        let store = InMemoryLibraryStore::new();
        store
            .save_track(track_at("Artist/Album/01 Song.flac"))
            .expect("seed row");
        recover(&root, &store).expect("recover");

        assert!(destination.exists(), "a file with a row is kept");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recover_is_a_noop_without_a_journal() {
        let root = unique_root();
        let store = InMemoryLibraryStore::new();
        recover(&root, &store).expect("recover");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
