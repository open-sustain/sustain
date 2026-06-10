// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Callbacks the playlist sidebar fires: context-menu actions (new
//! playlist/folder/smart playlist, rename, delete, edit), per-playlist
//! analysis/retrieve runs, drag-to-move reordering, selection-driven view
//! switches, and track drops onto a playlist.

use super::*;

pub(super) fn sidebar_action_callback(
    parent: &gtk::ApplicationWindow,
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
) -> SidebarActionCallback {
    let parent = parent.clone();
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |action| match action {
        SidebarContextAction::Playlist => {
            create_new_playlist(&command_controller, &runtime, &sidebar);
        }
        SidebarContextAction::PlaylistFolder => {
            let existing_names: Vec<String> = runtime
                .borrow()
                .playlist_folders()
                .iter()
                .map(|folder| folder.name.clone())
                .collect();
            let name = unique_default_name(existing_names, NEW_PLAYLIST_FOLDER_DEFAULT_NAME);
            if command_controller.dispatch_succeeded(ApplicationCommand::CreatePlaylistFolder {
                name,
                parent_folder_id: None,
            }) {
                sidebar.refresh();
            }
        }
        SidebarContextAction::SmartPlaylist => {
            open_new_smart_playlist_editor(&parent, command_controller.clone(), &runtime, &sidebar);
        }
    })
}

pub(super) fn sidebar_rename_callback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
) -> crate::sidebar::SidebarRenameCallback {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |item, new_name| {
        let dispatched = match item {
            PlaylistItem::Playlist(playlist_id) => {
                command_controller.dispatch_succeeded(ApplicationCommand::RenamePlaylist {
                    playlist_id,
                    name: new_name,
                })
            }
            PlaylistItem::Folder(folder_id) => {
                command_controller.dispatch_succeeded(ApplicationCommand::RenamePlaylistFolder {
                    folder_id,
                    name: new_name,
                })
            }
            PlaylistItem::SmartPlaylist(smart_playlist_id) => {
                let Some(rules) = runtime
                    .borrow()
                    .smart_playlists()
                    .iter()
                    .find(|smart| smart.id == smart_playlist_id)
                    .map(|smart| smart.rules.clone())
                else {
                    return;
                };
                command_controller.dispatch_succeeded(ApplicationCommand::UpdateSmartPlaylist {
                    smart_playlist_id,
                    name: new_name,
                    rules,
                })
            }
        };
        if dispatched {
            sidebar.refresh();
        }
    })
}

pub(super) fn sidebar_edit_smart_playlist_callback(
    parent: &gtk::ApplicationWindow,
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
) -> crate::sidebar::SidebarEditSmartPlaylistCallback {
    let parent = parent.clone();
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |smart_playlist_id| {
        let snapshot = runtime
            .borrow()
            .smart_playlists()
            .iter()
            .find(|smart| smart.id == smart_playlist_id)
            .map(|smart| (smart.name.clone(), smart.rules.clone()));
        let Some((name, rules)) = snapshot else {
            return;
        };
        let sidebar_for_saved = sidebar.clone();
        open_smart_playlist_editor(
            &parent,
            command_controller.clone(),
            Rc::new(move || sidebar_for_saved.refresh()),
            SmartPlaylistEditorMode::Edit {
                smart_playlist_id,
                name,
                rules,
            },
        );
    })
}

pub(super) fn sidebar_delete_callback(
    command_controller: &SharedCommandController,
    sidebar: &PlaylistSidebar,
) -> crate::sidebar::SidebarDeleteCallback {
    let command_controller = command_controller.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |item| {
        let dispatched = match item {
            PlaylistItem::Playlist(playlist_id) => command_controller
                .dispatch_succeeded(ApplicationCommand::DeletePlaylist { playlist_id }),
            PlaylistItem::Folder(folder_id) => command_controller
                .dispatch_succeeded(ApplicationCommand::DeletePlaylistFolder { folder_id }),
            PlaylistItem::SmartPlaylist(smart_playlist_id) => command_controller
                .dispatch_succeeded(ApplicationCommand::DeleteSmartPlaylist { smart_playlist_id }),
        };
        if dispatched {
            sidebar.refresh();
        }
    })
}

pub(super) fn sidebar_analysis_run_callback(
    runtime: &SharedRuntime,
) -> crate::sidebar::SidebarAnalysisRunCallback {
    let runtime = runtime.clone();
    Rc::new(move |item, request| {
        // The runtime decides accept vs deny (based on the global
        // setting) and pushes the matching ephemeral notification; we
        // don't act on the return value.
        let _ = runtime
            .borrow_mut()
            .request_playlist_analysis_run(item, request);
    })
}

pub(super) fn sidebar_online_run_callback(
    runtime: &SharedRuntime,
) -> crate::sidebar::SidebarOnlineRunCallback {
    let runtime = runtime.clone();
    Rc::new(move |item, request| {
        let _ = runtime
            .borrow_mut()
            .request_playlist_online_run(item, request);
    })
}

pub(super) fn sidebar_analysis_enabled_query(
    runtime: &SharedRuntime,
) -> crate::sidebar::SidebarAnalysisEnabledQuery {
    let runtime = runtime.clone();
    Rc::new(move |capability| analysis_capability_enabled(&runtime, capability))
}

pub(super) fn sidebar_online_busy_query(
    runtime: &SharedRuntime,
) -> crate::sidebar::SidebarOnlineBusyQuery {
    let runtime = runtime.clone();
    Rc::new(move || runtime.borrow().is_online_retrieval_running())
}

pub(super) fn sidebar_move_callback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
) -> crate::sidebar::SidebarMoveCallback {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |source, target, position| {
        let Some((target_parent_folder_id, target_position)) =
            resolve_move_target(&runtime.borrow(), source, target, position)
        else {
            return;
        };
        if command_controller.dispatch_succeeded(ApplicationCommand::MovePlaylistItem {
            item: source,
            target_parent_folder_id,
            position: target_position,
        }) {
            sidebar.refresh();
        }
    })
}

fn resolve_move_target(
    runtime: &ApplicationRuntime,
    source: PlaylistItem,
    target: PlaylistItem,
    drop_position: crate::sidebar::DropPosition,
) -> Option<(Option<sustain_app_runtime::PlaylistFolderId>, u32)> {
    use crate::sidebar::DropPosition;
    if source == target {
        return None;
    }
    let (target_parent, target_index) = match target {
        PlaylistItem::Folder(folder_id) => {
            if matches!(drop_position, DropPosition::Into) {
                let child_count = runtime
                    .playlist_folders()
                    .iter()
                    .filter(|folder| folder.parent_folder_id == Some(folder_id))
                    .count()
                    + runtime
                        .playlists()
                        .iter()
                        .filter(|playlist| playlist.parent_folder_id == Some(folder_id))
                        .count()
                    + runtime
                        .smart_playlists()
                        .iter()
                        .filter(|smart| smart.parent_folder_id == Some(folder_id))
                        .count();
                return Some((Some(folder_id), child_count as u32));
            }
            let folder = runtime
                .playlist_folders()
                .iter()
                .find(|folder| folder.id == folder_id)?;
            (folder.parent_folder_id, folder.position)
        }
        PlaylistItem::Playlist(target_id) => {
            let playlist = runtime
                .playlists()
                .iter()
                .find(|playlist| playlist.id == target_id)?;
            (playlist.parent_folder_id, playlist.position)
        }
        PlaylistItem::SmartPlaylist(target_id) => {
            let smart = runtime
                .smart_playlists()
                .iter()
                .find(|smart| smart.id == target_id)?;
            (smart.parent_folder_id, smart.position)
        }
    };

    let position = match drop_position {
        DropPosition::Above => target_index,
        DropPosition::Below => target_index.saturating_add(1),
        DropPosition::Into => target_index,
    };
    Some((target_parent, position))
}

pub(super) fn sidebar_selection_changed_callback(
    runtime: &SharedRuntime,
    playlists_table: &TrackTable,
    playlists_refresh: &PlaylistsViewRefreshContext,
    content_stack: &gtk::Stack,
    visible_summary_refresh: VisibleSummaryRefreshCallback,
    current_search_text: &Rc<RefCell<String>>,
) -> crate::sidebar::SidebarSelectionChangedCallback {
    let runtime = runtime.clone();
    let playlists_table = playlists_table.clone();
    let playlists_refresh = playlists_refresh.clone();
    let content_stack = content_stack.clone();
    let current_search_text = current_search_text.clone();

    Rc::new(move |selection| {
        let selection_profile = sustain_profiler::ProfileScope::start();
        sustain_profiler::profile_mark!(
            selection_profile,
            "playlist selection: sidebar callback entered (kind={})",
            selection_profile_kind(selection)
        );
        // Layout + default sort are cheap and harmless even when the
        // playlists view is not visible — they only set widget state
        // that any future visit will rely on.
        let regular_playlist_selected = matches!(
            selection,
            Some(SidebarSelection::Item(PlaylistItem::Playlist(_)))
        );
        let layout_profile = sustain_profiler::ProfileScope::start();
        if let Some(mut layout) = layout_for_selection(&runtime.borrow(), selection) {
            // A regular playlist always lands on the play-order (status) sort
            // applied just below, so drop any persisted column sort here: letting
            // apply_layout install it would only have apply_playlist_default_sort
            // immediately resort the whole model a second time. Avoiding that
            // double resort is part of the #226 playlist-switch freeze fix; do
            // not restore the sort on this path without re-measuring switches.
            if regular_playlist_selected {
                layout.sort = None;
            }
            playlists_table.apply_layout(&layout);
        }
        if regular_playlist_selected {
            playlists_table.apply_playlist_default_sort();
        }
        sustain_profiler::profile_mark!(
            layout_profile,
            "playlist selection: column-layout load/apply phase"
        );
        // The sidebar selection is the sole driver of the content
        // stack: Music → SONGS_VIEW, Albums → ALBUMS_VIEW, a playlist
        // item → PLAYLISTS_VIEW. A null selection means nothing is
        // active and we fall back to the cheap Songs page.
        let target = match selection {
            Some(SidebarSelection::Music) | None => SONGS_VIEW,
            Some(SidebarSelection::Albums) => ALBUMS_VIEW,
            Some(SidebarSelection::Statistics) => STATISTICS_VIEW,
            Some(SidebarSelection::Item(_)) => PLAYLISTS_VIEW,
        };
        let switching_to_playlists = target == PLAYLISTS_VIEW
            && content_stack.visible_child_name().as_deref() != Some(PLAYLISTS_VIEW);
        if switching_to_playlists {
            playlists_refresh.mark_dirty();
        }
        let stack_profile = sustain_profiler::ProfileScope::start();
        if content_stack.visible_child_name().as_deref() != Some(target) {
            content_stack.set_visible_child_name(target);
        }
        sustain_profiler::profile_mark!(
            stack_profile,
            "playlist selection: stack switch phase (target={target})"
        );
        let search_text = current_search_text.borrow().clone();
        let playlists_refreshed = if switching_to_playlists && !playlists_refresh.dirty() {
            true
        } else {
            refresh_playlists_view_if_visible(
                &runtime.borrow(),
                &playlists_refresh,
                selection,
                &search_text,
            )
        };
        if !playlists_refreshed {
            visible_summary_refresh();
        }
    })
}

fn selection_profile_kind(selection: Option<SidebarSelection>) -> &'static str {
    match selection {
        Some(SidebarSelection::Music) => "music",
        Some(SidebarSelection::Albums) => "albums",
        Some(SidebarSelection::Statistics) => "statistics",
        Some(SidebarSelection::Item(PlaylistItem::Playlist(_))) => "regular-playlist",
        Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(_))) => "smart-playlist",
        Some(SidebarSelection::Item(PlaylistItem::Folder(_))) => "folder",
        None => "none",
    }
}

pub(super) fn sidebar_tracks_drop_callback(
    command_controller: &SharedCommandController,
    library_changed_holder: &LibraryChangedHolder,
) -> crate::sidebar::SidebarTracksDropCallback {
    let command_controller = command_controller.clone();
    let library_changed_holder = library_changed_holder.clone();

    Rc::new(move |target, track_ids| {
        let PlaylistItem::Playlist(playlist_id) = target else {
            return;
        };
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
        if let Some(callback) = library_changed_holder.borrow().as_ref() {
            callback();
        }
    })
}
