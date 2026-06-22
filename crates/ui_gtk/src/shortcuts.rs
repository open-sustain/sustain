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
//! Actions that conflict with text editing are disabled while an
//! [`gtk::Editable`] owns focus. That keeps the full shortcut surface in the
//! application action registry without stealing Space, Ctrl+A, Ctrl+L, or
//! Ctrl+Left / Ctrl+Right from text fields.

use std::{collections::HashSet, rc::Rc};

use gtk::gio;
use gtk::prelude::*;
use sustain_app_runtime::{ApplicationCommand, PlaybackCommand, PlaylistId, PlaylistItem};

use super::{
    ALBUMS_VIEW, DUPLICATES_VIEW, LibraryChangedHolder, PLAYLISTS_VIEW, PlaybackChangedCallback,
    SONGS_VIEW, SharedRuntime, TrackRowChangedHolder, TrackRowsChangedHolder,
    albums::AlbumsView,
    artwork_loader::ArtworkLoader,
    command_controller::SharedCommandController,
    duplicates::DuplicatesView,
    main_window::SidebarCollapseController,
    sidebar::PlaylistSidebar,
    sidebar_context::{
        NEW_PLAYLIST_DEFAULT_NAME, NEW_SMART_PLAYLIST_DEFAULT_NAME, unique_default_name,
    },
    smart_playlist_editor::{SmartPlaylistEditorMode, open_smart_playlist_editor},
    titlebar::Titlebar,
    track_context::{TrackActionInvocation, TrackContextInvocationState},
    track_context_ops::{GetInfoCallbackContext, get_info_callback, show_in_folder_callback},
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
    pub(crate) duplicates_view: DuplicatesView,
    pub(crate) albums_view: AlbumsView,
    pub(crate) content_stack: gtk::Stack,
    pub(crate) toggle_or_start_playback: Rc<dyn Fn()>,
    pub(crate) playback_changed: PlaybackChangedCallback,
    pub(crate) library_changed_holder: LibraryChangedHolder,
    pub(crate) track_row_changed_holder: TrackRowChangedHolder,
    pub(crate) track_rows_changed_holder: TrackRowsChangedHolder,
    pub(crate) artwork_loader: ArtworkLoader,
    pub(crate) track_context_invocation: TrackContextInvocationState,
}

pub(crate) fn install_global_shortcuts(context: GlobalShortcutContext) {
    let mut focus_aware_actions = Vec::new();

    install_new_playlist(&context);
    install_new_smart_playlist(&context);
    install_focus_search(&context);
    install_get_info(&context);
    install_show_in_folder(&context);
    install_play_pause(&context, &mut focus_aware_actions);
    install_previous_track(&context, &mut focus_aware_actions);
    install_next_track(&context, &mut focus_aware_actions);
    install_jump_to_current_track(&context, &mut focus_aware_actions);
    install_select_all(&context, &mut focus_aware_actions);
    install_close_window(&context);
    install_quit(&context);
    install_shortcuts_overlay(&context);
    install_focus_aware_action_state(&context.window, focus_aware_actions);
}

#[derive(Clone, Copy)]
struct ShortcutDescription {
    action: &'static str,
    accelerator: &'static str,
    title: &'static str,
}

const fn shortcut(
    action: &'static str,
    accelerator: &'static str,
    title: &'static str,
) -> ShortcutDescription {
    ShortcutDescription {
        action,
        accelerator,
        title,
    }
}

const NEW_PLAYLIST: ShortcutDescription = shortcut("new-playlist", "<Primary>n", "New playlist");
const NEW_SMART_PLAYLIST: ShortcutDescription = shortcut(
    "new-smart-playlist",
    "<Primary><Alt>n",
    "New smart playlist",
);
const FOCUS_SEARCH: ShortcutDescription =
    shortcut("focus-search", "<Primary>f", "Focus search bar");
const GET_INFO: ShortcutDescription = shortcut("get-info", "<Primary>i", "Get Info");
const SHOW_IN_FOLDER: ShortcutDescription = shortcut(
    "show-in-folder",
    "<Primary>r",
    "Reveal selected track in Files",
);
const PLAY_PAUSE: ShortcutDescription = shortcut("play-pause", "space", "Play / pause");
const PREVIOUS_TRACK: ShortcutDescription =
    shortcut("previous-track", "<Primary>Left", "Previous track");
const NEXT_TRACK: ShortcutDescription = shortcut("next-track", "<Primary>Right", "Next track");
const JUMP_TO_CURRENT_TRACK: ShortcutDescription = shortcut(
    "jump-to-current-track",
    "<Primary>l",
    "Jump to currently playing track",
);
const SELECT_ALL: ShortcutDescription = shortcut("select-all", "<Primary>a", "Select all tracks");
const CLOSE_WINDOW: ShortcutDescription = shortcut("close-window", "<Primary>w", "Close window");
const QUIT: ShortcutDescription = shortcut("quit", "<Primary>q", "Quit Sustain");
const SHOW_SHORTCUTS: ShortcutDescription =
    shortcut("show-shortcuts", "<Primary>question", "Keyboard shortcuts");
pub(crate) const PREFERENCES_ACCELERATOR: &str = "<Primary>comma";
const PREFERENCES: ShortcutDescription =
    shortcut("preferences", PREFERENCES_ACCELERATOR, "Preferences");

const PLAYBACK_SHORTCUTS: &[ShortcutDescription] = &[
    PLAY_PAUSE,
    PREVIOUS_TRACK,
    NEXT_TRACK,
    JUMP_TO_CURRENT_TRACK,
];
const LIBRARY_SHORTCUTS: &[ShortcutDescription] = &[
    NEW_PLAYLIST,
    NEW_SMART_PLAYLIST,
    FOCUS_SEARCH,
    SELECT_ALL,
    GET_INFO,
    SHOW_IN_FOLDER,
];
const APPLICATION_SHORTCUTS: &[ShortcutDescription] =
    &[PREFERENCES, SHOW_SHORTCUTS, CLOSE_WINDOW, QUIT];

fn install_new_playlist(context: &GlobalShortcutContext) {
    if context.app.lookup_action(NEW_PLAYLIST.action).is_some() {
        return;
    }
    let action = gio::SimpleAction::new(NEW_PLAYLIST.action, None);
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
    register_action(&context.app, NEW_PLAYLIST, &action);
}

fn install_new_smart_playlist(context: &GlobalShortcutContext) {
    if context
        .app
        .lookup_action(NEW_SMART_PLAYLIST.action)
        .is_some()
    {
        return;
    }
    let action = gio::SimpleAction::new(NEW_SMART_PLAYLIST.action, None);
    let parent = context.window.clone();
    let command_controller = context.command_controller.clone();
    let runtime = context.runtime.clone();
    let sidebar = context.sidebar.clone();
    let sidebar_collapse = context.sidebar_collapse.clone();
    action.connect_activate(move |_action, _parameter| {
        sidebar_collapse.expand_if_collapsed();
        open_new_smart_playlist_editor(&parent, command_controller.clone(), &runtime, &sidebar);
    });
    register_action(&context.app, NEW_SMART_PLAYLIST, &action);
}

fn install_focus_search(context: &GlobalShortcutContext) {
    if context.app.lookup_action(FOCUS_SEARCH.action).is_some() {
        return;
    }
    let action = gio::SimpleAction::new(FOCUS_SEARCH.action, None);
    let titlebar = context.titlebar.clone();
    action.connect_activate(move |_action, _parameter| {
        titlebar.focus_search();
    });
    register_action(&context.app, FOCUS_SEARCH, &action);
}

fn install_get_info(context: &GlobalShortcutContext) {
    if context.app.lookup_action(GET_INFO.action).is_some() {
        return;
    }
    let action = gio::SimpleAction::new(GET_INFO.action, None);
    let parent_window = context.window.clone().upcast::<gtk::Window>();
    let callback = get_info_callback(GetInfoCallbackContext {
        parent_window: &parent_window,
        runtime: &context.runtime,
        command_controller: &context.command_controller,
        library_changed_holder: &context.library_changed_holder,
        track_row_changed_holder: &context.track_row_changed_holder,
        track_rows_changed_holder: &context.track_rows_changed_holder,
        playback_changed: context.playback_changed.clone(),
        artwork_loader: &context.artwork_loader,
    });
    let songs_table = context.songs_table.clone();
    let playlists_table = context.playlists_table.clone();
    let duplicates_view = context.duplicates_view.clone();
    let content_stack = context.content_stack.clone();
    let track_context_invocation = context.track_context_invocation.clone();
    action.connect_activate(move |_action, _parameter| {
        let invocation =
            track_context_invocation
                .current()
                .unwrap_or_else(|| TrackActionInvocation {
                    selected_track_ids: current_view_selection(
                        &content_stack,
                        &songs_table,
                        &playlists_table,
                        &duplicates_view,
                    ),
                    displayed_track_ids: current_view_order(
                        &content_stack,
                        &songs_table,
                        &playlists_table,
                        &duplicates_view,
                    ),
                });
        if invocation.selected_track_ids.is_empty() {
            return;
        }
        callback(invocation);
    });
    register_action(&context.app, GET_INFO, &action);
}

fn install_show_in_folder(context: &GlobalShortcutContext) {
    if context.app.lookup_action(SHOW_IN_FOLDER.action).is_some() {
        return;
    }
    let action = gio::SimpleAction::new(SHOW_IN_FOLDER.action, None);
    let callback = show_in_folder_callback(
        &context.runtime,
        &context.window.clone().upcast::<gtk::Window>(),
    );
    let songs_table = context.songs_table.clone();
    let playlists_table = context.playlists_table.clone();
    let duplicates_view = context.duplicates_view.clone();
    let content_stack = context.content_stack.clone();
    let track_context_invocation = context.track_context_invocation.clone();
    action.connect_activate(move |_action, _parameter| {
        let invocation =
            track_context_invocation
                .current()
                .unwrap_or_else(|| TrackActionInvocation {
                    selected_track_ids: current_view_selection(
                        &content_stack,
                        &songs_table,
                        &playlists_table,
                        &duplicates_view,
                    ),
                    displayed_track_ids: Vec::new(),
                });
        if invocation.selected_track_ids.is_empty() {
            return;
        }
        // Multi-row scope: act on the first selected track. Opening one
        // file-manager window per track on a large selection would be
        // hostile; cross-folder selections still resolve to a single,
        // predictable parent directory.
        callback(invocation);
    });
    register_action(&context.app, SHOW_IN_FOLDER, &action);
}

fn install_play_pause(
    context: &GlobalShortcutContext,
    focus_aware_actions: &mut Vec<gio::SimpleAction>,
) {
    let toggle_or_start_playback = context.toggle_or_start_playback.clone();
    install_focus_aware_action(
        &context.app,
        PLAY_PAUSE,
        move || toggle_or_start_playback(),
        focus_aware_actions,
    );
}

fn install_previous_track(
    context: &GlobalShortcutContext,
    focus_aware_actions: &mut Vec<gio::SimpleAction>,
) {
    let command_controller = context.command_controller.clone();
    let playback_changed = context.playback_changed.clone();
    install_focus_aware_action(
        &context.app,
        PREVIOUS_TRACK,
        move || {
            dispatch_transport(
                &command_controller,
                &playback_changed,
                PlaybackCommand::PlayPreviousTrack,
            );
        },
        focus_aware_actions,
    );
}

fn install_next_track(
    context: &GlobalShortcutContext,
    focus_aware_actions: &mut Vec<gio::SimpleAction>,
) {
    let command_controller = context.command_controller.clone();
    let playback_changed = context.playback_changed.clone();
    install_focus_aware_action(
        &context.app,
        NEXT_TRACK,
        move || {
            // Match the top-bar Next button: a user-requested advance is a
            // skip, unlike end-of-stream auto-advance.
            dispatch_transport(
                &command_controller,
                &playback_changed,
                PlaybackCommand::SkipCurrentTrack,
            );
        },
        focus_aware_actions,
    );
}

fn dispatch_transport(
    command_controller: &SharedCommandController,
    playback_changed: &PlaybackChangedCallback,
    command: PlaybackCommand,
) {
    if command_controller.dispatch_succeeded(ApplicationCommand::Playback(command)) {
        playback_changed();
    }
}

fn install_jump_to_current_track(
    context: &GlobalShortcutContext,
    focus_aware_actions: &mut Vec<gio::SimpleAction>,
) {
    let runtime = context.runtime.clone();
    let songs_table = context.songs_table.clone();
    let playlists_table = context.playlists_table.clone();
    let albums_view = context.albums_view.clone();
    let content_stack = context.content_stack.clone();
    let sidebar = context.sidebar.clone();
    install_focus_aware_action(
        &context.app,
        JUMP_TO_CURRENT_TRACK,
        move || {
            jump_to_current_track(
                &runtime,
                &songs_table,
                &playlists_table,
                &albums_view,
                &content_stack,
                &sidebar,
            );
        },
        focus_aware_actions,
    );
}

fn install_select_all(
    context: &GlobalShortcutContext,
    focus_aware_actions: &mut Vec<gio::SimpleAction>,
) {
    let songs_table = context.songs_table.clone();
    let playlists_table = context.playlists_table.clone();
    let duplicates_view = context.duplicates_view.clone();
    let content_stack = context.content_stack.clone();
    install_focus_aware_action(
        &context.app,
        SELECT_ALL,
        move || {
            select_all_in_current_view(
                &content_stack,
                &songs_table,
                &playlists_table,
                &duplicates_view,
            );
        },
        focus_aware_actions,
    );
}

fn install_close_window(context: &GlobalShortcutContext) {
    let window = context.window.downgrade();
    install_action(&context.app, CLOSE_WINDOW, move || {
        if let Some(window) = window.upgrade() {
            window.close();
        }
    });
}

fn install_quit(context: &GlobalShortcutContext) {
    let app = context.app.downgrade();
    install_action(&context.app, QUIT, move || {
        if let Some(app) = app.upgrade() {
            app.quit();
        }
    });
}

#[allow(deprecated)]
fn install_shortcuts_overlay(context: &GlobalShortcutContext) {
    // GtkShortcutsWindow is the GTK-only shortcuts overlay. GTK 4.18
    // deprecated it in favour of libadwaita's replacement, but Sustain does
    // not depend on libadwaita. Keep this one boundary explicit until the
    // application's toolkit policy changes.
    let overlay = build_shortcuts_overlay(&context.window);
    context.window.set_help_overlay(Some(&overlay));

    let overlay = overlay.downgrade();
    install_action(&context.app, SHOW_SHORTCUTS, move || {
        if let Some(overlay) = overlay.upgrade() {
            overlay.present();
        }
    });
}

#[allow(deprecated)]
fn build_shortcuts_overlay(window: &gtk::ApplicationWindow) -> gtk::ShortcutsWindow {
    let overlay = gtk::ShortcutsWindow::builder()
        .transient_for(window)
        .modal(true)
        .build();
    let section = gtk::ShortcutsSection::builder()
        .section_name("general")
        .title("Sustain shortcuts")
        .build();
    for (title, shortcuts) in [
        ("Playback", PLAYBACK_SHORTCUTS),
        ("Library", LIBRARY_SHORTCUTS),
        ("Application", APPLICATION_SHORTCUTS),
    ] {
        let group = gtk::ShortcutsGroup::builder().title(title).build();
        for shortcut in shortcuts {
            group.add_shortcut(
                &gtk::ShortcutsShortcut::builder()
                    .title(shortcut.title)
                    .accelerator(shortcut.accelerator)
                    .action_name(detailed_action_name(*shortcut))
                    .build(),
            );
        }
        section.add_group(&group);
    }
    overlay.add_section(&section);
    overlay
}

fn install_action(
    app: &gtk::Application,
    shortcut: ShortcutDescription,
    callback: impl Fn() + 'static,
) {
    if app.lookup_action(shortcut.action).is_some() {
        return;
    }
    let action = gio::SimpleAction::new(shortcut.action, None);
    action.connect_activate(move |_action, _parameter| callback());
    register_action(app, shortcut, &action);
}

fn install_focus_aware_action(
    app: &gtk::Application,
    shortcut: ShortcutDescription,
    callback: impl Fn() + 'static,
    actions: &mut Vec<gio::SimpleAction>,
) {
    if app.lookup_action(shortcut.action).is_some() {
        return;
    }
    let action = gio::SimpleAction::new(shortcut.action, None);
    action.connect_activate(move |_action, _parameter| callback());
    register_action(app, shortcut, &action);
    actions.push(action);
}

fn register_action(
    app: &gtk::Application,
    shortcut: ShortcutDescription,
    action: &gio::SimpleAction,
) {
    app.add_action(action);
    app.set_accels_for_action(&detailed_action_name(shortcut), &[shortcut.accelerator]);
}

fn detailed_action_name(shortcut: ShortcutDescription) -> String {
    format!("app.{}", shortcut.action)
}

fn install_focus_aware_action_state(
    window: &gtk::ApplicationWindow,
    actions: Vec<gio::SimpleAction>,
) {
    set_focus_aware_actions_enabled(window, &actions);
    window.connect_focus_widget_notify(move |window| {
        set_focus_aware_actions_enabled(window, &actions);
    });
}

fn set_focus_aware_actions_enabled(window: &gtk::ApplicationWindow, actions: &[gio::SimpleAction]) {
    let enabled = !focus_accepts_text(window);
    for action in actions {
        action.set_enabled(enabled);
    }
}

fn focus_accepts_text(window: &gtk::ApplicationWindow) -> bool {
    let Some(focus) = gtk::prelude::RootExt::focus(window) else {
        return false;
    };
    widget_or_ancestor_accepts_text(focus)
}

fn widget_or_ancestor_accepts_text(mut focus: gtk::Widget) -> bool {
    loop {
        if focus.is::<gtk::Editable>() {
            return true;
        }

        let Some(parent) = focus.parent() else {
            return false;
        };
        focus = parent;
    }
}

/// Reveal the currently playing track in the active view, or fall back to
/// Music if the active view cannot show it. Paused tracks still qualify.
fn jump_to_current_track(
    runtime: &SharedRuntime,
    songs_table: &TrackTable,
    playlists_table: &TrackTable,
    albums_view: &AlbumsView,
    content_stack: &gtk::Stack,
    sidebar: &PlaylistSidebar,
) {
    let Some(track_id) = runtime
        .borrow()
        .now_playing()
        .track
        .as_ref()
        .map(|track| track.id)
    else {
        return;
    };

    let revealed_in_active = match content_stack.visible_child_name().as_deref() {
        Some(ALBUMS_VIEW) => albums_view.reveal_album_for_track(track_id),
        Some(PLAYLISTS_VIEW) => playlists_table.reveal_track(track_id),
        Some(SONGS_VIEW) => songs_table.reveal_track(track_id),
        _ => false,
    };

    if revealed_in_active {
        return;
    }

    sidebar.select_music();
    songs_table.reveal_track(track_id);
}

fn select_all_in_current_view(
    content_stack: &gtk::Stack,
    songs_table: &TrackTable,
    playlists_table: &TrackTable,
    duplicates_view: &DuplicatesView,
) {
    match content_stack.visible_child_name().as_deref() {
        Some(SONGS_VIEW) => songs_table.select_all(),
        Some(PLAYLISTS_VIEW) => playlists_table.select_all(),
        Some(DUPLICATES_VIEW) => duplicates_view.select_all(),
        _ => {}
    }
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
    duplicates_view: &DuplicatesView,
) -> Vec<sustain_app_runtime::TrackId> {
    match content_stack.visible_child_name().as_deref() {
        Some(SONGS_VIEW) => songs_table.selected_track_ids(),
        Some(PLAYLISTS_VIEW) => playlists_table.selected_track_ids(),
        Some(DUPLICATES_VIEW) => duplicates_view.selected_track_ids(),
        _ => Vec::new(),
    }
}

fn current_view_order(
    content_stack: &gtk::Stack,
    songs_table: &TrackTable,
    playlists_table: &TrackTable,
    duplicates_view: &DuplicatesView,
) -> Vec<sustain_app_runtime::TrackId> {
    match content_stack.visible_child_name().as_deref() {
        Some(SONGS_VIEW) => songs_table.ordered_track_ids(),
        Some(PLAYLISTS_VIEW) => playlists_table.ordered_track_ids(),
        Some(DUPLICATES_VIEW) => duplicates_view.ordered_track_ids(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::with_gtk;

    #[test]
    fn overlay_descriptors_keep_actions_and_accelerators_unique() {
        let shortcuts = PLAYBACK_SHORTCUTS
            .iter()
            .chain(LIBRARY_SHORTCUTS)
            .chain(APPLICATION_SHORTCUTS);
        let mut actions = HashSet::new();
        let mut accelerators = HashSet::new();

        for shortcut in shortcuts {
            assert!(
                actions.insert(shortcut.action),
                "duplicate shortcut action: {}",
                shortcut.action
            );
            assert!(
                accelerators.insert(shortcut.accelerator),
                "duplicate shortcut accelerator: {}",
                shortcut.accelerator
            );
        }
    }

    #[test]
    fn editable_focus_detection_yields_to_text_widgets() {
        let _ran = with_gtk(|| {
            assert!(widget_or_ancestor_accepts_text(
                gtk::Entry::new().upcast::<gtk::Widget>()
            ));
            assert!(!widget_or_ancestor_accepts_text(
                gtk::Label::new(Some("not editable")).upcast::<gtk::Widget>()
            ));
        });
    }

    #[test]
    fn shortcuts_overlay_constructs_on_the_gtk_thread() {
        let _ran = with_gtk(|| {
            let window = gtk::ApplicationWindow::default();
            let overlay = build_shortcuts_overlay(&window);

            for shortcut in PLAYBACK_SHORTCUTS
                .iter()
                .chain(LIBRARY_SHORTCUTS)
                .chain(APPLICATION_SHORTCUTS)
            {
                assert!(
                    gtk::accelerator_parse(shortcut.accelerator).is_some(),
                    "invalid shortcut accelerator: {}",
                    shortcut.accelerator
                );
            }
            assert!(overlay.is_modal());
            assert_eq!(overlay.transient_for().as_ref(), Some(window.upcast_ref()));

            overlay.close();
            window.close();
        });
    }
}
