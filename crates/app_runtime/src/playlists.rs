// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::BTreeSet;

use sustain_domain::{
    Playlist, PlaylistEntry, PlaylistFolderId, PlaylistId, TrackColumnLayoutScope, TrackId,
};
use sustain_library_store::LibraryStore;

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, playlist_items,
};

impl ApplicationRuntime {
    pub(super) fn create_playlist(
        &mut self,
        name: String,
        parent_folder_id: Option<PlaylistFolderId>,
    ) -> ApplicationRuntimeResult<()> {
        let name = normalized_playlist_name(name)?;
        let library_store = self.library_store()?;
        playlist_items::ensure_parent_folder_exists(library_store, parent_folder_id)?;
        let position = playlist_items::next_sibling_position(library_store, parent_folder_id)?;
        let playlists = library_store
            .playlists()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let playlist = Playlist {
            id: next_playlist_id(&playlists)?,
            name,
            parent_folder_id,
            position,
            entries: Vec::new(),
        };
        library_store
            .save_playlist(playlist)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn rename_playlist(
        &mut self,
        playlist_id: PlaylistId,
        name: String,
    ) -> ApplicationRuntimeResult<()> {
        let name = normalized_playlist_name(name)?;
        let library_store = self.library_store()?;
        let Some(mut playlist) = library_store
            .playlist(playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::PlaylistNotFound);
        };

        playlist.name = name;
        library_store
            .save_playlist(playlist)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn delete_playlist(
        &mut self,
        playlist_id: PlaylistId,
    ) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        let Some(removed) = library_store
            .playlist(playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::PlaylistNotFound);
        };

        library_store
            .delete_playlist(playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.forget_track_column_layout(TrackColumnLayoutScope::Playlist(playlist_id));
        playlist_items::compact_sibling_positions(library_store, removed.parent_folder_id)?;
        self.reload_playlist_state()
    }

    pub(super) fn add_tracks_to_playlist(
        &mut self,
        playlist_id: PlaylistId,
        track_ids: Vec<TrackId>,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_tracks_are_in_library(&track_ids)?;
        let library_store = self.library_store()?;
        let Some(mut playlist) = library_store
            .playlist(playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::PlaylistNotFound);
        };

        let mut existing_track_ids = playlist
            .entries
            .iter()
            .map(|entry| entry.track_id)
            .collect::<BTreeSet<_>>();
        for track_id in unique_track_ids(track_ids) {
            if existing_track_ids.insert(track_id) {
                playlist.entries.push(PlaylistEntry {
                    playlist_id,
                    track_id,
                    position: playlist.entries.len() as u32,
                });
            }
        }
        reindex_playlist_entries(&mut playlist);

        library_store
            .save_playlist(playlist)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn remove_tracks_from_playlist(
        &mut self,
        playlist_id: PlaylistId,
        track_ids: Vec<TrackId>,
    ) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        let Some(mut playlist) = library_store
            .playlist(playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::PlaylistNotFound);
        };

        let removed_track_ids = track_ids.into_iter().collect::<BTreeSet<_>>();
        playlist
            .entries
            .retain(|entry| !removed_track_ids.contains(&entry.track_id));
        reindex_playlist_entries(&mut playlist);

        library_store
            .save_playlist(playlist)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn move_playlist_entries(
        &mut self,
        playlist_id: PlaylistId,
        track_ids: Vec<TrackId>,
        new_position: u32,
    ) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        let Some(mut playlist) = library_store
            .playlist(playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::PlaylistNotFound);
        };

        // Authoritative source order is the playlist's own entry order,
        // not the caller's vec order — the UI may have read the dragged
        // selection from a sorted view that doesn't match the playlist's
        // logical order, and the post-move contiguous block must reflect
        // the playlist, not the view.
        let moving_set: BTreeSet<TrackId> = track_ids.iter().copied().collect();
        if moving_set.is_empty() {
            return Err(ApplicationRuntimeError::PlaylistEntryNotFound);
        }
        let mut moving_entries: Vec<PlaylistEntry> = Vec::with_capacity(moving_set.len());
        playlist.entries.retain(|entry| {
            if moving_set.contains(&entry.track_id) {
                moving_entries.push(entry.clone());
                false
            } else {
                true
            }
        });
        if moving_entries.is_empty() {
            return Err(ApplicationRuntimeError::PlaylistEntryNotFound);
        }

        let insert_at = usize::try_from(new_position)
            .unwrap_or(usize::MAX)
            .min(playlist.entries.len());
        for (offset, entry) in moving_entries.into_iter().enumerate() {
            playlist.entries.insert(insert_at + offset, entry);
        }
        reindex_playlist_entries(&mut playlist);

        library_store
            .save_playlist(playlist)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(crate) fn library_store(&self) -> ApplicationRuntimeResult<&dyn LibraryStore> {
        self.library_store
            .as_deref()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)
    }

    pub(crate) fn reload_playlist_state(&mut self) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        let playlists = library_store
            .playlists()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let playlist_folders = library_store
            .playlist_folders()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let smart_playlists = library_store
            .smart_playlists()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.playlists = playlists;
        self.playlist_folders = playlist_folders;
        self.smart_playlists = smart_playlists;
        Ok(())
    }

    fn ensure_tracks_are_in_library(&self, track_ids: &[TrackId]) -> ApplicationRuntimeResult<()> {
        let library_track_ids = self
            .library_tracks
            .iter()
            .map(|track| track.id)
            .collect::<BTreeSet<_>>();
        if track_ids
            .iter()
            .all(|track_id| library_track_ids.contains(track_id))
        {
            Ok(())
        } else {
            Err(ApplicationRuntimeError::TrackUnavailable)
        }
    }
}

pub(crate) fn normalized_playlist_name(name: String) -> ApplicationRuntimeResult<String> {
    crate::normalized_name(name, || ApplicationRuntimeError::InvalidPlaylistName)
}

fn next_playlist_id(playlists: &[Playlist]) -> ApplicationRuntimeResult<PlaylistId> {
    let next_id = playlists
        .iter()
        .map(|playlist| playlist.id.get())
        .max()
        .unwrap_or_default()
        .checked_add(1)
        .and_then(PlaylistId::new)
        .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(next_id)
}

fn unique_track_ids(track_ids: Vec<TrackId>) -> Vec<TrackId> {
    let mut seen = BTreeSet::new();
    let mut unique = Vec::new();
    for track_id in track_ids {
        if seen.insert(track_id) {
            unique.push(track_id);
        }
    }
    unique
}

fn reindex_playlist_entries(playlist: &mut Playlist) {
    for (position, entry) in playlist.entries.iter_mut().enumerate() {
        entry.playlist_id = playlist.id;
        entry.position = position as u32;
    }
}
