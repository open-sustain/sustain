// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The SQLite-backed [`LibraryStore`] implementation.
//!
//! The trait methods here are deliberately thin: each acquires the connection
//! guard and delegates to a per-table free function in the submodules below,
//! which hold the actual SQL. Splitting by table keeps each file focused while
//! the single trait impl (Rust permits only one) stays a readable index of the
//! whole store surface. The free functions take `&Connection` (or
//! `&mut Connection` when they open a transaction), so they are independent of
//! the guard/locking concern and easy to compose — `save_tracks` simply calls
//! `tracks::save_track` inside its transaction, for example.

use super::*;

mod analysis;
mod column_layouts;
mod devices;
mod online;
mod playlists;
mod smart_playlists;
mod smart_shuffle;
mod synced_lyrics;
mod tracks;

/// Chunk size for `IN (...)` filters that are split to stay under SQLite's
/// bound-parameter limit. Shared by the analysis and online filter builders.
const FILTER_IN_LIST_CHUNK_SIZE: usize = 500;

impl LibraryStore for SqliteLibraryStore {
    fn save_track(&self, track: Track) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::save_track(&connection, &track)
    }

    fn save_tracks(&self, tracks: &[Track]) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        self::tracks::save_tracks(&mut connection, tracks)
    }

    fn reconcile_scanned_tracks(&self, tracks: &[Track]) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        self::tracks::reconcile_scanned_tracks(&mut connection, tracks)
    }

    fn update_track_location(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::update_track_location(&connection, track_id, location)
    }

    fn update_track_locations(&self, updates: &[(TrackId, TrackLocation)]) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tracks::update_track_locations(&mut connection, updates)
    }

    fn update_track_rating(&self, track_id: TrackId, rating: Rating) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::update_track_rating(&connection, track_id, rating)
    }

    fn update_track_statistics(
        &self,
        track_id: TrackId,
        statistics: &PlayStatistics,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::update_track_statistics(&connection, track_id, statistics)
    }

    fn apply_track_metadata_change(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::apply_track_metadata_change(&connection, track_id, change)
    }

    fn fill_missing_track_metadata(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::fill_missing_track_metadata(&connection, track_id, change)
    }

    fn apply_track_metadata_change_and_location(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
        location: &TrackLocation,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tracks::apply_track_metadata_change_and_location(
            &mut connection,
            track_id,
            change,
            location,
        )
    }

    fn delete_track(&self, track_id: TrackId) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::delete_track(&connection, track_id)
    }

    fn track(&self, track_id: TrackId) -> StoreResult<Option<Track>> {
        let connection = self.connection_guard()?;
        tracks::track(&connection, track_id)
    }

    fn tracks(&self) -> StoreResult<Vec<Track>> {
        let connection = self.connection_guard()?;
        tracks::tracks(&connection)
    }

    fn distinct_genres(&self) -> StoreResult<Vec<String>> {
        let connection = self.connection_guard()?;
        tracks::distinct_genres(&connection)
    }

    fn save_playlist(&self, playlist: Playlist) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        playlists::save_playlist(&mut connection, playlist)
    }

    fn playlist(&self, playlist_id: PlaylistId) -> StoreResult<Option<Playlist>> {
        let connection = self.connection_guard()?;
        playlists::playlist(&connection, playlist_id)
    }

    fn playlists(&self) -> StoreResult<Vec<Playlist>> {
        let connection = self.connection_guard()?;
        playlists::playlists(&connection)
    }

    fn delete_playlist(&self, playlist_id: PlaylistId) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        playlists::delete_playlist(&connection, playlist_id)
    }

    fn save_playlist_folder(&self, folder: PlaylistFolder) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        playlists::save_playlist_folder(&connection, folder)
    }

    fn playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<Option<PlaylistFolder>> {
        let connection = self.connection_guard()?;
        playlists::playlist_folder(&connection, folder_id)
    }

    fn playlist_folders(&self) -> StoreResult<Vec<PlaylistFolder>> {
        let connection = self.connection_guard()?;
        playlists::playlist_folders(&connection)
    }

    fn delete_playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        playlists::delete_playlist_folder(&connection, folder_id)
    }

    fn save_smart_playlist(&self, smart_playlist: SmartPlaylist) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        smart_playlists::save_smart_playlist(&mut connection, smart_playlist)
    }

    fn smart_playlist(
        &self,
        smart_playlist_id: SmartPlaylistId,
    ) -> StoreResult<Option<SmartPlaylist>> {
        let connection = self.connection_guard()?;
        smart_playlists::smart_playlist(&connection, smart_playlist_id)
    }

    fn smart_playlists(&self) -> StoreResult<Vec<SmartPlaylist>> {
        let connection = self.connection_guard()?;
        smart_playlists::smart_playlists(&connection)
    }

    fn delete_smart_playlist(&self, smart_playlist_id: SmartPlaylistId) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        smart_playlists::delete_smart_playlist(&connection, smart_playlist_id)
    }

    fn load_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
    ) -> StoreResult<Option<TrackColumnLayout>> {
        let connection = self.connection_guard()?;
        column_layouts::load_track_column_layout(&connection, scope)
    }

    fn save_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
        layout: &TrackColumnLayout,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        column_layouts::save_track_column_layout(&mut connection, scope, layout)
    }

    fn delete_track_column_layout(&self, scope: TrackColumnLayoutScope) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        column_layouts::delete_track_column_layout(&connection, scope)
    }

    fn record_analysis(
        &self,
        track_id: TrackId,
        analysis: &TrackAnalysis,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        analysis::record_analysis(&mut connection, track_id, analysis, capabilities, context)
    }

    fn record_analysis_attempt_failure(
        &self,
        track_id: TrackId,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        analysis::record_analysis_attempt_failure(&connection, track_id, capabilities, context)
    }

    fn tracks_needing_analysis(
        &self,
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        let connection = self.connection_guard()?;
        analysis::tracks_needing_analysis(&connection, capabilities, analyzer_version, limit)
    }

    fn filter_tracks_needing_analysis(
        &self,
        track_ids: &[TrackId],
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        let connection = self.connection_guard()?;
        analysis::filter_tracks_needing_analysis(
            &connection,
            track_ids,
            capabilities,
            analyzer_version,
        )
    }

    fn load_waveform(&self, track_id: TrackId) -> StoreResult<Option<StoredWaveform>> {
        let connection = self.connection_guard()?;
        analysis::load_waveform(&connection, track_id)
    }

    fn load_all_acoustics(&self) -> StoreResult<Vec<(TrackId, AcousticFeatures)>> {
        let connection = self.connection_guard()?;
        analysis::load_all_acoustics(&connection)
    }

    fn record_synced_lyrics(
        &self,
        track_id: TrackId,
        lyrics: &SyncedLyrics,
        source: &str,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        synced_lyrics::record_synced_lyrics(&connection, track_id, lyrics, source)
    }

    fn load_synced_lyrics(&self, track_id: TrackId) -> StoreResult<Option<StoredSyncedLyrics>> {
        let connection = self.connection_guard()?;
        synced_lyrics::load_synced_lyrics(&connection, track_id)
    }

    fn clear_synced_lyrics(&self, track_id: TrackId) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        synced_lyrics::clear_synced_lyrics(&connection, track_id)
    }

    fn record_online_attempt(
        &self,
        track_id: TrackId,
        capabilities: OnlineCapabilities,
        context: OnlineContext,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        online::record_online_attempt(&connection, track_id, capabilities, context)
    }

    fn tracks_needing_online(
        &self,
        capabilities: OnlineCapabilities,
        provider_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        let connection = self.connection_guard()?;
        online::tracks_needing_online(&connection, capabilities, provider_version, limit)
    }

    fn filter_tracks_needing_online(
        &self,
        track_ids: &[TrackId],
        capabilities: OnlineCapabilities,
        provider_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        let connection = self.connection_guard()?;
        online::filter_tracks_needing_online(&connection, track_ids, capabilities, provider_version)
    }

    fn save_smart_shuffle_index(&self, index: &StoredSmartShuffleIndex) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        smart_shuffle::save_smart_shuffle_index(&connection, index)
    }

    fn load_smart_shuffle_index(&self) -> StoreResult<Option<StoredSmartShuffleIndex>> {
        let connection = self.connection_guard()?;
        smart_shuffle::load_smart_shuffle_index(&connection)
    }

    fn clear_smart_shuffle_index(&self) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        smart_shuffle::clear_smart_shuffle_index(&connection)
    }

    fn save_sync_device(&self, device: &SyncDevice) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        devices::save_sync_device(&connection, device)
    }

    fn sync_device(&self, id: &SyncDeviceId) -> StoreResult<Option<SyncDevice>> {
        let connection = self.connection_guard()?;
        devices::sync_device(&connection, id)
    }

    fn sync_devices(&self) -> StoreResult<Vec<SyncDevice>> {
        let connection = self.connection_guard()?;
        devices::sync_devices(&connection)
    }

    fn delete_sync_device(&self, id: &SyncDeviceId) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        devices::delete_sync_device(&connection, id)
    }

    fn save_device_selection(
        &self,
        id: &SyncDeviceId,
        selection: &[PlaylistItem],
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        devices::save_device_selection(&mut connection, id, selection)
    }

    fn device_selection(&self, id: &SyncDeviceId) -> StoreResult<Vec<PlaylistItem>> {
        let connection = self.connection_guard()?;
        devices::device_selection(&connection, id)
    }

    fn save_device_manifest(
        &self,
        id: &SyncDeviceId,
        entries: &[SyncManifestEntry],
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        devices::save_device_manifest(&mut connection, id, entries)
    }

    fn device_manifest(&self, id: &SyncDeviceId) -> StoreResult<Vec<SyncManifestEntry>> {
        let connection = self.connection_guard()?;
        devices::device_manifest(&connection, id)
    }
}
