// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Neutral inputs and outputs for the sync engine.
//!
//! The runtime resolves a device's ticked playlists (smart playlists
//! re-evaluated every sync) into a flat track set and hands the engine
//! these plain structs, so the engine never reaches into the library
//! database or the DSP pipeline.

use std::{ops::Deref, path::PathBuf};

use sustain_domain::{
    DeviceRelativePath, MusicalKey, SourceFileStat, SourceFingerprint, SyncDevice,
    SyncManifestEntry, TrackContentHash, TrackId, WaveformSegments,
};

/// Capacity of the filesystem behind an opened device mount root.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceCapacity {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

/// One observation of a source audio file. A matching cached SHA-256 is
/// optional during non-mutating planning; sync preparation resolves every
/// missing hash on its worker before any destination write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceSnapshot {
    pub stat: SourceFileStat,
    pub content_hash: Option<TrackContentHash>,
}

impl SourceSnapshot {
    pub const fn provisional(stat: SourceFileStat) -> Self {
        Self {
            stat,
            content_hash: None,
        }
    }

    pub fn resolved(fingerprint: SourceFingerprint) -> Self {
        Self {
            stat: fingerprint.stat,
            content_hash: Some(fingerprint.content_hash),
        }
    }

    pub fn fingerprint_token(&self) -> String {
        match &self.content_hash {
            Some(hash) => format!("sha256:{}", hash.as_str()),
            None => format!(
                "stat:{}:{}:{}:{}:{}",
                self.stat.device,
                self.stat.inode,
                self.stat.size_bytes,
                self.stat.modified_at_ns,
                self.stat.changed_at_ns,
            ),
        }
    }
}

/// One track in the resolved set to sync. Carries everything the writers need
/// plus a live source observation for staleness detection.
#[derive(Clone, Debug)]
pub struct SyncInputTrack {
    pub track_id: TrackId,
    /// Absolute path to the source audio file in the library.
    pub source_path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub year: Option<u32>,
    pub duration_ms: u32,
    /// 0 (unrated) through 5.
    pub rating: u8,
    pub bpm: Option<f32>,
    pub key: Option<MusicalKey>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: u32,
    pub bit_depth: u16,
    pub source: SourceSnapshot,
    /// `YYYY-MM-DD` the track entered the library, for the Pioneer PDB.
    pub date_added: Option<String>,
    /// Lower-case file extension without the dot (e.g. `mp3`).
    pub extension: String,
    /// Preview waveform (Pioneer layout only). `None` when the track has
    /// not been waveform-analysed.
    pub waveform_preview: Option<WaveformSegments>,
    /// Detail waveform (Pioneer layout only).
    pub waveform_detail: Option<WaveformSegments>,
    /// Embedded cover-art bytes (Pioneer layout only), rendered into the
    /// drive's artwork thumbnails. `None` when the file has no cover or
    /// the layout does not use artwork.
    pub cover_art: Option<Vec<u8>>,
}

/// One resolved playlist: a name and the indices (into the request's
/// track slice) of its tracks, in order.
#[derive(Clone, Debug)]
pub struct SyncInputPlaylist {
    pub name: String,
    pub track_indices: Vec<usize>,
}

/// A complete sync request.
#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub device: SyncDevice,
    /// Mount point of the device (filesystem root we write under).
    pub mount_path: PathBuf,
    /// The resolved track set (deduplicated by track).
    pub tracks: Vec<SyncInputTrack>,
    /// The ticked playlists, referencing `tracks` by index.
    pub playlists: Vec<SyncInputPlaylist>,
    /// What Sustain last wrote to this device.
    pub previous_manifest: Vec<SyncManifestEntry>,
    /// Delete on-device files no longer in the selection.
    pub remove_stale: bool,
    /// `YYYY-MM-DD` stamped into the Pioneer analyze-date field.
    pub export_date: String,
}

/// A sync request whose every source observation carries a SHA-256. Only this
/// type can reach the mutating engine, preventing provisional planning tokens
/// from ever being persisted into a device manifest.
#[derive(Clone, Debug)]
pub struct PreparedSyncRequest(SyncRequest);

impl PreparedSyncRequest {
    pub fn new(request: SyncRequest) -> Result<Self, SyncError> {
        if request
            .tracks
            .iter()
            .all(|track| track.source.content_hash.is_some())
        {
            Ok(Self(request))
        } else {
            Err(SyncError::Preparation(
                "one or more source fingerprints are unresolved".to_owned(),
            ))
        }
    }
}

impl Deref for PreparedSyncRequest {
    type Target = SyncRequest;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A planned on-device file: which track, where it goes (relative to the
/// device root, forward-slash separated, no leading slash), and the
/// source fingerprint it should carry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    pub track_index: usize,
    pub rel_path: DeviceRelativePath,
    pub fingerprint: String,
}

/// One genre's share of the selection's on-device footprint. `genre` is
/// `None` for tracks with no (or blank) genre tag. Drives the occupation
/// bar's per-genre stacking.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct GenreBytes {
    pub genre: Option<String>,
    pub bytes: u64,
}

/// Summary of what a sync would do, for the confirmation step before any
/// destructive removal.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncPlan {
    /// New files to write.
    pub to_copy: usize,
    /// Existing files whose source changed and will be overwritten.
    pub to_update: usize,
    /// On-device files no longer in the selection (candidates for
    /// removal). Paths are relative to the device root.
    pub to_remove: Vec<DeviceRelativePath>,
    /// Files already present and current.
    pub unchanged: usize,
    /// Total bytes the copy/update step will transfer.
    pub bytes_to_copy: u64,
    /// Total bytes the selection occupies on the device once synced — the
    /// sum over every placement, whether already present or not (so it
    /// reflects the layout's deduplication). Drives the occupation bar.
    pub bytes_total: u64,
    /// The same `bytes_total` footprint broken down per genre, ordered
    /// largest first (ties broken by genre name for determinism). Sums to
    /// `bytes_total`. Drives the occupation bar's per-genre stacking.
    pub genre_bytes: Vec<GenreBytes>,
}

/// Stage the engine is in, for progress reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncStage {
    Copying,
    WritingPlaylists,
    WritingDatabase,
    Removing,
}

/// Progress tick from the engine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyncProgress {
    pub stage: SyncStage,
    pub completed: usize,
    pub total: usize,
}

/// Result of a completed sync.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncOutcome {
    pub copied: usize,
    pub updated: usize,
    pub removed: usize,
    pub unchanged: usize,
    /// The new manifest to persist.
    pub manifest: Vec<SyncManifestEntry>,
    /// True if the run stopped early because cancellation was requested.
    pub cancelled: bool,
}

/// Sync failure.
#[derive(Debug)]
pub enum SyncError {
    /// A filesystem operation failed.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// The Pioneer PDB could not be assembled.
    Pdb(sustain_pioneer::PdbError),
    /// Live source preparation failed before removable-media mutation.
    Preparation(String),
    /// The selection resolved to no tracks.
    Empty,
    /// A unique on-device destination path could not be planned (the
    /// allocator exhausted its disambiguation attempts, or the final
    /// placement set still held a duplicate path). Planning fails closed
    /// before any filesystem mutation rather than letting two tracks
    /// overwrite each other on the device.
    Planning(String),
}

impl SyncError {
    /// Build an [`SyncError::Io`] for a path. Public so the runtime's sync
    /// preparation can report a source-file read failure with its path.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub(crate) fn planning(message: impl Into<String>) -> Self {
        Self::Planning(message.into())
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Pdb(error) => write!(f, "Pioneer database: {error}"),
            Self::Preparation(message) => write!(f, "source preparation failed: {message}"),
            Self::Empty => write!(f, "the selection contains no tracks"),
            Self::Planning(message) => write!(f, "device path planning failed: {message}"),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Pdb(error) => Some(error),
            Self::Empty | Self::Planning(_) | Self::Preparation(_) => None,
        }
    }
}
