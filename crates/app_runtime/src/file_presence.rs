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
    /// The path is confirmed not to exist (`ErrorKind::NotFound`).
    Absent,
    /// The presence could not be determined (permission denied, transient
    /// I/O error). Callers must fail closed.
    ProbeFailed,
}

/// Probe whether a track's target can currently be reached. This follows
/// symlinks: a dangling link is not a playable audio file and therefore counts
/// as absent.
pub(crate) fn probe_file_presence(path: &Path) -> FilePresence {
    classify_probe(std::fs::metadata(path))
}

/// Probe whether a directory entry exists without following symlinks. This is
/// used by Move to Trash: a dangling symlink is still a real entry that the
/// user asked Sustain to remove.
pub(crate) fn probe_path_entry_presence(path: &Path) -> FilePresence {
    classify_probe(std::fs::symlink_metadata(path))
}

fn classify_probe(result: std::io::Result<std::fs::Metadata>) -> FilePresence {
    match result {
        Ok(_) => FilePresence::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FilePresence::Absent,
        Err(_) => FilePresence::ProbeFailed,
    }
}
