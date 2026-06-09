// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use sustain_domain::{PlaylistFolder, PlaylistFolderId};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, playlist_items,
};

impl ApplicationRuntime {
    pub(super) fn create_playlist_folder(
        &mut self,
        name: String,
        parent_folder_id: Option<PlaylistFolderId>,
    ) -> ApplicationRuntimeResult<()> {
        let name = normalized_folder_name(name)?;
        let library_store = self.library_store()?;
        playlist_items::ensure_parent_folder_exists(library_store, parent_folder_id)?;
        let position = playlist_items::next_sibling_position(library_store, parent_folder_id)?;
        let folders = library_store
            .playlist_folders()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let folder = PlaylistFolder {
            id: next_folder_id(&folders)?,
            name,
            parent_folder_id,
            position,
        };
        library_store
            .save_playlist_folder(folder)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn rename_playlist_folder(
        &mut self,
        folder_id: PlaylistFolderId,
        name: String,
    ) -> ApplicationRuntimeResult<()> {
        let name = normalized_folder_name(name)?;
        let library_store = self.library_store()?;
        let Some(mut folder) = library_store
            .playlist_folder(folder_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::PlaylistFolderNotFound);
        };

        folder.name = name;
        library_store
            .save_playlist_folder(folder)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn delete_playlist_folder(
        &mut self,
        folder_id: PlaylistFolderId,
    ) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        let Some(removed) = library_store
            .playlist_folder(folder_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::PlaylistFolderNotFound);
        };

        library_store
            .delete_playlist_folder(folder_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.clear_track_column_layout_cache();
        playlist_items::compact_sibling_positions(library_store, removed.parent_folder_id)?;
        self.reload_playlist_state()
    }
}

fn normalized_folder_name(name: String) -> ApplicationRuntimeResult<String> {
    crate::normalized_name(name, || ApplicationRuntimeError::InvalidPlaylistFolderName)
}

fn next_folder_id(folders: &[PlaylistFolder]) -> ApplicationRuntimeResult<PlaylistFolderId> {
    let next_id = folders
        .iter()
        .map(|folder| folder.id.get())
        .max()
        .unwrap_or_default()
        .checked_add(1)
        .and_then(PlaylistFolderId::new)
        .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(next_id)
}
