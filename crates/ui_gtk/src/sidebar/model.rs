// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gio, glib};
use sustain_app_runtime::{
    ApplicationRuntime, Playlist, PlaylistFolder, PlaylistFolderId, PlaylistItem, SmartPlaylist,
};

#[derive(Clone, Debug, PartialEq)]
pub(super) struct SidebarItem {
    pub(super) name: String,
    pub(super) item: PlaylistItem,
}

impl SidebarItem {
    pub(super) fn icon_name(&self) -> &'static str {
        match self.item {
            PlaylistItem::Folder(_) => "folder-symbolic",
            PlaylistItem::Playlist(_) => "view-list-symbolic",
            PlaylistItem::SmartPlaylist(_) => "emblem-system-symbolic",
        }
    }
}

#[derive(Default)]
struct SidebarSnapshot {
    items_by_parent: BTreeMap<Option<PlaylistFolderId>, Vec<SidebarItem>>,
}

impl SidebarSnapshot {
    fn from_runtime(runtime: &ApplicationRuntime) -> Self {
        Self::build(
            runtime.playlist_folders(),
            runtime.playlists(),
            runtime.smart_playlists(),
        )
    }

    fn build(
        folders: &[PlaylistFolder],
        playlists: &[Playlist],
        smart_playlists: &[SmartPlaylist],
    ) -> Self {
        let mut items_by_parent: BTreeMap<Option<PlaylistFolderId>, Vec<(u32, SidebarItem)>> =
            BTreeMap::new();

        for folder in folders {
            items_by_parent
                .entry(folder.parent_folder_id)
                .or_default()
                .push((
                    folder.position,
                    SidebarItem {
                        name: folder.name.clone(),
                        item: PlaylistItem::Folder(folder.id),
                    },
                ));
        }
        for playlist in playlists {
            items_by_parent
                .entry(playlist.parent_folder_id)
                .or_default()
                .push((
                    playlist.position,
                    SidebarItem {
                        name: playlist.name.clone(),
                        item: PlaylistItem::Playlist(playlist.id),
                    },
                ));
        }
        for smart in smart_playlists {
            items_by_parent
                .entry(smart.parent_folder_id)
                .or_default()
                .push((
                    smart.position,
                    SidebarItem {
                        name: smart.name.clone(),
                        item: PlaylistItem::SmartPlaylist(smart.id),
                    },
                ));
        }

        let items_by_parent = items_by_parent
            .into_iter()
            .map(|(parent, mut bucket)| {
                bucket.sort_by_key(|(position, _)| *position);
                (parent, bucket.into_iter().map(|(_, item)| item).collect())
            })
            .collect();

        Self { items_by_parent }
    }

    fn items_under(&self, parent: Option<PlaylistFolderId>) -> &[SidebarItem] {
        self.items_by_parent
            .get(&parent)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
}

pub(super) fn selected_item(selection: &gtk::SingleSelection) -> Option<PlaylistItem> {
    let row = selection
        .selected_item()
        .and_downcast::<gtk::TreeListRow>()?;
    playlist_item_of(&row.item()?)
}

pub(super) fn select_item(selection: &gtk::SingleSelection, target: PlaylistItem) -> bool {
    let position = row_position(selection, target);
    selection.set_selected(position.unwrap_or(gtk::INVALID_LIST_POSITION));
    position.is_some()
}

/// The position of `target`'s row among the tree's visible rows.
pub(super) fn row_position(rows: &impl IsA<gio::ListModel>, target: PlaylistItem) -> Option<u32> {
    (0..rows.n_items()).find(|&position| {
        rows.item(position)
            .and_downcast::<gtk::TreeListRow>()
            .and_then(|row| row.item())
            .and_then(|object| playlist_item_of(&object))
            == Some(target)
    })
}

/// The sidebar item a list row is bound to right now. Row controllers
/// hold their list item weakly, since the item owns the row's widgets.
pub(super) fn bound_sidebar_item(list_item: &glib::WeakRef<gtk::ListItem>) -> Option<SidebarItem> {
    let object = bound_row_object(list_item)?;
    let boxed = object.downcast_ref::<glib::BoxedAnyObject>()?;
    boxed
        .try_borrow::<SidebarItem>()
        .ok()
        .map(|item| item.clone())
}

/// The playlist item a list row is bound to right now.
pub(super) fn bound_playlist_item(
    list_item: &glib::WeakRef<gtk::ListItem>,
) -> Option<PlaylistItem> {
    playlist_item_of(&bound_row_object(list_item)?)
}

fn bound_row_object(list_item: &glib::WeakRef<gtk::ListItem>) -> Option<glib::Object> {
    list_item
        .upgrade()?
        .item()
        .and_downcast::<gtk::TreeListRow>()?
        .item()
}

/// The playlist item a tree row's inner object stands for.
pub(super) fn playlist_item_of(object: &glib::Object) -> Option<PlaylistItem> {
    let boxed = object.downcast_ref::<glib::BoxedAnyObject>()?;
    let sidebar_item = boxed.try_borrow::<SidebarItem>().ok()?;
    Some(sidebar_item.item)
}

/// The sidebar's playlist tree. Built once; [`Self::update`] brings it in
/// line with the runtime in place, so the list keeps its scroll position
/// and the folders the user collapsed stay collapsed.
pub(super) struct SidebarTree {
    model: gtk::TreeListModel,
    root: gio::ListStore,
    snapshot: Rc<RefCell<SidebarSnapshot>>,
    /// The child store of each folder whose row is expanded. The tree
    /// creates one when a folder expands and drops it when it collapses.
    folder_stores: Rc<RefCell<HashMap<PlaylistFolderId, glib::WeakRef<gio::ListStore>>>>,
}

impl SidebarTree {
    pub(super) fn new(runtime: &ApplicationRuntime) -> Self {
        Self::with_snapshot(SidebarSnapshot::from_runtime(runtime))
    }

    fn with_snapshot(snapshot: SidebarSnapshot) -> Self {
        let snapshot = Rc::new(RefCell::new(snapshot));
        let root = list_store_for(snapshot.borrow().items_under(None));
        let folder_stores: Rc<RefCell<HashMap<_, glib::WeakRef<gio::ListStore>>>> = Rc::default();
        let model = {
            let snapshot = snapshot.clone();
            let folder_stores = folder_stores.clone();
            gtk::TreeListModel::new(root.clone(), false, true, move |object| {
                let folder_id = folder_of(object)?;
                let store = list_store_for(snapshot.borrow().items_under(Some(folder_id)));
                folder_stores
                    .borrow_mut()
                    .insert(folder_id, store.downgrade());
                Some(store.upcast())
            })
        };
        Self {
            model,
            root,
            snapshot,
            folder_stores,
        }
    }

    pub(super) fn model(&self) -> &gtk::TreeListModel {
        &self.model
    }

    /// Bring the tree in line with the runtime's folders, playlists, and
    /// smart playlists. Each level changes by one splice over the span
    /// that differs, so rows outside it keep their widgets.
    pub(super) fn update(&self, runtime: &ApplicationRuntime) {
        self.apply(SidebarSnapshot::from_runtime(runtime));
    }

    fn apply(&self, snapshot: SidebarSnapshot) {
        let collapsed = self.collapsed_folders();
        *self.snapshot.borrow_mut() = snapshot;

        let root_items = self.snapshot.borrow().items_under(None).to_vec();
        splice_to(&self.root, &root_items);
        // Folders the root splice (re)created built their stores from the
        // new snapshot; the ones it kept still hold the old contents.
        let expanded_folders: Vec<(PlaylistFolderId, gio::ListStore)> = {
            let mut folder_stores = self.folder_stores.borrow_mut();
            folder_stores.retain(|_, store| store.upgrade().is_some());
            folder_stores
                .iter()
                .filter_map(|(folder_id, store)| Some((*folder_id, store.upgrade()?)))
                .collect()
        };
        for (folder_id, store) in expanded_folders {
            let items = self.snapshot.borrow().items_under(Some(folder_id)).to_vec();
            splice_to(&store, &items);
        }

        // A folder row the splice recreated comes back expanded.
        self.collapse(&collapsed);
    }

    fn collapsed_folders(&self) -> HashSet<PlaylistFolderId> {
        (0..self.model.n_items())
            .filter_map(|position| self.model.row(position))
            .filter(|row| row.is_expandable() && !row.is_expanded())
            .filter_map(|row| folder_of(&row.item()?))
            .collect()
    }

    fn collapse(&self, folders: &HashSet<PlaylistFolderId>) {
        // Collapsing a row removes its children after it, so the walk
        // re-reads the length as it goes.
        let mut position = 0;
        while let Some(row) = self.model.row(position) {
            if row.is_expanded()
                && row
                    .item()
                    .and_then(|object| folder_of(&object))
                    .is_some_and(|folder_id| folders.contains(&folder_id))
            {
                row.set_expanded(false);
            }
            position += 1;
        }
    }
}

fn folder_of(object: &glib::Object) -> Option<PlaylistFolderId> {
    match playlist_item_of(object)? {
        PlaylistItem::Folder(folder_id) => Some(folder_id),
        PlaylistItem::Playlist(_) | PlaylistItem::SmartPlaylist(_) => None,
    }
}

fn list_store_for(items: &[SidebarItem]) -> gio::ListStore {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    for item in items {
        store.append(&glib::BoxedAnyObject::new(item.clone()));
    }
    store
}

/// Make `store` hold `target` with a single splice: the leading and
/// trailing items it already shares with `target` stay, the span between
/// them is replaced.
fn splice_to(store: &gio::ListStore, target: &[SidebarItem]) {
    let current: Vec<SidebarItem> = store
        .iter::<glib::BoxedAnyObject>()
        .filter_map(Result::ok)
        .filter_map(|boxed| {
            boxed
                .try_borrow::<SidebarItem>()
                .ok()
                .map(|item| item.clone())
        })
        .collect();
    let (removed, added) = differing_span(&current, target);
    if removed.is_empty() && added.is_empty() {
        return;
    }
    let replacements: Vec<glib::BoxedAnyObject> = target[added.clone()]
        .iter()
        .map(|item| glib::BoxedAnyObject::new(item.clone()))
        .collect();
    store.splice(removed.start as u32, removed.len() as u32, &replacements);
}

/// The span of `current` to replace, and the span of `target` to replace
/// it with, leaving the longest shared prefix and suffix in place.
fn differing_span(
    current: &[SidebarItem],
    target: &[SidebarItem],
) -> (std::ops::Range<usize>, std::ops::Range<usize>) {
    let prefix = current
        .iter()
        .zip(target)
        .take_while(|(current, target)| current == target)
        .count();
    let suffix = current[prefix..]
        .iter()
        .rev()
        .zip(target[prefix..].iter().rev())
        .take_while(|(current, target)| current == target)
        .count();
    (
        prefix..current.len() - suffix,
        prefix..target.len() - suffix,
    )
}

#[cfg(test)]
mod tests {
    use sustain_app_runtime::{
        PlaylistId, SmartPlaylistId, SmartPlaylistMatchKind, SmartPlaylistRule,
        SmartPlaylistRuleSet, SmartPlaylistTextField, SmartPlaylistTextOperator,
    };

    use super::*;

    fn folder(
        id: i64,
        name: &str,
        parent: Option<PlaylistFolderId>,
        position: u32,
    ) -> PlaylistFolder {
        PlaylistFolder {
            id: PlaylistFolderId::new(id).expect("positive folder id"),
            name: name.to_owned(),
            parent_folder_id: parent,
            position,
        }
    }

    fn playlist(id: i64, name: &str, parent: Option<PlaylistFolderId>, position: u32) -> Playlist {
        Playlist {
            id: PlaylistId::new(id).expect("positive playlist id"),
            name: name.to_owned(),
            parent_folder_id: parent,
            position,
            entries: Vec::new(),
        }
    }

    fn smart_playlist(
        id: i64,
        name: &str,
        parent: Option<PlaylistFolderId>,
        position: u32,
    ) -> SmartPlaylist {
        SmartPlaylist {
            id: SmartPlaylistId::new(id).expect("positive smart playlist id"),
            name: name.to_owned(),
            parent_folder_id: parent,
            position,
            rules: SmartPlaylistRuleSet {
                match_kind: SmartPlaylistMatchKind::All,
                rules: vec![SmartPlaylistRule::Text {
                    field: SmartPlaylistTextField::Genre,
                    operator: SmartPlaylistTextOperator::Is,
                    value: "Trip-Hop".to_owned(),
                }],
                limit: None,
            },
        }
    }

    #[test]
    fn snapshot_groups_items_by_parent_and_orders_them_by_position() {
        let root_folder = folder(1, "Mixes", None, 1);
        let root_playlist = playlist(1, "Drive", None, 0);
        let root_smart = smart_playlist(1, "Recent", None, 2);
        let nested_playlist = playlist(2, "Inside", Some(root_folder.id), 0);

        let snapshot = SidebarSnapshot::build(
            std::slice::from_ref(&root_folder),
            &[root_playlist.clone(), nested_playlist.clone()],
            std::slice::from_ref(&root_smart),
        );

        let root_items: Vec<PlaylistItem> = snapshot
            .items_under(None)
            .iter()
            .map(|item| item.item)
            .collect();
        assert_eq!(
            root_items,
            vec![
                PlaylistItem::Playlist(root_playlist.id),
                PlaylistItem::Folder(root_folder.id),
                PlaylistItem::SmartPlaylist(root_smart.id),
            ]
        );

        let nested_items: Vec<PlaylistItem> = snapshot
            .items_under(Some(root_folder.id))
            .iter()
            .map(|item| item.item)
            .collect();
        assert_eq!(
            nested_items,
            vec![PlaylistItem::Playlist(nested_playlist.id)]
        );

        assert!(
            snapshot
                .items_under(Some(PlaylistFolderId::new(999).expect("positive id")))
                .is_empty()
        );
    }

    fn named(item: PlaylistItem, name: &str) -> SidebarItem {
        SidebarItem {
            name: name.to_owned(),
            item,
        }
    }

    #[test]
    fn differing_span_leaves_the_shared_prefix_and_suffix() {
        let id = |id| PlaylistItem::Playlist(PlaylistId::new(id).expect("positive id"));
        let [a, b, c, d] = [1, 2, 3, 4].map(|n| named(id(n), &n.to_string()));
        let b_renamed = named(b.item, "renamed");
        let current = [a.clone(), b.clone(), c.clone(), d.clone()];

        assert_eq!(differing_span(&current, &current), (4..4, 4..4));
        assert_eq!(
            differing_span(&current, &[a.clone(), b_renamed, c.clone(), d.clone()]),
            (1..2, 1..2),
            "a rename replaces only its row"
        );
        assert_eq!(
            differing_span(&current, &[a.clone(), c.clone(), d.clone()]),
            (1..2, 1..1),
            "a deletion removes only its row"
        );
        assert_eq!(
            differing_span(
                &current,
                &[a.clone(), b.clone(), c.clone(), d.clone(), a.clone()]
            ),
            (4..4, 4..5),
            "an addition inserts only its row"
        );
        assert_eq!(
            differing_span(&current, &[a, c, d, b]),
            (1..4, 1..4),
            "a move replaces the rows it moved across"
        );
    }

    fn row_names(tree: &SidebarTree) -> Vec<String> {
        (0..tree.model().n_items())
            .filter_map(|position| tree.model().row(position)?.item())
            .filter_map(|object| {
                let boxed = object.downcast::<glib::BoxedAnyObject>().ok()?;
                let name = boxed.try_borrow::<SidebarItem>().ok()?.name.clone();
                Some(name)
            })
            .collect()
    }

    /// An update changes only the rows that differ: the rest keep their
    /// objects (and so their widgets), and a folder the user collapsed
    /// stays collapsed even when the update recreates its row.
    #[test]
    fn an_update_keeps_unchanged_rows_and_collapsed_folders() {
        let ran = crate::test_support::with_gtk(|| {
            let mixes = folder(1, "Mixes", None, 1);
            let snapshot = |inside_name: &str, other_position: u32, four_position: u32| {
                let other = folder(2, "Other", None, other_position);
                SidebarSnapshot::build(
                    &[mixes.clone(), other.clone()],
                    &[
                        playlist(1, "One", None, 0),
                        playlist(2, inside_name, Some(mixes.id), 0),
                        playlist(3, "Deep", Some(other.id), 0),
                        playlist(4, "Four", None, four_position),
                    ],
                    &[],
                )
            };
            let tree = SidebarTree::with_snapshot(snapshot("Inside", 2, 3));
            assert_eq!(
                row_names(&tree),
                ["One", "Mixes", "Inside", "Other", "Deep", "Four"]
            );
            let first_row_item = tree.model().row(0).and_then(|row| row.item());
            tree.model()
                .row(3)
                .expect("the Other folder row")
                .set_expanded(false);

            // Rename the playlist inside Mixes; move Four above Other.
            tree.apply(snapshot("Renamed", 3, 2));

            assert_eq!(
                row_names(&tree),
                ["One", "Mixes", "Renamed", "Four", "Other"],
                "Other was recreated by the move and stays collapsed"
            );
            assert_eq!(
                tree.model().row(0).and_then(|row| row.item()),
                first_row_item,
                "an unchanged row keeps its object"
            );
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
