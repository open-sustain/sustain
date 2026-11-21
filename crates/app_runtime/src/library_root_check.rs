// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Keeping the stored track availability in step with the library root.
//!
//! A track's location is relative to the library root, so changing the root
//! changes which files the tracks are, and every track is checked against
//! the new one once it is saved. The store records, with what a check found
//! and in the same transaction, the root the stored availability now
//! reflects; a check that could not reach every track records none.
//! Settings and SQLite are separate stores, so the two can part: the check
//! may fail to commit after the settings are saved, a library may predate
//! the record, a check may have been incomplete. A root the record does not
//! name is checked again as the library opens, off the GTK thread. A root
//! that is not a folder is never checked against: an unplugged drive does
//! not make every track missing.

use std::path::{Path, PathBuf};

use sustain_domain::{Track, TrackAvailability, TrackLocation};
use sustain_library_store::LibraryStore;

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, NotificationCategory,
    NotificationSeverity,
    file_presence::{FilePresence, probe_library_file_presence},
    notifications,
};

/// What a check of the tracks against a library root found, for the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AvailabilityCheckSummary {
    /// Tracks found absent, whether or not they were already marked so.
    pub(crate) missing: usize,
    /// Tracks whose file could not be probed; their availability stands.
    pub(crate) unresolved: usize,
    /// Tracks whose stored availability the check changed.
    pub(crate) changed: usize,
    pub(crate) total: usize,
}

/// A committed check of the tracks against a library root.
pub(crate) struct AvailabilityCheck {
    /// The availability changes the store applied, by track index.
    applied: Vec<(usize, TrackLocation)>,
    pub(crate) summary: AvailabilityCheckSummary,
}

/// What opening the library under its root did to the stored availability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AvailabilityAtOpen {
    Unchanged,
    /// A check was due and ran.
    Checked(AvailabilityCheckSummary),
    /// A check was due, and the store refused it; it stays due.
    Failed,
}

/// Why the tracks are checked against a library root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckOccasion {
    /// The user chose the root this session: whatever folder it is, what
    /// it holds is the answer, even none of the tracks.
    RootChosen,
    /// The library opens under a root the stored availability does not
    /// reflect: an earlier check failed, was incomplete, or predates the
    /// record. The root may no longer be what it was chosen as, such as
    /// the empty mountpoint left in place of an unplugged drive: a root
    /// that holds none of the tracks is not taken as the answer; the check
    /// waits for it.
    Recovery,
}

/// Check `tracks`, the library's as it opens, against `library_root` when
/// the record does not name it, and update `tracks` and `checked_root` to
/// what the store then holds. Runs where the library is read: off the GTK
/// thread for a deferred hydration.
pub(crate) fn settle_availability_at_open(
    store: &dyn LibraryStore,
    tracks: &mut [Track],
    library_root: Option<&Path>,
    checked_root: &mut Option<PathBuf>,
) -> AvailabilityAtOpen {
    let Some(library_root) = library_root else {
        return AvailabilityAtOpen::Unchanged;
    };
    if checked_root.as_deref() == Some(library_root) {
        return AvailabilityAtOpen::Unchanged;
    }
    match check_track_availability(
        store,
        tracks,
        library_root,
        checked_root,
        CheckOccasion::Recovery,
        probe_library_file_presence,
    ) {
        Ok(Some(check)) => {
            for (index, location) in check.applied {
                tracks[index].location = location;
            }
            AvailabilityAtOpen::Checked(check.summary)
        }
        Ok(None) => AvailabilityAtOpen::Unchanged,
        Err(_) => AvailabilityAtOpen::Failed,
    }
}

/// Probe each track under `library_root` and commit the availability that
/// changed, with the record: the root when every track was resolved, none
/// otherwise, so the tracks the check could not reach are checked again.
/// `None` when the check waits for its root.
fn check_track_availability(
    store: &dyn LibraryStore,
    tracks: &[Track],
    library_root: &Path,
    checked_root: &mut Option<PathBuf>,
    occasion: CheckOccasion,
    probe: impl Fn(&Path) -> FilePresence,
) -> ApplicationRuntimeResult<Option<AvailabilityCheck>> {
    if ensure_library_root_is_a_folder(library_root).is_err() {
        return Ok(None);
    }
    let mut changes = Vec::new();
    let mut present = 0usize;
    let mut missing_unchanged = 0usize;
    let mut unresolved = 0usize;
    for (index, track) in tracks.iter().enumerate() {
        let availability = match probe(&track.location.absolute_path(library_root)) {
            FilePresence::Present => {
                present += 1;
                TrackAvailability::Available
            }
            FilePresence::Absent => TrackAvailability::Missing,
            FilePresence::ProbeFailed => {
                unresolved += 1;
                continue;
            }
        };
        let missing = availability == TrackAvailability::Missing;
        if missing != track.location.is_missing() {
            changes.push((
                index,
                track.location.clone().with_availability(availability),
            ));
        } else if missing {
            missing_unchanged += 1;
        }
    }
    if occasion == CheckOccasion::Recovery && present == 0 && !tracks.is_empty() {
        return Ok(None);
    }
    let checked = (unresolved == 0).then(|| library_root.to_path_buf());
    let updates = changes
        .iter()
        .map(|(index, location)| (tracks[*index].id, location.clone()))
        .collect::<Vec<_>>();
    let applied = store
        .record_track_availability_check(&updates, checked.as_deref())
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    *checked_root = checked;
    // A change that did not apply was to a track moved meanwhile, which
    // the check therefore says nothing about.
    let applied = changes
        .into_iter()
        .zip(applied)
        .filter_map(|(change, applied)| applied.then_some(change))
        .collect::<Vec<_>>();
    let newly_missing = applied
        .iter()
        .filter(|(_, location)| location.is_missing())
        .count();
    Ok(Some(AvailabilityCheck {
        summary: AvailabilityCheckSummary {
            missing: missing_unchanged + newly_missing,
            unresolved,
            changed: applied.len(),
            total: tracks.len(),
        },
        applied,
    }))
}

impl ApplicationRuntime {
    /// Forget the checked root before a root change is saved: should the
    /// check of the new root not commit, the record cannot name it from an
    /// earlier check, of a root chosen again, and the library's next
    /// opening checks it. Nothing is saved if the store refuses.
    pub(crate) fn forget_checked_library_root(&mut self) -> ApplicationRuntimeResult<()> {
        if let Some(store) = &self.library_store {
            store
                .forget_checked_library_root()
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }
        self.checked_library_root = None;
        Ok(())
    }

    /// Check every track against the library root the user just chose
    /// and saved, and tell the user what was found. A root change is
    /// structural reconciliation, semantically closer to a scan than to a
    /// preference toggle: the user just typed or picked the new root, and
    /// the cost is bounded by the library size. No other save stats tracks:
    /// the management-mode switch must not freeze the UI on a 10k library.
    /// Checking needs the tracks; before hydration completes, the check as
    /// the library opens covers it. The settings are saved either way, so a
    /// check that fails is reported, not returned; the record then does not
    /// name the root, and the library's next opening checks it.
    pub(crate) fn check_track_availability_under_chosen_root(&mut self) {
        if self.library_hydration_state() != crate::LibraryHydrationState::Ready {
            return;
        }
        let Some(library_root) = self.settings.library.path.clone() else {
            return;
        };
        match self.check_track_availability_with(
            &library_root,
            CheckOccasion::RootChosen,
            probe_library_file_presence,
        ) {
            Ok(Some(summary)) => {
                self.report_availability_check(summary, CheckOccasion::RootChosen);
            }
            // The root stopped being a folder since it was chosen; the
            // check waits for it.
            Ok(None) => {}
            Err(_) => self.report_availability_check_failure(),
        }
    }

    /// Check every track against `library_root` with `probe` and adopt
    /// what the store committed.
    fn check_track_availability_with(
        &mut self,
        library_root: &Path,
        occasion: CheckOccasion,
        probe: impl Fn(&Path) -> FilePresence,
    ) -> ApplicationRuntimeResult<Option<AvailabilityCheckSummary>> {
        let Some(store) = self.library_store.clone() else {
            return Ok(None);
        };
        let Some(check) = check_track_availability(
            store.as_ref(),
            &self.library_tracks,
            library_root,
            &mut self.checked_library_root,
            occasion,
            probe,
        )?
        else {
            return Ok(None);
        };
        let changed = !check.applied.is_empty();
        for (index, location) in check.applied {
            let mut track = self.library_tracks[index].clone();
            track.location = location;
            self.store_library_track(index, track);
        }
        if changed {
            self.refresh_playback_queue_track_ids();
        }
        Ok(Some(check.summary))
    }

    /// [`Self::check_track_availability_with`] for a root the user chose,
    /// for tests to decide what each probe finds.
    #[cfg(test)]
    pub(super) fn check_chosen_root_availability_with(
        &mut self,
        library_root: &Path,
        probe: impl Fn(&Path) -> FilePresence,
    ) -> ApplicationRuntimeResult<Option<AvailabilityCheckSummary>> {
        self.check_track_availability_with(library_root, CheckOccasion::RootChosen, probe)
    }

    /// Tell the user what opening the library did to its availability:
    /// nothing when a due check changed nothing and resolved every track.
    pub(crate) fn report_availability_at_open(&mut self, outcome: AvailabilityAtOpen) {
        match outcome {
            AvailabilityAtOpen::Checked(summary)
                if summary.changed > 0 || summary.unresolved > 0 =>
            {
                self.report_availability_check(summary, CheckOccasion::Recovery);
            }
            AvailabilityAtOpen::Unchanged | AvailabilityAtOpen::Checked(_) => {}
            AvailabilityAtOpen::Failed => self.report_availability_check_failure(),
        }
    }

    fn report_availability_check_failure(&mut self) {
        self.push_ephemeral_notification(
            NotificationCategory::LibraryScan,
            NotificationSeverity::Warning,
            notifications::library_root_check_failed_text(),
        );
    }

    /// Tell the user what a check found: as the outcome of the root they
    /// chose, or, for one as the library opens, of the root they kept.
    fn report_availability_check(
        &mut self,
        summary: AvailabilityCheckSummary,
        occasion: CheckOccasion,
    ) {
        let severity = if summary.missing > 0 || summary.unresolved > 0 {
            NotificationSeverity::Warning
        } else {
            NotificationSeverity::Info
        };
        let outcome_text = match occasion {
            CheckOccasion::RootChosen => notifications::library_path_change_outcome_text,
            CheckOccasion::Recovery => notifications::library_root_check_outcome_text,
        };
        self.push_ephemeral_notification(
            NotificationCategory::LibraryScan,
            severity,
            outcome_text(summary.missing, summary.unresolved, summary.total),
        );
    }
}

/// `Ok` when `path` is a folder Sustain can list and open files in: read
/// and search permission, as the effective user. A missing path, or one
/// that names a file, is not a folder; any other failure to reach it, such
/// as a permission refused on it or on the way to it, leaves it
/// inaccessible.
pub(crate) fn ensure_library_root_is_a_folder(path: &Path) -> ApplicationRuntimeResult<()> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(ApplicationRuntimeError::LibraryPathNotAFolder),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Err(ApplicationRuntimeError::LibraryPathNotAFolder);
        }
        Err(_) => return Err(ApplicationRuntimeError::LibraryPathInaccessible),
    }
    rustix::fs::accessat(
        rustix::fs::CWD,
        path,
        rustix::fs::Access::READ_OK | rustix::fs::Access::EXEC_OK,
        rustix::fs::AtFlags::EACCESS,
    )
    .map_err(|_| ApplicationRuntimeError::LibraryPathInaccessible)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::ensure_library_root_is_a_folder;
    use crate::ApplicationRuntimeError;

    #[test]
    fn a_library_root_is_a_folder_a_missing_or_file_path_or_an_unreadable_one() {
        let root = std::env::temp_dir().join(format!("sustain_root_check_{}", std::process::id()));
        let locked = root.join("locked");
        std::fs::create_dir_all(locked.join("inside")).expect("create folders");
        std::fs::write(root.join("file.flac"), b"audio").expect("write a file");

        assert_eq!(ensure_library_root_is_a_folder(&root), Ok(()));
        assert_eq!(
            ensure_library_root_is_a_folder(&root.join("missing")),
            Err(ApplicationRuntimeError::LibraryPathNotAFolder)
        );
        assert_eq!(
            ensure_library_root_is_a_folder(&root.join("file.flac")),
            Err(ApplicationRuntimeError::LibraryPathNotAFolder)
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock the folder");
        let behind_a_refusal = ensure_library_root_is_a_folder(&locked.join("inside"));
        let refused = ensure_library_root_is_a_folder(&locked);
        // Listable, but its files cannot be opened.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o400))
            .expect("make the folder listable only");
        let unsearchable = ensure_library_root_is_a_folder(&locked);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock the folder");
        std::fs::remove_dir_all(&root).expect("remove test folders");

        // Root reads through any permission; the refusal needs a user.
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert_eq!(
            behind_a_refusal,
            Err(ApplicationRuntimeError::LibraryPathInaccessible)
        );
        assert_eq!(
            refused,
            Err(ApplicationRuntimeError::LibraryPathInaccessible)
        );
        assert_eq!(
            unsearchable,
            Err(ApplicationRuntimeError::LibraryPathInaccessible)
        );
    }
}
