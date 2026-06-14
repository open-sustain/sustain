// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{path::PathBuf, time::Duration};

use crate::{
    DeviceLayout, DuplicateConsolidationRequest, FilesPerFolderCap, LibraryQuery, MetadataChange,
    PlaybackCommand, PlaylistFolderId, PlaylistId, PlaylistItem, Rating, SmartPlaylistId,
    SmartPlaylistRuleSet, SyncDeviceId, TrackId, UserSettings,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationCommand {
    Playback(PlaybackCommand),
    SetRating {
        track_id: TrackId,
        rating: Rating,
    },
    CreatePlaylist {
        name: String,
        parent_folder_id: Option<PlaylistFolderId>,
    },
    RenamePlaylist {
        playlist_id: PlaylistId,
        name: String,
    },
    DeletePlaylist {
        playlist_id: PlaylistId,
    },
    AddTracksToPlaylist {
        playlist_id: PlaylistId,
        track_ids: Vec<TrackId>,
    },
    RemoveTracksFromPlaylist {
        playlist_id: PlaylistId,
        track_ids: Vec<TrackId>,
    },
    /// Reorder one or more tracks within an existing playlist.
    ///
    /// The moved tracks are extracted from the playlist (preserving the
    /// authoritative entry order among themselves), then re-inserted as a
    /// single contiguous block at `new_position`. `new_position` is the
    /// insertion index in the *post-removal* entries list and is clamped to
    /// the list's length, so the saturating `u32::MAX` value lands the
    /// block at the tail.
    MovePlaylistEntries {
        playlist_id: PlaylistId,
        track_ids: Vec<TrackId>,
        new_position: u32,
    },
    CreatePlaylistFolder {
        name: String,
        parent_folder_id: Option<PlaylistFolderId>,
    },
    RenamePlaylistFolder {
        folder_id: PlaylistFolderId,
        name: String,
    },
    DeletePlaylistFolder {
        folder_id: PlaylistFolderId,
    },
    CreateSmartPlaylist {
        name: String,
        parent_folder_id: Option<PlaylistFolderId>,
        rules: SmartPlaylistRuleSet,
    },
    UpdateSmartPlaylist {
        smart_playlist_id: SmartPlaylistId,
        name: String,
        rules: SmartPlaylistRuleSet,
    },
    DeleteSmartPlaylist {
        smart_playlist_id: SmartPlaylistId,
    },
    MovePlaylistItem {
        item: PlaylistItem,
        target_parent_folder_id: Option<PlaylistFolderId>,
        position: u32,
    },
    UpdateMetadata {
        track_id: TrackId,
        change: Box<MetadataChange>,
    },
    ResetPlayCount {
        track_id: TrackId,
    },
    SetArtwork {
        track_id: TrackId,
        artwork: Option<Vec<u8>>,
    },
    /// Trigger an explicit remote artwork lookup for `track_id`.
    /// The runtime enqueues the work on the background fetcher and
    /// returns immediately; the outcome is delivered later through
    /// the runtime's artwork-fetch result sink.
    FetchArtwork {
        track_id: TrackId,
    },
    RemoveTrackFromLibrary {
        track_id: TrackId,
    },
    MoveTrackToTrash {
        track_id: TrackId,
    },
    /// Attach an existing missing library row to a replacement audio file.
    /// The runtime preserves the row id and all authoritative library data;
    /// only the source location and disposable file-derived observations
    /// change.
    RelocateMissingTrack {
        track_id: TrackId,
        replacement_path: PathBuf,
    },
    /// Download the best available audio for one YouTube video through the
    /// user's installed `yt-dlp`, validate it, and replace this row's source
    /// file without changing its library identity.
    ReplaceTrackAudioFromYoutube {
        track_id: TrackId,
        url: String,
    },
    ConsolidateDuplicateTracks(DuplicateConsolidationRequest),
    AddExternalLibraryItems {
        paths: Vec<PathBuf>,
    },
    UpdateSettings(UserSettings),
    ScanLibrary {
        library_path: PathBuf,
    },

    // --- Device sync (issues #23 / #24) ---
    /// Set the on-drive layout written for a device. Creates the
    /// device's saved-config row if it does not exist yet.
    SetDeviceLayout {
        device_id: SyncDeviceId,
        layout: DeviceLayout,
    },
    /// Set the sub-path under the device root to sync into (empty = root).
    SetDeviceSubPath {
        device_id: SyncDeviceId,
        sub_path: crate::DeviceRelativePath,
    },
    /// Set the per-folder file cap for the folder-per-playlist layout.
    SetDeviceFilesPerFolderCap {
        device_id: SyncDeviceId,
        cap: FilesPerFolderCap,
    },
    /// Replace the ticked playlists/smart-playlists for a device.
    SetDeviceSelection {
        device_id: SyncDeviceId,
        selection: Vec<PlaylistItem>,
    },
    /// Replace the ticked artists for a device.
    SetDeviceArtistSelection {
        device_id: SyncDeviceId,
        artists: Vec<String>,
    },
    /// Rename the device as shown in the sidebar.
    RenameDevice {
        device_id: SyncDeviceId,
        label: String,
    },
    /// Forget a device: drop its saved selection, options, and manifest.
    /// Does not touch the device's contents.
    ForgetDevice {
        device_id: SyncDeviceId,
    },
    /// Start an incremental sync of the device's ticked playlists/artists. When
    /// `remove_stale` is false, tracks no longer in the selection are
    /// left on the device; when true, they are deleted (the UI confirms
    /// destructive removals before setting this).
    SyncDevice {
        device_id: SyncDeviceId,
        remove_stale: bool,
    },
    /// Run analysis (BPM / key / waveform) over the tracks in a device's
    /// ticked playlists/artists that are still missing it — the Pioneer
    /// export's "analyse the missing ones" action.
    AnalyzeDeviceTracks {
        device_id: SyncDeviceId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationQuery {
    ListTracks(LibraryQuery),
    ListPlaylists,
    TrackDetails(TrackId),
    SearchTracks {
        search_text: String,
    },
    PlayStatistics(TrackId),
    CurrentPlaybackState,
    Settings,
    TotalDuration(LibraryQuery),
    SelectionDuration {
        track_ids: Vec<TrackId>,
    },
    PlaybackPosition {
        track_id: TrackId,
        position: Duration,
    },
}
