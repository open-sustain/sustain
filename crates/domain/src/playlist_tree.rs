// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The tree the user arranges playlists in: folders, playlists and smart
//! playlists, each under its parent folder in the order the user gave it.
//! The sidebar shows this tree; every other list of playlists follows its
//! order.

use std::collections::BTreeMap;

use crate::{Playlist, PlaylistFolder, PlaylistFolderId, PlaylistItem, SmartPlaylist};

/// One folder, playlist, or smart playlist of a [`PlaylistTree`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaylistTreeNode {
    pub item: PlaylistItem,
    pub name: String,
}

/// Folders, playlists, and smart playlists grouped under their parent
/// folder, each group in the user's order.
#[derive(Debug, Default)]
pub struct PlaylistTree {
    children: BTreeMap<Option<PlaylistFolderId>, Vec<PlaylistTreeNode>>,
}

impl PlaylistTree {
    pub fn new(
        folders: &[PlaylistFolder],
        playlists: &[Playlist],
        smart_playlists: &[SmartPlaylist],
    ) -> Self {
        let mut positioned: BTreeMap<Option<PlaylistFolderId>, Vec<(u32, PlaylistTreeNode)>> =
            BTreeMap::new();
        let mut add = |parent, position, item, name: &str| {
            positioned.entry(parent).or_default().push((
                position,
                PlaylistTreeNode {
                    item,
                    name: name.to_owned(),
                },
            ));
        };
        for folder in folders {
            add(
                folder.parent_folder_id,
                folder.position,
                PlaylistItem::Folder(folder.id),
                &folder.name,
            );
        }
        for playlist in playlists {
            add(
                playlist.parent_folder_id,
                playlist.position,
                PlaylistItem::Playlist(playlist.id),
                &playlist.name,
            );
        }
        for smart in smart_playlists {
            add(
                smart.parent_folder_id,
                smart.position,
                PlaylistItem::SmartPlaylist(smart.id),
                &smart.name,
            );
        }

        let children = positioned
            .into_iter()
            .map(|(parent, mut group)| {
                group.sort_by_key(|(position, node)| (*position, tie_break(node.item)));
                (parent, group.into_iter().map(|(_, node)| node).collect())
            })
            .collect();
        Self { children }
    }

    /// The nodes directly under `parent` (`None`: the top level), in order.
    pub fn children(&self, parent: Option<PlaylistFolderId>) -> &[PlaylistTreeNode] {
        self.children
            .get(&parent)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }

    /// Every node reachable from the top level, in the order the tree
    /// lists them fully expanded: each folder followed by its contents.
    pub fn depth_first(&self) -> Vec<&PlaylistTreeNode> {
        let mut nodes = Vec::new();
        let mut pending: Vec<&PlaylistTreeNode> = self.children(None).iter().rev().collect();
        while let Some(node) = pending.pop() {
            nodes.push(node);
            if let PlaylistItem::Folder(folder_id) = node.item {
                pending.extend(self.children(Some(folder_id)).iter().rev());
            }
        }
        nodes
    }
}

/// Orders siblings that share a position: folders, then playlists, then
/// smart playlists, each by id.
fn tie_break(item: PlaylistItem) -> (u8, i64) {
    match item {
        PlaylistItem::Folder(id) => (0, id.get()),
        PlaylistItem::Playlist(id) => (1, id.get()),
        PlaylistItem::SmartPlaylist(id) => (2, id.get()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlaylistId, SmartPlaylistId, SmartPlaylistMatchKind, SmartPlaylistRuleSet};

    fn folder(id: i64, parent: Option<PlaylistFolderId>, position: u32) -> PlaylistFolder {
        PlaylistFolder {
            id: PlaylistFolderId::new(id).expect("positive id"),
            name: format!("folder {id}"),
            parent_folder_id: parent,
            position,
        }
    }

    fn playlist(id: i64, parent: Option<PlaylistFolderId>, position: u32) -> Playlist {
        Playlist {
            id: PlaylistId::new(id).expect("positive id"),
            name: format!("playlist {id}"),
            parent_folder_id: parent,
            position,
            entries: Vec::new(),
        }
    }

    fn smart_playlist(id: i64, parent: Option<PlaylistFolderId>, position: u32) -> SmartPlaylist {
        SmartPlaylist {
            id: SmartPlaylistId::new(id).expect("positive id"),
            name: format!("smart {id}"),
            parent_folder_id: parent,
            position,
            rules: SmartPlaylistRuleSet {
                match_kind: SmartPlaylistMatchKind::All,
                rules: Vec::new(),
                limit: None,
            },
        }
    }

    fn items(nodes: &[PlaylistTreeNode]) -> Vec<PlaylistItem> {
        nodes.iter().map(|node| node.item).collect()
    }

    #[test]
    fn groups_nodes_by_parent_and_orders_them_by_position() {
        let mixes = folder(1, None, 1);
        let tree = PlaylistTree::new(
            std::slice::from_ref(&mixes),
            &[playlist(1, None, 0), playlist(2, Some(mixes.id), 0)],
            &[smart_playlist(1, None, 2)],
        );

        assert_eq!(
            items(tree.children(None)),
            [
                PlaylistItem::Playlist(PlaylistId::new(1).expect("positive id")),
                PlaylistItem::Folder(mixes.id),
                PlaylistItem::SmartPlaylist(SmartPlaylistId::new(1).expect("positive id")),
            ]
        );
        assert_eq!(
            items(tree.children(Some(mixes.id))),
            [PlaylistItem::Playlist(
                PlaylistId::new(2).expect("positive id")
            )]
        );
        assert!(
            tree.children(Some(PlaylistFolderId::new(999).expect("positive id")))
                .is_empty()
        );
    }

    #[test]
    fn depth_first_lists_each_folder_before_its_contents() {
        let outer = folder(1, None, 1);
        let inner = folder(2, Some(outer.id), 1);
        let tree = PlaylistTree::new(
            &[inner.clone(), outer.clone()],
            &[
                playlist(1, None, 2),
                playlist(2, Some(outer.id), 0),
                playlist(3, Some(inner.id), 0),
            ],
            &[smart_playlist(1, None, 0)],
        );

        let names: Vec<&str> = tree
            .depth_first()
            .into_iter()
            .map(|node| node.name.as_str())
            .collect();
        assert_eq!(
            names,
            [
                "smart 1",
                "folder 1",
                "playlist 2",
                "folder 2",
                "playlist 3",
                "playlist 1"
            ]
        );
    }
}
