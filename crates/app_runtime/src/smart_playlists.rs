// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use sustain_domain::{
    PlaylistFolderId, SmartPlaylist, SmartPlaylistId, SmartPlaylistRuleSet, TrackColumnLayoutScope,
    default_smart_playlists,
};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, playlist_items,
};

impl ApplicationRuntime {
    // Installs the iTunes-style starter set on a freshly created
    // database. Caller is responsible for invoking this exactly once,
    // immediately after the library DB file is created (see
    // `SqliteLibraryStore::was_freshly_created`). Calling it on a DB
    // that already holds smart playlists will collide on the seeded ids
    // — that is a caller bug, not a state to recover from here.
    pub fn seed_default_smart_playlists(&mut self) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        for smart_playlist in default_smart_playlists(1) {
            library_store
                .save_smart_playlist(smart_playlist)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }
        self.reload_playlist_state()
    }

    pub(super) fn create_smart_playlist(
        &mut self,
        name: String,
        parent_folder_id: Option<PlaylistFolderId>,
        rules: SmartPlaylistRuleSet,
    ) -> ApplicationRuntimeResult<()> {
        let name = normalized_smart_playlist_name(name)?;
        validate_rule_set(&rules)?;
        let library_store = self.library_store()?;
        playlist_items::ensure_parent_folder_exists(library_store, parent_folder_id)?;
        let position = playlist_items::next_sibling_position(library_store, parent_folder_id)?;
        let smart_playlists = library_store
            .smart_playlists()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let smart_playlist = SmartPlaylist {
            id: next_smart_playlist_id(&smart_playlists)?,
            name,
            parent_folder_id,
            position,
            rules,
        };
        library_store
            .save_smart_playlist(smart_playlist)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn update_smart_playlist(
        &mut self,
        smart_playlist_id: SmartPlaylistId,
        name: String,
        rules: SmartPlaylistRuleSet,
    ) -> ApplicationRuntimeResult<()> {
        let name = normalized_smart_playlist_name(name)?;
        validate_rule_set(&rules)?;
        let library_store = self.library_store()?;
        let Some(mut smart_playlist) = library_store
            .smart_playlist(smart_playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::SmartPlaylistNotFound);
        };

        smart_playlist.name = name;
        smart_playlist.rules = rules;
        library_store
            .save_smart_playlist(smart_playlist)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.reload_playlist_state()
    }

    pub(super) fn delete_smart_playlist(
        &mut self,
        smart_playlist_id: SmartPlaylistId,
    ) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        let Some(removed) = library_store
            .smart_playlist(smart_playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        else {
            return Err(ApplicationRuntimeError::SmartPlaylistNotFound);
        };

        library_store
            .delete_smart_playlist(smart_playlist_id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.forget_track_column_layout(TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id));
        playlist_items::compact_sibling_positions(library_store, removed.parent_folder_id)?;
        self.reload_playlist_state()
    }
}

fn normalized_smart_playlist_name(name: String) -> ApplicationRuntimeResult<String> {
    crate::normalized_name(name, || ApplicationRuntimeError::InvalidSmartPlaylistName)
}

fn validate_rule_set(rules: &SmartPlaylistRuleSet) -> ApplicationRuntimeResult<()> {
    if rules.rules.is_empty() {
        Err(ApplicationRuntimeError::InvalidSmartPlaylistRules)
    } else {
        Ok(())
    }
}

fn next_smart_playlist_id(
    smart_playlists: &[SmartPlaylist],
) -> ApplicationRuntimeResult<SmartPlaylistId> {
    let next_id = smart_playlists
        .iter()
        .map(|smart| smart.id.get())
        .max()
        .unwrap_or_default()
        .checked_add(1)
        .and_then(SmartPlaylistId::new)
        .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(next_id)
}
