// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Fault-injecting [`LibraryStore`] wrapper for scheduler tests.
//!
//! The analysis and online schedulers must treat a rejected SQLite
//! write as a real failure (issue #92): no false "completed", no
//! `track_updated` for a row that was not persisted, a visible
//! notification, and bounded retries rather than an expensive hot loop.
//! Exercising that needs a store that succeeds normally but can be told
//! to fail a specific write on demand.
//!
//! [`FaultyStore`] wraps a real backing store (the in-memory one in
//! tests) and delegates every method to it, overriding only the
//! persistence writes tests need to observe. Each override consults a
//! toggle the test flips and counts its invocations so a test can assert
//! the expected failure or ordering contract.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sustain_library_store::{
    AcousticFeatures, AnalysisCapabilities, AnalysisContext, DuplicateConsolidationPlan,
    LibraryStore, MetadataChange, OnlineCapabilities, OnlineContext, PendingTagMirror,
    PlayStatistics, Playlist, PlaylistFolder, PlaylistFolderId, PlaylistId, PlaylistItem, Rating,
    SmartPlaylist, SmartPlaylistId, SourceFingerprint, StoreError, StoreResult,
    StoredSmartShuffleIndex, StoredSyncedLyrics, StoredTagMirrorArtwork, StoredWaveform,
    SyncDevice, SyncDeviceId, SyncManifestEntry, SyncedLyrics, TagMirrorArtwork, Track,
    TrackAnalysis, TrackAudioProperties, TrackColumnLayout, TrackColumnLayoutScope, TrackId,
    TrackLocation,
};

/// A [`LibraryStore`] that delegates to an inner store but can be told
/// to reject the persistence writes the schedulers rely on, or to delay
/// the bulk reads the Smart Shuffle rebuild performs (so a test can
/// prove that work runs off the caller's thread — #93).
pub(crate) struct FaultyStore {
    inner: Arc<dyn LibraryStore>,
    fail_record_analysis: AtomicBool,
    fail_attempt_failure: AtomicBool,
    fail_online_attempt: AtomicBool,
    fail_device_manifest: AtomicBool,
    fail_save_tracks: AtomicBool,
    fail_flush_durable: AtomicBool,
    availability_path_replacement: Mutex<Option<TrackLocation>>,
    record_analysis_calls: AtomicU32,
    attempt_failure_calls: AtomicU32,
    online_attempt_calls: AtomicU32,
    device_manifest_calls: AtomicU32,
    save_tracks_calls: AtomicU32,
    flush_durable_calls: AtomicU32,
    operation_log: Mutex<Vec<&'static str>>,
    /// Artificial latency injected into `tracks` and `load_all_acoustics`,
    /// in milliseconds. Zero (the default) is a no-op.
    read_delay_millis: AtomicU64,
}

impl FaultyStore {
    pub(crate) fn new(inner: Arc<dyn LibraryStore>) -> Self {
        Self {
            inner,
            fail_record_analysis: AtomicBool::new(false),
            fail_attempt_failure: AtomicBool::new(false),
            fail_online_attempt: AtomicBool::new(false),
            fail_device_manifest: AtomicBool::new(false),
            fail_save_tracks: AtomicBool::new(false),
            fail_flush_durable: AtomicBool::new(false),
            availability_path_replacement: Mutex::new(None),
            record_analysis_calls: AtomicU32::new(0),
            attempt_failure_calls: AtomicU32::new(0),
            online_attempt_calls: AtomicU32::new(0),
            device_manifest_calls: AtomicU32::new(0),
            save_tracks_calls: AtomicU32::new(0),
            flush_durable_calls: AtomicU32::new(0),
            operation_log: Mutex::new(Vec::new()),
            read_delay_millis: AtomicU64::new(0),
        }
    }

    pub(crate) fn set_read_delay(&self, delay: Duration) {
        self.read_delay_millis
            .store(delay.as_millis() as u64, Ordering::SeqCst);
    }

    fn sleep_read_delay(&self) {
        let millis = self.read_delay_millis.load(Ordering::SeqCst);
        if millis > 0 {
            std::thread::sleep(Duration::from_millis(millis));
        }
    }

    pub(crate) fn set_fail_record_analysis(&self, on: bool) {
        self.fail_record_analysis.store(on, Ordering::SeqCst);
    }

    pub(crate) fn set_fail_attempt_failure(&self, on: bool) {
        self.fail_attempt_failure.store(on, Ordering::SeqCst);
    }

    pub(crate) fn set_fail_online_attempt(&self, on: bool) {
        self.fail_online_attempt.store(on, Ordering::SeqCst);
    }

    pub(crate) fn set_fail_device_manifest(&self, on: bool) {
        self.fail_device_manifest.store(on, Ordering::SeqCst);
    }

    pub(crate) fn set_fail_save_tracks(&self, on: bool) {
        self.fail_save_tracks.store(on, Ordering::SeqCst);
    }

    pub(crate) fn set_fail_flush_durable(&self, on: bool) {
        self.fail_flush_durable.store(on, Ordering::SeqCst);
    }

    pub(crate) fn replace_path_before_next_availability_update(&self, location: TrackLocation) {
        *self
            .availability_path_replacement
            .lock()
            .expect("availability-path replacement lock is available") = Some(location);
    }

    pub(crate) fn record_analysis_calls(&self) -> u32 {
        self.record_analysis_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn attempt_failure_calls(&self) -> u32 {
        self.attempt_failure_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn online_attempt_calls(&self) -> u32 {
        self.online_attempt_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn device_manifest_calls(&self) -> u32 {
        self.device_manifest_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn save_tracks_calls(&self) -> u32 {
        self.save_tracks_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn flush_durable_calls(&self) -> u32 {
        self.flush_durable_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn operation_log(&self) -> Vec<&'static str> {
        self.operation_log
            .lock()
            .expect("faulty-store operation log lock is available")
            .clone()
    }

    fn record_operation(&self, operation: &'static str) {
        self.operation_log
            .lock()
            .expect("faulty-store operation log lock is available")
            .push(operation);
    }
}

impl LibraryStore for FaultyStore {
    // --- Overridden persistence writes -----------------------------------

    fn record_analysis(
        &self,
        track_id: TrackId,
        analysis: &TrackAnalysis,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        self.record_analysis_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_record_analysis.load(Ordering::SeqCst) {
            return Err(StoreError::Database(
                "injected record_analysis failure".to_owned(),
            ));
        }
        self.inner
            .record_analysis(track_id, analysis, capabilities, context)
    }

    fn record_analysis_attempt_failure(
        &self,
        track_id: TrackId,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        self.attempt_failure_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_attempt_failure.load(Ordering::SeqCst) {
            return Err(StoreError::Database(
                "injected record_analysis_attempt_failure failure".to_owned(),
            ));
        }
        self.inner
            .record_analysis_attempt_failure(track_id, capabilities, context)
    }

    fn record_online_attempt(
        &self,
        track_id: TrackId,
        capabilities: OnlineCapabilities,
        context: OnlineContext,
    ) -> StoreResult<()> {
        self.online_attempt_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_online_attempt.load(Ordering::SeqCst) {
            return Err(StoreError::Database(
                "injected record_online_attempt failure".to_owned(),
            ));
        }
        self.inner
            .record_online_attempt(track_id, capabilities, context)
    }

    // --- Plain delegation ------------------------------------------------

    fn save_track(&self, track: Track) -> StoreResult<()> {
        self.inner.save_track(track)
    }

    fn save_tracks(&self, tracks: &[Track]) -> StoreResult<()> {
        self.record_operation("save_tracks");
        self.save_tracks_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_save_tracks.load(Ordering::SeqCst) {
            return Err(StoreError::Database(
                "injected save_tracks failure".to_owned(),
            ));
        }
        self.inner.save_tracks(tracks)
    }

    fn reconcile_scanned_tracks(&self, tracks: &[Track]) -> StoreResult<()> {
        self.inner.reconcile_scanned_tracks(tracks)
    }

    fn update_track_location(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
    ) -> StoreResult<()> {
        self.inner.update_track_location(track_id, location)
    }

    fn update_track_availability_if_path_matches(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
    ) -> StoreResult<bool> {
        let replacement = self
            .availability_path_replacement
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .take();
        if let Some(replacement) = replacement {
            self.inner.update_track_location(track_id, &replacement)?;
        }
        self.inner
            .update_track_availability_if_path_matches(track_id, location)
    }

    fn relocate_track_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
        file_size_bytes: u64,
    ) -> StoreResult<()> {
        self.inner
            .relocate_track_and_enqueue_mirror(track_id, location, file_size_bytes)
    }

    fn replace_track_audio(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
        audio_properties: TrackAudioProperties,
        file_size_bytes: u64,
        has_embedded_artwork: bool,
    ) -> StoreResult<()> {
        self.inner.replace_track_audio(
            track_id,
            location,
            audio_properties,
            file_size_bytes,
            has_embedded_artwork,
        )
    }

    fn update_track_rating(&self, track_id: TrackId, rating: Rating) -> StoreResult<()> {
        self.inner.update_track_rating(track_id, rating)
    }

    fn update_track_rating_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        rating: Rating,
    ) -> StoreResult<()> {
        self.inner
            .update_track_rating_and_enqueue_mirror(track_id, rating)
    }

    fn update_track_statistics(
        &self,
        track_id: TrackId,
        statistics: &PlayStatistics,
    ) -> StoreResult<()> {
        self.inner.update_track_statistics(track_id, statistics)
    }

    fn apply_track_metadata_change(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        self.inner.apply_track_metadata_change(track_id, change)
    }

    fn apply_track_metadata_change_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        self.inner
            .apply_track_metadata_change_and_enqueue_mirror(track_id, change)
    }

    fn apply_track_metadata_change_and_location_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
        location: &TrackLocation,
    ) -> StoreResult<()> {
        self.inner
            .apply_track_metadata_change_and_location_and_enqueue_mirror(track_id, change, location)
    }

    fn fill_missing_track_metadata(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        self.inner.fill_missing_track_metadata(track_id, change)
    }

    fn fill_missing_track_metadata_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<bool> {
        self.inner
            .fill_missing_track_metadata_and_enqueue_mirror(track_id, change)
    }

    fn delete_track(&self, track_id: TrackId) -> StoreResult<()> {
        self.inner.delete_track(track_id)
    }

    fn commit_duplicate_consolidation(&self, plan: &DuplicateConsolidationPlan) -> StoreResult<()> {
        self.inner.commit_duplicate_consolidation(plan)
    }

    fn track(&self, track_id: TrackId) -> StoreResult<Option<Track>> {
        self.inner.track(track_id)
    }

    fn tracks(&self) -> StoreResult<Vec<Track>> {
        self.sleep_read_delay();
        self.inner.tracks()
    }

    fn flush_durable(&self) -> StoreResult<()> {
        self.record_operation("flush_durable");
        self.flush_durable_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_flush_durable.load(Ordering::SeqCst) {
            return Err(StoreError::Database(
                "injected flush_durable failure".to_owned(),
            ));
        }
        self.inner.flush_durable()
    }

    fn publish_tag_mirror_artwork(&self, bytes: &[u8]) -> StoreResult<StoredTagMirrorArtwork> {
        self.inner.publish_tag_mirror_artwork(bytes)
    }

    fn enqueue_tag_mirror_artwork(
        &self,
        track_id: TrackId,
        artwork: TagMirrorArtwork,
    ) -> StoreResult<()> {
        self.inner.enqueue_tag_mirror_artwork(track_id, artwork)
    }

    fn tag_mirrors_due(&self, now_unix: i64, limit: usize) -> StoreResult<Vec<PendingTagMirror>> {
        self.inner.tag_mirrors_due(now_unix, limit)
    }

    fn next_tag_mirror_attempt_at(&self) -> StoreResult<Option<i64>> {
        self.inner.next_tag_mirror_attempt_at()
    }

    fn complete_tag_mirror(&self, track_id: TrackId, generation: u64) -> StoreResult<bool> {
        self.inner.complete_tag_mirror(track_id, generation)
    }

    fn record_tag_mirror_failure(
        &self,
        track_id: TrackId,
        generation: u64,
        next_attempt_at_unix: i64,
        error: &str,
    ) -> StoreResult<bool> {
        self.inner
            .record_tag_mirror_failure(track_id, generation, next_attempt_at_unix, error)
    }

    fn read_tag_mirror_artwork(&self, artwork: &StoredTagMirrorArtwork) -> StoreResult<Vec<u8>> {
        self.inner.read_tag_mirror_artwork(artwork)
    }

    fn garbage_collect_tag_mirror_artwork(&self) -> StoreResult<()> {
        self.inner.garbage_collect_tag_mirror_artwork()
    }

    fn save_playlist(&self, playlist: Playlist) -> StoreResult<()> {
        self.inner.save_playlist(playlist)
    }

    fn playlist(&self, playlist_id: PlaylistId) -> StoreResult<Option<Playlist>> {
        self.inner.playlist(playlist_id)
    }

    fn playlists(&self) -> StoreResult<Vec<Playlist>> {
        self.inner.playlists()
    }

    fn delete_playlist(&self, playlist_id: PlaylistId) -> StoreResult<()> {
        self.inner.delete_playlist(playlist_id)
    }

    fn save_playlist_folder(&self, folder: PlaylistFolder) -> StoreResult<()> {
        self.inner.save_playlist_folder(folder)
    }

    fn playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<Option<PlaylistFolder>> {
        self.inner.playlist_folder(folder_id)
    }

    fn playlist_folders(&self) -> StoreResult<Vec<PlaylistFolder>> {
        self.inner.playlist_folders()
    }

    fn delete_playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<()> {
        self.inner.delete_playlist_folder(folder_id)
    }

    fn save_smart_playlist(&self, smart_playlist: SmartPlaylist) -> StoreResult<()> {
        self.inner.save_smart_playlist(smart_playlist)
    }

    fn smart_playlist(
        &self,
        smart_playlist_id: SmartPlaylistId,
    ) -> StoreResult<Option<SmartPlaylist>> {
        self.inner.smart_playlist(smart_playlist_id)
    }

    fn smart_playlists(&self) -> StoreResult<Vec<SmartPlaylist>> {
        self.inner.smart_playlists()
    }

    fn delete_smart_playlist(&self, smart_playlist_id: SmartPlaylistId) -> StoreResult<()> {
        self.inner.delete_smart_playlist(smart_playlist_id)
    }

    fn load_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
    ) -> StoreResult<Option<TrackColumnLayout>> {
        self.inner.load_track_column_layout(scope)
    }

    fn save_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
        layout: &TrackColumnLayout,
    ) -> StoreResult<()> {
        self.inner.save_track_column_layout(scope, layout)
    }

    fn delete_track_column_layout(&self, scope: TrackColumnLayoutScope) -> StoreResult<()> {
        self.inner.delete_track_column_layout(scope)
    }

    fn tracks_needing_analysis(
        &self,
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .tracks_needing_analysis(capabilities, analyzer_version, limit)
    }

    fn filter_tracks_needing_analysis(
        &self,
        track_ids: &[TrackId],
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .filter_tracks_needing_analysis(track_ids, capabilities, analyzer_version)
    }

    fn load_waveform(&self, track_id: TrackId) -> StoreResult<Option<StoredWaveform>> {
        self.inner.load_waveform(track_id)
    }

    fn load_all_acoustics(&self) -> StoreResult<Vec<(TrackId, AcousticFeatures)>> {
        self.sleep_read_delay();
        self.inner.load_all_acoustics()
    }

    fn record_synced_lyrics(
        &self,
        track_id: TrackId,
        lyrics: &SyncedLyrics,
        source: &str,
    ) -> StoreResult<()> {
        self.inner.record_synced_lyrics(track_id, lyrics, source)
    }

    fn load_synced_lyrics(&self, track_id: TrackId) -> StoreResult<Option<StoredSyncedLyrics>> {
        self.inner.load_synced_lyrics(track_id)
    }

    fn clear_synced_lyrics(&self, track_id: TrackId) -> StoreResult<()> {
        self.inner.clear_synced_lyrics(track_id)
    }

    fn tracks_needing_online(
        &self,
        capabilities: OnlineCapabilities,
        provider_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .tracks_needing_online(capabilities, provider_version, limit)
    }

    fn filter_tracks_needing_online(
        &self,
        track_ids: &[TrackId],
        capabilities: OnlineCapabilities,
        provider_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .filter_tracks_needing_online(track_ids, capabilities, provider_version)
    }

    fn save_smart_shuffle_index(&self, index: &StoredSmartShuffleIndex) -> StoreResult<()> {
        self.inner.save_smart_shuffle_index(index)
    }

    fn load_smart_shuffle_index(&self) -> StoreResult<Option<StoredSmartShuffleIndex>> {
        self.inner.load_smart_shuffle_index()
    }

    fn clear_smart_shuffle_index(&self) -> StoreResult<()> {
        self.inner.clear_smart_shuffle_index()
    }

    fn source_fingerprint(&self, track_id: TrackId) -> StoreResult<Option<SourceFingerprint>> {
        self.inner.source_fingerprint(track_id)
    }

    fn save_source_fingerprint(
        &self,
        track_id: TrackId,
        fingerprint: &SourceFingerprint,
    ) -> StoreResult<()> {
        self.inner.save_source_fingerprint(track_id, fingerprint)
    }

    fn invalidate_source_fingerprint(&self, track_id: TrackId) -> StoreResult<()> {
        self.inner.invalidate_source_fingerprint(track_id)
    }

    fn save_sync_device(&self, device: &SyncDevice) -> StoreResult<()> {
        self.inner.save_sync_device(device)
    }

    fn sync_device(&self, id: &SyncDeviceId) -> StoreResult<Option<SyncDevice>> {
        self.inner.sync_device(id)
    }

    fn sync_devices(&self) -> StoreResult<Vec<SyncDevice>> {
        self.inner.sync_devices()
    }

    fn delete_sync_device(&self, id: &SyncDeviceId) -> StoreResult<()> {
        self.inner.delete_sync_device(id)
    }

    fn save_device_selection(
        &self,
        id: &SyncDeviceId,
        selection: &[PlaylistItem],
    ) -> StoreResult<()> {
        self.inner.save_device_selection(id, selection)
    }

    fn device_selection(&self, id: &SyncDeviceId) -> StoreResult<Vec<PlaylistItem>> {
        self.inner.device_selection(id)
    }

    fn save_device_manifest(
        &self,
        id: &SyncDeviceId,
        entries: &[SyncManifestEntry],
    ) -> StoreResult<()> {
        self.device_manifest_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_device_manifest.load(Ordering::SeqCst) {
            return Err(StoreError::Database(
                "injected save_device_manifest failure".to_owned(),
            ));
        }
        self.inner.save_device_manifest(id, entries)
    }

    fn device_manifest(&self, id: &SyncDeviceId) -> StoreResult<Vec<SyncManifestEntry>> {
        self.inner.device_manifest(id)
    }
}
