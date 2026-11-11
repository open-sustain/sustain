// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! What a sync run changes on the device, recorded file by file.
//!
//! A run reports the manifest of what the device holds when it ends, but
//! a run that never ends — the process killed, the machine powered off
//! part-way through a long export — reports nothing. The journal is told
//! of each change as it is made, so the next run knows which files the
//! interrupted one already wrote and does not copy them again.

use std::error::Error;

use sustain_domain::{DeviceRelativePath, SyncManifestEntry};

/// Why the journal could not record a change.
pub type JournalError = Box<dyn Error + Send + Sync>;

/// Where a sync run records each change it makes to what the device
/// holds. Applied in order to the manifest the run started from, the
/// changes give what the device holds at that point of the run.
pub trait ManifestJournal {
    /// The device now holds `entry`'s file at its path.
    fn record(&mut self, entry: &SyncManifestEntry) -> Result<(), JournalError>;

    /// The device no longer holds a file the manifest can vouch for at
    /// `path`, spelled exactly so.
    fn forget(&mut self, path: &DeviceRelativePath) -> Result<(), JournalError>;
}
