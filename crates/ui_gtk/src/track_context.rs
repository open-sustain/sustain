// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{
    AnalysisCapability, AnalysisRunRequest, OnlineCapability, OnlineRunRequest, PlaylistId, TrackId,
};

pub(crate) type TrackActionCallback = Rc<dyn Fn(TrackActionInvocation)>;
pub(crate) type TrackActionVisibility = Rc<dyn Fn(&[TrackId]) -> bool>;
pub(crate) type AddToPlaylistProvider = Rc<dyn Fn() -> Vec<AddToPlaylistEntry>>;
pub(crate) type AddToPlaylistCallback = Rc<dyn Fn(PlaylistId, Vec<TrackId>)>;
/// Invoked when the user picks any entry inside the "Analyze"
/// submenu of the track context menu. The request carries either
/// a single capability or the `All` bundle.
pub(crate) type TrackAnalyzeRunCallback = Rc<dyn Fn(Vec<TrackId>, AnalysisRunRequest)>;
/// Invoked when the user picks any entry inside the "Retrieve"
/// submenu of the track context menu.
pub(crate) type TrackRetrieveRunCallback = Rc<dyn Fn(Vec<TrackId>, OnlineRunRequest)>;
/// Queries whether an analysis capability is globally enabled (i.e.
/// covered by the background sweep). Submenu entries whose
/// capability returns `true` here are rendered insensitive.
pub(crate) type TrackAnalyzeEnabledQuery = Rc<dyn Fn(AnalysisCapability) -> bool>;
/// Queries whether the online retrieval process is running right now.
/// When it returns `true` the Retrieve submenu entries are rendered
/// insensitive — a manual retrieval is offered whenever the process is
/// idle (regardless of the background toggle) and suppressed only while
/// a run is in flight (issue #61).
pub(crate) type TrackRetrieveBusyQuery = Rc<dyn Fn() -> bool>;
type PendingConfirmCallback = Rc<RefCell<Option<Box<dyn FnOnce(Vec<TrackId>)>>>>;
const ADD_TO_PLAYLIST_MAX_VISIBLE_HEIGHT: i32 = 360;
const ADD_TO_PLAYLIST_MAX_LABEL_CHARS: i32 = 48;

#[derive(Clone, Debug)]
pub(crate) struct TrackActionInvocation {
    pub(crate) selected_track_ids: Vec<TrackId>,
    pub(crate) displayed_track_ids: Vec<TrackId>,
}

#[derive(Clone, Debug)]
pub(crate) struct AddToPlaylistEntry {
    pub playlist_id: PlaylistId,
    pub display_path: String,
}

#[derive(Clone)]
struct AddToPlaylistAction {
    provider: AddToPlaylistProvider,
    callback: AddToPlaylistCallback,
}

#[derive(Clone)]
struct AnalyzeMenu {
    run: TrackAnalyzeRunCallback,
    enabled: TrackAnalyzeEnabledQuery,
}

#[derive(Clone)]
struct RetrieveMenu {
    run: TrackRetrieveRunCallback,
    busy: TrackRetrieveBusyQuery,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrackContextActionId {
    PlayNext,
    AddToQueue,
    GetInfo,
    CopyFiles,
    ShowInFolder,
    ShowAlbum,
    RemoveFromLibrary,
    MoveToTrash,
    RemoveFromPlaylist,
}

impl TrackContextActionId {
    fn css_class(self) -> &'static str {
        match self {
            Self::PlayNext => "track-context-play-next",
            Self::AddToQueue => "track-context-add-to-queue",
            Self::GetInfo => "track-context-get-info",
            Self::CopyFiles => "track-context-copy-files",
            Self::ShowInFolder => "track-context-show-in-folder",
            Self::ShowAlbum => "track-context-show-album",
            Self::RemoveFromLibrary => "track-context-remove-from-library",
            Self::MoveToTrash => "track-context-move-to-trash",
            Self::RemoveFromPlaylist => "track-context-remove-from-playlist",
        }
    }
}

/// Visual grouping inside the popover. Sections render in the order
/// declared below, with a horizontal separator drawn between any two
/// adjacent non-empty groups. The "Add to Playlist" submenu button (when
/// present) is treated as an implicit first group above `Queue`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrackContextActionSection {
    /// Playback queue manipulation: Play Next, Add to Queue.
    Queue,
    /// Track inspection: Get Info.
    Info,
    /// Library navigation: Show Album.
    Navigate,
    /// On-disk file operations: Copy, Show in Folder.
    Files,
    /// Removals: Remove from Library, Move to Trash, Remove from Playlist.
    Destructive,
}

/// Section render order. Used by the popover builder to walk groups in a
/// stable, declaration-driven sequence.
const TRACK_CONTEXT_SECTION_ORDER: &[TrackContextActionSection] = &[
    TrackContextActionSection::Queue,
    TrackContextActionSection::Info,
    TrackContextActionSection::Navigate,
    TrackContextActionSection::Files,
    TrackContextActionSection::Destructive,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackSelectionRequirement {
    AtLeastOne,
    Single,
}

const TRACK_SELECTION_REQUIREMENTS: &[TrackSelectionRequirement] = &[
    TrackSelectionRequirement::AtLeastOne,
    TrackSelectionRequirement::Single,
];

impl TrackSelectionRequirement {
    fn accepts(self, selected_count: usize) -> bool {
        match self {
            Self::AtLeastOne => selected_count > 0,
            Self::Single => selected_count == 1,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TrackActionConfirmation {
    None,
    MoveToTrash,
}

#[derive(Clone)]
pub(crate) struct TrackContextAction {
    id: TrackContextActionId,
    label: &'static str,
    section: TrackContextActionSection,
    selection: TrackSelectionRequirement,
    confirmation: TrackActionConfirmation,
    /// When `Some`, the predicate is evaluated each time the menu is popped
    /// and the action is hidden if it returns `false`. Used for actions that
    /// only make sense in a specific view (e.g. Remove from Playlist only
    /// when a regular playlist is currently selected in the sidebar).
    visibility: Option<TrackActionVisibility>,
    callback: TrackActionCallback,
}

impl TrackContextAction {
    pub(crate) fn play_next(
        callback: TrackActionCallback,
        visibility: TrackActionVisibility,
    ) -> Self {
        Self {
            id: TrackContextActionId::PlayNext,
            label: "Play Next",
            section: TrackContextActionSection::Queue,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::None,
            visibility: Some(visibility),
            callback,
        }
    }

    pub(crate) fn add_to_queue(
        callback: TrackActionCallback,
        visibility: TrackActionVisibility,
    ) -> Self {
        Self {
            id: TrackContextActionId::AddToQueue,
            label: "Add to Queue",
            section: TrackContextActionSection::Queue,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::None,
            visibility: Some(visibility),
            callback,
        }
    }

    pub(crate) fn get_info(callback: TrackActionCallback) -> Self {
        Self {
            id: TrackContextActionId::GetInfo,
            label: "Get Info",
            section: TrackContextActionSection::Info,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::None,
            visibility: None,
            callback,
        }
    }

    pub(crate) fn copy_files(callback: TrackActionCallback) -> Self {
        Self {
            id: TrackContextActionId::CopyFiles,
            label: "Copy",
            section: TrackContextActionSection::Files,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::None,
            visibility: None,
            callback,
        }
    }

    pub(crate) fn show_in_folder(callback: TrackActionCallback) -> Self {
        Self {
            id: TrackContextActionId::ShowInFolder,
            label: "Show in Folder",
            section: TrackContextActionSection::Files,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::None,
            visibility: None,
            callback,
        }
    }

    pub(crate) fn show_album(
        callback: TrackActionCallback,
        visibility: TrackActionVisibility,
    ) -> Self {
        Self {
            id: TrackContextActionId::ShowAlbum,
            label: "Show Album",
            section: TrackContextActionSection::Navigate,
            selection: TrackSelectionRequirement::Single,
            confirmation: TrackActionConfirmation::None,
            visibility: Some(visibility),
            callback,
        }
    }

    pub(crate) fn remove_from_library(callback: TrackActionCallback) -> Self {
        Self {
            id: TrackContextActionId::RemoveFromLibrary,
            label: "Remove from Library",
            section: TrackContextActionSection::Destructive,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::None,
            visibility: None,
            callback,
        }
    }

    pub(crate) fn move_to_trash(callback: TrackActionCallback) -> Self {
        Self {
            id: TrackContextActionId::MoveToTrash,
            label: "Move to Trash",
            section: TrackContextActionSection::Destructive,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::MoveToTrash,
            visibility: None,
            callback,
        }
    }

    pub(crate) fn remove_from_playlist(
        callback: TrackActionCallback,
        visibility: TrackActionVisibility,
    ) -> Self {
        Self {
            id: TrackContextActionId::RemoveFromPlaylist,
            label: "Remove from Playlist",
            section: TrackContextActionSection::Destructive,
            selection: TrackSelectionRequirement::AtLeastOne,
            confirmation: TrackActionConfirmation::None,
            visibility: Some(visibility),
            callback,
        }
    }

    fn is_available(&self, track_ids: &[TrackId]) -> bool {
        if !self.selection.accepts(track_ids.len()) {
            return false;
        }
        if let Some(predicate) = &self.visibility
            && !predicate(track_ids)
        {
            return false;
        }
        true
    }
}

#[derive(Clone)]
pub(crate) struct TrackContextActionSet {
    actions: Vec<TrackContextAction>,
}

impl TrackContextActionSet {
    pub(crate) fn new(actions: Vec<TrackContextAction>) -> Self {
        debug_assert!(
            actions
                .iter()
                .all(|action| TRACK_SELECTION_REQUIREMENTS.contains(&action.selection))
        );
        Self { actions }
    }

    fn available_actions<'a>(
        &'a self,
        track_ids: &'a [TrackId],
    ) -> impl Iterator<Item = &'a TrackContextAction> {
        self.actions
            .iter()
            .filter(move |action| action.is_available(track_ids))
    }
}

#[derive(Clone)]
pub(crate) struct TrackRowContextMenu {
    actions: TrackContextActionSet,
    parent_window: gtk::Window,
    add_to_playlist: Option<AddToPlaylistAction>,
    analyze: Option<AnalyzeMenu>,
    retrieve: Option<RetrieveMenu>,
}

impl TrackRowContextMenu {
    pub(crate) fn new(actions: TrackContextActionSet, parent_window: gtk::Window) -> Self {
        Self {
            actions,
            parent_window,
            add_to_playlist: None,
            analyze: None,
            retrieve: None,
        }
    }

    pub(crate) fn with_add_to_playlist(
        mut self,
        provider: AddToPlaylistProvider,
        callback: AddToPlaylistCallback,
    ) -> Self {
        self.add_to_playlist = Some(AddToPlaylistAction { provider, callback });
        self
    }

    /// Install the "Analyze\u{2026}" submenu. The submenu exposes
    /// BPM / Key / Waveform / All; each per-capability entry is
    /// rendered insensitive whenever the `enabled` query returns
    /// `true` for it (i.e. the background sweep is already covering
    /// that capability). `All` is always sensitive.
    pub(crate) fn with_analyze_menu(
        mut self,
        run: TrackAnalyzeRunCallback,
        enabled: TrackAnalyzeEnabledQuery,
    ) -> Self {
        self.analyze = Some(AnalyzeMenu { run, enabled });
        self
    }

    /// Install the "Retrieve\u{2026}" submenu. Counterpart of
    /// [`Self::with_analyze_menu`] for the online scheduler. Unlike
    /// Analyze, the entries are insensitive only while the retrieval
    /// process is running (`busy`), not when the background toggle is
    /// on — a manual retrieval re-contacts tracks that previously found
    /// nothing (issue #61).
    pub(crate) fn with_retrieve_menu(
        mut self,
        run: TrackRetrieveRunCallback,
        busy: TrackRetrieveBusyQuery,
    ) -> Self {
        self.retrieve = Some(RetrieveMenu { run, busy });
        self
    }

    pub(crate) fn popup_at(
        &self,
        track_ids: Vec<TrackId>,
        displayed_track_ids: Vec<TrackId>,
        anchor: &impl IsA<gtk::Widget>,
        x: f64,
        y: f64,
    ) {
        if track_ids.is_empty() {
            return;
        }

        self.popup_at_parent(track_ids, displayed_track_ids, anchor, anchor, x, y);
    }

    pub(crate) fn popup_at_parent(
        &self,
        track_ids: Vec<TrackId>,
        displayed_track_ids: Vec<TrackId>,
        anchor: &impl IsA<gtk::Widget>,
        popover_parent: &impl IsA<gtk::Widget>,
        x: f64,
        y: f64,
    ) {
        if track_ids.is_empty() {
            return;
        }

        let (parent_x, parent_y) = if anchor.as_ref() == popover_parent.as_ref() {
            (x, y)
        } else {
            let Some(point) = anchor.as_ref().compute_point(
                popover_parent.as_ref(),
                &gtk::graphene::Point::new(x as f32, y as f32),
            ) else {
                return;
            };
            (point.x() as f64, point.y() as f64)
        };

        let popover = gtk::Popover::new();
        popover.set_has_arrow(false);
        popover.add_css_class("compact-context-menu");
        popover.set_parent(popover_parent.as_ref());
        popover.set_child(Some(&self.menu_content(
            &popover,
            track_ids,
            displayed_track_ids,
        )));

        let popover_for_close = popover.clone();
        popover.connect_closed(move |_| {
            popover_for_close.unparent();
        });

        let rect = gdk::Rectangle::new(parent_x as i32, parent_y as i32, 1, 1);
        popover.set_pointing_to(Some(&rect));
        popover.popup();
    }

    fn menu_content(
        &self,
        popover: &gtk::Popover,
        track_ids: Vec<TrackId>,
        displayed_track_ids: Vec<TrackId>,
    ) -> gtk::Stack {
        let root = gtk::Stack::new();
        root.set_hhomogeneous(false);
        root.set_vhomogeneous(false);
        root.set_transition_type(gtk::StackTransitionType::None);
        let (main_page, triggers) = self.build_main_page(popover, &track_ids, &displayed_track_ids);
        root.add_named(&main_page, Some("main"));

        if let Some(add) = &self.add_to_playlist {
            let (playlist_page, back_button) =
                build_add_to_playlist_page(add, popover, track_ids.clone());
            root.add_named(&playlist_page, Some("playlist"));
            wire_submenu(&root, triggers.add_to_playlist, "playlist", back_button);
        }

        if let Some(analyze) = &self.analyze {
            let (page, back_button) =
                build_analyze_submenu_page(analyze, popover, track_ids.clone());
            root.add_named(&page, Some("analyze"));
            wire_submenu(&root, triggers.analyze, "analyze", back_button);
        }

        if let Some(retrieve) = &self.retrieve {
            let (page, back_button) = build_retrieve_submenu_page(retrieve, popover, track_ids);
            root.add_named(&page, Some("retrieve"));
            wire_submenu(&root, triggers.retrieve, "retrieve", back_button);
        }

        root
    }

    fn build_main_page(
        &self,
        popover: &gtk::Popover,
        track_ids: &[TrackId],
        displayed_track_ids: &[TrackId],
    ) -> (gtk::Box, MainPageTriggers) {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("track-context-menu");

        let mut triggers = MainPageTriggers::default();
        let mut prior_group_rendered = false;
        if self.add_to_playlist.is_some() {
            let button = submenu_button("Add to Playlist\u{2026}");
            content.append(&button);
            triggers.add_to_playlist = Some(button);
            prior_group_rendered = true;
        }

        let available: Vec<&TrackContextAction> =
            self.actions.available_actions(track_ids).collect();

        for &section in TRACK_CONTEXT_SECTION_ORDER {
            // The Analyze / Retrieve submenu triggers sit between the
            // last non-destructive section and the destructive one.
            // Injecting them here keeps the visual order
            // (queue → info → navigate → files → background ops →
            // destructive) and respects the existing inter-group
            // separator rule.
            if section == TrackContextActionSection::Destructive
                && (self.analyze.is_some() || self.retrieve.is_some())
            {
                if prior_group_rendered {
                    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
                    separator.add_css_class("track-context-menu-separator");
                    content.append(&separator);
                }
                if self.analyze.is_some() {
                    let button = submenu_button("Analyze\u{2026}");
                    content.append(&button);
                    triggers.analyze = Some(button);
                }
                if self.retrieve.is_some() {
                    let button = submenu_button("Retrieve\u{2026}");
                    content.append(&button);
                    triggers.retrieve = Some(button);
                }
                prior_group_rendered = true;
            }

            let mut group_iter = available
                .iter()
                .copied()
                .filter(|action| action.section == section)
                .peekable();
            if group_iter.peek().is_none() {
                continue;
            }

            if prior_group_rendered {
                let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
                separator.add_css_class("track-context-menu-separator");
                content.append(&separator);
            }

            for action in group_iter {
                self.append_action_button(
                    &content,
                    popover,
                    action,
                    track_ids,
                    displayed_track_ids,
                );
            }
            prior_group_rendered = true;
        }

        (content, triggers)
    }

    fn append_action_button(
        &self,
        content: &gtk::Box,
        popover: &gtk::Popover,
        action: &TrackContextAction,
        track_ids: &[TrackId],
        displayed_track_ids: &[TrackId],
    ) {
        let button = context_menu_button(action);
        let action = action.clone();
        let parent = self.parent_window.clone();
        let popover = popover.clone();
        let track_ids = track_ids.to_vec();
        let displayed_track_ids = displayed_track_ids.to_vec();
        button.connect_clicked(move |_| {
            popover.popdown();
            run_context_action(
                &action,
                &parent,
                TrackActionInvocation {
                    selected_track_ids: track_ids.clone(),
                    displayed_track_ids: displayed_track_ids.clone(),
                },
            );
        });
        content.append(&button);
    }
}

/// Buttons on the main page that, when clicked, switch the popover
/// to a submenu page. `None` means the matching submenu was never
/// installed.
#[derive(Default)]
struct MainPageTriggers {
    add_to_playlist: Option<gtk::Button>,
    analyze: Option<gtk::Button>,
    retrieve: Option<gtk::Button>,
}

/// Connect a submenu trigger to its stack page. No-op if `trigger` is
/// `None` (the submenu was not installed).
fn wire_submenu(
    stack: &gtk::Stack,
    trigger: Option<gtk::Button>,
    submenu_name: &'static str,
    back: gtk::Button,
) {
    if let Some(trigger) = trigger {
        let stack_weak = stack.downgrade();
        trigger.connect_clicked(move |_| {
            if let Some(stack) = stack_weak.upgrade() {
                stack.set_visible_child_name(submenu_name);
            }
        });
    }
    let stack_weak = stack.downgrade();
    back.connect_clicked(move |_| {
        if let Some(stack) = stack_weak.upgrade() {
            stack.set_visible_child_name("main");
        }
    });
}

fn build_analyze_submenu_page(
    menu: &AnalyzeMenu,
    popover: &gtk::Popover,
    track_ids: Vec<TrackId>,
) -> (gtk::Box, gtk::Button) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("track-context-menu");
    content.add_css_class("track-context-submenu");

    let back_button = submenu_back_button("Analyze");
    content.append(&back_button);

    for (label_text, capability) in [
        ("BPM", AnalysisCapability::Bpm),
        ("Key", AnalysisCapability::Key),
        ("Audio", AnalysisCapability::Audio),
    ] {
        let button = context_menu_button_with_label(label_text);
        button.set_sensitive(!(menu.enabled)(capability));
        let popover = popover.clone();
        let run = menu.run.clone();
        let track_ids = track_ids.clone();
        button.connect_clicked(move |_| {
            popover.popdown();
            run(track_ids.clone(), AnalysisRunRequest::Single(capability));
        });
        content.append(&button);
    }

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.add_css_class("track-context-menu-separator");
    content.append(&separator);

    let all_button = context_menu_button_with_label("All");
    let popover_for_all = popover.clone();
    let run_for_all = menu.run.clone();
    let track_ids_for_all = track_ids;
    all_button.connect_clicked(move |_| {
        popover_for_all.popdown();
        run_for_all(track_ids_for_all.clone(), AnalysisRunRequest::All);
    });
    content.append(&all_button);

    (content, back_button)
}

fn build_retrieve_submenu_page(
    menu: &RetrieveMenu,
    popover: &gtk::Popover,
    track_ids: Vec<TrackId>,
) -> (gtk::Box, gtk::Button) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("track-context-menu");
    content.add_css_class("track-context-submenu");

    let back_button = submenu_back_button("Retrieve");
    content.append(&back_button);

    // Every entry is offered unless a retrieval run is in flight: a
    // manual retrieval re-contacts tracks that previously found nothing,
    // independent of the background toggle. While the process is running
    // the whole submenu is suppressed to avoid stacking duplicate work.
    let busy = (menu.busy)();

    for (label_text, capability) in [
        ("Lyrics", OnlineCapability::Lyrics),
        ("Tags", OnlineCapability::Tags),
        ("Artwork", OnlineCapability::Artwork),
    ] {
        let button = context_menu_button_with_label(label_text);
        button.set_sensitive(!busy);
        let popover = popover.clone();
        let run = menu.run.clone();
        let track_ids = track_ids.clone();
        button.connect_clicked(move |_| {
            popover.popdown();
            run(track_ids.clone(), OnlineRunRequest::Single(capability));
        });
        content.append(&button);
    }

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.add_css_class("track-context-menu-separator");
    content.append(&separator);

    let all_button = context_menu_button_with_label("All");
    all_button.set_sensitive(!busy);
    let popover_for_all = popover.clone();
    let run_for_all = menu.run.clone();
    let track_ids_for_all = track_ids;
    all_button.connect_clicked(move |_| {
        popover_for_all.popdown();
        run_for_all(track_ids_for_all.clone(), OnlineRunRequest::All);
    });
    content.append(&all_button);

    (content, back_button)
}

fn build_add_to_playlist_page(
    action: &AddToPlaylistAction,
    popover: &gtk::Popover,
    track_ids: Vec<TrackId>,
) -> (gtk::Box, gtk::Button) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("track-context-menu");
    content.add_css_class("track-context-submenu");

    let back_button = submenu_back_button("Back");
    content.append(&back_button);

    let entries = (action.provider)();
    if entries.is_empty() {
        let empty_label = gtk::Label::new(Some("No playlists."));
        empty_label.set_xalign(0.0);
        empty_label.add_css_class("dim-label");
        empty_label.set_margin_top(6);
        empty_label.set_margin_bottom(6);
        empty_label.set_margin_start(8);
        empty_label.set_margin_end(8);
        content.append(&empty_label);
    } else {
        let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
        for entry in entries {
            let button = context_menu_button_with_label(&entry.display_path);
            let callback = action.callback.clone();
            let popover_for_pick = popover.clone();
            let track_ids = track_ids.clone();
            let playlist_id = entry.playlist_id;
            button.connect_clicked(move |_| {
                popover_for_pick.popdown();
                callback(playlist_id, track_ids.clone());
            });
            list.append(&button);
        }

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_max_content_height(ADD_TO_PLAYLIST_MAX_VISIBLE_HEIGHT);
        scroller.set_child(Some(&list));
        content.append(&scroller);
    }

    (content, back_button)
}

fn submenu_button(label_text: &str) -> gtk::Button {
    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);

    let chevron = gtk::Image::from_icon_name("go-next-symbolic");
    chevron.set_pixel_size(12);
    chevron.add_css_class("track-context-submenu-chevron");

    let box_widget = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    box_widget.append(&label);
    box_widget.append(&chevron);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("track-context-menu-item");
    button.set_child(Some(&box_widget));
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

fn submenu_back_button(label_text: &str) -> gtk::Button {
    let caret = gtk::Image::from_icon_name("go-previous-symbolic");
    caret.set_pixel_size(12);
    caret.add_css_class("track-context-submenu-back-caret");

    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Start);
    label.set_hexpand(true);
    label.set_margin_start(6);

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.append(&caret);
    row.append(&label);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("track-context-menu-item");
    button.add_css_class("track-context-submenu-back");
    button.set_child(Some(&row));
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

fn context_menu_button_with_label(label_text: &str) -> gtk::Button {
    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Fill);
    label.set_hexpand(true);
    label.set_width_chars(1);
    label.set_max_width_chars(ADD_TO_PLAYLIST_MAX_LABEL_CHARS);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("track-context-menu-item");
    button.set_child(Some(&label));
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

fn context_menu_button(action: &TrackContextAction) -> gtk::Button {
    let text = gtk::Label::new(Some(action.label));
    text.set_xalign(0.0);
    text.set_halign(gtk::Align::Fill);
    text.set_hexpand(true);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("track-context-menu-item");
    button.add_css_class(action.id.css_class());
    button.set_child(Some(&text));
    button.set_halign(gtk::Align::Fill);
    button.set_hexpand(true);
    button
}

fn run_context_action(
    action: &TrackContextAction,
    parent: &gtk::Window,
    invocation: TrackActionInvocation,
) {
    match action.confirmation {
        TrackActionConfirmation::None => {
            (action.callback)(invocation);
        }
        TrackActionConfirmation::MoveToTrash => {
            let callback = action.callback.clone();
            let displayed_track_ids = invocation.displayed_track_ids;
            confirm_move_to_trash(
                parent,
                invocation.selected_track_ids,
                move |confirmed_ids| {
                    callback(TrackActionInvocation {
                        selected_track_ids: confirmed_ids,
                        displayed_track_ids,
                    });
                },
            );
        }
    }
}

fn confirm_move_to_trash(
    parent: &gtk::Window,
    track_ids: Vec<TrackId>,
    on_confirm: impl FnOnce(Vec<TrackId>) + 'static,
) {
    let detail = trash_confirmation_detail(track_ids.len());

    let window = gtk::Window::builder()
        .title("Move to Trash")
        .transient_for(parent)
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
    let trash_button = gtk::Button::with_label("Move to Trash");
    trash_button.add_css_class("destructive-action");

    let window_for_cancel = window.clone();
    cancel_button.connect_clicked(move |_| {
        window_for_cancel.close();
    });

    let confirm_callback: PendingConfirmCallback =
        Rc::new(RefCell::new(Some(Box::new(on_confirm))));
    let callback_for_trash = confirm_callback.clone();
    let window_for_trash = window.clone();
    trash_button.connect_clicked(move |_| {
        if let Some(callback) = callback_for_trash.borrow_mut().take() {
            callback(track_ids.clone());
        }
        window_for_trash.close();
    });

    buttons.append(&cancel_button);
    buttons.append(&trash_button);
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

fn trash_confirmation_detail(count: usize) -> String {
    if count == 1 {
        "The audio file will be moved to the system trash and the track will be removed from the library.".to_owned()
    } else {
        format!(
            "The {count} audio files will be moved to the system trash and the tracks will be removed from the library."
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use sustain_app_runtime::TrackId;

    use super::{
        TrackActionCallback, TrackActionVisibility, TrackContextAction, TrackContextActionId,
        TrackContextActionSection, TrackSelectionRequirement, trash_confirmation_detail,
    };

    #[test]
    fn single_track_confirmation_detail_uses_singular_phrasing() {
        let detail = trash_confirmation_detail(1);
        assert!(detail.contains("audio file will be moved"));
    }

    #[test]
    fn multi_track_confirmation_detail_uses_plural_phrasing_with_count() {
        let detail = trash_confirmation_detail(3);
        assert!(detail.contains("3 audio files"));
    }

    #[test]
    fn declared_actions_have_stable_identity_and_labels() {
        let callback = no_op_callback();
        let actions = [
            TrackContextAction::play_next(callback.clone(), always_visible()),
            TrackContextAction::add_to_queue(callback.clone(), always_visible()),
            TrackContextAction::get_info(callback.clone()),
            TrackContextAction::copy_files(callback.clone()),
            TrackContextAction::show_in_folder(callback.clone()),
            TrackContextAction::show_album(callback.clone(), always_visible()),
            TrackContextAction::remove_from_library(callback.clone()),
            TrackContextAction::move_to_trash(callback),
        ];

        assert_eq!(actions[0].id, TrackContextActionId::PlayNext);
        assert_eq!(actions[0].label, "Play Next");
        assert_eq!(actions[1].id, TrackContextActionId::AddToQueue);
        assert_eq!(actions[1].label, "Add to Queue");
        assert_eq!(actions[2].id, TrackContextActionId::GetInfo);
        assert_eq!(actions[2].label, "Get Info");
        assert_eq!(actions[3].id, TrackContextActionId::CopyFiles);
        assert_eq!(actions[3].label, "Copy");
        assert_eq!(actions[4].id, TrackContextActionId::ShowInFolder);
        assert_eq!(actions[4].label, "Show in Folder");
        assert_eq!(actions[5].id, TrackContextActionId::ShowAlbum);
        assert_eq!(actions[5].label, "Show Album");
        assert_eq!(actions[6].id, TrackContextActionId::RemoveFromLibrary);
        assert_eq!(actions[6].label, "Remove from Library");
        assert_eq!(actions[7].id, TrackContextActionId::MoveToTrash);
        assert_eq!(actions[7].label, "Move to Trash");
    }

    #[test]
    fn get_info_accepts_multi_selection() {
        let action = TrackContextAction::get_info(no_op_callback());
        let one = TrackId::new(1).expect("positive track id");
        let two = TrackId::new(2).expect("positive track id");

        assert!(!action.is_available(&[]));
        assert!(action.is_available(&[one]));
        assert!(action.is_available(&[one, two]));
    }

    #[test]
    fn actions_are_assigned_to_their_visual_sections() {
        let callback = no_op_callback();
        assert_eq!(
            TrackContextAction::play_next(callback.clone(), always_visible()).section,
            TrackContextActionSection::Queue,
        );
        assert_eq!(
            TrackContextAction::add_to_queue(callback.clone(), always_visible()).section,
            TrackContextActionSection::Queue,
        );
        assert_eq!(
            TrackContextAction::get_info(callback.clone()).section,
            TrackContextActionSection::Info,
        );
        assert_eq!(
            TrackContextAction::show_album(callback.clone(), always_visible()).section,
            TrackContextActionSection::Navigate,
        );
        assert_eq!(
            TrackContextAction::copy_files(callback.clone()).section,
            TrackContextActionSection::Files,
        );
        assert_eq!(
            TrackContextAction::show_in_folder(callback.clone()).section,
            TrackContextActionSection::Files,
        );
        assert_eq!(
            TrackContextAction::remove_from_library(callback.clone()).section,
            TrackContextActionSection::Destructive,
        );
        assert_eq!(
            TrackContextAction::move_to_trash(callback.clone()).section,
            TrackContextActionSection::Destructive,
        );
        assert_eq!(
            TrackContextAction::remove_from_playlist(callback, always_visible()).section,
            TrackContextActionSection::Destructive,
        );
    }

    #[test]
    fn play_next_is_hidden_when_visibility_predicate_returns_false() {
        let callback = no_op_callback();
        let track_id = TrackId::new(1).expect("positive track id");

        let visible = TrackContextAction::play_next(callback.clone(), always_visible());
        assert!(visible.is_available(&[track_id]));

        let hidden = TrackContextAction::play_next(callback, never_visible());
        assert!(!hidden.is_available(&[track_id]));
    }

    #[test]
    fn add_to_queue_is_hidden_when_visibility_predicate_returns_false() {
        let callback = no_op_callback();
        let track_id = TrackId::new(1).expect("positive track id");

        let visible = TrackContextAction::add_to_queue(callback.clone(), always_visible());
        assert!(visible.is_available(&[track_id]));

        let hidden = TrackContextAction::add_to_queue(callback, never_visible());
        assert!(!hidden.is_available(&[track_id]));
    }

    #[test]
    fn show_album_is_hidden_when_visibility_predicate_returns_false() {
        let callback = no_op_callback();
        let track_id = TrackId::new(1).expect("positive track id");

        let visible = TrackContextAction::show_album(callback.clone(), always_visible());
        assert!(visible.is_available(&[track_id]));

        let hidden = TrackContextAction::show_album(callback, never_visible());
        assert!(!hidden.is_available(&[track_id]));
    }

    #[test]
    fn show_album_requires_single_selection() {
        let action = TrackContextAction::show_album(no_op_callback(), always_visible());
        let one = TrackId::new(1).expect("positive track id");
        let two = TrackId::new(2).expect("positive track id");

        assert!(!action.is_available(&[]));
        assert!(action.is_available(&[one]));
        assert!(!action.is_available(&[one, two]));
    }

    #[test]
    fn action_selection_requirements_are_deterministic() {
        assert!(!TrackSelectionRequirement::AtLeastOne.accepts(0));
        assert!(TrackSelectionRequirement::AtLeastOne.accepts(2));
        assert!(TrackSelectionRequirement::Single.accepts(1));
        assert!(!TrackSelectionRequirement::Single.accepts(2));
    }

    fn no_op_callback() -> TrackActionCallback {
        Rc::new({
            let calls = Rc::new(RefCell::new(0usize));
            move |_track_ids| {
                *calls.borrow_mut() += 1;
            }
        })
    }

    fn always_visible() -> TrackActionVisibility {
        Rc::new(|_track_ids| true)
    }

    fn never_visible() -> TrackActionVisibility {
        Rc::new(|_track_ids| false)
    }
}
