// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use sustain_domain::{PlaylistFolderId, PlaylistItem};
use sustain_library_store::LibraryStore;

use crate::{ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult};

#[derive(Clone, Debug)]
pub(crate) struct Sibling {
    pub item: PlaylistItem,
    pub position: u32,
}

pub(crate) fn ensure_parent_folder_exists(
    library_store: &dyn LibraryStore,
    parent_folder_id: Option<PlaylistFolderId>,
) -> ApplicationRuntimeResult<()> {
    let Some(parent_folder_id) = parent_folder_id else {
        return Ok(());
    };
    let exists = library_store
        .playlist_folder(parent_folder_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        .is_some();
    if exists {
        Ok(())
    } else {
        Err(ApplicationRuntimeError::PlaylistFolderNotFound)
    }
}

pub(crate) fn siblings_in_folder(
    library_store: &dyn LibraryStore,
    parent_folder_id: Option<PlaylistFolderId>,
) -> ApplicationRuntimeResult<Vec<Sibling>> {
    let folders = library_store
        .playlist_folders()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let playlists = library_store
        .playlists()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let smart_playlists = library_store
        .smart_playlists()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;

    let mut siblings = Vec::new();
    for folder in folders
        .into_iter()
        .filter(|folder| folder.parent_folder_id == parent_folder_id)
    {
        siblings.push(Sibling {
            item: PlaylistItem::Folder(folder.id),
            position: folder.position,
        });
    }
    for playlist in playlists
        .into_iter()
        .filter(|playlist| playlist.parent_folder_id == parent_folder_id)
    {
        siblings.push(Sibling {
            item: PlaylistItem::Playlist(playlist.id),
            position: playlist.position,
        });
    }
    for smart in smart_playlists
        .into_iter()
        .filter(|smart| smart.parent_folder_id == parent_folder_id)
    {
        siblings.push(Sibling {
            item: PlaylistItem::SmartPlaylist(smart.id),
            position: smart.position,
        });
    }
    siblings.sort_by_key(|sibling| (sibling.position, playlist_item_sort_key(sibling.item)));
    Ok(siblings)
}

pub(crate) fn next_sibling_position(
    library_store: &dyn LibraryStore,
    parent_folder_id: Option<PlaylistFolderId>,
) -> ApplicationRuntimeResult<u32> {
    Ok(siblings_in_folder(library_store, parent_folder_id)?.len() as u32)
}

pub(crate) fn compact_sibling_positions(
    library_store: &dyn LibraryStore,
    parent_folder_id: Option<PlaylistFolderId>,
) -> ApplicationRuntimeResult<()> {
    let siblings = siblings_in_folder(library_store, parent_folder_id)?;
    apply_positions(library_store, &siblings, parent_folder_id)
}

fn apply_positions(
    library_store: &dyn LibraryStore,
    siblings: &[Sibling],
    parent_folder_id: Option<PlaylistFolderId>,
) -> ApplicationRuntimeResult<()> {
    for (index, sibling) in siblings.iter().enumerate() {
        let new_position = index as u32;
        update_item_placement(library_store, sibling.item, parent_folder_id, new_position)?;
    }
    Ok(())
}

fn update_item_placement(
    library_store: &dyn LibraryStore,
    item: PlaylistItem,
    parent_folder_id: Option<PlaylistFolderId>,
    position: u32,
) -> ApplicationRuntimeResult<()> {
    match item {
        PlaylistItem::Playlist(playlist_id) => {
            let Some(mut playlist) = library_store
                .playlist(playlist_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
            else {
                return Err(ApplicationRuntimeError::PlaylistNotFound);
            };
            if playlist.parent_folder_id != parent_folder_id || playlist.position != position {
                playlist.parent_folder_id = parent_folder_id;
                playlist.position = position;
                library_store
                    .save_playlist(playlist)
                    .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
            }
        }
        PlaylistItem::SmartPlaylist(smart_playlist_id) => {
            let Some(mut smart) = library_store
                .smart_playlist(smart_playlist_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
            else {
                return Err(ApplicationRuntimeError::SmartPlaylistNotFound);
            };
            if smart.parent_folder_id != parent_folder_id || smart.position != position {
                smart.parent_folder_id = parent_folder_id;
                smart.position = position;
                library_store
                    .save_smart_playlist(smart)
                    .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
            }
        }
        PlaylistItem::Folder(folder_id) => {
            let Some(mut folder) = library_store
                .playlist_folder(folder_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
            else {
                return Err(ApplicationRuntimeError::PlaylistFolderNotFound);
            };
            if folder.parent_folder_id != parent_folder_id || folder.position != position {
                folder.parent_folder_id = parent_folder_id;
                folder.position = position;
                library_store
                    .save_playlist_folder(folder)
                    .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
            }
        }
    }
    Ok(())
}

fn playlist_item_sort_key(item: PlaylistItem) -> (u8, i64) {
    match item {
        PlaylistItem::Folder(id) => (0, id.get()),
        PlaylistItem::Playlist(id) => (1, id.get()),
        PlaylistItem::SmartPlaylist(id) => (2, id.get()),
    }
}

fn current_parent_of(
    library_store: &dyn LibraryStore,
    item: PlaylistItem,
) -> ApplicationRuntimeResult<Option<PlaylistFolderId>> {
    Ok(match item {
        PlaylistItem::Playlist(playlist_id) => {
            library_store
                .playlist(playlist_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
                .ok_or(ApplicationRuntimeError::PlaylistNotFound)?
                .parent_folder_id
        }
        PlaylistItem::SmartPlaylist(smart_playlist_id) => {
            library_store
                .smart_playlist(smart_playlist_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
                .ok_or(ApplicationRuntimeError::SmartPlaylistNotFound)?
                .parent_folder_id
        }
        PlaylistItem::Folder(folder_id) => {
            library_store
                .playlist_folder(folder_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
                .ok_or(ApplicationRuntimeError::PlaylistFolderNotFound)?
                .parent_folder_id
        }
    })
}

fn folder_is_descendant_of(
    library_store: &dyn LibraryStore,
    candidate: PlaylistFolderId,
    ancestor: PlaylistFolderId,
) -> ApplicationRuntimeResult<bool> {
    let folders = library_store
        .playlist_folders()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let mut current = Some(candidate);
    while let Some(node) = current {
        if node == ancestor {
            return Ok(true);
        }
        current = folders
            .iter()
            .find(|folder| folder.id == node)
            .and_then(|folder| folder.parent_folder_id);
    }
    Ok(false)
}

impl ApplicationRuntime {
    pub(super) fn move_playlist_item(
        &mut self,
        item: PlaylistItem,
        target_parent_folder_id: Option<PlaylistFolderId>,
        position: u32,
    ) -> ApplicationRuntimeResult<()> {
        let library_store = self.library_store()?;
        ensure_parent_folder_exists(library_store, target_parent_folder_id)?;

        if let PlaylistItem::Folder(folder_id) = item
            && let Some(target) = target_parent_folder_id
            && folder_is_descendant_of(library_store, target, folder_id)?
        {
            return Err(ApplicationRuntimeError::PlaylistFolderWouldCycle);
        }

        let source_parent = current_parent_of(library_store, item)?;

        if source_parent == target_parent_folder_id {
            let mut siblings: Vec<Sibling> = siblings_in_folder(library_store, source_parent)?
                .into_iter()
                .filter(|sibling| sibling.item != item)
                .collect();
            let target_index = (position as usize).min(siblings.len());
            siblings.insert(
                target_index,
                Sibling {
                    item,
                    position: target_index as u32,
                },
            );
            apply_positions(library_store, &siblings, source_parent)?;
        } else {
            let source_siblings: Vec<Sibling> = siblings_in_folder(library_store, source_parent)?
                .into_iter()
                .filter(|sibling| sibling.item != item)
                .collect();
            apply_positions(library_store, &source_siblings, source_parent)?;

            let mut target_siblings = siblings_in_folder(library_store, target_parent_folder_id)?;
            let target_index = (position as usize).min(target_siblings.len());
            target_siblings.insert(
                target_index,
                Sibling {
                    item,
                    position: target_index as u32,
                },
            );
            apply_positions(library_store, &target_siblings, target_parent_folder_id)?;
        }

        self.reload_playlist_state()
    }
}
