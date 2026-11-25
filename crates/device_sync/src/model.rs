// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Neutral inputs and outputs for the sync engine.
//!
//! The runtime resolves a device's ticked playlists (smart playlists
//! re-evaluated every sync) into a flat track set and hands the engine
//! these plain structs, so the engine never reaches into the library
//! database or the DSP pipeline.

use std::{ops::Deref, path::PathBuf, time::Duration};

use sustain_domain::{
    BeatGrid, Bpm, CatalogueRecord, DeviceLayout, DeviceRelativePath, MusicalKey, SourceFileStat,
    SourceFingerprint, SyncDevice, SyncManifestEntry, TrackContentHash, TrackId, WaveformSegments,
};
use sustain_ipod::{CoverId, CoverSet};
use sustain_pioneer::ArtworkSet;

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
    pub composer: Option<String>,
    pub comment: Option<String>,
    pub album_artist: Option<String>,
    pub compilation: bool,
    /// The "sort as" overrides of the title, artist, album, album artist
    /// and composer.
    pub sort_title: Option<String>,
    pub sort_artist: Option<String>,
    pub sort_album: Option<String>,
    pub sort_album_artist: Option<String>,
    pub sort_composer: Option<String>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_total: Option<u32>,
    pub year: Option<u32>,
    pub duration_ms: u32,
    /// 0 (unrated) through 5.
    pub rating: u8,
    pub bpm: Option<Bpm>,
    pub key: Option<MusicalKey>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: u32,
    /// Bits per sample. Lossy formats have none and carry 16, the value
    /// rekordbox writes for them.
    pub bit_depth: u16,
    pub source: SourceSnapshot,
    /// When the track entered the library, as seconds since the Unix
    /// epoch.
    pub date_added: Option<i64>,
    /// Lower-case file extension without the dot (e.g. `mp3`).
    pub extension: String,
}

/// One resolved playlist: a name and the indices (into the request's
/// track slice) of its tracks, in order.
#[derive(Clone, Debug)]
pub struct SyncInputPlaylist {
    pub name: String,
    pub track_indices: Vec<usize>,
}

/// A complete sync request.
///
/// The request is transport-agnostic: it says *what* to put on the device,
/// not *where* the device is. The caller opens a [`crate::DeviceTransport`]
/// and hands it to [`crate::plan`] / [`crate::sync`] alongside this.
#[derive(Clone, Debug)]
pub struct SyncRequest {
    pub device: SyncDevice,
    /// The resolved track set (deduplicated by track).
    pub tracks: Vec<SyncInputTrack>,
    /// The ticked playlists, referencing `tracks` by index.
    pub playlists: Vec<SyncInputPlaylist>,
    /// What Sustain last wrote to this device.
    pub previous_manifest: Vec<SyncManifestEntry>,
    /// The playlist files the previous syncs wrote and left on the device,
    /// relative to the sync base like the manifest's paths. A recorded
    /// file no playlist of this sync writes is stale; no other file is.
    pub previous_playlist_files: Vec<DeviceRelativePath>,
    /// The catalogue databases and artwork the previous syncs left on the
    /// device: a Pioneer export's or an iPod library's. Empty for the
    /// other layouts.
    pub previous_catalogue: CatalogueRecord,
    /// Delete on-device files no longer in the selection.
    pub remove_stale: bool,
    /// `YYYY-MM-DD` stamped into the Pioneer analyze-date field (see
    /// [`calendar_date`]).
    pub export_date: String,
}

/// What Sustain measured about one track, for its Pioneer analysis files.
/// Times are on Sustain's gapless timeline; `timeline_delay` moves them
/// onto the player's.
#[derive(Clone, Debug, Default)]
pub struct PioneerTrackAnalysis {
    pub waveform_preview: Option<WaveformSegments>,
    pub waveform_detail: Option<WaveformSegments>,
    pub beat_grid: Option<BeatGrid>,
    /// How far the player's timeline runs behind Sustain's for this file
    /// (`sustain_pioneer::timeline::device_delay`).
    pub timeline_delay: Duration,
}

/// Worker-hydrated assets for one Pioneer track.
pub struct PreparedPioneerTrackAssets {
    pub(crate) analysis: PioneerTrackAnalysis,
    pub(crate) artwork_id: u32,
}

/// Worker-hydrated assets for one Pioneer export.
///
/// Source cover bytes are processed and discarded as each track is hydrated.
/// [`ArtworkSet`] retains only one pair of rendered thumbnails per distinct
/// cover, while each track retains its compact PDB artwork id.
#[derive(Default)]
pub struct PreparedPioneerAssets {
    pub(crate) tracks: Vec<PreparedPioneerTrackAssets>,
    pub(crate) artwork: ArtworkSet,
}

impl PreparedPioneerAssets {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_track(&mut self, analysis: PioneerTrackAnalysis, cover_art: Option<&[u8]>) {
        let artwork_id = cover_art
            .and_then(|bytes| self.artwork.add(bytes).ok())
            .unwrap_or(0);
        self.tracks.push(PreparedPioneerTrackAssets {
            analysis,
            artwork_id,
        });
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    pub fn artwork_count(&self) -> usize {
        self.artwork.len()
    }
}

/// A Unix timestamp (seconds) as the `YYYY-MM-DD` the Pioneer database
/// stamps, in UTC, without a date-library dependency (Howard Hinnant's civil-from-days algorithm).
pub fn calendar_date(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

/// Worker-hydrated covers for one iPod library.
///
/// Source cover bytes are decoded and discarded as each track is hydrated:
/// [`CoverSet`] keeps one compact prepared image per distinct cover, and
/// each track keeps the id of its own.
pub struct PreparedIpodCovers {
    pub(crate) covers: CoverSet,
    pub(crate) tracks: Vec<Option<CoverId>>,
}

impl PreparedIpodCovers {
    pub fn new() -> Self {
        Self {
            covers: CoverSet::new(sustain_ipod::LARGEST_COVER_EDGE),
            tracks: Vec::new(),
        }
    }

    pub fn push_track(&mut self, cover_art: Option<&[u8]>) {
        let cover = cover_art.and_then(|bytes| self.covers.add(bytes));
        self.tracks.push(cover);
    }

    pub fn track_count(&self) -> usize {
        self.tracks.len()
    }

    pub fn cover_count(&self) -> usize {
        self.covers.len()
    }
}

impl Default for PreparedIpodCovers {
    fn default() -> Self {
        Self::new()
    }
}

/// What a layout needs beyond the request itself, hydrated on the worker
/// before the device is touched: a Pioneer export's analysis and artwork,
/// an iPod library's covers.
pub enum PreparedAssets {
    /// The layout needs nothing more.
    None,
    Pioneer(PreparedPioneerAssets),
    Ipod(PreparedIpodCovers),
}

impl PreparedAssets {
    /// What `layout` will be given: the assets it needs, empty.
    pub fn for_layout(layout: DeviceLayout) -> Self {
        match layout {
            DeviceLayout::Pioneer => Self::Pioneer(PreparedPioneerAssets::new()),
            DeviceLayout::Ipod => Self::Ipod(PreparedIpodCovers::new()),
            DeviceLayout::M3u | DeviceLayout::FolderPerPlaylist => Self::None,
        }
    }
}

/// A sync request whose every source observation carries a SHA-256. Only this
/// type can reach the mutating engine, preventing provisional planning tokens
/// from ever being persisted into a device manifest. Pioneer and iPod
/// requests also carry worker-hydrated assets so database/audio parsing is
/// complete before removable-media mutation begins.
pub struct PreparedSyncRequest {
    request: SyncRequest,
    assets: PreparedAssets,
}

impl PreparedSyncRequest {
    pub fn new(request: SyncRequest, assets: PreparedAssets) -> Result<Self, SyncError> {
        if !request
            .tracks
            .iter()
            .all(|track| track.source.content_hash.is_some())
        {
            return Err(SyncError::Preparation(
                "one or more source fingerprints are unresolved".to_owned(),
            ));
        }
        let tracks = request.tracks.len();
        let fits = match (request.device.layout, &assets) {
            (DeviceLayout::Pioneer, PreparedAssets::Pioneer(assets)) => {
                assets.tracks.len() == tracks
            }
            (DeviceLayout::Ipod, PreparedAssets::Ipod(covers)) => covers.tracks.len() == tracks,
            (DeviceLayout::M3u | DeviceLayout::FolderPerPlaylist, PreparedAssets::None) => true,
            _ => false,
        };
        if !fits {
            return Err(SyncError::Preparation(
                "the prepared assets do not match the layout or the resolved track set".to_owned(),
            ));
        }
        Ok(Self { request, assets })
    }

    pub(crate) fn pioneer_assets(&self) -> Result<&PreparedPioneerAssets, SyncError> {
        match &self.assets {
            PreparedAssets::Pioneer(assets) => Ok(assets),
            PreparedAssets::None | PreparedAssets::Ipod(_) => Err(SyncError::Preparation(
                "Pioneer assets were not prepared".to_owned(),
            )),
        }
    }

    pub(crate) fn ipod_covers(&self) -> Result<&PreparedIpodCovers, SyncError> {
        match &self.assets {
            PreparedAssets::Ipod(covers) => Ok(covers),
            PreparedAssets::None | PreparedAssets::Pioneer(_) => Err(SyncError::Preparation(
                "iPod covers were not prepared".to_owned(),
            )),
        }
    }
}

impl Deref for PreparedSyncRequest {
    type Target = SyncRequest;

    fn deref(&self) -> &Self::Target {
        &self.request
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
    /// removal): audio files, then playlist files no playlist of the
    /// selection writes any more. Paths are relative to the sync base.
    pub to_remove: Vec<DeviceRelativePath>,
    /// Files already present and current.
    pub unchanged: usize,
    /// Total bytes the copy/update step will transfer. For an iPod, also
    /// an estimate of the space its covers need beyond what its current
    /// cover files hold.
    pub bytes_to_copy: u64,
    /// Total bytes the selection occupies on the device once synced — the
    /// sum over every placement, whether already present or not (so it
    /// reflects the layout's deduplication). Drives the occupation bar.
    pub bytes_total: u64,
    /// The same `bytes_total` footprint broken down per genre, ordered
    /// largest first (ties broken by genre name for determinism). Sums to
    /// `bytes_total`. Drives the occupation bar's per-genre stacking.
    pub genre_bytes: Vec<GenreBytes>,
    /// What keeps the device from syncing at all, if anything.
    pub blocker: Option<SyncBlocker>,
}

/// A state of the device a sync refuses to change, until the user
/// deliberately resolves it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncBlocker {
    /// The drive holds a Pioneer export Sustain did not write, or one
    /// changed since Sustain wrote it — by rekordbox, or by a player that
    /// recorded history on it. Writing Sustain's export would erase its
    /// catalog, history and artwork; the user must remove the drive's
    /// `PIONEER/rekordbox` and `PIONEER/Artwork` folders first. The rest
    /// of `PIONEER`, the player's settings and profile, is not in the way.
    ForeignPioneerExport,
    /// The iPod is formatted for a Mac (HFS+), which Linux cannot safely
    /// write. Restoring it with iTunes for Windows formats it FAT32.
    MacFormattedIpod,
    /// The device is mounted read-only.
    ReadOnlyDevice,
    /// The iPod's model is unknown, or one Sustain does not support.
    UnsupportedIpod,
    /// The iPod's library must be signed with its serial number, and the
    /// serial number could not be read.
    UnidentifiedIpod,
    /// The iPod holds a library Sustain did not write — iTunes' or
    /// Finder's. Writing Sustain's would orphan its music; the user must
    /// restore the iPod first, after copying off anything to keep.
    ForeignIpodLibrary,
}

impl std::fmt::Display for SyncBlocker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ForeignPioneerExport => {
                write!(
                    f,
                    "the drive holds a Pioneer export Sustain did not write; \
                     remove its PIONEER/rekordbox and PIONEER/Artwork folders to sync"
                )
            }
            Self::MacFormattedIpod => write!(
                f,
                "the iPod is formatted for a Mac; restore it with iTunes for Windows to sync"
            ),
            Self::ReadOnlyDevice => write!(f, "the device is mounted read-only"),
            Self::UnsupportedIpod => write!(f, "this iPod model is not supported"),
            Self::UnidentifiedIpod => write!(
                f,
                "the iPod's serial number, needed to sign its library, could not be read"
            ),
            Self::ForeignIpodLibrary => write!(
                f,
                "the iPod holds a library Sustain did not write; restore the iPod to sync"
            ),
        }
    }
}

/// Stage the engine is in, for progress reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyncStage {
    Preparing,
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
    /// Per-track copy failures that did not abort the whole sync.
    pub copy_failures: Vec<SyncTrackFailure>,
    /// Selected tracks left off the device because its player cannot
    /// play their format.
    pub unplayable: usize,
    /// The new manifest to persist.
    pub manifest: Vec<SyncManifestEntry>,
    /// The playlist files the device holds from Sustain, to persist.
    pub playlist_files: Vec<DeviceRelativePath>,
    /// The catalogue databases and artwork the device holds from Sustain,
    /// to persist.
    pub catalogue: CatalogueRecord,
    /// True if the run stopped early because cancellation was requested.
    pub cancelled: bool,
}

/// One recoverable track copy failure.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncTrackFailure {
    pub track_id: TrackId,
    pub source_path: PathBuf,
    pub on_device_path: DeviceRelativePath,
    pub message: String,
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
    /// The iPod catalogue could not be assembled.
    Ipod(sustain_ipod::WriteError),
    /// An iPod cover file could not be rendered.
    IpodCover(sustain_ipod::RenderError),
    /// A change to the device could not be recorded in the journal.
    Journal(crate::journal::JournalError),
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
    /// The device is in a state the sync refuses to change.
    Blocked(SyncBlocker),
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
            Self::Ipod(error) => write!(f, "iPod library: {error}"),
            Self::IpodCover(error) => write!(f, "iPod covers: {error}"),
            Self::Journal(error) => write!(f, "could not record the sync's progress: {error}"),
            Self::Preparation(message) => write!(f, "source preparation failed: {message}"),
            Self::Empty => write!(f, "the selection contains no tracks"),
            Self::Planning(message) => write!(f, "device path planning failed: {message}"),
            Self::Blocked(blocker) => write!(f, "{blocker}"),
        }
    }
}

impl std::error::Error for SyncError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Pdb(error) => Some(error),
            Self::Ipod(error) => Some(error),
            Self::IpodCover(error) => Some(error),
            Self::Journal(error) => Some(error.as_ref()),
            Self::Empty | Self::Planning(_) | Self::Preparation(_) | Self::Blocked(_) => None,
        }
    }
}

/// A sync that stopped on an error, and what the device holds after it.
#[derive(Debug)]
pub struct SyncFailure {
    pub error: SyncError,
    /// The manifest to persist: the previous one when the run stopped
    /// before touching the device, otherwise the files it kept or wrote
    /// plus every previous file it left alone.
    pub manifest: Vec<SyncManifestEntry>,
    /// The playlist files the device holds from Sustain, to persist: the
    /// previous record, less those the run removed, plus those it wrote.
    pub playlist_files: Vec<DeviceRelativePath>,
    /// The catalogue databases and artwork the device holds from Sustain,
    /// to persist, kept current as the run changed them.
    pub catalogue: CatalogueRecord,
}

impl std::fmt::Display for SyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for SyncFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

#[cfg(test)]
mod tests {
    use super::calendar_date;

    #[test]
    fn formats_known_dates() {
        assert_eq!(calendar_date(0), "1970-01-01");
        assert_eq!(calendar_date(1_700_000_000), "2023-11-14");
        assert_eq!(calendar_date(1_780_012_800), "2026-05-29");
    }
}
