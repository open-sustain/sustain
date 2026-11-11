// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

/// The version of the analyzer behind each analysis capability. An
/// attempt stamps each capability it covers with that capability's
/// version, and a capability last attempted by an older version needs
/// analysis again — so a change to one analyzer measures only its own
/// capability again.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AnalyzerVersions {
    /// Tempo and beat grid.
    pub bpm: u32,
    /// Musical key.
    pub key: u32,
    /// The full-decode pass: waveforms and acoustic features.
    pub audio: u32,
}
