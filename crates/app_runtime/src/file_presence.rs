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

/// Probe whether a track's target can currently be reached. This follows
/// symlinks: a dangling link is not a playable audio file and therefore counts
/// as absent, and so does anything other than a regular file.
pub(crate) fn probe_file_presence(path: &Path) -> FilePresence {
    match classify_probe(std::fs::metadata(path)) {
        Ok(metadata) if metadata.is_file() => FilePresence::Present,
        Ok(_) => FilePresence::Absent,
        Err(presence) => presence,
    }
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
fn classify_probe(
    result: std::io::Result<std::fs::Metadata>,
) -> Result<std::fs::Metadata, FilePresence> {
    result.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            FilePresence::Absent
        } else {
            FilePresence::ProbeFailed
        }
    })
}
