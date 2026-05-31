// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{
    AnalysisCapability, AnalysisRunRequest, OnlineCapability, OnlineRunRequest, PlaylistItem,
};

use super::{
    AnalysisEnabledQueryHolder, AnalysisRunCallbackHolder, DeleteCallbackHolder,
    EditSmartPlaylistCallbackHolder, OnlineBusyQueryHolder, OnlineRunCallbackHolder,
    RenameCallbackHolder,
};

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
    let popover = gtk::Popover::new();
    popover.set_has_arrow(false);
    popover.add_css_class("compact-context-menu");
    popover.set_parent(anchor);

    let root = gtk::Stack::new();
    root.set_hhomogeneous(false);
    root.set_vhomogeneous(false);
    root.set_transition_type(gtk::StackTransitionType::None);

    let main_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_page.add_css_class("sidebar-context-menu");

    if let PlaylistItem::SmartPlaylist(smart_playlist_id) = context.item {
        let edit_button = row_action_button("Edit\u{2026}");
        let popover_for_edit = popover.clone();
        let on_edit = context.on_edit_smart_playlist.clone();
        edit_button.connect_clicked(move |_| {
            popover_for_edit.popdown();
            if let Some(callback) = on_edit.borrow().as_ref() {
                callback(smart_playlist_id);
            }
        });
        main_page.append(&edit_button);
    }

    let rename_button = row_action_button("Rename");
    let popover_for_rename = popover.clone();
    let name_stack_for_rename = context.name_stack.clone();
    let label_for_rename = context.label.clone();
    let entry_for_rename = context.entry.clone();
    rename_button.connect_clicked(move |_| {
        popover_for_rename.popdown();
        begin_rename(&name_stack_for_rename, &label_for_rename, &entry_for_rename);
    });
    main_page.append(&rename_button);

    let delete_button = row_action_button(delete_label_for(context.item));
    let popover_for_delete = popover.clone();
    let anchor_for_delete = anchor.clone();
    let item_for_delete = context.item;
    let current_name_for_delete = context.current_name.clone();
    let on_delete_for_delete = context.on_delete.clone();
    delete_button.connect_clicked(move |_| {
        popover_for_delete.popdown();
        confirm_and_delete(
            &anchor_for_delete,
            item_for_delete,
            current_name_for_delete.clone(),
            on_delete_for_delete.clone(),
        );
    });
    main_page.append(&delete_button);

    root.add_named(&main_page, Some("main"));

    // Folders don't carry tracks of their own, so the run submenus
    // only show up for Playlist and SmartPlaylist rows.
    if matches!(
        context.item,
        PlaylistItem::Playlist(_) | PlaylistItem::SmartPlaylist(_)
    ) {
        main_page.append(&row_separator());

        let analyze_trigger = row_submenu_button("Analyze\u{2026}");
        main_page.append(&analyze_trigger);

        let retrieve_trigger = row_submenu_button("Retrieve\u{2026}");
        main_page.append(&retrieve_trigger);

        let analyze_page = build_analyze_submenu_page(&popover, &context);
        root.add_named(&analyze_page, Some("analyze"));

        let retrieve_page = build_retrieve_submenu_page(&popover, &context);
        root.add_named(&retrieve_page, Some("retrieve"));

        wire_submenu_trigger(&root, &analyze_trigger, "analyze", &analyze_page);
        wire_submenu_trigger(&root, &retrieve_trigger, "retrieve", &retrieve_page);
    }

    popover.set_child(Some(&root));

    let popover_for_close = popover.clone();
    popover.connect_closed(move |_| {
        popover_for_close.unparent();
    });

    let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));
    popover.popup();
}

/// Build the "Analyze" submenu page: BPM / Key / Waveform / All.
/// Each per-capability button is insensitive when the matching global
/// toggle is on (the background sweep is already going to cover it).
/// `All` is always sensitive and always submits the full mask.
fn build_analyze_submenu_page(popover: &gtk::Popover, context: &SidebarRowContext) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.add_css_class("sidebar-context-menu");
    page.add_css_class("sidebar-context-submenu");

    let back = row_submenu_back_button("Analyze");
    page.append(&back);
    // The "Back" semantics are wired by `wire_submenu_trigger`, which
    // also caches the back button via the page's first child.

    let analysis_globally_on = |capability: AnalysisCapability| -> bool {
        context
            .analysis_enabled_query
            .borrow()
            .as_ref()
            .map(|query| query(capability))
            .unwrap_or(false)
    };

    for (label_text, capability) in [
        ("BPM", AnalysisCapability::Bpm),
        ("Key", AnalysisCapability::Key),
        ("Audio", AnalysisCapability::Audio),
    ] {
        let button = row_action_button(label_text);
        button.set_sensitive(!analysis_globally_on(capability));
        let popover_for_run = popover.clone();
        let item_for_run = context.item;
        let on_analysis_run = context.on_analysis_run.clone();
        button.connect_clicked(move |_| {
            popover_for_run.popdown();
            if let Some(callback) = on_analysis_run.borrow().as_ref() {
                callback(item_for_run, AnalysisRunRequest::Single(capability));
            }
        });
        page.append(&button);
    }

    page.append(&row_separator());

    let all_button = row_action_button("All");
    let popover_for_all = popover.clone();
    let item_for_all = context.item;
    let on_analysis_run = context.on_analysis_run.clone();
    all_button.connect_clicked(move |_| {
        popover_for_all.popdown();
        if let Some(callback) = on_analysis_run.borrow().as_ref() {
            callback(item_for_all, AnalysisRunRequest::All);
        }
    });
    page.append(&all_button);

    page
}

/// Build the "Retrieve" submenu page: Lyrics / Tags / Artwork / All.
/// Unlike Analyze, entries are insensitive only while a retrieval run
/// is in flight — when the process is idle they are clickable
/// regardless of the background toggle, so a manual retrieval can
/// re-contact tracks that previously found nothing (issue #61).
fn build_retrieve_submenu_page(popover: &gtk::Popover, context: &SidebarRowContext) -> gtk::Box {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.add_css_class("sidebar-context-menu");
    page.add_css_class("sidebar-context-submenu");

    let back = row_submenu_back_button("Retrieve");
    page.append(&back);

    let online_busy = context
        .online_busy_query
        .borrow()
        .as_ref()
        .map(|query| query())
        .unwrap_or(false);

    for (label_text, capability) in [
        ("Lyrics", OnlineCapability::Lyrics),
        ("Tags", OnlineCapability::Tags),
        ("Artwork", OnlineCapability::Artwork),
    ] {
        let button = row_action_button(label_text);
        button.set_sensitive(!online_busy);
        let popover_for_run = popover.clone();
        let item_for_run = context.item;
        let on_online_run = context.on_online_run.clone();
        button.connect_clicked(move |_| {
            popover_for_run.popdown();
            if let Some(callback) = on_online_run.borrow().as_ref() {
                callback(item_for_run, OnlineRunRequest::Single(capability));
            }
        });
        page.append(&button);
    }

    page.append(&row_separator());

    let all_button = row_action_button("All");
    all_button.set_sensitive(!online_busy);
    let popover_for_all = popover.clone();
    let item_for_all = context.item;
    let on_online_run = context.on_online_run.clone();
    all_button.connect_clicked(move |_| {
        popover_for_all.popdown();
        if let Some(callback) = on_online_run.borrow().as_ref() {
            callback(item_for_all, OnlineRunRequest::All);
        }
    });
    page.append(&all_button);

    page
}

/// Wire a main-page submenu trigger to its stack page and the page's
/// first-child Back button to the main page.
fn wire_submenu_trigger(
    stack: &gtk::Stack,
    trigger: &gtk::Button,
    submenu_name: &'static str,
    submenu: &gtk::Box,
) {
    let stack_weak = stack.downgrade();
    trigger.connect_clicked(move |_| {
        if let Some(stack) = stack_weak.upgrade() {
            stack.set_visible_child_name(submenu_name);
        }
    });

    let Some(first_child) = submenu.first_child() else {
        return;
    };
    let Ok(back) = first_child.downcast::<gtk::Button>() else {
        return;
    };
    let stack_weak = stack.downgrade();
    back.connect_clicked(move |_| {
        if let Some(stack) = stack_weak.upgrade() {
            stack.set_visible_child_name("main");
        }
    });
}

fn row_action_button(label_text: &str) -> gtk::Button {
    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Fill);
    label.set_hexpand(true);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("sidebar-context-menu-item");
    button.set_child(Some(&label));
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

/// Submenu trigger on the main page: label with a trailing chevron
/// mirroring the track context menu's "Add to Playlist..." style.
fn row_submenu_button(label_text: &str) -> gtk::Button {
    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);

    let chevron = gtk::Image::from_icon_name("go-next-symbolic");
    chevron.set_pixel_size(12);
    chevron.add_css_class("sidebar-context-submenu-chevron");

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.append(&label);
    row.append(&chevron);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("sidebar-context-menu-item");
    button.set_child(Some(&row));
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

/// Back button at the top of a submenu page. The label echoes the
/// submenu's name (e.g. "Analyze") so users know which submenu they
/// are in. Returning to the main page is wired by
/// `wire_submenu_trigger` from this button's first-child slot.
fn row_submenu_back_button(parent_label: &str) -> gtk::Button {
    let caret = gtk::Image::from_icon_name("go-previous-symbolic");
    caret.set_pixel_size(12);
    caret.add_css_class("sidebar-context-submenu-back-caret");

    let label = gtk::Label::new(Some(parent_label));
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    label.set_margin_start(6);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.append(&caret);
    row.append(&label);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("sidebar-context-menu-item");
    button.add_css_class("sidebar-context-submenu-back");
    button.set_child(Some(&row));
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

fn row_separator() -> gtk::Separator {
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.add_css_class("sidebar-context-menu-separator");
    separator
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

    let window = gtk::Window::builder()
        .title(title)
        .transient_for(&parent_window)
        .modal(true)
        .resizable(false)
        .default_width(440)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_end(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);

    let detail_label = gtk::Label::new(Some(&detail));
    detail_label.add_css_class("dim-label");
    detail_label.set_xalign(0.0);
    detail_label.set_wrap(true);
    content.append(&detail_label);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);

    let cancel_button = gtk::Button::with_label("Cancel");
    let delete_button = gtk::Button::with_label(button_label);
    delete_button.add_css_class("destructive-action");

    let window_for_cancel = window.clone();
    cancel_button.connect_clicked(move |_| {
        window_for_cancel.close();
    });

    let window_for_delete = window.clone();
    delete_button.connect_clicked(move |_| {
        if let Some(callback) = on_delete.borrow().as_ref() {
            callback(item);
        }
        window_for_delete.close();
    });

    buttons.append(&cancel_button);
    buttons.append(&delete_button);
    content.append(&buttons);
    window.set_child(Some(&content));
    window.set_default_widget(Some(&cancel_button));

    let key_controller = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    window.present();
    cancel_button.grab_focus();
}
