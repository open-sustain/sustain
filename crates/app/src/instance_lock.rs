// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Filesystem-level single-instance enforcement.
//!
//! Sustain is a single-instance application: the GTK application id is the
//! fixed reverse-DNS string defined in `main.rs`, so GApplication's own
//! D-Bus uniqueness check routes any second activation to the first
//! instance's window. This module is the belt-and-suspenders layer that
//! covers the cases GApplication cannot — most importantly the integrity
//! rationale itself (interleaved SQLite writes across multi-statement
//! invariants, racing tag/rating writes, duplicate scan inserts, MPRIS
//! bus-name collisions) when two processes somehow bypass the D-Bus
//! check (different session buses, manual `GApplicationFlags`, NFS-mounted
//! libraries, etc.).
//!
//! [`acquire`] takes an `flock(LOCK_EX | LOCK_NB)` on a sidecar `.lock`
//! file beside the library database, and another on the folder holding
//! the settings file, each where it really is (see [`StorageLayout`]): a
//! database given by a link is locked as the file it leads to, and two
//! instances on different libraries cannot share, and each save whole
//! over the other's, one settings file. The locks are released when the
//! returned [`InstanceLock`] is dropped (process exit, including crashes,
//! since the kernel closes file descriptors on `exit_group`). The
//! database's lock is a sidecar so it cannot be confused with SQLite's own
//! POSIX-fcntl transaction locks; the settings' is their folder, as the
//! settings file itself is atomically replaced on every save, and a folder
//! takes a lock without a file being written in it. A lock file that is a
//! link is refused: through it, the lock would be taken on whatever it
//! leads to.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{FlockOperation, flock};

use crate::storage_layout::{StorageLayout, StoragePart};

/// Holds the live lock file descriptors for the process lifetime. Dropping
/// the value (process exit, panic-unwind to `main`) releases the
/// kernel-level flocks automatically; we intentionally do not unlock
/// explicitly because the OS-level release on `close(2)` is the canonical
/// contract and a manual `flock(LOCK_UN)` would be redundant.
#[must_use = "the instance lock is released when this value is dropped"]
pub(crate) struct InstanceLock {
    _database: File,
    _settings: File,
}

/// Outcome of a single-instance acquire attempt. `Held` and `Failed` name
/// the part whose lock was not taken and the path of its lock file, so
/// callers can include them in diagnostic messages.
pub(crate) enum AcquireOutcome {
    Acquired(InstanceLock),
    Held {
        part: StoragePart,
        lock_path: PathBuf,
    },
    Failed {
        lock_path: PathBuf,
        error: io::Error,
    },
}

/// Try to acquire the instance locks on the library database, then on the
/// settings file, of `layout`.
///
/// Each lock file is a zero-byte sidecar named by [`StorageLayout`]. Its
/// folder is created if it does not yet exist — the typical case is a
/// fresh install where the SQLite file has not been opened yet, so the
/// XDG data directory may not be on disk at acquire time.
pub(crate) fn acquire(layout: &StorageLayout) -> AcquireOutcome {
    let database = match lock(
        StoragePart::Database,
        &layout.database_lock(),
        LockTarget::File,
    ) {
        Ok(file) => file,
        Err(outcome) => return outcome,
    };
    match lock(
        StoragePart::Settings,
        layout.settings_lock(),
        LockTarget::Folder,
    ) {
        Ok(settings) => AcquireOutcome::Acquired(InstanceLock {
            _database: database,
            _settings: settings,
        }),
        Err(outcome) => outcome,
    }
}

/// What a lock is taken on.
#[derive(Clone, Copy)]
enum LockTarget {
    /// A zero-byte file, created if missing.
    File,
    /// A folder, created if missing.
    Folder,
}

fn lock(part: StoragePart, lock_path: &Path, target: LockTarget) -> Result<File, AcquireOutcome> {
    let failed = |error| AcquireOutcome::Failed {
        lock_path: lock_path.to_path_buf(),
        error,
    };
    let folder = match target {
        LockTarget::File => lock_path.parent(),
        LockTarget::Folder => Some(lock_path),
    };
    if let Some(folder) = folder {
        std::fs::create_dir_all(folder).map_err(failed)?;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    match target {
        LockTarget::File => options
            .create(true)
            .write(true)
            .truncate(false)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32),
        LockTarget::Folder => options.custom_flags(rustix::fs::OFlags::DIRECTORY.bits() as i32),
    };
    let file = options.open(lock_path).map_err(failed)?;
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(file),
        Err(errno) if errno == rustix::io::Errno::WOULDBLOCK => Err(AcquireOutcome::Held {
            part,
            lock_path: lock_path.to_path_buf(),
        }),
        Err(errno) => Err(failed(io::Error::from(errno))),
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test failures use panic! to report context")]
mod tests {
    use super::*;

    fn layout(dir: &Path, settings: &str, database: &str) -> StorageLayout {
        StorageLayout {
            settings: dir.join(settings),
            database: dir.join(database),
            cache_dir: dir.join("cache"),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sustain_instance_lock_{name}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn second_acquire_against_the_same_library_reports_held() {
        let dir = scratch("library");
        let layout = layout(&dir, "settings.toml", "library.sqlite");

        let first = match acquire(&layout) {
            AcquireOutcome::Acquired(lock) => lock,
            other => panic!(
                "first acquire should succeed; got {}",
                describe_outcome(&other)
            ),
        };

        match acquire(&layout) {
            AcquireOutcome::Held { part, lock_path } => {
                assert_eq!(part, StoragePart::Database);
                assert_eq!(lock_path, dir.join("library.sqlite.lock"));
            }
            other => panic!(
                "second acquire should report Held; got {}",
                describe_outcome(&other)
            ),
        }

        drop(first);

        // After releasing the first lock, a fresh acquire should succeed.
        match acquire(&layout) {
            AcquireOutcome::Acquired(_) => {}
            other => panic!(
                "post-release acquire should succeed; got {}",
                describe_outcome(&other)
            ),
        }

        std::fs::remove_dir_all(&dir).expect("clean up temp dir");
    }

    #[test]
    fn a_second_library_sharing_the_settings_file_reports_them_held() {
        let dir = scratch("settings");

        let _first = match acquire(&layout(&dir, "settings.toml", "first.sqlite")) {
            AcquireOutcome::Acquired(lock) => lock,
            other => panic!(
                "first acquire should succeed; got {}",
                describe_outcome(&other)
            ),
        };

        match acquire(&layout(&dir, "settings.toml", "second.sqlite")) {
            AcquireOutcome::Held { part, lock_path } => {
                assert_eq!(part, StoragePart::Settings);
                assert_eq!(lock_path, dir);
            }
            other => panic!(
                "a second library on the same settings should report Held; got {}",
                describe_outcome(&other)
            ),
        }

        std::fs::remove_dir_all(&dir).expect("clean up temp dir");
    }

    #[test]
    fn a_lock_file_that_is_a_link_is_refused() {
        let dir = scratch("link");
        std::fs::write(dir.join("elsewhere"), b"").expect("create a file");
        std::os::unix::fs::symlink(dir.join("elsewhere"), dir.join("library.sqlite.lock"))
            .expect("link the lock file");

        match acquire(&layout(&dir, "settings.toml", "library.sqlite")) {
            AcquireOutcome::Failed { lock_path, .. } => {
                assert_eq!(lock_path, dir.join("library.sqlite.lock"));
            }
            other => panic!(
                "a lock file that is a link should fail; got {}",
                describe_outcome(&other)
            ),
        }

        std::fs::remove_dir_all(&dir).expect("clean up temp dir");
    }

    fn describe_outcome(outcome: &AcquireOutcome) -> &'static str {
        match outcome {
            AcquireOutcome::Acquired(_) => "Acquired",
            AcquireOutcome::Held { .. } => "Held",
            AcquireOutcome::Failed { .. } => "Failed",
        }
    }
}
