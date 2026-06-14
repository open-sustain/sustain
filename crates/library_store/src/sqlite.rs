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
mod duplicate_consolidation;
mod online;
mod playlists;
mod smart_playlists;
mod smart_shuffle;
mod source_fingerprints;
mod synced_lyrics;
mod tag_mirror;
mod tracks;

/// Chunk size for `IN (...)` filters that are split to stay under SQLite's
/// bound-parameter limit. Shared by the analysis and online filter builders.
const FILTER_IN_LIST_CHUNK_SIZE: usize = 500;

fn metadata_text_param(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.trim().is_empty())
}

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

    fn update_track_availability_if_path_matches(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
    ) -> StoreResult<bool> {
        let connection = self.connection_guard()?;
        tracks::update_track_availability_if_path_matches(&connection, track_id, location)
    }

    fn update_track_locations(&self, updates: &[(TrackId, TrackLocation)]) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tracks::update_track_locations(&mut connection, updates)
    }

    fn relocate_track_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
        file_size_bytes: u64,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tag_mirror::relocate_track_and_enqueue(&mut connection, track_id, location, file_size_bytes)
    }

    fn replace_track_audio(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
        audio_properties: TrackAudioProperties,
        file_size_bytes: u64,
        has_embedded_artwork: bool,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tracks::replace_audio(
            &mut connection,
            track_id,
            location,
            audio_properties,
            file_size_bytes,
            has_embedded_artwork,
        )
    }

    fn update_track_rating(&self, track_id: TrackId, rating: Rating) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::update_track_rating(&connection, track_id, rating)
    }

    fn update_track_rating_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        rating: Rating,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tag_mirror::update_track_rating_and_enqueue(&mut connection, track_id, rating)
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

    fn apply_track_metadata_change_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tag_mirror::apply_track_metadata_change_and_enqueue(&mut connection, track_id, change)
    }

    fn fill_missing_track_metadata(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tracks::fill_missing_track_metadata(&connection, track_id, change)
    }

    fn fill_missing_track_metadata_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<bool> {
        let mut connection = self.connection_guard()?;
        tag_mirror::fill_missing_track_metadata_and_enqueue(&mut connection, track_id, change)
    }

    fn apply_track_metadata_change_and_location_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
        location: &TrackLocation,
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        tag_mirror::apply_track_metadata_change_and_location_and_enqueue(
            &mut connection,
            track_id,
            change,
            location,
        )
    }

    fn commit_duplicate_consolidation(&self, plan: &DuplicateConsolidationPlan) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        duplicate_consolidation::commit(&mut connection, plan)
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

    fn flush_durable(&self) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        // In WAL mode with synchronous=NORMAL, ordinary commits deliberately
        // avoid fsync; a completed checkpoint is the durability boundary.
        // FULL waits for readers/writers and the returned frame counts prove
        // that the entire WAL was checkpointed before an external recovery
        // journal may be removed.
        let (busy, log_frames, checkpointed_frames) = connection
            .query_row("PRAGMA wal_checkpoint(FULL)", [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(StoreError::from)?;
        if busy == 0 && log_frames == checkpointed_frames {
            Ok(())
        } else {
            Err(StoreError::Database(format!(
                "SQLite WAL durability checkpoint incomplete: busy={busy}, log={log_frames}, checkpointed={checkpointed_frames}"
            )))
        }
    }

    fn publish_tag_mirror_artwork(&self, bytes: &[u8]) -> StoreResult<StoredTagMirrorArtwork> {
        self.tag_mirror_blobs.publish(bytes)
    }

    fn enqueue_tag_mirror_artwork(
        &self,
        track_id: TrackId,
        artwork: TagMirrorArtwork,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        tag_mirror::enqueue_artwork(&connection, track_id, artwork)
    }

    fn tag_mirrors_due(&self, now_unix: i64, limit: usize) -> StoreResult<Vec<PendingTagMirror>> {
        let connection = self.connection_guard()?;
        tag_mirror::due(&connection, now_unix, limit)
    }

    fn next_tag_mirror_attempt_at(&self) -> StoreResult<Option<i64>> {
        let connection = self.connection_guard()?;
        tag_mirror::next_attempt_at(&connection)
    }

    fn complete_tag_mirror(&self, track_id: TrackId, generation: u64) -> StoreResult<bool> {
        let connection = self.connection_guard()?;
        tag_mirror::complete(&connection, track_id, generation)
    }

    fn record_tag_mirror_failure(
        &self,
        track_id: TrackId,
        generation: u64,
        next_attempt_at_unix: i64,
        error: &str,
    ) -> StoreResult<bool> {
        let connection = self.connection_guard()?;
        tag_mirror::record_failure(
            &connection,
            track_id,
            generation,
            next_attempt_at_unix,
            error,
        )
    }

    fn read_tag_mirror_artwork(&self, artwork: &StoredTagMirrorArtwork) -> StoreResult<Vec<u8>> {
        self.tag_mirror_blobs.read(artwork)
    }

    fn garbage_collect_tag_mirror_artwork(&self) -> StoreResult<()> {
        let snapshot = std::time::SystemTime::now();
        let referenced = {
            let connection = self.connection_guard()?;
            tag_mirror::referenced_artwork(&connection)?
        };
        self.tag_mirror_blobs.garbage_collect(&referenced, snapshot)
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
        let mut connection = self.connection_guard()?;
        column_layouts::delete_track_column_layout(&mut connection, scope)
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

    fn source_fingerprint(&self, track_id: TrackId) -> StoreResult<Option<SourceFingerprint>> {
        let connection = self.connection_guard()?;
        source_fingerprints::source_fingerprint(&connection, track_id)
    }

    fn save_source_fingerprint(
        &self,
        track_id: TrackId,
        fingerprint: &SourceFingerprint,
    ) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        source_fingerprints::save_source_fingerprint(&connection, track_id, fingerprint)
    }

    fn invalidate_source_fingerprint(&self, track_id: TrackId) -> StoreResult<()> {
        let connection = self.connection_guard()?;
        source_fingerprints::invalidate_source_fingerprint(&connection, track_id)
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

    fn save_device_artist_selection(
        &self,
        id: &SyncDeviceId,
        artists: &[String],
    ) -> StoreResult<()> {
        let mut connection = self.connection_guard()?;
        devices::save_device_artist_selection(&mut connection, id, artists)
    }

    fn device_artist_selection(&self, id: &SyncDeviceId) -> StoreResult<Vec<String>> {
        let connection = self.connection_guard()?;
        devices::device_artist_selection(&connection, id)
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
