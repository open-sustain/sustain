// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    path::Path,
    sync::{Mutex, MutexGuard},
};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter, types::Value as SqlValue};
use sustain_domain::SmartPlaylistRuleSet;
pub use sustain_domain::{
    AcousticFeatures, LibraryQuery, Playlist, PlaylistFolder, PlaylistFolderId, PlaylistId,
    PlaylistItem, Rating, SmartPlaylist, SmartPlaylistId, SyncDevice, SyncDeviceId,
    SyncManifestEntry, SyncedLyrics, Track, TrackAnalysis, TrackColumnEntry, TrackColumnLayout,
    TrackColumnLayoutScope, TrackId, WaveformSegments,
};

mod memory;
mod query;
mod schema;
mod sqlite;
mod sqlite_rows;

pub use memory::InMemoryLibraryStore;

use query::{sort_tracks, track_matches_search};
use schema::{
    DELETE_SMART_SHUFFLE_INDEX_SQL, DELETE_TRACK_SYNCED_LYRICS_SQL, FILL_TRACK_BPM_IF_NULL_SQL,
    FILL_TRACK_MUSICAL_KEY_IF_NULL_SQL, SAVE_TRACK_SQL, SCHEMA_SQL, SELECT_ALL_TRACK_ACOUSTICS_SQL,
    SELECT_ALL_TRACKS_SQL, SELECT_SMART_SHUFFLE_INDEX_SQL, SELECT_TRACK_BY_ID_SQL,
    SELECT_TRACK_SYNCED_LYRICS_SQL, SELECT_TRACK_WAVEFORM_SQL, SELECT_TRACKS_NEEDING_ANALYSIS_SQL,
    SELECT_TRACKS_NEEDING_ONLINE_SQL, UPSERT_SMART_SHUFFLE_INDEX_SQL, UPSERT_TRACK_ACOUSTICS_SQL,
    UPSERT_TRACK_ANALYSIS_SQL, UPSERT_TRACK_ONLINE_STATUS_SQL, UPSERT_TRACK_SYNCED_LYRICS_SQL,
    UPSERT_TRACK_WAVEFORM_SQL,
};
use sqlite_rows::{
    blob_to_waveform_segments, build_limit, duration_to_seconds, limit_selection_name,
    load_smart_playlist_rules, match_kind_from_name, match_kind_name, optional_i64,
    optional_playlist_folder_id_from_row, optional_string, playlist_entries,
    playlist_folder_from_row, playlist_id_from_db, rule_to_columns, smart_playlist_id_from_db,
    system_time_to_unix, track_from_row, u32_from_row, waveform_segments_to_blob,
};

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreError {
    Database(String),
    InvalidStoredId(i64),
    InvalidStoredPath(String),
    InvalidStoredEnum(String),
    StoreUnavailable,
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

/// Which DSP passes a write or query applies to.
///
/// On the write path (`record_analysis`, `record_analysis_attempt_failure`)
/// only the requested capabilities get their `*_attempted_at_unix`
/// timestamps stamped; the others preserve whatever value was already
/// stored. On the read path (`tracks_needing_analysis`) a track
/// qualifies as needing analysis if **any** of the requested
/// capabilities is either un-attempted (NULL) or stamped by an older
/// `analyzer_version`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnalysisCapabilities {
    pub bpm: bool,
    pub key: bool,
    /// The single heavy full-decode pass. One decode produces both the
    /// waveform overview and the perceptual acoustic features (loudness,
    /// onset density, timbre) Smart Shuffle consumes; the waveform and the
    /// acoustics are byproducts of the same work, so they share one
    /// attempt timestamp and one opt-in toggle.
    pub audio: bool,
}

impl AnalysisCapabilities {
    /// No capabilities requested. Useful as a sentinel; passing this
    /// to a write call is a no-op.
    pub const fn none() -> Self {
        Self {
            bpm: false,
            key: false,
            audio: false,
        }
    }

    /// All capabilities requested.
    pub const fn all() -> Self {
        Self {
            bpm: true,
            key: true,
            audio: true,
        }
    }

    pub const fn is_empty(self) -> bool {
        !(self.bpm || self.key || self.audio)
    }
}

/// Which network-bound retrievals a write or query applies to.
///
/// Symmetric counterpart of [`AnalysisCapabilities`] for online work
/// (artwork, tag enrichment, lyrics). Tag-fetch is included in the
/// flag set but is not yet wired through the runtime — the
/// scheduler ignores it today and the field exists so the storage
/// layer's row shape does not have to change when tag-fetch lands.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OnlineCapabilities {
    pub artwork: bool,
    pub tags: bool,
    pub lyrics: bool,
}

impl OnlineCapabilities {
    pub const fn none() -> Self {
        Self {
            artwork: false,
            tags: false,
            lyrics: false,
        }
    }

    pub const fn all() -> Self {
        Self {
            artwork: true,
            tags: true,
            lyrics: true,
        }
    }

    pub const fn is_empty(self) -> bool {
        !(self.artwork || self.tags || self.lyrics)
    }
}

/// Per-attempt facts stamped onto `track_online_status`. `provider_version`
/// is the watermark the scheduler compares against to invalidate stale
/// rows after a provider switch or significant client change; `now_unix`
/// is the wall clock at the moment the attempt completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OnlineContext {
    pub provider_version: u32,
    pub now_unix: i64,
}

/// Both waveform tiers as returned by [`LibraryStore::load_waveform`].
/// Cheap to clone (`Vec<u8>` of segment bytes, decoded once into
/// `WaveformSegment` on read). Renderers typically request this lazily
/// — only for the active track — so the BLOB pages are touched on
/// demand rather than during library load.
#[derive(Clone, Debug, PartialEq)]
pub struct StoredWaveform {
    pub preview: WaveformSegments,
    pub detail: WaveformSegments,
}

/// Per-attempt facts the storage layer stamps onto `track_analysis`
/// alongside the capability timestamps. `analyzer_version` is the
/// watermark the scheduler compares against to invalidate stale rows
/// after a DSP change; `now_unix` is the wall clock at the moment the
/// attempt completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AnalysisContext {
    pub analyzer_version: u32,
    pub now_unix: i64,
}

/// Synced lyrics as returned by [`LibraryStore::load_synced_lyrics`].
/// `source` is the short provider identifier under which the lines were
/// originally written (e.g. `"lrclib"`); a future diagnostic UI can
/// surface it without reaching back into logs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSyncedLyrics {
    pub lyrics: SyncedLyrics,
    pub source: String,
}

/// Persisted Smart Shuffle index — the background rebuild writes one
/// of these into the `smart_shuffle_index` table, and the runtime
/// reads it back at startup. There is no trained model; the blob holds
/// the prepared, library-dependent state (genre IDF and, later,
/// normalization statistics) defined by `sustain_smart_shuffle`.
/// `schema_version` is broken out of the blob so the runtime can
/// discard a stale-shaped blob without paying to deserialise it first.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredSmartShuffleIndex {
    pub index_blob: Vec<u8>,
    pub schema_version: u32,
}

pub trait LibraryStore: Send + Sync {
    fn save_track(&self, track: Track) -> StoreResult<()>;
    fn save_tracks(&self, tracks: &[Track]) -> StoreResult<()> {
        for track in tracks {
            self.save_track(track.clone())?;
        }
        Ok(())
    }
    fn delete_track(&self, track_id: TrackId) -> StoreResult<()>;
    fn track(&self, track_id: TrackId) -> StoreResult<Option<Track>>;
    fn tracks(&self) -> StoreResult<Vec<Track>>;

    /// Return the set of distinct, non-empty genre values currently
    /// stored on tracks in the library. Order is not specified; the
    /// caller normalizes for whatever comparison it needs. Used by
    /// the online tag-enrichment path to bias its genre choice
    /// toward genres the user already curates, avoiding genre
    /// sprawl (e.g. picking "House" over "Electronica" when the
    /// library already has House tracks). Default implementation
    /// scans `tracks()` for crates that don't have a cheaper
    /// projection.
    fn distinct_genres(&self) -> StoreResult<Vec<String>> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for track in self.tracks()? {
            if let Some(genre) = track.metadata.genre
                && !genre.trim().is_empty()
            {
                seen.insert(genre);
            }
        }
        Ok(seen.into_iter().collect())
    }
    fn save_playlist(&self, playlist: Playlist) -> StoreResult<()>;
    fn playlist(&self, playlist_id: PlaylistId) -> StoreResult<Option<Playlist>>;
    fn playlists(&self) -> StoreResult<Vec<Playlist>>;
    fn delete_playlist(&self, playlist_id: PlaylistId) -> StoreResult<()>;
    fn save_playlist_folder(&self, folder: PlaylistFolder) -> StoreResult<()>;
    fn playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<Option<PlaylistFolder>>;
    fn playlist_folders(&self) -> StoreResult<Vec<PlaylistFolder>>;
    fn delete_playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<()>;
    fn save_smart_playlist(&self, smart_playlist: SmartPlaylist) -> StoreResult<()>;
    fn smart_playlist(
        &self,
        smart_playlist_id: SmartPlaylistId,
    ) -> StoreResult<Option<SmartPlaylist>>;
    fn smart_playlists(&self) -> StoreResult<Vec<SmartPlaylist>>;
    fn delete_smart_playlist(&self, smart_playlist_id: SmartPlaylistId) -> StoreResult<()>;
    fn load_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
    ) -> StoreResult<Option<TrackColumnLayout>>;
    fn save_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
        layout: &TrackColumnLayout,
    ) -> StoreResult<()>;
    fn delete_track_column_layout(&self, scope: TrackColumnLayoutScope) -> StoreResult<()>;

    /// Persist a successful analysis pass. Stamps each requested
    /// capability's `*_attempted_at_unix` timestamp regardless of
    /// whether the corresponding field in `analysis` is `Some`
    /// (a successful run that produced no result is still an
    /// attempt, and we want the scheduler to stop retrying it).
    ///
    /// Writes the waveform BLOBs only when the waveform capability
    /// was requested and `analysis.waveform_detail` is non-empty.
    /// Updates `tracks.bpm` and `tracks.musical_key` **only when
    /// those columns are currently NULL** — file-tag values from
    /// import and explicit user edits win, the analyzer fills in
    /// missing data rather than overriding existing data.
    fn record_analysis(
        &self,
        track_id: TrackId,
        analysis: &TrackAnalysis,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()>;

    /// Record a failed analysis attempt. Stamps the requested
    /// capabilities' `*_attempted_at_unix` so the scheduler does not
    /// keep retrying every cycle; does not touch
    /// `tracks.bpm`/`musical_key` or the waveform table.
    fn record_analysis_attempt_failure(
        &self,
        track_id: TrackId,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()>;

    /// Return up to `limit` track IDs that need at least one of the
    /// requested capabilities re-run. Excludes tracks marked missing.
    /// Returns an empty list when `capabilities.is_empty()`.
    fn tracks_needing_analysis(
        &self,
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>>;

    /// Filter `track_ids` down to the subset that still needs at
    /// least one of the requested capabilities run, preserving input
    /// order. Same predicate as [`Self::tracks_needing_analysis`]
    /// (also excludes tracks marked missing). Used by the per-set
    /// explicit run path so re-analyzing a playlist whose tracks are
    /// already cached is a no-op.
    fn filter_tracks_needing_analysis(
        &self,
        track_ids: &[TrackId],
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
    ) -> StoreResult<Vec<TrackId>>;

    /// Load the stored waveform for a track, if any. Returns `None`
    /// when the track has not been waveform-analyzed yet (or analysis
    /// failed).
    fn load_waveform(&self, track_id: TrackId) -> StoreResult<Option<StoredWaveform>>;

    /// Load every track's acoustic features in one sweep, for the
    /// Smart Shuffle index rebuild. Tracks without an acoustics row
    /// are simply absent from the result — the scorer masks them.
    fn load_all_acoustics(&self) -> StoreResult<Vec<(TrackId, AcousticFeatures)>>;

    /// Persist time-coded lyrics for a track. Overwrites any previous
    /// entry — synced lyrics are a single-shot store-or-replace, not
    /// a merge. Passing an empty [`SyncedLyrics`] is a no-op and
    /// leaves any existing row untouched (call [`Self::clear_synced_lyrics`]
    /// to delete instead).
    fn record_synced_lyrics(
        &self,
        track_id: TrackId,
        lyrics: &SyncedLyrics,
        source: &str,
    ) -> StoreResult<()>;

    /// Load synced lyrics for a track, if any.
    fn load_synced_lyrics(&self, track_id: TrackId) -> StoreResult<Option<StoredSyncedLyrics>>;

    /// Drop any stored synced lyrics for the track. Used by the
    /// "remove lyrics" path (a future UI affordance) and by the
    /// online scheduler when a re-fetch returns nothing.
    fn clear_synced_lyrics(&self, track_id: TrackId) -> StoreResult<()>;

    /// Stamp an online retrieval attempt against the track. Each
    /// requested capability's `*_attempted_at_unix` is set to the
    /// supplied wall clock; capabilities that were not requested
    /// preserve their previous value. This is the only write the
    /// online scheduler makes regardless of whether the underlying
    /// provider returned data — "attempted but found nothing" must
    /// be recorded so the scheduler does not retry every cycle.
    fn record_online_attempt(
        &self,
        track_id: TrackId,
        capabilities: OnlineCapabilities,
        context: OnlineContext,
    ) -> StoreResult<()>;

    /// Return up to `limit` track IDs that need at least one of the
    /// requested online capabilities attempted. Excludes tracks
    /// marked missing. Returns an empty list when
    /// `capabilities.is_empty()`.
    fn tracks_needing_online(
        &self,
        capabilities: OnlineCapabilities,
        provider_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>>;

    /// Filter `track_ids` down to the subset that still needs at
    /// least one of the requested online capabilities attempted,
    /// preserving input order. Same predicate as
    /// [`Self::tracks_needing_online`] (the artwork branch still
    /// excludes tracks with embedded artwork).
    fn filter_tracks_needing_online(
        &self,
        track_ids: &[TrackId],
        capabilities: OnlineCapabilities,
        provider_version: u32,
    ) -> StoreResult<Vec<TrackId>>;

    /// Write (or overwrite) the singleton Smart Shuffle index. The
    /// blob format is opaque to the store; `sustain_smart_shuffle`
    /// decides its shape, and `schema_version` lets a future
    /// incompatible change cause the runtime to discard the stored row
    /// at load time without deserialising it.
    fn save_smart_shuffle_index(&self, index: &StoredSmartShuffleIndex) -> StoreResult<()>;

    /// Load the Smart Shuffle index, if one has been written. Returns
    /// `None` when the table is empty (the index has never been built
    /// yet).
    fn load_smart_shuffle_index(&self) -> StoreResult<Option<StoredSmartShuffleIndex>>;

    /// Drop any stored Smart Shuffle index. Used by the runtime when a
    /// load discovers a schema mismatch, so the next rebuild starts
    /// from a clean slate.
    fn clear_smart_shuffle_index(&self) -> StoreResult<()>;

    // --- Device sync (#23 / #24) ---

    /// Insert or update a device's saved configuration, keyed by its
    /// Sustain device id.
    fn save_sync_device(&self, device: &SyncDevice) -> StoreResult<()>;

    /// Load a device's saved configuration, if Sustain knows it.
    fn sync_device(&self, id: &SyncDeviceId) -> StoreResult<Option<SyncDevice>>;

    /// All devices Sustain has configuration for.
    fn sync_devices(&self) -> StoreResult<Vec<SyncDevice>>;

    /// Forget a device: drop its config, selection, and manifest.
    fn delete_sync_device(&self, id: &SyncDeviceId) -> StoreResult<()>;

    /// Replace a device's ticked-playlist selection (in display order).
    fn save_device_selection(
        &self,
        id: &SyncDeviceId,
        selection: &[PlaylistItem],
    ) -> StoreResult<()>;

    /// Load a device's ticked-playlist selection.
    fn device_selection(&self, id: &SyncDeviceId) -> StoreResult<Vec<PlaylistItem>>;

    /// Replace a device's on-device manifest (what was last written).
    fn save_device_manifest(
        &self,
        id: &SyncDeviceId,
        entries: &[SyncManifestEntry],
    ) -> StoreResult<()>;

    /// Load a device's on-device manifest.
    fn device_manifest(&self, id: &SyncDeviceId) -> StoreResult<Vec<SyncManifestEntry>>;

    fn tracks_matching(&self, query: LibraryQuery) -> StoreResult<Vec<Track>> {
        let mut tracks = if let Some(playlist_id) = query.playlist_id {
            let Some(playlist) = self.playlist(playlist_id)? else {
                return Ok(Vec::new());
            };
            let tracks_by_id = self
                .tracks()?
                .into_iter()
                .map(|track| (track.id, track))
                .collect::<BTreeMap<_, _>>();

            playlist
                .entries
                .into_iter()
                .filter_map(|entry| tracks_by_id.get(&entry.track_id).cloned())
                .collect()
        } else {
            self.tracks()?
        };

        if let Some(search_text) = query.search_text.as_deref() {
            tracks.retain(|track| track_matches_search(track, search_text));
        }

        sort_tracks(&mut tracks, query.sort, query.honor_sort_tags);
        Ok(tracks)
    }
}

#[derive(Debug)]
pub struct SqliteLibraryStore {
    connection: Mutex<Connection>,
    freshly_created: bool,
}

impl SqliteLibraryStore {
    pub fn open_default() -> StoreResult<Self> {
        Self::open(default_database_path().ok_or(StoreError::StoreUnavailable)?)
    }

    pub fn open(path: impl AsRef<Path>) -> StoreResult<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| StoreError::Database(error.to_string()))?;
        }
        let freshly_created = !path.exists();
        let connection = Connection::open(path).map_err(StoreError::from)?;
        Self::from_connection(connection, freshly_created)
    }

    pub fn open_in_memory() -> StoreResult<Self> {
        let connection = Connection::open_in_memory().map_err(StoreError::from)?;
        Self::from_connection(connection, true)
    }

    fn from_connection(connection: Connection, freshly_created: bool) -> StoreResult<Self> {
        // SQLite silently ignores WAL on `:memory:` databases, so tests
        // keep working without a special case here.
        connection
            .execute_batch(
                r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                "#,
            )
            .map_err(StoreError::from)?;
        let store = Self {
            connection: Mutex::new(connection),
            freshly_created,
        };
        store.migrate()?;
        Ok(store)
    }

    // True when this open() call brought the database file into
    // existence. Callers (typically the runtime at startup) use this to
    // decide whether to perform one-shot first-run setup such as seeding
    // the default smart playlists. Always true for in-memory stores.
    pub fn was_freshly_created(&self) -> bool {
        self.freshly_created
    }

    fn connection_guard(&self) -> StoreResult<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)
    }

    fn migrate(&self) -> StoreResult<()> {
        self.connection_guard()?
            .execute_batch(SCHEMA_SQL)
            .map_err(StoreError::from)
    }
}

/// Resolve the on-disk SQLite library database path Sustain reads from when
/// no explicit override is supplied. Returns `None` when neither
/// `XDG_DATA_HOME` nor `HOME` is set, in which case no default location can be
/// derived and the caller is expected to surface the failure (Sustain has no
/// reasonable fallback there).
///
/// Exposed publicly so callers that need the resolved path *before* opening
/// the store (e.g. the application-startup single-instance lock, which
/// flocks a sidecar in the same directory) can reuse the same resolution
/// rule rather than re-deriving it.
pub fn default_database_path() -> Option<std::path::PathBuf> {
    if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
        return Some(
            std::path::PathBuf::from(data_home)
                .join("sustain")
                .join("library.sqlite"),
        );
    }

    std::env::var_os("HOME").map(|home| {
        std::path::PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("sustain")
            .join("library.sqlite")
    })
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
