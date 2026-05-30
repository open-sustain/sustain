// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::BTreeMap,
    sync::{Mutex, MutexGuard},
};

use sustain_domain::TrackAnalysis;

use crate::{
    AcousticFeatures, AnalysisCapabilities, AnalysisContext, LibraryStore, OnlineCapabilities,
    OnlineContext, Playlist, PlaylistFolder, PlaylistFolderId, PlaylistId, PlaylistItem,
    SmartPlaylist, SmartPlaylistId, StoreError, StoreResult, StoredSmartShuffleIndex,
    StoredSyncedLyrics, StoredWaveform, SyncDevice, SyncDeviceId, SyncManifestEntry, SyncedLyrics,
    Track, TrackColumnLayout, TrackColumnLayoutScope, TrackId,
};

#[derive(Debug, Default)]
pub struct InMemoryLibraryStore {
    tracks: Mutex<BTreeMap<TrackId, Track>>,
    playlists: Mutex<BTreeMap<PlaylistId, Playlist>>,
    folders: Mutex<BTreeMap<PlaylistFolderId, PlaylistFolder>>,
    smart_playlists: Mutex<BTreeMap<SmartPlaylistId, SmartPlaylist>>,
    default_layout: Mutex<Option<TrackColumnLayout>>,
    playlist_layouts: Mutex<BTreeMap<PlaylistId, TrackColumnLayout>>,
    smart_playlist_layouts: Mutex<BTreeMap<SmartPlaylistId, TrackColumnLayout>>,
    analysis_bookkeeping: Mutex<BTreeMap<TrackId, AnalysisBookkeeping>>,
    waveforms: Mutex<BTreeMap<TrackId, StoredWaveform>>,
    synced_lyrics: Mutex<BTreeMap<TrackId, StoredSyncedLyrics>>,
    online_bookkeeping: Mutex<BTreeMap<TrackId, OnlineBookkeeping>>,
    smart_shuffle_index: Mutex<Option<StoredSmartShuffleIndex>>,
    acoustics: Mutex<BTreeMap<TrackId, AcousticFeatures>>,
    sync_devices: Mutex<BTreeMap<SyncDeviceId, SyncDevice>>,
    device_selections: Mutex<BTreeMap<SyncDeviceId, Vec<PlaylistItem>>>,
    device_manifests: Mutex<BTreeMap<SyncDeviceId, Vec<SyncManifestEntry>>>,
}

/// In-memory mirror of one `track_analysis` row. Mirrors the SQLite
/// COALESCE semantics: an unsupplied `*_attempted_at_unix` keeps its
/// previous value rather than reverting to `None`.
#[derive(Clone, Copy, Debug, Default)]
struct AnalysisBookkeeping {
    bpm_attempted_at_unix: Option<i64>,
    key_attempted_at_unix: Option<i64>,
    audio_attempted_at_unix: Option<i64>,
    analyzer_version: u32,
}

/// In-memory mirror of one `track_online_status` row. Same COALESCE
/// semantics as [`AnalysisBookkeeping`].
#[derive(Clone, Copy, Debug, Default)]
struct OnlineBookkeeping {
    artwork_attempted_at_unix: Option<i64>,
    tags_attempted_at_unix: Option<i64>,
    lyrics_attempted_at_unix: Option<i64>,
    provider_version: u32,
}

impl InMemoryLibraryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn tracks_guard(&self) -> StoreResult<MutexGuard<'_, BTreeMap<TrackId, Track>>> {
        self.tracks.lock().map_err(|_| StoreError::StoreUnavailable)
    }

    fn playlists_guard(&self) -> StoreResult<MutexGuard<'_, BTreeMap<PlaylistId, Playlist>>> {
        self.playlists
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)
    }

    fn folders_guard(
        &self,
    ) -> StoreResult<MutexGuard<'_, BTreeMap<PlaylistFolderId, PlaylistFolder>>> {
        self.folders
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)
    }

    fn smart_playlists_guard(
        &self,
    ) -> StoreResult<MutexGuard<'_, BTreeMap<SmartPlaylistId, SmartPlaylist>>> {
        self.smart_playlists
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)
    }
}

impl LibraryStore for InMemoryLibraryStore {
    fn save_track(&self, track: Track) -> StoreResult<()> {
        self.tracks_guard()?.insert(track.id, track);
        Ok(())
    }

    fn save_tracks(&self, tracks: &[Track]) -> StoreResult<()> {
        let mut stored_tracks = self.tracks_guard()?;
        for track in tracks {
            stored_tracks.insert(track.id, track.clone());
        }
        Ok(())
    }

    fn delete_track(&self, track_id: TrackId) -> StoreResult<()> {
        let mut tracks = self.tracks_guard()?;
        tracks.remove(&track_id);
        drop(tracks);
        for playlist in self.playlists_guard()?.values_mut() {
            playlist.entries.retain(|entry| entry.track_id != track_id);
        }
        Ok(())
    }

    fn track(&self, track_id: TrackId) -> StoreResult<Option<Track>> {
        Ok(self.tracks_guard()?.get(&track_id).cloned())
    }

    fn tracks(&self) -> StoreResult<Vec<Track>> {
        Ok(self.tracks_guard()?.values().cloned().collect())
    }

    fn save_playlist(&self, playlist: Playlist) -> StoreResult<()> {
        self.playlists_guard()?.insert(playlist.id, playlist);
        Ok(())
    }

    fn playlist(&self, playlist_id: PlaylistId) -> StoreResult<Option<Playlist>> {
        Ok(self.playlists_guard()?.get(&playlist_id).cloned())
    }

    fn playlists(&self) -> StoreResult<Vec<Playlist>> {
        Ok(self.playlists_guard()?.values().cloned().collect())
    }

    fn delete_playlist(&self, playlist_id: PlaylistId) -> StoreResult<()> {
        self.playlists_guard()?.remove(&playlist_id);
        self.playlist_layouts
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .remove(&playlist_id);
        Ok(())
    }

    fn save_playlist_folder(&self, folder: PlaylistFolder) -> StoreResult<()> {
        self.folders_guard()?.insert(folder.id, folder);
        Ok(())
    }

    fn playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<Option<PlaylistFolder>> {
        Ok(self.folders_guard()?.get(&folder_id).cloned())
    }

    fn playlist_folders(&self) -> StoreResult<Vec<PlaylistFolder>> {
        Ok(self.folders_guard()?.values().cloned().collect())
    }

    fn delete_playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<()> {
        let mut deleted = std::collections::BTreeSet::new();
        deleted.insert(folder_id);

        let mut folders = self.folders_guard()?;
        loop {
            let mut grew = false;
            for child_id in folders.keys().copied().collect::<Vec<_>>() {
                if deleted.contains(&child_id) {
                    continue;
                }
                let child = folders.get(&child_id).expect("iterated id exists in map");
                if let Some(parent) = child.parent_folder_id {
                    if deleted.contains(&parent) {
                        deleted.insert(child_id);
                        grew = true;
                    }
                }
            }
            if !grew {
                break;
            }
        }
        folders.retain(|id, _| !deleted.contains(id));
        drop(folders);

        let mut playlists = self.playlists_guard()?;
        let surviving_playlists: std::collections::BTreeSet<PlaylistId> = playlists
            .iter()
            .filter_map(|(id, playlist)| match playlist.parent_folder_id {
                Some(parent) if deleted.contains(&parent) => None,
                _ => Some(*id),
            })
            .collect();
        playlists.retain(|id, _| surviving_playlists.contains(id));
        drop(playlists);

        let mut smart_playlists = self.smart_playlists_guard()?;
        let surviving_smart_playlists: std::collections::BTreeSet<SmartPlaylistId> =
            smart_playlists
                .iter()
                .filter_map(|(id, smart)| match smart.parent_folder_id {
                    Some(parent) if deleted.contains(&parent) => None,
                    _ => Some(*id),
                })
                .collect();
        smart_playlists.retain(|id, _| surviving_smart_playlists.contains(id));
        drop(smart_playlists);

        self.playlist_layouts
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .retain(|id, _| surviving_playlists.contains(id));
        self.smart_playlist_layouts
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .retain(|id, _| surviving_smart_playlists.contains(id));
        Ok(())
    }

    fn save_smart_playlist(&self, smart_playlist: SmartPlaylist) -> StoreResult<()> {
        self.smart_playlists_guard()?
            .insert(smart_playlist.id, smart_playlist);
        Ok(())
    }

    fn smart_playlist(
        &self,
        smart_playlist_id: SmartPlaylistId,
    ) -> StoreResult<Option<SmartPlaylist>> {
        Ok(self
            .smart_playlists_guard()?
            .get(&smart_playlist_id)
            .cloned())
    }

    fn smart_playlists(&self) -> StoreResult<Vec<SmartPlaylist>> {
        Ok(self.smart_playlists_guard()?.values().cloned().collect())
    }

    fn delete_smart_playlist(&self, smart_playlist_id: SmartPlaylistId) -> StoreResult<()> {
        self.smart_playlists_guard()?.remove(&smart_playlist_id);
        self.smart_playlist_layouts
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .remove(&smart_playlist_id);
        Ok(())
    }

    fn load_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
    ) -> StoreResult<Option<TrackColumnLayout>> {
        match scope {
            TrackColumnLayoutScope::Default => Ok(self
                .default_layout
                .lock()
                .map_err(|_| StoreError::StoreUnavailable)?
                .clone()),
            TrackColumnLayoutScope::Playlist(playlist_id) => Ok(self
                .playlist_layouts
                .lock()
                .map_err(|_| StoreError::StoreUnavailable)?
                .get(&playlist_id)
                .cloned()),
            TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => Ok(self
                .smart_playlist_layouts
                .lock()
                .map_err(|_| StoreError::StoreUnavailable)?
                .get(&smart_playlist_id)
                .cloned()),
        }
    }

    fn save_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
        layout: &TrackColumnLayout,
    ) -> StoreResult<()> {
        match scope {
            TrackColumnLayoutScope::Default => {
                *self
                    .default_layout
                    .lock()
                    .map_err(|_| StoreError::StoreUnavailable)? = Some(layout.clone());
            }
            TrackColumnLayoutScope::Playlist(playlist_id) => {
                self.playlist_layouts
                    .lock()
                    .map_err(|_| StoreError::StoreUnavailable)?
                    .insert(playlist_id, layout.clone());
            }
            TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => {
                self.smart_playlist_layouts
                    .lock()
                    .map_err(|_| StoreError::StoreUnavailable)?
                    .insert(smart_playlist_id, layout.clone());
            }
        }
        Ok(())
    }

    fn delete_track_column_layout(&self, scope: TrackColumnLayoutScope) -> StoreResult<()> {
        match scope {
            TrackColumnLayoutScope::Default => {
                *self
                    .default_layout
                    .lock()
                    .map_err(|_| StoreError::StoreUnavailable)? = None;
            }
            TrackColumnLayoutScope::Playlist(playlist_id) => {
                self.playlist_layouts
                    .lock()
                    .map_err(|_| StoreError::StoreUnavailable)?
                    .remove(&playlist_id);
            }
            TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => {
                self.smart_playlist_layouts
                    .lock()
                    .map_err(|_| StoreError::StoreUnavailable)?
                    .remove(&smart_playlist_id);
            }
        }
        Ok(())
    }

    fn record_analysis(
        &self,
        track_id: TrackId,
        analysis: &TrackAnalysis,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        if capabilities.is_empty() {
            return Ok(());
        }
        let mut bookkeeping = self
            .analysis_bookkeeping
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?;
        let entry = bookkeeping.entry(track_id).or_default();
        if capabilities.bpm {
            entry.bpm_attempted_at_unix = Some(context.now_unix);
        }
        if capabilities.key {
            entry.key_attempted_at_unix = Some(context.now_unix);
        }
        if capabilities.audio {
            entry.audio_attempted_at_unix = Some(context.now_unix);
        }
        entry.analyzer_version = context.analyzer_version;
        drop(bookkeeping);

        if capabilities.audio && !analysis.waveform_detail.segments.is_empty() {
            self.waveforms
                .lock()
                .map_err(|_| StoreError::StoreUnavailable)?
                .insert(
                    track_id,
                    StoredWaveform {
                        preview: analysis.waveform_preview.clone(),
                        detail: analysis.waveform_detail.clone(),
                    },
                );
        }

        if capabilities.audio
            && let Some(acoustics) = analysis.acoustics
        {
            self.acoustics
                .lock()
                .map_err(|_| StoreError::StoreUnavailable)?
                .insert(track_id, acoustics);
        }

        // Fill tracks.bpm / metadata.key only when currently empty —
        // mirrors the SQL backend's "fill if NULL" semantic.
        let mut tracks = self.tracks_guard()?;
        if let Some(track) = tracks.get_mut(&track_id) {
            if capabilities.bpm
                && let Some(bpm) = analysis.bpm
                && track.metadata.bpm.is_none()
            {
                track.metadata.bpm = Some(bpm.round() as u32);
            }
            if capabilities.key
                && let Some(key) = analysis.key
                && track.metadata.key.is_none()
            {
                track.metadata.key = Some(key.short_code().to_string());
            }
        }
        Ok(())
    }

    fn record_analysis_attempt_failure(
        &self,
        track_id: TrackId,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        if capabilities.is_empty() {
            return Ok(());
        }
        let mut bookkeeping = self
            .analysis_bookkeeping
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?;
        let entry = bookkeeping.entry(track_id).or_default();
        if capabilities.bpm {
            entry.bpm_attempted_at_unix = Some(context.now_unix);
        }
        if capabilities.key {
            entry.key_attempted_at_unix = Some(context.now_unix);
        }
        if capabilities.audio {
            entry.audio_attempted_at_unix = Some(context.now_unix);
        }
        entry.analyzer_version = context.analyzer_version;
        Ok(())
    }

    fn tracks_needing_analysis(
        &self,
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        if capabilities.is_empty() {
            return Ok(Vec::new());
        }
        let tracks = self.tracks_guard()?;
        let bookkeeping = self
            .analysis_bookkeeping
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?;
        let mut out = Vec::new();
        for (track_id, track) in tracks.iter() {
            if out.len() >= limit {
                break;
            }
            if track.location.is_missing() {
                continue;
            }
            let book = bookkeeping.get(track_id).copied().unwrap_or_default();
            let needs_bpm = capabilities.bpm
                && (book.bpm_attempted_at_unix.is_none()
                    || book.analyzer_version < analyzer_version);
            let needs_key = capabilities.key
                && (book.key_attempted_at_unix.is_none()
                    || book.analyzer_version < analyzer_version);
            let needs_audio = capabilities.audio
                && (book.audio_attempted_at_unix.is_none()
                    || book.analyzer_version < analyzer_version);
            if needs_bpm || needs_key || needs_audio {
                out.push(*track_id);
            }
        }
        Ok(out)
    }

    fn filter_tracks_needing_analysis(
        &self,
        track_ids: &[TrackId],
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        if capabilities.is_empty() || track_ids.is_empty() {
            return Ok(Vec::new());
        }
        let tracks = self.tracks_guard()?;
        let bookkeeping = self
            .analysis_bookkeeping
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?;
        let mut out = Vec::with_capacity(track_ids.len());
        for track_id in track_ids {
            let Some(track) = tracks.get(track_id) else {
                continue;
            };
            if track.location.is_missing() {
                continue;
            }
            let book = bookkeeping.get(track_id).copied().unwrap_or_default();
            let needs_bpm = capabilities.bpm
                && (book.bpm_attempted_at_unix.is_none()
                    || book.analyzer_version < analyzer_version);
            let needs_key = capabilities.key
                && (book.key_attempted_at_unix.is_none()
                    || book.analyzer_version < analyzer_version);
            let needs_audio = capabilities.audio
                && (book.audio_attempted_at_unix.is_none()
                    || book.analyzer_version < analyzer_version);
            if needs_bpm || needs_key || needs_audio {
                out.push(*track_id);
            }
        }
        Ok(out)
    }

    fn load_all_acoustics(&self) -> StoreResult<Vec<(TrackId, AcousticFeatures)>> {
        Ok(self
            .acoustics
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .iter()
            .map(|(id, features)| (*id, *features))
            .collect())
    }

    fn load_waveform(&self, track_id: TrackId) -> StoreResult<Option<StoredWaveform>> {
        Ok(self
            .waveforms
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .get(&track_id)
            .cloned())
    }

    fn record_synced_lyrics(
        &self,
        track_id: TrackId,
        lyrics: &SyncedLyrics,
        source: &str,
    ) -> StoreResult<()> {
        if lyrics.is_empty() {
            return Ok(());
        }
        self.synced_lyrics
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .insert(
                track_id,
                StoredSyncedLyrics {
                    lyrics: lyrics.clone(),
                    source: source.to_owned(),
                },
            );
        Ok(())
    }

    fn load_synced_lyrics(&self, track_id: TrackId) -> StoreResult<Option<StoredSyncedLyrics>> {
        Ok(self
            .synced_lyrics
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .get(&track_id)
            .cloned())
    }

    fn clear_synced_lyrics(&self, track_id: TrackId) -> StoreResult<()> {
        self.synced_lyrics
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .remove(&track_id);
        Ok(())
    }

    fn save_smart_shuffle_index(&self, index: &StoredSmartShuffleIndex) -> StoreResult<()> {
        *self
            .smart_shuffle_index
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)? = Some(index.clone());
        Ok(())
    }

    fn load_smart_shuffle_index(&self) -> StoreResult<Option<StoredSmartShuffleIndex>> {
        Ok(self
            .smart_shuffle_index
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .clone())
    }

    fn clear_smart_shuffle_index(&self) -> StoreResult<()> {
        *self
            .smart_shuffle_index
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)? = None;
        Ok(())
    }

    fn record_online_attempt(
        &self,
        track_id: TrackId,
        capabilities: OnlineCapabilities,
        context: OnlineContext,
    ) -> StoreResult<()> {
        if capabilities.is_empty() {
            return Ok(());
        }
        let mut bookkeeping = self
            .online_bookkeeping
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?;
        let entry = bookkeeping.entry(track_id).or_default();
        if capabilities.artwork {
            entry.artwork_attempted_at_unix = Some(context.now_unix);
        }
        if capabilities.tags {
            entry.tags_attempted_at_unix = Some(context.now_unix);
        }
        if capabilities.lyrics {
            entry.lyrics_attempted_at_unix = Some(context.now_unix);
        }
        entry.provider_version = context.provider_version;
        Ok(())
    }

    fn tracks_needing_online(
        &self,
        capabilities: OnlineCapabilities,
        provider_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        if capabilities.is_empty() {
            return Ok(Vec::new());
        }
        let tracks = self.tracks_guard()?;
        let bookkeeping = self
            .online_bookkeeping
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?;
        let mut out = Vec::new();
        for (track_id, track) in tracks.iter() {
            if out.len() >= limit {
                break;
            }
            if track.location.is_missing() {
                continue;
            }
            let book = bookkeeping.get(track_id).copied().unwrap_or_default();
            // Mirror the SQL guard: a track with a known embedded
            // picture is excluded from the artwork-needs clause, even
            // at a fresh `provider_version`. `None` is treated as
            // "not yet scanned" → still a candidate.
            let has_artwork = track.has_embedded_artwork.unwrap_or(false);
            let needs_artwork = capabilities.artwork
                && !has_artwork
                && (book.artwork_attempted_at_unix.is_none()
                    || book.provider_version < provider_version);
            let needs_tags = capabilities.tags
                && (book.tags_attempted_at_unix.is_none()
                    || book.provider_version < provider_version);
            let needs_lyrics = capabilities.lyrics
                && (book.lyrics_attempted_at_unix.is_none()
                    || book.provider_version < provider_version);
            if needs_artwork || needs_tags || needs_lyrics {
                out.push(*track_id);
            }
        }
        Ok(out)
    }

    fn filter_tracks_needing_online(
        &self,
        track_ids: &[TrackId],
        capabilities: OnlineCapabilities,
        provider_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        if capabilities.is_empty() || track_ids.is_empty() {
            return Ok(Vec::new());
        }
        let tracks = self.tracks_guard()?;
        let bookkeeping = self
            .online_bookkeeping
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?;
        let mut out = Vec::with_capacity(track_ids.len());
        for track_id in track_ids {
            let Some(track) = tracks.get(track_id) else {
                continue;
            };
            if track.location.is_missing() {
                continue;
            }
            let book = bookkeeping.get(track_id).copied().unwrap_or_default();
            let has_artwork = track.has_embedded_artwork.unwrap_or(false);
            let needs_artwork = capabilities.artwork
                && !has_artwork
                && (book.artwork_attempted_at_unix.is_none()
                    || book.provider_version < provider_version);
            let needs_tags = capabilities.tags
                && (book.tags_attempted_at_unix.is_none()
                    || book.provider_version < provider_version);
            let needs_lyrics = capabilities.lyrics
                && (book.lyrics_attempted_at_unix.is_none()
                    || book.provider_version < provider_version);
            if needs_artwork || needs_tags || needs_lyrics {
                out.push(*track_id);
            }
        }
        Ok(out)
    }

    fn save_sync_device(&self, device: &SyncDevice) -> StoreResult<()> {
        self.sync_devices
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .insert(device.id.clone(), device.clone());
        Ok(())
    }

    fn sync_device(&self, id: &SyncDeviceId) -> StoreResult<Option<SyncDevice>> {
        Ok(self
            .sync_devices
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .get(id)
            .cloned())
    }

    fn sync_devices(&self) -> StoreResult<Vec<SyncDevice>> {
        Ok(self
            .sync_devices
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .values()
            .cloned()
            .collect())
    }

    fn delete_sync_device(&self, id: &SyncDeviceId) -> StoreResult<()> {
        self.sync_devices
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .remove(id);
        self.device_selections
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .remove(id);
        self.device_manifests
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .remove(id);
        Ok(())
    }

    fn save_device_selection(
        &self,
        id: &SyncDeviceId,
        selection: &[PlaylistItem],
    ) -> StoreResult<()> {
        self.device_selections
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .insert(id.clone(), selection.to_vec());
        Ok(())
    }

    fn device_selection(&self, id: &SyncDeviceId) -> StoreResult<Vec<PlaylistItem>> {
        Ok(self
            .device_selections
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .get(id)
            .cloned()
            .unwrap_or_default())
    }

    fn save_device_manifest(
        &self,
        id: &SyncDeviceId,
        entries: &[SyncManifestEntry],
    ) -> StoreResult<()> {
        self.device_manifests
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .insert(id.clone(), entries.to_vec());
        Ok(())
    }

    fn device_manifest(&self, id: &SyncDeviceId) -> StoreResult<Vec<SyncManifestEntry>> {
        Ok(self
            .device_manifests
            .lock()
            .map_err(|_| StoreError::StoreUnavailable)?
            .get(id)
            .cloned()
            .unwrap_or_default())
    }
}
