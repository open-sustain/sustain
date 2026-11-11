// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Playlists view: the lazy view activator and dirty-flag rebuild, the
//! playlists-header state projection, the per-playlist/smart-playlist row
//! construction, and the add-to-playlist menu provider and callback.

use super::*;

#[derive(Clone)]
pub(super) struct PlaylistsViewRefreshContext {
    content_stack: ContentStack,
    playlists_table: TrackTable,
    playlists_header: PlaylistsHeader,
    status_bar: StatusBar,
    playlists_dirty: Rc<Cell<bool>>,
}

impl PlaylistsViewRefreshContext {
    pub(super) fn new(
        content_stack: &ContentStack,
        playlists_table: &TrackTable,
        playlists_header: &PlaylistsHeader,
        status_bar: &StatusBar,
        playlists_dirty: &Rc<Cell<bool>>,
    ) -> Self {
        Self {
            content_stack: content_stack.clone(),
            playlists_table: playlists_table.clone(),
            playlists_header: playlists_header.clone(),
            status_bar: status_bar.clone(),
            playlists_dirty: playlists_dirty.clone(),
        }
    }

    pub(super) fn mark_dirty(&self) {
        self.playlists_dirty.set(true);
    }

    pub(super) fn dirty(&self) -> bool {
        self.playlists_dirty.get()
    }

    pub(super) fn is_visible(&self) -> bool {
        self.content_stack.visible_child_name().as_deref() == Some(PLAYLISTS_VIEW)
    }

    pub(super) fn mark_dirty_and_was_dirty(&self) -> bool {
        self.playlists_dirty.replace(true)
    }

    /// Show `item`'s current name in the header, after a rename of the
    /// shown item. Its rows are unchanged, so the table is left alone.
    pub(super) fn retitle(&self, runtime: &ApplicationRuntime, item: PlaylistItem) {
        if let Some(title) = playlist_item_title(runtime, item) {
            self.playlists_header.set_title(&title);
        }
    }
}

/// Mirror of `install_albums_view_activator` for the Playlists view.
/// The table is built empty and stays empty while another page is
/// visible; `library_changed` / selection-changed / search rebuilds
/// flip a `dirty` flag instead of running `replace_rows`. When the
/// user picks a playlist row in the sidebar, the activator pays the
/// rebuild cost once with the current state and clears the flag.
/// Music is the default landing page, so in the common case the
/// playlists table is never populated for a session that does not
/// visit a playlist.
pub(super) fn install_playlists_view_activator(
    refresh_context: &PlaylistsViewRefreshContext,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
    current_search_text: &Rc<RefCell<String>>,
) {
    let runtime = runtime.clone();
    let refresh_context = refresh_context.clone();
    let sidebar = sidebar.clone();
    let current_search_text = current_search_text.clone();
    let content_stack = refresh_context.content_stack.clone();
    content_stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() != Some(PLAYLISTS_VIEW) {
            return;
        }
        if !refresh_context.dirty() {
            return;
        }
        let search_text = current_search_text.borrow().clone();
        rebuild_playlists_view(
            &runtime.borrow(),
            &refresh_context.playlists_table,
            &refresh_context.playlists_header,
            &refresh_context.status_bar,
            sidebar.current_selection(),
            &search_text,
            RowSetUpdate::Replace,
        );
        refresh_context.playlists_dirty.set(false);
    });
}

/// Rebuild the playlists table only when the user is actually looking
/// at it. Triggers that fire while another view is visible (library
/// scan completion, search keystrokes, sidebar selection change) just
/// flip the dirty flag; `install_playlists_view_activator` runs the
/// rebuild on the next visit. See its doc-comment for the rationale.
pub(super) fn refresh_playlists_view_if_visible(
    runtime: &ApplicationRuntime,
    refresh_context: &PlaylistsViewRefreshContext,
    sidebar_selection: Option<SidebarSelection>,
    search_text: &str,
    update: RowSetUpdate,
) -> bool {
    if refresh_context.is_visible() {
        rebuild_playlists_view(
            runtime,
            &refresh_context.playlists_table,
            &refresh_context.playlists_header,
            &refresh_context.status_bar,
            sidebar_selection,
            search_text,
            update,
        );
        refresh_context.playlists_dirty.set(false);
        true
    } else {
        refresh_context.mark_dirty();
        false
    }
}

/// Unconditional rebuild of the playlists view (header + track table)
/// from the current selection + search filter. Header summary is derived
/// from the same row set fed to the table, so the visible "N songs, X
/// duration" text always matches what's drawn below it.
fn rebuild_playlists_view(
    runtime: &ApplicationRuntime,
    playlists_table: &TrackTable,
    playlists_header: &PlaylistsHeader,
    status_bar: &StatusBar,
    sidebar_selection: Option<SidebarSelection>,
    search_text: &str,
    update: RowSetUpdate,
) {
    let rows_profile = sustain_profiler::ProfileScope::start();
    let rows = playlist_table_rows_for(runtime, sidebar_selection, search_text);
    let row_count = rows.len();
    sustain_profiler::profile_mark!(rows_profile, "playlist rows built ({} rows)", row_count);
    let summary = TrackSummary::of_rows(&rows);
    playlists_header.set_state(playlists_header_state_for(
        runtime,
        sidebar_selection,
        summary,
    ));
    status_bar.update_summary(summary);
    let show_profile = sustain_profiler::ProfileScope::start();
    playlists_table.show_rows(rows, update);
    sustain_profiler::profile_mark!(
        show_profile,
        "playlist table rows shown ({update:?}, {} rows)",
        row_count
    );
}

fn playlists_header_state_for(
    runtime: &ApplicationRuntime,
    selection: Option<SidebarSelection>,
    summary: TrackSummary,
) -> Option<PlaylistsHeaderState> {
    // Music / Albums selections do not render the playlists header at
    // all (the stack shows a different child).
    let Some(SidebarSelection::Item(item)) = selection else {
        return None;
    };
    let title = playlist_item_title(runtime, item)?;
    Some(PlaylistsHeaderState { title, summary })
}

/// The name the playlists header shows for a sidebar item. Folders
/// aggregate their children in the sidebar but are not themselves a
/// playable track set, so the header has nothing meaningful to show.
fn playlist_item_title(runtime: &ApplicationRuntime, item: PlaylistItem) -> Option<String> {
    match item {
        PlaylistItem::Playlist(id) => runtime
            .playlists()
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name.clone()),
        PlaylistItem::SmartPlaylist(id) => runtime
            .smart_playlists()
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name.clone()),
        PlaylistItem::Folder(_) => None,
    }
}

pub(super) fn playlist_table_rows_for(
    runtime: &ApplicationRuntime,
    selection: Option<SidebarSelection>,
    search_text: &str,
) -> Vec<TrackTableRow> {
    // Carry the playlist_position alongside each Track so the row built
    // below mirrors PlaylistEntry::position one-to-one for the regular-
    // playlist branch. Library / Smart Playlist selections never have an
    // authoritative play-order, so their pairs hold None — those rows
    // collate equal under the status column sorter and are unaffected by
    // the play-order sort.
    let candidates: Vec<(Track, Option<u32>)> = match selection {
        // The playlists table mirrors the Music view's rows when the
        // Music entry is selected — same library track set, no
        // play-position. (PLAYLISTS_VIEW is not actually shown for
        // Music / Albums, but the table-rebuild path is shared.)
        Some(SidebarSelection::Music) => runtime
            .library_tracks()
            .iter()
            .map(|track| (track.clone(), None))
            .collect(),
        Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) => {
            let Some(playlist) = runtime
                .playlists()
                .iter()
                .find(|playlist| playlist.id == playlist_id)
            else {
                return Vec::new();
            };
            let mut entries: Vec<&PlaylistEntry> = playlist.entries.iter().collect();
            entries.sort_by_key(|entry| entry.position);
            entries
                .into_iter()
                .filter_map(|entry| {
                    runtime
                        .library_track(entry.track_id)
                        .cloned()
                        .map(|track| (track, Some(entry.position)))
                })
                .collect()
        }
        Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(smart_playlist_id))) => runtime
            .smart_playlist_matching_tracks(smart_playlist_id)
            .into_iter()
            .map(|track| (track.clone(), None))
            .collect(),
        _ => return Vec::new(),
    };

    let honor_sort_tags = runtime.settings().library.honor_sort_tags;
    let normalized = normalize_query(search_text);
    candidates
        .into_iter()
        .filter(|(track, _)| normalized.is_empty() || runtime.search_matches(track.id, &normalized))
        .map(|(track, position)| {
            TrackTableRow::from_track(&track, honor_sort_tags).with_playlist_position(position)
        })
        .collect()
}

pub(super) fn add_to_playlist_provider(runtime: &SharedRuntime) -> AddToPlaylistProvider {
    let runtime = runtime.clone();
    Rc::new(move || {
        let runtime = runtime.borrow();
        let folders: HashMap<PlaylistFolderId, &PlaylistFolder> = runtime
            .playlist_folders()
            .iter()
            .map(|folder| (folder.id, folder))
            .collect();
        let mut entries: Vec<AddToPlaylistEntry> = runtime
            .playlists()
            .iter()
            .map(|playlist| AddToPlaylistEntry {
                playlist_id: playlist.id,
                display_path: playlist_display_path(playlist, &folders),
            })
            .collect();
        entries.sort_by(|left, right| {
            left.display_path
                .to_lowercase()
                .cmp(&right.display_path.to_lowercase())
        });
        entries
    })
}

pub(super) fn add_to_playlist_callback(
    command_controller: &SharedCommandController,
    playlist_entries_changed_holder: &PlaylistEntriesChangedHolder,
) -> AddToPlaylistCallback {
    let command_controller = command_controller.clone();
    let playlist_entries_changed_holder = playlist_entries_changed_holder.clone();

    Rc::new(move |playlist_id, track_ids| {
        if track_ids.is_empty() {
            return;
        }
        let dispatched =
            command_controller.dispatch_succeeded(ApplicationCommand::AddTracksToPlaylist {
                playlist_id,
                track_ids,
            });
        if !dispatched {
            return;
        }
        if let Some(callback) = playlist_entries_changed_holder.borrow().as_ref() {
            callback(&PlaylistEntriesChange::Added(playlist_id));
        }
    })
}

fn playlist_display_path(
    playlist: &Playlist,
    folders: &HashMap<PlaylistFolderId, &PlaylistFolder>,
) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut current = playlist.parent_folder_id;
    while let Some(folder_id) = current {
        let Some(folder) = folders.get(&folder_id) else {
            break;
        };
        segments.push(folder.name.clone());
        current = folder.parent_folder_id;
    }
    segments.reverse();
    segments.push(playlist.name.clone());
    segments.join(" / ")
}
