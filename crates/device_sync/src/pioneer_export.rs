// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Which parts of a drive's Pioneer export are Sustain's to replace.
//!
//! A Pioneer export is one catalog: the track database, the artwork it
//! names, and the analysis files. Sustain writes it whole, so it may only
//! write over an export it left on the drive itself. Another database —
//! one rekordbox exported, or Sustain's own after a player recorded history
//! in it — holds a catalog, history and artwork Sustain cannot carry over;
//! so does a catalog Sustain never writes beside it, such as rekordbox's
//! OneLibrary database. A sync refuses to touch the drive until the user
//! removes them.

use std::io;

use sustain_domain::{CatalogueRecord, DeviceRelativePath};
use sustain_pioneer::artwork::ARTWORK_BUCKET;

use crate::model::{SyncBlocker, SyncError};
use crate::transport::DeviceTransport;

/// The drive's track database, relative to the sync base.
pub(crate) fn database_path() -> DeviceRelativePath {
    DeviceRelativePath::new(sustain_pioneer::PDB_RELATIVE_PATH)
        .expect("static Pioneer PDB path is safe")
}

/// The artwork bucket, relative to the sync base.
pub(crate) fn artwork_bucket() -> DeviceRelativePath {
    DeviceRelativePath::new(ARTWORK_BUCKET).expect("static Pioneer artwork bucket path is safe")
}

/// Fails with [`SyncBlocker::ForeignPioneerExport`] unless the drive's
/// Pioneer catalog, if it holds one, is exactly one `record` says Sustain
/// may have left there: no catalog Sustain never writes, and a track
/// database of a recorded size and digest. The database is read only if
/// its size is one Sustain recorded, and then no further than that size.
pub(crate) fn ensure_database_is_ours(
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    record: &CatalogueRecord,
) -> Result<(), SyncError> {
    let foreign = Err(SyncError::Blocked(SyncBlocker::ForeignPioneerExport));
    for catalog in sustain_pioneer::FOREIGN_CATALOG_RELATIVE_PATHS {
        let path =
            base.join(&DeviceRelativePath::new(catalog).expect("static catalog path is safe"));
        if transport
            .regular_file_len(&path)
            .map_err(|error| SyncError::io(path.as_str(), error))?
            .is_some()
        {
            return foreign;
        }
    }
    let path = base.join(&database_path());
    let io_error = |error: io::Error| SyncError::io(path.as_str(), error);
    let Some(len) = transport.regular_file_len(&path).map_err(io_error)? else {
        return Ok(());
    };
    if !record
        .databases
        .iter()
        .any(|database| database.size_bytes == len)
    {
        return foreign;
    }
    let bytes = transport.read_file(&path, len).map_err(io_error)?;
    if record
        .databases
        .contains(&crate::catalogue::identity(&bytes))
    {
        Ok(())
    } else {
        foreign
    }
}

/// Fails with [`SyncBlocker::ForeignPioneerExport`] if a file the sync
/// would write as artwork is already on the drive and is not one Sustain
/// wrote there.
pub(crate) fn ensure_artwork_is_ours<'a>(
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    record: &CatalogueRecord,
    to_write: impl IntoIterator<Item = &'a DeviceRelativePath>,
) -> Result<(), SyncError> {
    for path in to_write {
        if record.artwork.contains(path) {
            continue;
        }
        let on_device = base.join(path);
        let present = transport
            .regular_file_len(&on_device)
            .map_err(|error| SyncError::io(on_device.as_str(), error))?
            .is_some();
        if present {
            return Err(SyncError::Blocked(SyncBlocker::ForeignPioneerExport));
        }
    }
    Ok(())
}
