// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::Path;

/// Three-state result of probing whether a filesystem path exists,
/// distinguishing a *proven* absence from a probe that could not answer.
/// `Path::exists` collapses the latter two into `false`, which is unsafe when
/// availability drives persistent library state or destructive actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FilePresence {
    Present,
    /// The path is confirmed to hold no file of the probed kind: nothing at
    /// all (`ErrorKind::NotFound`), or — for a track — something that is
    /// not a regular file, like a directory reusing a removed file's name.
    Absent,
    /// The presence could not be determined (permission denied, transient
    /// I/O error). Callers must fail closed.
    ProbeFailed,
}

/// Probe whether a track's file can currently be reached. This follows
/// symlinks: a dangling link is not a playable audio file and therefore counts
/// as absent, and so does anything other than a regular file, or a link
/// turned to a file of another kind
/// ([`sustain_metadata::resolve_track_file`]).
pub(crate) fn probe_file_presence(path: &Path) -> FilePresence {
    match classify_probe(sustain_metadata::resolve_track_file(path)) {
        Ok(Some(_)) => FilePresence::Present,
        Ok(None) => FilePresence::Absent,
        Err(presence) => presence,
    }
}

/// [`probe_file_presence`] for a check of the whole library, which, like a
/// scan, does not take a link that leads nowhere as proof of absence: a
/// track reached through one, such as a link to an unmounted drive, could
/// not be checked.
pub(crate) fn probe_library_file_presence(path: &Path) -> FilePresence {
    match probe_file_presence(path) {
        FilePresence::Absent if leads_through_dangling_link(path) => FilePresence::ProbeFailed,
        presence => presence,
    }
}

/// Whether `path`, or a folder on the way to it, is a link that does not
/// resolve: to nothing, through a file, or in a loop.
fn leads_through_dangling_link(path: &Path) -> bool {
    path.ancestors().any(|entry| {
        std::fs::symlink_metadata(entry).is_ok_and(|metadata| metadata.is_symlink())
            && std::fs::metadata(entry).is_err()
    })
}

/// Probe whether a directory entry exists without following symlinks. This is
/// retained as a regression helper for the semantic distinction between a
/// reachable file and a pathname entry.
#[cfg(test)]
pub(crate) fn probe_path_entry_presence(path: &Path) -> FilePresence {
    classify_probe(std::fs::symlink_metadata(path))
        .map_or_else(|presence| presence, |_| FilePresence::Present)
}

/// The metadata of an existing entry, or the presence a failed probe means.
fn classify_probe<T>(result: std::io::Result<T>) -> Result<T, FilePresence> {
    result.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FilePresence::Absent
        } else {
            FilePresence::ProbeFailed
        }
    })
}
