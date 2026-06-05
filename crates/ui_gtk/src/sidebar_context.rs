// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{collections::HashSet, rc::Rc};

use gtk::prelude::*;
use gtk::{gdk, gio};
use sustain_i18n::gettext;

/// Default name a newly created playlist is given. Localized, because it
/// becomes the playlist's stored name — a French session creates "playlist
/// sans titre", not "untitled playlist".
pub(crate) fn new_playlist_default_name() -> String {
    gettext("untitled playlist")
}

/// Default name for a newly created playlist folder.
pub(crate) fn new_playlist_folder_default_name() -> String {
    gettext("untitled folder")
}

/// Default name for a newly created smart playlist.
pub(crate) fn new_smart_playlist_default_name() -> String {
    gettext("untitled smart playlist")
}

const SIDEBAR_CONTEXT_ACTION_GROUP: &str = "sidebar-context";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SidebarContextAction {
    Playlist,
    SmartPlaylist,
    PlaylistFolder,
}

impl SidebarContextAction {
    fn label(self) -> String {
        match self {
            Self::Playlist => gettext("New Playlist"),
            Self::SmartPlaylist => gettext("New Smart Playlist…"),
            Self::PlaylistFolder => gettext("New Playlist Folder"),
        }
    }

    fn detailed_action(self) -> &'static str {
        match self {
            Self::Playlist => "app.new-playlist",
            Self::SmartPlaylist => "app.new-smart-playlist",
            Self::PlaylistFolder => "sidebar-context.new-playlist-folder",
        }
    }
}

const SIDEBAR_CONTEXT_ACTIONS: &[SidebarContextAction] = &[
    SidebarContextAction::Playlist,
    SidebarContextAction::SmartPlaylist,
    SidebarContextAction::PlaylistFolder,
];

pub(crate) type SidebarActionCallback = Rc<dyn Fn(SidebarContextAction)>;

#[derive(Clone)]
pub(crate) struct SidebarContextMenu {
    on_action: SidebarActionCallback,
}

impl SidebarContextMenu {
    pub(crate) fn new(on_action: SidebarActionCallback) -> Self {
        Self { on_action }
    }

    pub(crate) fn install_on(&self, anchor: &gtk::Widget) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);

        // GtkPopoverMenu resolves model-backed actions from its attach widget
        // and that widget's ancestors. Application actions are inherited from
        // the window; this sidebar-specific action lives on the anchor.
        anchor.insert_action_group(
            SIDEBAR_CONTEXT_ACTION_GROUP,
            Some(&sidebar_context_action_group(self.on_action.clone())),
        );
        let anchor_widget = anchor.clone();
        gesture.connect_pressed(move |gesture, _n_press, x, y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            popup_menu(&anchor_widget, x, y);
        });
        anchor.add_controller(gesture);
    }
}

fn sidebar_context_action_group(on_action: SidebarActionCallback) -> gio::SimpleActionGroup {
    let actions = gio::SimpleActionGroup::new();
    let action = gio::SimpleAction::new("new-playlist-folder", None);
    action.connect_activate(move |_action, _parameter| {
        on_action(SidebarContextAction::PlaylistFolder)
    });
    actions.add_action(&action);
    actions
}

fn popup_menu(anchor: &gtk::Widget, x: f64, y: f64) {
    let menu = gio::Menu::new();
    for action in SIDEBAR_CONTEXT_ACTIONS.iter().copied() {
        menu.append(Some(&action.label()), Some(action.detailed_action()));
    }

    let popover = gtk::PopoverMenu::from_model(None::<&gio::Menu>);
    popover.set_has_arrow(false);
    popover.add_css_class("compact-context-menu");
    popover.set_parent(anchor);
    // Install the model after attaching so its tracker inherits application
    // actions from the window and this anchor's sidebar-local action group.
    popover.set_menu_model(Some(&menu));

    let popover_for_close = popover.clone();
    popover.connect_closed(move |_| {
        let popover_for_close = popover_for_close.clone();
        // GtkModelButton activates its GAction after popping down. Keep the
        // anchor ancestry alive until that activation has resolved.
        gtk::glib::idle_add_local_once(move || popover_for_close.unparent());
    });

    let rect = gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));
    popover.popup();
}

pub(crate) fn unique_default_name<I, S>(existing_names: I, base: &str) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let existing: HashSet<String> = existing_names
        .into_iter()
        .map(|name| name.as_ref().to_owned())
        .collect();
    let mut candidate = base.to_owned();
    let mut suffix: u32 = 2;
    while existing.contains(&candidate) {
        candidate = format!("{base} {suffix}");
        suffix = suffix
            .checked_add(1)
            .expect("suffix exceeds u32::MAX, which is impossible for any realistic library");
    }
    candidate
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_default_name_returns_base_when_unused() {
        let existing: [&str; 0] = [];
        assert_eq!(
            unique_default_name(existing, &new_playlist_default_name()),
            "untitled playlist"
        );
    }

    #[test]
    fn unique_default_name_appends_smallest_free_suffix() {
        let existing = ["untitled playlist", "untitled playlist 2"];
        assert_eq!(
            unique_default_name(existing, &new_playlist_default_name()),
            "untitled playlist 3"
        );
    }

    #[test]
    fn unique_default_name_skips_already_taken_higher_suffixes() {
        let existing = ["untitled folder 2", "untitled folder 3"];
        assert_eq!(
            unique_default_name(existing, &new_playlist_folder_default_name()),
            "untitled folder"
        );
    }

    #[test]
    fn action_labels_match_the_product_contract() {
        assert_eq!(SidebarContextAction::Playlist.label(), "New Playlist");
        assert_eq!(
            SidebarContextAction::SmartPlaylist.label(),
            "New Smart Playlist\u{2026}"
        );
        assert_eq!(
            SidebarContextAction::PlaylistFolder.label(),
            "New Playlist Folder"
        );
    }

    #[test]
    fn action_models_use_registered_application_actions_when_shortcuts_exist() {
        assert_eq!(
            SidebarContextAction::Playlist.detailed_action(),
            "app.new-playlist"
        );
        assert_eq!(
            SidebarContextAction::SmartPlaylist.detailed_action(),
            "app.new-smart-playlist"
        );
        assert_eq!(
            SidebarContextAction::PlaylistFolder.detailed_action(),
            "sidebar-context.new-playlist-folder"
        );
    }
}
