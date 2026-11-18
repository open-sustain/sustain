// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Crash-recovery journal for the per-track CD-import publish/row transition.
//!
//! The filesystem (publishing the encoded file) and SQLite (inserting the
//! authoritative row) cannot commit atomically. The worker records the
//! in-flight transition here *before* publishing and clears it only after
//! the row is durably flushed — or after its own rollback has completed —
//! so at most one entry can survive. Paths are recorded relative to the
//! library root, so the entry stays meaningful when the same library is
//! reopened through another mount point. The entry names the rip by its
//! file identity, so [`recover`] only ever removes that exact file, whatever
//! now sits at its paths. Recovery first makes the visible SQLite state
//! durable, then:
//!
//! * destination is the rip **and** a library row references it → the
//!   import completed in the window between the flush and the journal clear;
//!   keep the file and drop the entry (finish);
//! * destination is the rip with **no** row → an orphan published file;
//!   delete it (roll back);
//! * destination is anything else → the rip was never published there, and
//!   the file belongs to someone else; keep it;
//!
//! and in every case the rip's leftover staging name is removed. Recovery
//! that cannot flush, inspect, or remove keeps the entry, so the next
//! import retries instead of forgetting an orphan.

use std::fs::{self, File};
use std::io::Write;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use sustain_domain::TrackRelativePath;
use sustain_library_store::LibraryStore;

use crate::managed_library::file_ops::{
    FileIdentity, open_regular_file, remove_regular_file_matching_capability,
};
use crate::managed_library::prune_empty_ancestor_directories_for_sources;
use crate::{ApplicationRuntimeError, ApplicationRuntimeResult};

const JOURNAL_FILE_NAME: &str = ".sustain-cd-import-journal";
/// 7 ASCII bytes + a 1-byte format version. Version 2 records root-relative
/// paths and the rip's file identity; an entry in an earlier format cannot
/// prove which file is the rip, so recovery drops it without removing
/// anything.
const MAGIC: &[u8; 8] = b"SUSCDI\x00\x02";

struct PendingTransition {
    destination: TrackRelativePath,
    staging: TrackRelativePath,
    rip: FileIdentity,
}

fn journal_path(library_root: &Path) -> PathBuf {
    library_root.join(JOURNAL_FILE_NAME)
}

/// Record the in-flight publish of the `rip` staged at `staging` to
/// `destination` — both inside `library_root` — before it happens. Durable
/// on return: the bytes and the directory entry are both fsynced.
pub(crate) fn write_pending(
    library_root: &Path,
    destination: &Path,
    staging: &Path,
    rip: FileIdentity,
) -> ApplicationRuntimeResult<()> {
    let destination = relative_to_root(library_root, destination)?;
    let staging = relative_to_root(library_root, staging)?;
    let mut bytes = Vec::with_capacity(80);
    bytes.extend_from_slice(MAGIC);
    append_path(&mut bytes, destination.as_path());
    append_path(&mut bytes, staging.as_path());
    bytes.extend_from_slice(&rip.device.to_le_bytes());
    bytes.extend_from_slice(&rip.inode.to_le_bytes());
    atomic_write(&journal_path(library_root), &bytes)
}

fn relative_to_root(
    library_root: &Path,
    path: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    path.strip_prefix(library_root)
        .ok()
        .and_then(|relative| TrackRelativePath::new(relative.to_path_buf()))
        .ok_or(ApplicationRuntimeError::CdImportFailed)
}

/// Remove the journal once the row is durably committed, or once the
/// worker's own rollback has fully undone the transition.
pub(crate) fn clear_pending(library_root: &Path) -> ApplicationRuntimeResult<()> {
    let path = journal_path(library_root);
    match fs::remove_file(&path) {
        Ok(()) => sync_parent_dir(&path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(ApplicationRuntimeError::CdImportFailed),
    }
}

/// Finish or roll back any pending transition. A no-op when no journal is
/// present. The worker also calls this to roll back its own failed
/// transition, so a rollback that cannot complete leaves the entry for the
/// next import.
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
        // A row the crashed worker inserted may be visible but not yet
        // durable, and a compensating delete may be visible but not yet
        // durable either. Flush first, so the file decision below rests on
        // what a later power loss cannot revert.
        library_store
            .flush_durable()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let destination = transition.destination.resolve(library_root);
        remove_if_rip(&transition.staging.resolve(library_root), transition.rip)?;
        if !destination_has_row(&transition.destination, library_store)?
            && remove_if_rip(&destination, transition.rip)?
        {
            // Orphan published file: no row points at it, so removing it is
            // the rollback. Tidy the now-possibly-empty managed folders too.
            prune_empty_ancestor_directories_for_sources(
                library_root,
                std::slice::from_ref(&destination),
            );
        }
    }

    // Corrupt, from an earlier format, or fully reconciled — either way the
    // journal has served its purpose and must not gate the next import.
    clear_pending(library_root)
}

/// Remove `path` if it is the rip, reporting whether it was. Anything else
/// at `path` is not Sustain's to remove, and is recognized from its
/// metadata alone, without needing read access. A path that cannot be
/// inspected or removed fails recovery so the journal survives for a retry.
fn remove_if_rip(path: &Path, rip: FileIdentity) -> ApplicationRuntimeResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {
            if FileIdentity::of(&metadata) != rip {
                return Ok(false);
            }
        }
        // A link, directory, or special file is never the rip.
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(ApplicationRuntimeError::CdImportFailed),
    }
    let file = open_regular_file(path).map_err(|_| ApplicationRuntimeError::CdImportFailed)?;
    if file.identity() != rip {
        return Ok(false);
    }
    remove_regular_file_matching_capability(path, &file)
        .map(|()| true)
        .map_err(|_| ApplicationRuntimeError::CdImportFailed)
}

fn destination_has_row(
    destination: &TrackRelativePath,
    library_store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<bool> {
    library_store
        .track_id_at(destination)
        .map(|track_id| track_id.is_some())
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
}

fn append_path(buffer: &mut Vec<u8>, path: &Path) {
    let raw = path.as_os_str().as_bytes();
    buffer.extend_from_slice(&(raw.len() as u32).to_le_bytes());
    buffer.extend_from_slice(raw);
}

fn parse(bytes: &[u8]) -> Option<PendingTransition> {
    let rest = bytes.strip_prefix(MAGIC.as_slice())?;
    let (destination, rest) = take_path(rest)?;
    let (staging, rest) = take_path(rest)?;
    let (device, rest) = take_u64(rest)?;
    let (inode, _rest) = take_u64(rest)?;
    Some(PendingTransition {
        destination: TrackRelativePath::new(destination)?,
        staging: TrackRelativePath::new(staging)?,
        rip: FileIdentity { device, inode },
    })
}

fn take_path(bytes: &[u8]) -> Option<(PathBuf, &[u8])> {
    let (length_bytes, rest) = bytes.split_at_checked(4)?;
    let length = u32::from_le_bytes(length_bytes.try_into().ok()?) as usize;
    let (raw, rest) = rest.split_at_checked(length)?;
    Some((PathBuf::from(std::ffi::OsStr::from_bytes(raw)), rest))
}

fn take_u64(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let (value, rest) = bytes.split_at_checked(8)?;
    Some((u64::from_le_bytes(value.try_into().ok()?), rest))
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
    use sustain_library_store::{LibraryStore, SqliteLibraryStore};

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

    /// Stage a rip and journal its publish to `destination`, as the worker
    /// does right before the no-overwrite move.
    fn journal_rip(root: &Path, destination: &Path) -> PathBuf {
        let staging = root.join(".staging.flac");
        fs::write(&staging, b"rip").expect("write staging");
        let rip = crate::managed_library::file_ops::regular_file_identity(&staging)
            .expect("rip identity");
        write_pending(root, destination, &staging, rip).expect("write journal");
        fs::create_dir_all(destination.parent().expect("dest parent")).expect("dirs");
        staging
    }

    #[test]
    fn recover_rolls_back_an_orphan_published_file() {
        let root = unique_root();
        let destination = root.join("Artist/Album/01 Song.flac");
        let staging = journal_rip(&root, &destination);
        // Interrupted mid-move: both names are the rip.
        fs::hard_link(&staging, &destination).expect("publish");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
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
        let staging = journal_rip(&root, &destination);
        fs::rename(&staging, &destination).expect("publish");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store
            .save_track(track_at("Artist/Album/01 Song.flac"))
            .expect("seed row");
        recover(&root, &store).expect("recover");

        assert!(destination.exists(), "a file with a row is kept");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// The rip was never published, and the user put their own file at its
    /// destination before the next import: it has no row, but it is not
    /// the rip, so it stays.
    #[test]
    fn recover_keeps_a_file_at_the_destination_that_is_not_the_rip() {
        let root = unique_root();
        let destination = root.join("Artist/Album/01 Song.flac");
        let staging = journal_rip(&root, &destination);
        fs::write(&destination, b"my music").expect("user's file");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        recover(&root, &store).expect("recover");

        assert_eq!(fs::read(&destination).expect("user's file"), b"my music");
        assert!(!staging.exists(), "the unpublished rip is removed");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// The journal names its paths relative to the library root, so a
    /// library reopened through another path to the same folder — the old
    /// path still reachable — finds the row that claims the rip and keeps it.
    #[test]
    fn recover_keeps_a_committed_rip_when_the_library_is_reached_through_another_path() {
        let root = unique_root();
        let destination = root.join("Artist/Album/01 Song.flac");
        let staging = journal_rip(&root, &destination);
        fs::rename(&staging, &destination).expect("publish");
        let alias = root.with_extension("alias");
        std::os::unix::fs::symlink(&root, &alias).expect("a second path to the library");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store
            .save_track(track_at("Artist/Album/01 Song.flac"))
            .expect("seed row");
        recover(&alias, &store).expect("recover");

        assert!(destination.exists(), "a file with a row is kept");
        assert!(!journal_path(&root).exists());
        fs::remove_file(alias).expect("remove alias");
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// Another file at the destination is recognized as not the rip from
    /// its metadata, so recovery neither needs to read it nor stalls on it.
    #[test]
    fn recover_passes_over_an_unreadable_file_that_is_not_the_rip() {
        use std::os::unix::fs::PermissionsExt;

        let root = unique_root();
        let destination = root.join("Artist/Album/01 Song.flac");
        let staging = journal_rip(&root, &destination);
        fs::write(&destination, b"my music").expect("user's file");
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o000))
            .expect("make the user's file unreadable");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        let recovered = recover(&root, &store);
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o644))
            .expect("restore permissions");

        assert_eq!(recovered, Ok(()));
        assert_eq!(fs::read(&destination).expect("user's file"), b"my music");
        assert!(!staging.exists(), "the unpublished rip is removed");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// Recovery that cannot tell whether the rip is at its destination
    /// fails and keeps the entry, then finishes once the path resolves.
    /// (A path through a file stands in for any error other than absence;
    /// permission errors would not reproduce for a privileged test user.)
    #[test]
    fn recover_keeps_the_journal_while_the_destination_cannot_be_inspected() {
        let root = unique_root();
        let destination = root.join("Artist/Album/01 Song.flac");
        let staging = journal_rip(&root, &destination);
        let album = destination.parent().expect("album");
        fs::remove_dir(album).expect("remove album folder");
        fs::write(album, b"not a folder").expect("block the album path");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        assert_eq!(
            recover(&root, &store),
            Err(ApplicationRuntimeError::CdImportFailed)
        );
        assert!(
            journal_path(&root).exists(),
            "the entry survives for a retry"
        );

        fs::remove_file(album).expect("unblock the album path");
        recover(&root, &store).expect("recover");
        assert!(!staging.exists(), "the unpublished rip is removed");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// A version-1 entry carries no rip identity, so it cannot prove which
    /// file is Sustain's: recovery drops it and removes nothing.
    #[test]
    fn recover_drops_an_earlier_format_entry_without_removing_files() {
        let root = unique_root();
        let destination = root.join("01 Song.flac");
        fs::write(&destination, b"my music").expect("user's file");
        let mut bytes = b"SUSCDI\x00\x01".to_vec();
        append_path(&mut bytes, &destination);
        append_path(&mut bytes, &root.join(".staging.flac"));
        atomic_write(&journal_path(&root), &bytes).expect("write old journal");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        recover(&root, &store).expect("recover");

        assert!(destination.exists());
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn recover_is_a_noop_without_a_journal() {
        let root = unique_root();
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        recover(&root, &store).expect("recover");
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }
}
