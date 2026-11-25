// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! How a catalogue database Sustain wrote is recognised on a device.

use sha2::{Digest, Sha256};
use sustain_domain::CatalogueDatabase;

/// The exact database `bytes` make: what identifies the database a sync
/// left on a device — a Pioneer `export.pdb`, an iPod `iTunesDB`.
pub(crate) fn identity(bytes: &[u8]) -> CatalogueDatabase {
    CatalogueDatabase {
        size_bytes: u64::try_from(bytes.len()).expect("a byte length fits in u64"),
        sha256: Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    }
}
