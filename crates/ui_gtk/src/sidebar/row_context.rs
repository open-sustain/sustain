// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::sync::atomic::{AtomicU64, Ordering};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use sustain_app_runtime::{
    AnalysisCapability, AnalysisRunRequest, OnlineCapability, OnlineRunRequest, PlaylistItem,
};

use super::{
    AnalysisEnabledQueryHolder, AnalysisRunCallbackHolder, DeleteCallbackHolder,
    EditSmartPlaylistCallbackHolder, OnlineBusyQueryHolder, OnlineRunCallbackHolder,
    RenameCallbackHolder,
};

const ROW_CONTEXT_ACTION_GROUP: &str = "playlist-row-context";
const EDIT_ACTION: &str = "edit-smart-playlist";
const RENAME_ACTION: &str = "rename";
const DELETE_ACTION: &str = "delete";
const ANALYZE_BPM_ACTION: &str = "analyze-bpm";
const ANALYZE_KEY_ACTION: &str = "analyze-key";
const ANALYZE_AUDIO_ACTION: &str = "analyze-audio";
const ANALYZE_ALL_ACTION: &str = "analyze-all";
const RETRIEVE_LYRICS_ACTION: &str = "retrieve-lyrics";
const RETRIEVE_TAGS_ACTION: &str = "retrieve-tags";
const RETRIEVE_ARTWORK_ACTION: &str = "retrieve-artwork";
const RETRIEVE_ALL_ACTION: &str = "retrieve-all";
static NEXT_ROW_CONTEXT_MENU_SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub(super) struct SidebarRowContext {
    pub(super) item: PlaylistItem,
    pub(super) current_name: String,
    pub(super) name_stack: gtk::Stack,
    pub(super) label: gtk::Label,
    pub(super) entry: gtk::Entry,
    pub(super) on_delete: DeleteCallbackHolder,
    pub(super) on_edit_smart_playlist: EditSmartPlaylistCallbackHolder,
    pub(super) on_analysis_run: AnalysisRunCallbackHolder,
    pub(super) on_online_run: OnlineRunCallbackHolder,
    pub(super) analysis_enabled_query: AnalysisEnabledQueryHolder,
    pub(super) online_busy_query: OnlineBusyQueryHolder,
}

pub(super) fn attach_row_context_menu(row: &gtk::Widget, context: SidebarRowContext) {
    remove_secondary_gestures(row);

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);
    let row_widget = row.clone();
    gesture.connect_pressed(move |gesture, _n_press, x, y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        popup_row_context_menu(&row_widget, context.clone(), x, y);
    });
    row.add_controller(gesture);
}

fn remove_secondary_gestures(widget: &gtk::Widget) {
    let controllers = widget.observe_controllers();
    let mut to_remove: Vec<gtk::EventController> = Vec::new();
    for index in 0..controllers.n_items() {
        let Some(object) = controllers.item(index) else {
            continue;
        };
        let Some(gesture) = object.downcast_ref::<gtk::GestureClick>() else {
            continue;
        };
        if gesture.button() != gdk::BUTTON_SECONDARY {
            continue;
        }
        if let Ok(controller) = object.downcast::<gtk::EventController>() {
            to_remove.push(controller);
        }
    }
    for controller in to_remove {
        widget.remove_controller(&controller);
    }
}

fn popup_row_context_menu(anchor: &gtk::Widget, context: SidebarRowContext, x: f64, y: f64) {
    let invocation_serial = NEXT_ROW_CONTEXT_MENU_SERIAL.fetch_add(1, Ordering::Relaxed);
    let local_action_group = format!("{ROW_CONTEXT_ACTION_GROUP}-{invocation_serial}");
    let action_group = gio::SimpleActionGroup::new();
    let menu = row_context_menu_model(&context, &action_group, &local_action_group, anchor);

    anchor.insert_action_group(&local_action_group, Some(&action_group));

    let popover = playlist_row_context_popover();
    popover.set_has_arrow(false);
    popover.add_css_class("compact-context-menu");
    popover.set_parent(anchor);
    popover.set_menu_model(Some(&menu));

    let popover_for_close = popover.clone();
    let action_scope = anchor.downgrade();
    popover.connect_closed(move |_| {
        let popover_for_close = popover_for_close.clone();
        let action_scope = action_scope.clone();
        let local_action_group = local_action_group.clone();
        glib::idle_add_local_once(move || {
            popover_for_close.unparent();
            if let Some(action_scope) = action_scope.upgrade() {
                action_scope
                    .insert_action_group(&local_action_group, None::<&gio::SimpleActionGroup>);
            }
        });
    });

    let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));
    popover.popup();
}

fn playlist_row_context_popover() -> gtk::PopoverMenu {
    gtk::PopoverMenu::builder()
        .flags(gtk::PopoverMenuFlags::NESTED)
        .build()
}

fn row_context_menu_model(
    context: &SidebarRowContext,
    action_group: &gio::SimpleActionGroup,
    local_action_group: &str,
    anchor: &gtk::Widget,
) -> gio::Menu {
    let menu = gio::Menu::new();

    if let PlaylistItem::SmartPlaylist(smart_playlist_id) = context.item {
        let on_edit = context.on_edit_smart_playlist.clone();
        add_local_action(action_group, EDIT_ACTION, true, move || {
            if let Some(callback) = on_edit.borrow().as_ref() {
                callback(smart_playlist_id);
            }
        });
        append_local_action_item(&menu, "Edit\u{2026}", local_action_group, EDIT_ACTION);
    }

    let name_stack_for_rename = context.name_stack.clone();
    let label_for_rename = context.label.clone();
    let entry_for_rename = context.entry.clone();
    add_local_action(action_group, RENAME_ACTION, true, move || {
        begin_rename(&name_stack_for_rename, &label_for_rename, &entry_for_rename);
    });
    append_local_action_item(&menu, "Rename", local_action_group, RENAME_ACTION);

    let anchor_for_delete = anchor.clone();
    let item_for_delete = context.item;
    let current_name_for_delete = context.current_name.clone();
    let on_delete_for_delete = context.on_delete.clone();
    add_local_action(action_group, DELETE_ACTION, true, move || {
        confirm_and_delete(
            &anchor_for_delete,
            item_for_delete,
            current_name_for_delete.clone(),
            on_delete_for_delete.clone(),
        );
    });
    append_local_action_item(
        &menu,
        delete_label_for(context.item),
        local_action_group,
        DELETE_ACTION,
    );

    if matches!(
        context.item,
        PlaylistItem::Playlist(_) | PlaylistItem::SmartPlaylist(_)
    ) {
        let run_section = gio::Menu::new();
        run_section.append_submenu(
            Some("Analyze\u{2026}"),
            &analyze_submenu_model(context, action_group, local_action_group),
        );
        run_section.append_submenu(
            Some("Retrieve\u{2026}"),
            &retrieve_submenu_model(context, action_group, local_action_group),
        );
        menu.append_section(None, &run_section);
    }

    menu
}

/// Build the "Analyze" submenu model: BPM / Key / Audio / All.
/// Each per-capability action is insensitive when the matching global
/// toggle is on (the background sweep is already going to cover it).
/// `All` is always sensitive and always submits the full mask.
fn analyze_submenu_model(
    context: &SidebarRowContext,
    action_group: &gio::SimpleActionGroup,
    local_action_group: &str,
) -> gio::Menu {
    let model = gio::Menu::new();
    let analysis_globally_on = |capability: AnalysisCapability| -> bool {
        context
            .analysis_enabled_query
            .borrow()
            .as_ref()
            .map(|query| query(capability))
            .unwrap_or(false)
    };

    for (action_name, label, capability) in [
        (ANALYZE_BPM_ACTION, "BPM", AnalysisCapability::Bpm),
        (ANALYZE_KEY_ACTION, "Key", AnalysisCapability::Key),
        (ANALYZE_AUDIO_ACTION, "Audio", AnalysisCapability::Audio),
    ] {
        let item = context.item;
        let on_analysis_run = context.on_analysis_run.clone();
        add_local_action(
            action_group,
            action_name,
            !analysis_globally_on(capability),
            move || {
                if let Some(callback) = on_analysis_run.borrow().as_ref() {
                    callback(item, AnalysisRunRequest::Single(capability));
                }
            },
        );
        append_local_action_item(&model, label, local_action_group, action_name);
    }

    let all = gio::Menu::new();
    let item = context.item;
    let on_analysis_run = context.on_analysis_run.clone();
    add_local_action(action_group, ANALYZE_ALL_ACTION, true, move || {
        if let Some(callback) = on_analysis_run.borrow().as_ref() {
            callback(item, AnalysisRunRequest::All);
        }
    });
    append_local_action_item(&all, "All", local_action_group, ANALYZE_ALL_ACTION);
    model.append_section(None, &all);

    model
}

/// Build the "Retrieve" submenu model: Lyrics / Tags / Artwork / All.
/// Unlike Analyze, entries are insensitive only while a retrieval run
/// is in flight — when the process is idle they are clickable
/// regardless of the background toggle, so a manual retrieval can
/// re-contact tracks that previously found nothing (issue #61).
fn retrieve_submenu_model(
    context: &SidebarRowContext,
    action_group: &gio::SimpleActionGroup,
    local_action_group: &str,
) -> gio::Menu {
    let model = gio::Menu::new();
    let online_busy = context
        .online_busy_query
        .borrow()
        .as_ref()
        .map(|query| query())
        .unwrap_or(false);

    for (action_name, label, capability) in [
        (RETRIEVE_LYRICS_ACTION, "Lyrics", OnlineCapability::Lyrics),
        (RETRIEVE_TAGS_ACTION, "Tags", OnlineCapability::Tags),
        (
            RETRIEVE_ARTWORK_ACTION,
            "Artwork",
            OnlineCapability::Artwork,
        ),
    ] {
        let item = context.item;
        let on_online_run = context.on_online_run.clone();
        add_local_action(action_group, action_name, !online_busy, move || {
            if let Some(callback) = on_online_run.borrow().as_ref() {
                callback(item, OnlineRunRequest::Single(capability));
            }
        });
        append_local_action_item(&model, label, local_action_group, action_name);
    }

    let all = gio::Menu::new();
    let item = context.item;
    let on_online_run = context.on_online_run.clone();
    add_local_action(action_group, RETRIEVE_ALL_ACTION, !online_busy, move || {
        if let Some(callback) = on_online_run.borrow().as_ref() {
            callback(item, OnlineRunRequest::All);
        }
    });
    append_local_action_item(&all, "All", local_action_group, RETRIEVE_ALL_ACTION);
    model.append_section(None, &all);

    model
}

fn append_local_action_item(
    menu: &gio::Menu,
    label: &str,
    local_action_group: &str,
    action_name: &str,
) {
    menu.append(
        Some(label),
        Some(&format!("{local_action_group}.{action_name}")),
    );
}

fn add_local_action(
    action_group: &gio::SimpleActionGroup,
    name: &str,
    enabled: bool,
    callback: impl Fn() + 'static,
) {
    let action = gio::SimpleAction::new(name, None);
    action.set_enabled(enabled);
    action.connect_activate(move |_action, _parameter| callback());
    action_group.add_action(&action);
}

fn delete_label_for(item: PlaylistItem) -> &'static str {
    match item {
        PlaylistItem::Folder(_) => "Delete Folder…",
        PlaylistItem::Playlist(_) => "Delete Playlist",
        PlaylistItem::SmartPlaylist(_) => "Delete Smart Playlist",
    }
}

pub(super) fn begin_rename(name_stack: &gtk::Stack, label: &gtk::Label, entry: &gtk::Entry) {
    entry.set_text(&label.text());
    name_stack.set_visible_child_name("entry");

    // When begin_rename is called from a closing popover (right-click "Rename")
    // or from connect_bind after a refresh, focus is still in flight and a
    // synchronous grab_focus loses the race. Defer to the next idle so the
    // entry actually receives the cursor.
    let entry = entry.clone();
    glib::idle_add_local_once(move || {
        entry.grab_focus();
        entry.select_region(0, -1);
    });
}

fn cancel_rename(name_stack: &gtk::Stack, label: &gtk::Label, entry: &gtk::Entry) {
    entry.set_text(&label.text());
    name_stack.set_visible_child_name("label");
}

fn commit_rename(
    name_stack: &gtk::Stack,
    label: &gtk::Label,
    entry: &gtk::Entry,
    item: PlaylistItem,
    on_rename: &RenameCallbackHolder,
) {
    let new_name = entry.text().to_string();
    let trimmed = new_name.trim();
    if trimmed.is_empty() || trimmed == label.text().as_str() {
        cancel_rename(name_stack, label, entry);
        return;
    }
    name_stack.set_visible_child_name("label");
    if let Some(callback) = on_rename.borrow().as_ref() {
        callback(item, trimmed.to_owned());
    }
}

pub(super) fn attach_rename_entry_signals(
    entry: &gtk::Entry,
    name_stack: &gtk::Stack,
    label: &gtk::Label,
    item: PlaylistItem,
    on_rename: RenameCallbackHolder,
) {
    remove_focus_controllers(entry);

    let name_stack_for_activate = name_stack.clone();
    let label_for_activate = label.clone();
    let on_rename_for_activate = on_rename.clone();
    entry.connect_activate(move |entry| {
        commit_rename(
            &name_stack_for_activate,
            &label_for_activate,
            entry,
            item,
            &on_rename_for_activate,
        );
    });

    let key_controller = gtk::EventControllerKey::new();
    let name_stack_for_escape = name_stack.clone();
    let label_for_escape = label.clone();
    let entry_for_escape = entry.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            cancel_rename(&name_stack_for_escape, &label_for_escape, &entry_for_escape);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    entry.add_controller(key_controller);

    let focus_controller = gtk::EventControllerFocus::new();
    let name_stack_for_focus = name_stack.clone();
    let label_for_focus = label.clone();
    let entry_for_focus = entry.clone();
    let on_rename_for_focus = on_rename.clone();
    focus_controller.connect_leave(move |_controller| {
        if name_stack_for_focus.visible_child_name().as_deref() == Some("entry") {
            commit_rename(
                &name_stack_for_focus,
                &label_for_focus,
                &entry_for_focus,
                item,
                &on_rename_for_focus,
            );
        }
    });
    entry.add_controller(focus_controller);
}

fn remove_focus_controllers(entry: &gtk::Entry) {
    let controllers = entry.observe_controllers();
    let mut to_remove: Vec<gtk::EventController> = Vec::new();
    for index in 0..controllers.n_items() {
        let Some(object) = controllers.item(index) else {
            continue;
        };
        if object.downcast_ref::<gtk::EventControllerKey>().is_some()
            || object.downcast_ref::<gtk::EventControllerFocus>().is_some()
        {
            if let Ok(controller) = object.downcast::<gtk::EventController>() {
                to_remove.push(controller);
            }
        }
    }
    for controller in to_remove {
        entry.remove_controller(&controller);
    }
}

fn confirm_and_delete(
    anchor: &gtk::Widget,
    item: PlaylistItem,
    current_name: String,
    on_delete: DeleteCallbackHolder,
) {
    let Some(root) = anchor.root() else {
        return;
    };
    let Ok(parent_window) = root.downcast::<gtk::Window>() else {
        return;
    };

    let (title, detail, button_label) = match item {
        PlaylistItem::Folder(_) => (
            "Delete Folder",
            format!(
                "\"{current_name}\" will be deleted along with every playlist, smart playlist, and folder inside it. This cannot be undone."
            ),
            "Delete Folder",
        ),
        PlaylistItem::Playlist(_) => (
            "Delete Playlist",
            format!(
                "\"{current_name}\" will be removed from the sidebar. The tracks themselves stay in your library."
            ),
            "Delete Playlist",
        ),
        PlaylistItem::SmartPlaylist(_) => (
            "Delete Smart Playlist",
            format!(
                "\"{current_name}\" will be removed from the sidebar. The tracks it currently matches stay in your library."
            ),
            "Delete Smart Playlist",
        ),
    };

    crate::confirmation::show_confirmation_alert(
        &parent_window,
        title,
        &detail,
        button_label,
        move || {
            if let Some(callback) = on_delete.borrow().as_ref() {
                callback(item);
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gtk::prelude::*;
    use gtk::{gio, glib};
    use sustain_app_runtime::{
        AnalysisCapability, AnalysisRunRequest, OnlineCapability, OnlineRunRequest,
        PlaylistFolderId, PlaylistId, PlaylistItem, SmartPlaylistId,
    };

    use super::{
        ANALYZE_ALL_ACTION, ANALYZE_AUDIO_ACTION, ANALYZE_BPM_ACTION, ANALYZE_KEY_ACTION,
        RETRIEVE_ALL_ACTION, RETRIEVE_ARTWORK_ACTION, RETRIEVE_LYRICS_ACTION, RETRIEVE_TAGS_ACTION,
        ROW_CONTEXT_ACTION_GROUP, SidebarRowContext, playlist_row_context_popover,
        popup_row_context_menu, row_context_menu_model,
    };

    #[test]
    fn playlist_row_context_popover_uses_nested_submenus() {
        let ran = crate::test_support::with_gtk(|| {
            let popover = playlist_row_context_popover();
            assert_eq!(
                popover.flags(),
                gtk::PopoverMenuFlags::NESTED,
                "playlist row context menus should use desktop nested submenus"
            );
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn playlist_row_context_model_contains_run_submenus_for_playlists() {
        let ran = crate::test_support::with_gtk(|| {
            let context = row_context(PlaylistItem::Playlist(playlist_id(5)));
            let action_group = gio::SimpleActionGroup::new();
            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let menu = row_context_menu_model(
                &context,
                &action_group,
                ROW_CONTEXT_ACTION_GROUP,
                anchor.upcast_ref(),
            );

            assert_eq!(menu.n_items(), 3);
            assert_menu_item_action(&menu, 0, "Rename", "playlist-row-context.rename");
            assert_menu_item_action(&menu, 1, "Delete Playlist", "playlist-row-context.delete");

            let run_section = menu
                .item_link(2, "section")
                .expect("playlist rows include a run section");
            assert_eq!(run_section.n_items(), 2);
            assert_menu_item_label(&run_section, 0, "Analyze…");
            assert_menu_item_label(&run_section, 1, "Retrieve…");

            let analyze = run_section
                .item_link(0, "submenu")
                .expect("Analyze is a nested submenu");
            assert_menu_item_action(&analyze, 0, "BPM", "playlist-row-context.analyze-bpm");
            assert_menu_item_action(&analyze, 1, "Key", "playlist-row-context.analyze-key");
            assert_menu_item_action(&analyze, 2, "Audio", "playlist-row-context.analyze-audio");
            assert!(
                analyze.item_attribute_value(0, "custom", None).is_none(),
                "playlist row submenus must be model-backed, not custom-child placeholders"
            );
            let analyze_all = analyze
                .item_link(3, "section")
                .expect("Analyze All is separated from individual capabilities");
            assert_menu_item_action(&analyze_all, 0, "All", "playlist-row-context.analyze-all");

            let retrieve = run_section
                .item_link(1, "submenu")
                .expect("Retrieve is a nested submenu");
            assert_menu_item_action(
                &retrieve,
                0,
                "Lyrics",
                "playlist-row-context.retrieve-lyrics",
            );
            assert_menu_item_action(&retrieve, 1, "Tags", "playlist-row-context.retrieve-tags");
            assert_menu_item_action(
                &retrieve,
                2,
                "Artwork",
                "playlist-row-context.retrieve-artwork",
            );
            let retrieve_all = retrieve
                .item_link(3, "section")
                .expect("Retrieve All is separated from individual capabilities");
            assert_menu_item_action(&retrieve_all, 0, "All", "playlist-row-context.retrieve-all");
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn folder_row_context_model_does_not_offer_run_submenus() {
        let ran = crate::test_support::with_gtk(|| {
            let context = row_context(PlaylistItem::Folder(folder_id(7)));
            let action_group = gio::SimpleActionGroup::new();
            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let menu = row_context_menu_model(
                &context,
                &action_group,
                ROW_CONTEXT_ACTION_GROUP,
                anchor.upcast_ref(),
            );

            assert_eq!(menu.n_items(), 2);
            assert_menu_item_action(&menu, 0, "Rename", "playlist-row-context.rename");
            assert_menu_item_action(&menu, 1, "Delete Folder…", "playlist-row-context.delete");
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn playlist_row_context_analysis_actions_invoke_callbacks_and_reflect_global_toggles() {
        let ran = crate::test_support::with_gtk(|| {
            let context = row_context(PlaylistItem::Playlist(playlist_id(11)));
            let received = Rc::new(RefCell::new(None));
            let received_for_callback = received.clone();
            context
                .on_analysis_run
                .replace(Some(Rc::new(move |item, request| {
                    received_for_callback.replace(Some((item, request)));
                })));
            context
                .analysis_enabled_query
                .replace(Some(Rc::new(|capability| {
                    capability == AnalysisCapability::Bpm
                })));

            let action_group = gio::SimpleActionGroup::new();
            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let _menu = row_context_menu_model(
                &context,
                &action_group,
                ROW_CONTEXT_ACTION_GROUP,
                anchor.upcast_ref(),
            );

            assert_action_enabled(&action_group, ANALYZE_BPM_ACTION, false);
            assert_action_enabled(&action_group, ANALYZE_KEY_ACTION, true);
            assert_action_enabled(&action_group, ANALYZE_AUDIO_ACTION, true);
            assert_action_enabled(&action_group, ANALYZE_ALL_ACTION, true);

            action_group.activate_action(ANALYZE_KEY_ACTION, None::<&glib::Variant>);
            assert_eq!(
                *received.borrow(),
                Some((
                    PlaylistItem::Playlist(playlist_id(11)),
                    AnalysisRunRequest::Single(AnalysisCapability::Key),
                ))
            );

            action_group.activate_action(ANALYZE_ALL_ACTION, None::<&glib::Variant>);
            assert_eq!(
                *received.borrow(),
                Some((
                    PlaylistItem::Playlist(playlist_id(11)),
                    AnalysisRunRequest::All
                ))
            );
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn playlist_row_context_retrieve_actions_disable_while_busy() {
        let ran = crate::test_support::with_gtk(|| {
            let context = row_context(PlaylistItem::SmartPlaylist(smart_playlist_id(13)));
            context.online_busy_query.replace(Some(Rc::new(|| true)));

            let action_group = gio::SimpleActionGroup::new();
            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let _menu = row_context_menu_model(
                &context,
                &action_group,
                ROW_CONTEXT_ACTION_GROUP,
                anchor.upcast_ref(),
            );

            assert_action_enabled(&action_group, RETRIEVE_LYRICS_ACTION, false);
            assert_action_enabled(&action_group, RETRIEVE_TAGS_ACTION, false);
            assert_action_enabled(&action_group, RETRIEVE_ARTWORK_ACTION, false);
            assert_action_enabled(&action_group, RETRIEVE_ALL_ACTION, false);
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn playlist_row_context_retrieve_action_invokes_callback_when_idle() {
        let ran = crate::test_support::with_gtk(|| {
            let context = row_context(PlaylistItem::SmartPlaylist(smart_playlist_id(17)));
            let received = Rc::new(RefCell::new(None));
            let received_for_callback = received.clone();
            context
                .on_online_run
                .replace(Some(Rc::new(move |item, request| {
                    received_for_callback.replace(Some((item, request)));
                })));
            context.online_busy_query.replace(Some(Rc::new(|| false)));

            let action_group = gio::SimpleActionGroup::new();
            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            let _menu = row_context_menu_model(
                &context,
                &action_group,
                ROW_CONTEXT_ACTION_GROUP,
                anchor.upcast_ref(),
            );

            assert_action_enabled(&action_group, RETRIEVE_ARTWORK_ACTION, true);
            action_group.activate_action(RETRIEVE_ARTWORK_ACTION, None::<&glib::Variant>);
            assert_eq!(
                *received.borrow(),
                Some((
                    PlaylistItem::SmartPlaylist(smart_playlist_id(17)),
                    OnlineRunRequest::Single(OnlineCapability::Artwork),
                ))
            );

            action_group.activate_action(RETRIEVE_ALL_ACTION, None::<&glib::Variant>);
            assert_eq!(
                *received.borrow(),
                Some((
                    PlaylistItem::SmartPlaylist(smart_playlist_id(17)),
                    OnlineRunRequest::All,
                ))
            );
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn playlist_row_context_popup_with_submenus_does_not_panic() {
        let ran = crate::test_support::with_gtk(|| {
            let window = gtk::Window::new();
            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            window.set_child(Some(&anchor));
            window.present();

            let context = row_context(PlaylistItem::Playlist(playlist_id(19)));
            popup_row_context_menu(anchor.upcast_ref(), context, 0.0, 0.0);
            let popover = descendant_of_type(anchor.upcast_ref(), "GtkPopoverMenu")
                .expect("playlist row context popover was parented to the anchor");
            popover
                .downcast::<gtk::Popover>()
                .expect("GtkPopoverMenu is a GtkPopover")
                .popdown();

            let ctx = glib::MainContext::default();
            while ctx.iteration(false) {}
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    fn row_context(item: PlaylistItem) -> SidebarRowContext {
        let name_stack = gtk::Stack::new();
        let label = gtk::Label::new(Some("Playlist"));
        let entry = gtk::Entry::new();
        name_stack.add_named(&label, Some("label"));
        name_stack.add_named(&entry, Some("entry"));
        name_stack.set_visible_child_name("label");

        SidebarRowContext {
            item,
            current_name: "Playlist".to_owned(),
            name_stack,
            label,
            entry,
            on_delete: Rc::new(RefCell::new(None)),
            on_edit_smart_playlist: Rc::new(RefCell::new(None)),
            on_analysis_run: Rc::new(RefCell::new(None)),
            on_online_run: Rc::new(RefCell::new(None)),
            analysis_enabled_query: Rc::new(RefCell::new(None)),
            online_busy_query: Rc::new(RefCell::new(None)),
        }
    }

    fn assert_action_enabled(
        action_group: &gio::SimpleActionGroup,
        action_name: &str,
        expected: bool,
    ) {
        let action = action_group
            .lookup_action(action_name)
            .expect("row context action should be registered");
        assert_eq!(action.is_enabled(), expected);
    }

    fn assert_menu_item_label(menu: &impl IsA<gio::MenuModel>, index: i32, expected: &str) {
        assert_eq!(
            menu.item_attribute_value(index, "label", Some(glib::VariantTy::STRING))
                .and_then(|value| value.get::<String>()),
            Some(expected.to_owned())
        );
    }

    fn assert_menu_item_action(
        menu: &impl IsA<gio::MenuModel>,
        index: i32,
        expected_label: &str,
        expected_action: &str,
    ) {
        assert_menu_item_label(menu, index, expected_label);
        assert_eq!(
            menu.item_attribute_value(index, "action", Some(glib::VariantTy::STRING))
                .and_then(|value| value.get::<String>()),
            Some(expected_action.to_owned())
        );
    }

    fn playlist_id(value: i64) -> PlaylistId {
        PlaylistId::new(value).expect("positive playlist id")
    }

    fn smart_playlist_id(value: i64) -> SmartPlaylistId {
        SmartPlaylistId::new(value).expect("positive smart playlist id")
    }

    fn folder_id(value: i64) -> PlaylistFolderId {
        PlaylistFolderId::new(value).expect("positive playlist folder id")
    }

    fn descendant_of_type(widget: &gtk::Widget, type_name: &str) -> Option<gtk::Widget> {
        let mut child = widget.first_child();
        while let Some(widget) = child {
            if widget.type_().name() == type_name {
                return Some(widget);
            }
            if let Some(found) = descendant_of_type(&widget, type_name) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }
}
