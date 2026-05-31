// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Application-level keyboard shortcuts wired as `gio::Action`s with
//! accelerators registered on the [`gtk::Application`].
//!
//! Routing the shortcuts through actions (rather than a raw
//! [`gtk::EventControllerKey`]) keeps them inspectable from the GTK
//! shortcuts overlay and from GNOME's standard accelerator-override
//! plumbing. Each action's callback inspects the live view state at fire
//! time so a Ctrl+I press always operates on the table the user is
//! currently looking at, with no need to swap action enabled-states on
//! every selection change.
//!
//! Committed shortcut scope:
//!
//! - `app.new-playlist`        → Ctrl+N         : new empty playlist
//! - `app.new-smart-playlist`  → Ctrl+Alt+N     : new smart playlist (opens editor)
//! - `app.focus-search`        → Ctrl+F         : focus topbar search & select-all
//! - `app.get-info`            → Ctrl+I         : Get Info on the visible-view selection
//! - `app.show-in-folder`      → Ctrl+R         : reveal first selected track in file manager
//!
//! Ctrl+L (Jump to Current Track) and the spacebar play/pause toggle are
//! kept on the raw key controller in `main_window.rs` because they need
//! focus-aware bypass logic that does not fit cleanly into the action
//! model (don't intercept while a text field has focus).

use std::{collections::HashSet, rc::Rc};

use gtk::gio;
use gtk::prelude::*;
use sustain_app_runtime::{ApplicationCommand, PlaylistId, PlaylistItem};

use super::{
    LibraryChangedHolder, PLAYLISTS_VIEW, SONGS_VIEW, SharedRuntime, TrackRowChangedHolder,
    artwork_loader::ArtworkLoader,
    command_controller::SharedCommandController,
    main_window::SidebarCollapseController,
    sidebar::PlaylistSidebar,
    sidebar_context::{
        NEW_PLAYLIST_DEFAULT_NAME, NEW_SMART_PLAYLIST_DEFAULT_NAME, unique_default_name,
    },
    smart_playlist_editor::{SmartPlaylistEditorMode, open_smart_playlist_editor},
    titlebar::Titlebar,
    track_context::TrackActionInvocation,
    track_context_ops::{get_info_callback, show_in_folder_callback},
    track_table::TrackTable,
};

pub(crate) struct GlobalShortcutContext {
    pub(crate) app: gtk::Application,
    pub(crate) window: gtk::ApplicationWindow,
    pub(crate) command_controller: SharedCommandController,
    pub(crate) runtime: SharedRuntime,
    pub(crate) sidebar: PlaylistSidebar,
    pub(crate) sidebar_collapse: SidebarCollapseController,
    pub(crate) titlebar: Titlebar,
    pub(crate) songs_table: TrackTable,
    pub(crate) playlists_table: TrackTable,
    pub(crate) content_stack: gtk::Stack,
    pub(crate) library_changed_holder: LibraryChangedHolder,
    pub(crate) track_row_changed_holder: TrackRowChangedHolder,
    pub(crate) artwork_loader: ArtworkLoader,
}

pub(crate) fn install_global_shortcuts(context: GlobalShortcutContext) {
    install_new_playlist(&context);
    install_new_smart_playlist(&context);
    install_focus_search(&context);
    install_get_info(&context);
    install_show_in_folder(&context);
}

fn install_new_playlist(context: &GlobalShortcutContext) {
    if context.app.lookup_action("new-playlist").is_some() {
        return;
    }
    let action = gio::SimpleAction::new("new-playlist", None);
    let command_controller = context.command_controller.clone();
    let runtime = context.runtime.clone();
    let sidebar = context.sidebar.clone();
    let sidebar_collapse = context.sidebar_collapse.clone();
    action.connect_activate(move |_action, _parameter| {
        // The just-created playlist row needs to be visible for its
        // armed inline rename to receive visible keystrokes.
        sidebar_collapse.expand_if_collapsed();
        create_new_playlist(&command_controller, &runtime, &sidebar);
    });
    context.app.add_action(&action);
    context
        .app
        .set_accels_for_action("app.new-playlist", &["<Primary>n"]);
}

fn install_new_smart_playlist(context: &GlobalShortcutContext) {
    if context.app.lookup_action("new-smart-playlist").is_some() {
        return;
    }
    let action = gio::SimpleAction::new("new-smart-playlist", None);
    let parent = context.window.clone();
    let command_controller = context.command_controller.clone();
    let runtime = context.runtime.clone();
    let sidebar = context.sidebar.clone();
    let sidebar_collapse = context.sidebar_collapse.clone();
    action.connect_activate(move |_action, _parameter| {
        sidebar_collapse.expand_if_collapsed();
        open_new_smart_playlist_editor(&parent, command_controller.clone(), &runtime, &sidebar);
    });
    context.app.add_action(&action);
    context
        .app
        .set_accels_for_action("app.new-smart-playlist", &["<Primary><Alt>n"]);
}

fn install_focus_search(context: &GlobalShortcutContext) {
    if context.app.lookup_action("focus-search").is_some() {
        return;
    }
    let action = gio::SimpleAction::new("focus-search", None);
    let titlebar = context.titlebar.clone();
    action.connect_activate(move |_action, _parameter| {
        titlebar.focus_search();
    });
    context.app.add_action(&action);
    context
        .app
        .set_accels_for_action("app.focus-search", &["<Primary>f"]);
}

fn install_get_info(context: &GlobalShortcutContext) {
    if context.app.lookup_action("get-info").is_some() {
        return;
    }
    let action = gio::SimpleAction::new("get-info", None);
    let callback = get_info_callback(
        &context.window.clone().upcast::<gtk::Window>(),
        &context.runtime,
        &context.command_controller,
        &context.library_changed_holder,
        &context.track_row_changed_holder,
        &context.artwork_loader,
    );
    let songs_table = context.songs_table.clone();
    let playlists_table = context.playlists_table.clone();
    let content_stack = context.content_stack.clone();
    action.connect_activate(move |_action, _parameter| {
        let selection = current_view_selection(&content_stack, &songs_table, &playlists_table);
        // Get Info is a single-track dialog; the context-menu version
        // requires `Single` selection and we keep the same contract for
        // the keyboard path. No-op rather than guessing which track of a
        // multi-row selection the user meant.
        if selection.len() != 1 {
            return;
        }
        callback(TrackActionInvocation {
            selected_track_ids: selection,
            displayed_track_ids: current_view_order(&content_stack, &songs_table, &playlists_table),
        });
    });
    context.app.add_action(&action);
    context
        .app
        .set_accels_for_action("app.get-info", &["<Primary>i"]);
}

fn install_show_in_folder(context: &GlobalShortcutContext) {
    if context.app.lookup_action("show-in-folder").is_some() {
        return;
    }
    let action = gio::SimpleAction::new("show-in-folder", None);
    let callback = show_in_folder_callback(
        &context.runtime,
        &context.window.clone().upcast::<gtk::Window>(),
    );
    let songs_table = context.songs_table.clone();
    let playlists_table = context.playlists_table.clone();
    let content_stack = context.content_stack.clone();
    action.connect_activate(move |_action, _parameter| {
        let selection = current_view_selection(&content_stack, &songs_table, &playlists_table);
        if selection.is_empty() {
            return;
        }
        // Multi-row scope: act on the first selected track. Opening one
        // file-manager window per track on a large selection would be
        // hostile; cross-folder selections still resolve to a single,
        // predictable parent directory.
        callback(TrackActionInvocation {
            selected_track_ids: selection,
            displayed_track_ids: Vec::new(),
        });
    });
    context.app.add_action(&action);
    context
        .app
        .set_accels_for_action("app.show-in-folder", &["<Primary>r"]);
}

/// Returns the selection from whichever track table is visible right now.
/// The Albums grid is intentionally excluded — it does not expose a
/// per-track selection model, so Get Info / Show in Folder are no-ops
/// in that mode. Callers must tolerate an empty vector for any non-track
/// view.
fn current_view_selection(
    content_stack: &gtk::Stack,
    songs_table: &TrackTable,
    playlists_table: &TrackTable,
) -> Vec<sustain_app_runtime::TrackId> {
    match content_stack.visible_child_name().as_deref() {
        Some(SONGS_VIEW) => songs_table.selected_track_ids(),
        Some(PLAYLISTS_VIEW) => playlists_table.selected_track_ids(),
        _ => Vec::new(),
    }
}

fn current_view_order(
    content_stack: &gtk::Stack,
    songs_table: &TrackTable,
    playlists_table: &TrackTable,
) -> Vec<sustain_app_runtime::TrackId> {
    match content_stack.visible_child_name().as_deref() {
        Some(SONGS_VIEW) => songs_table.ordered_track_ids(),
        Some(PLAYLISTS_VIEW) => playlists_table.ordered_track_ids(),
        _ => Vec::new(),
    }
}

/// Create a fresh empty playlist with a unique default name and refresh
/// the sidebar so the new row is visible immediately. Arms the inline
/// rename on the new row so the user can type the desired name without
/// a second action — matching the iTunes / Sustain sidebar context-menu
/// "New Playlist" flow this helper is shared with.
pub(crate) fn create_new_playlist(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
) {
    let (existing_ids, existing_names): (HashSet<PlaylistId>, Vec<String>) = {
        let runtime = runtime.borrow();
        let ids = runtime.playlists().iter().map(|p| p.id).collect();
        let names = runtime
            .playlists()
            .iter()
            .map(|playlist| playlist.name.clone())
            .collect();
        (ids, names)
    };
    let name = unique_default_name(existing_names, NEW_PLAYLIST_DEFAULT_NAME);
    if command_controller.dispatch_succeeded(ApplicationCommand::CreatePlaylist {
        name,
        parent_folder_id: None,
    }) {
        let new_id = runtime
            .borrow()
            .playlists()
            .iter()
            .map(|playlist| playlist.id)
            .find(|id| !existing_ids.contains(id));
        if let Some(id) = new_id {
            sidebar.arm_pending_rename(PlaylistItem::Playlist(id));
        }
        sidebar.refresh();
    }
}

/// Open the smart-playlist editor pre-populated with a unique default
/// name. On save the sidebar refreshes so the new entry is visible.
/// Shared between the sidebar context-menu "New Smart Playlist" action
/// and the Ctrl+Alt+N keyboard shortcut.
pub(crate) fn open_new_smart_playlist_editor(
    parent: &gtk::ApplicationWindow,
    command_controller: SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
) {
    let existing_names: Vec<String> = runtime
        .borrow()
        .smart_playlists()
        .iter()
        .map(|smart| smart.name.clone())
        .collect();
    let name = unique_default_name(existing_names, NEW_SMART_PLAYLIST_DEFAULT_NAME);
    let sidebar_for_saved = sidebar.clone();
    open_smart_playlist_editor(
        parent,
        command_controller,
        Rc::new(move || sidebar_for_saved.refresh()),
        SmartPlaylistEditorMode::Create { name },
    );
}
