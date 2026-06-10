// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
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
/// Invoked when the user picks any entry inside the "Fetch"
/// submenu of the track context menu.
pub(crate) type TrackRetrieveRunCallback = Rc<dyn Fn(Vec<TrackId>, OnlineRunRequest)>;
/// Queries whether an analysis capability is globally enabled (i.e.
/// covered by the background sweep). Submenu entries whose
/// capability returns `true` here are rendered insensitive.
pub(crate) type TrackAnalyzeEnabledQuery = Rc<dyn Fn(AnalysisCapability) -> bool>;
/// Queries whether the online retrieval process is running right now.
/// When it returns `true` the Fetch submenu entries are rendered
/// insensitive — a manual retrieval is offered whenever the process is
/// idle (regardless of the background toggle) and suppressed only while
/// a run is in flight (issue #61).
pub(crate) type TrackRetrieveBusyQuery = Rc<dyn Fn() -> bool>;
pub(crate) type YoutubeAudioReplacementCallback = Rc<dyn Fn(TrackId)>;
const TRACK_CONTEXT_ACTION_GROUP: &str = "track-context";
const ADD_TO_PLAYLIST_ACTION: &str = "add-to-playlist";

#[derive(Clone, Debug)]
pub(crate) struct TrackActionInvocation {
    pub(crate) selected_track_ids: Vec<TrackId>,
    pub(crate) displayed_track_ids: Vec<TrackId>,
}

#[derive(Clone, Default)]
pub(crate) struct TrackContextInvocationState {
    inner: Rc<RefCell<TrackContextInvocationStateInner>>,
}

#[derive(Default)]
struct TrackContextInvocationStateInner {
    next_serial: u64,
    active: Option<(u64, TrackActionInvocation)>,
}

impl TrackContextInvocationState {
    fn activate(&self, invocation: TrackActionInvocation) -> u64 {
        let mut inner = self.inner.borrow_mut();
        inner.next_serial = inner
            .next_serial
            .checked_add(1)
            .expect("track context invocation serial exhausted");
        let serial = inner.next_serial;
        inner.active = Some((serial, invocation));
        serial
    }

    fn clear_if_active(&self, serial: u64) {
        let mut inner = self.inner.borrow_mut();
        if inner.active.as_ref().map(|(active, _)| *active) == Some(serial) {
            inner.active = None;
        }
    }

    pub(crate) fn current(&self) -> Option<TrackActionInvocation> {
        self.inner
            .borrow()
            .active
            .as_ref()
            .map(|(_, invocation)| invocation.clone())
    }
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
    replace_audio_from_youtube: YoutubeAudioReplacementCallback,
    youtube_replacement_visibility: TrackActionVisibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrackContextActionId {
    PlayNext,
    AddToQueue,
    GetInfo,
    CopyFiles,
    ShowInFolder,
    ShowAlbum,
    ConsolidateDuplicates,
    RemoveFromLibrary,
    MoveToTrash,
    RemoveFromPlaylist,
}

impl TrackContextActionId {
    fn action_name(self) -> &'static str {
        match self {
            Self::PlayNext => "play-next",
            Self::AddToQueue => "add-to-queue",
            Self::GetInfo => "get-info",
            Self::CopyFiles => "copy-files",
            Self::ShowInFolder => "show-in-folder",
            Self::ShowAlbum => "show-album",
            Self::ConsolidateDuplicates => "consolidate-duplicates",
            Self::RemoveFromLibrary => "remove-from-library",
            Self::MoveToTrash => "move-to-trash",
            Self::RemoveFromPlaylist => "remove-from-playlist",
        }
    }

    fn detailed_action(self, local_action_group: &str) -> String {
        match self {
            Self::GetInfo => "app.get-info".to_owned(),
            Self::ShowInFolder => "app.show-in-folder".to_owned(),
            _ => format!("{local_action_group}.{}", self.action_name()),
        }
    }

    fn uses_application_action(self) -> bool {
        matches!(self, Self::GetInfo | Self::ShowInFolder)
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
    AtLeastTwo,
    Single,
}

const TRACK_SELECTION_REQUIREMENTS: &[TrackSelectionRequirement] = &[
    TrackSelectionRequirement::AtLeastOne,
    TrackSelectionRequirement::AtLeastTwo,
    TrackSelectionRequirement::Single,
];

impl TrackSelectionRequirement {
    fn accepts(self, selected_count: usize) -> bool {
        match self {
            Self::AtLeastOne => selected_count > 0,
            Self::AtLeastTwo => selected_count > 1,
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

    pub(crate) fn consolidate_duplicates(callback: TrackActionCallback) -> Self {
        Self {
            id: TrackContextActionId::ConsolidateDuplicates,
            label: "Consolidate to single track",
            section: TrackContextActionSection::Destructive,
            selection: TrackSelectionRequirement::AtLeastTwo,
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
    invocation_state: TrackContextInvocationState,
}

impl TrackRowContextMenu {
    pub(crate) fn new(
        actions: TrackContextActionSet,
        parent_window: gtk::Window,
        invocation_state: TrackContextInvocationState,
    ) -> Self {
        Self {
            actions,
            parent_window,
            add_to_playlist: None,
            analyze: None,
            retrieve: None,
            invocation_state,
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

    /// Install the "Fetch\u{2026}" submenu. Counterpart of
    /// [`Self::with_analyze_menu`] for the online scheduler. Unlike
    /// Analyze, the entries are insensitive only while the retrieval
    /// process is running (`busy`), not when the background toggle is
    /// on — a manual retrieval re-contacts tracks that previously found
    /// nothing (issue #61).
    pub(crate) fn with_retrieve_menu(
        mut self,
        run: TrackRetrieveRunCallback,
        busy: TrackRetrieveBusyQuery,
        replace_audio_from_youtube: YoutubeAudioReplacementCallback,
        youtube_replacement_visibility: TrackActionVisibility,
    ) -> Self {
        self.retrieve = Some(RetrieveMenu {
            run,
            busy,
            replace_audio_from_youtube,
            youtube_replacement_visibility,
        });
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

        let invocation = TrackActionInvocation {
            selected_track_ids: track_ids,
            displayed_track_ids,
        };
        let invocation_serial = self.invocation_state.activate(invocation.clone());
        let local_action_group = format!("{TRACK_CONTEXT_ACTION_GROUP}-{invocation_serial}");
        let action_group = gio::SimpleActionGroup::new();
        let menu = self.menu_model(&action_group, &local_action_group, &invocation);
        // GtkPopoverMenu resolves model-backed actions from its attach widget
        // and that widget's ancestors. Keep each popup's local namespace
        // distinct so deferred cleanup cannot remove a newer popup's actions.
        popover_parent
            .as_ref()
            .insert_action_group(&local_action_group, Some(&action_group));
        let popover = track_context_popover();
        popover.set_has_arrow(false);
        popover.add_css_class("compact-context-menu");
        popover.set_parent(popover_parent.as_ref());
        // GtkPopoverMenu builds its tracker when the model is installed. Do
        // that only after parenting so the tracker sees the popup-local group
        // and inherited application actions from the anchor hierarchy.
        popover.set_menu_model(Some(&menu));

        let popover_for_close = popover.clone();
        let invocation_state = self.invocation_state.clone();
        let action_scope = popover_parent.as_ref().downgrade();
        popover.connect_closed(move |_| {
            let invocation_state = invocation_state.clone();
            let action_scope = action_scope.clone();
            let local_action_group = local_action_group.clone();
            let popover_for_close = popover_for_close.clone();
            // GtkModelButton pops its containing popover down before asking
            // GtkActionHelper to activate the model-backed action. Preserve
            // the widget ancestry until the current event has completed so
            // the helper can still resolve this popup's action scope.
            glib::idle_add_local_once(move || {
                popover_for_close.unparent();
                invocation_state.clear_if_active(invocation_serial);
                if let Some(action_scope) = action_scope.upgrade() {
                    action_scope
                        .insert_action_group(&local_action_group, None::<&gio::SimpleActionGroup>);
                }
            });
        });

        let rect = gdk::Rectangle::new(parent_x as i32, parent_y as i32, 1, 1);
        popover.set_pointing_to(Some(&rect));
        popover.popup();
    }

    fn menu_model(
        &self,
        action_group: &gio::SimpleActionGroup,
        local_action_group: &str,
        invocation: &TrackActionInvocation,
    ) -> gio::Menu {
        let root = gio::Menu::new();
        if let Some(add) = &self.add_to_playlist {
            root.append_submenu(
                Some("Add to Playlist\u{2026}"),
                &add_to_playlist_submenu_model(
                    add,
                    action_group,
                    local_action_group,
                    &invocation.selected_track_ids,
                ),
            );
        }

        let available: Vec<&TrackContextAction> = self
            .actions
            .available_actions(&invocation.selected_track_ids)
            .collect();

        for &section in TRACK_CONTEXT_SECTION_ORDER {
            if section == TrackContextActionSection::Destructive
                && (self.analyze.is_some() || self.retrieve.is_some())
            {
                root.append_section(
                    None,
                    &self.background_submenu_section(action_group, local_action_group, invocation),
                );
            }

            let group = gio::Menu::new();
            for action in available
                .iter()
                .copied()
                .filter(|action| action.section == section)
            {
                self.append_action_item(
                    &group,
                    action_group,
                    local_action_group,
                    action,
                    invocation,
                );
            }
            if group.n_items() == 0 {
                continue;
            }
            root.append_section(None, &group);
        }

        root
    }

    fn append_action_item(
        &self,
        menu: &gio::Menu,
        action_group: &gio::SimpleActionGroup,
        local_action_group: &str,
        action: &TrackContextAction,
        invocation: &TrackActionInvocation,
    ) {
        if !action.id.uses_application_action() {
            let action_for_run = action.clone();
            let parent = self.parent_window.clone();
            let invocation = invocation.clone();
            add_local_action(action_group, action.id.action_name(), true, move || {
                run_context_action(&action_for_run, &parent, invocation.clone())
            });
        }
        menu.append(
            Some(action.label),
            Some(&action.id.detailed_action(local_action_group)),
        );
    }

    fn background_submenu_section(
        &self,
        action_group: &gio::SimpleActionGroup,
        local_action_group: &str,
        invocation: &TrackActionInvocation,
    ) -> gio::Menu {
        let section = gio::Menu::new();
        if let Some(analyze) = &self.analyze {
            section.append_submenu(
                Some("Analyze\u{2026}"),
                &analyze_submenu_model(
                    analyze,
                    action_group,
                    local_action_group,
                    &invocation.selected_track_ids,
                ),
            );
        }
        if let Some(retrieve) = &self.retrieve {
            section.append_submenu(
                Some("Fetch\u{2026}"),
                &retrieve_submenu_model(
                    retrieve,
                    action_group,
                    local_action_group,
                    &invocation.selected_track_ids,
                ),
            );
        }
        section
    }
}

fn track_context_popover() -> gtk::PopoverMenu {
    // The default GtkPopoverMenu mode slides forward into submenus. On this
    // desktop context menu that path can stall while GTK realizes the target
    // submenu contents, especially for the playlist list. Traditional nested
    // submenus avoid that forward-animation cost and match desktop menus.
    gtk::PopoverMenu::builder()
        .flags(gtk::PopoverMenuFlags::NESTED)
        .build()
}

fn add_to_playlist_submenu_model(
    action: &AddToPlaylistAction,
    action_group: &gio::SimpleActionGroup,
    local_action_group: &str,
    track_ids: &[TrackId],
) -> gio::Menu {
    let menu = gio::Menu::new();

    let entries = (action.provider)();
    if entries.is_empty() {
        menu.append(Some("No playlists."), None::<&str>);
        return menu;
    }

    let callback = action.callback.clone();
    let track_ids = track_ids.to_vec();
    let add_action = gio::SimpleAction::new(ADD_TO_PLAYLIST_ACTION, Some(glib::VariantTy::INT64));
    add_action.connect_activate(move |_action, parameter| {
        let Some(playlist_id) = parameter
            .and_then(glib::Variant::get::<i64>)
            .and_then(PlaylistId::new)
        else {
            return;
        };
        callback(playlist_id, track_ids.clone());
    });
    action_group.add_action(&add_action);

    let detailed_action = format!("{local_action_group}.{ADD_TO_PLAYLIST_ACTION}");
    for entry in entries {
        let item = gio::MenuItem::new(Some(&entry.display_path), None);
        item.set_action_and_target_value(
            Some(&detailed_action),
            Some(&entry.playlist_id.get().to_variant()),
        );
        menu.append_item(&item);
    }

    menu
}

fn analyze_submenu_model(
    menu: &AnalyzeMenu,
    action_group: &gio::SimpleActionGroup,
    local_action_group: &str,
    track_ids: &[TrackId],
) -> gio::Menu {
    let model = gio::Menu::new();
    for (name, label, capability) in [
        ("analyze-bpm", "BPM", AnalysisCapability::Bpm),
        ("analyze-key", "Key", AnalysisCapability::Key),
        ("analyze-audio", "Audio", AnalysisCapability::Audio),
    ] {
        let run = menu.run.clone();
        let track_ids = track_ids.to_vec();
        add_local_action(action_group, name, !(menu.enabled)(capability), move || {
            run(track_ids.clone(), AnalysisRunRequest::Single(capability));
        });
        model.append(Some(label), Some(&format!("{local_action_group}.{name}")));
    }
    let all = gio::Menu::new();
    let run = menu.run.clone();
    let track_ids = track_ids.to_vec();
    add_local_action(action_group, "analyze-all", true, move || {
        run(track_ids.clone(), AnalysisRunRequest::All);
    });
    all.append(
        Some("All"),
        Some(&format!("{local_action_group}.analyze-all")),
    );
    model.append_section(None, &all);
    model
}

fn retrieve_submenu_model(
    menu: &RetrieveMenu,
    action_group: &gio::SimpleActionGroup,
    local_action_group: &str,
    track_ids: &[TrackId],
) -> gio::Menu {
    let model = gio::Menu::new();
    let busy = (menu.busy)();
    for (name, label, capability) in [
        ("retrieve-lyrics", "Lyrics", OnlineCapability::Lyrics),
        ("retrieve-tags", "Tags", OnlineCapability::Tags),
        ("retrieve-artwork", "Artwork", OnlineCapability::Artwork),
    ] {
        let run = menu.run.clone();
        let track_ids = track_ids.to_vec();
        add_local_action(action_group, name, !busy, move || {
            run(track_ids.clone(), OnlineRunRequest::Single(capability));
        });
        model.append(Some(label), Some(&format!("{local_action_group}.{name}")));
    }
    let all = gio::Menu::new();
    let run = menu.run.clone();
    let all_track_ids = track_ids.to_vec();
    add_local_action(action_group, "retrieve-all", !busy, move || {
        run(all_track_ids.clone(), OnlineRunRequest::All);
    });
    all.append(
        Some("All"),
        Some(&format!("{local_action_group}.retrieve-all")),
    );
    model.append_section(None, &all);
    if (menu.youtube_replacement_visibility)(track_ids) {
        let youtube = gio::Menu::new();
        let replace_audio_from_youtube = menu.replace_audio_from_youtube.clone();
        let track_id = track_ids[0];
        add_local_action(action_group, "fetch-youtube-audio", true, move || {
            replace_audio_from_youtube(track_id)
        });
        youtube.append(
            Some("Audio from YouTube"),
            Some(&format!("{local_action_group}.fetch-youtube-audio")),
        );
        model.append_section(None, &youtube);
    }
    model
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
    crate::confirmation::show_confirmation_alert(
        parent,
        "Move to Trash",
        &detail,
        "Move to Trash",
        move || on_confirm(track_ids),
    );
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
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    use gtk::prelude::*;
    use gtk::{gio, glib};
    use sustain_app_runtime::{PlaylistId, TrackId};

    use super::{
        ADD_TO_PLAYLIST_ACTION, AddToPlaylistAction, AddToPlaylistEntry, TrackActionCallback,
        TrackActionInvocation, TrackActionVisibility, TrackContextAction, TrackContextActionId,
        TrackContextActionSection, TrackContextActionSet, TrackContextInvocationState,
        TrackRowContextMenu, TrackSelectionRequirement, add_to_playlist_submenu_model,
        track_context_popover, trash_confirmation_detail,
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
    fn action_models_use_registered_application_actions_when_shortcuts_exist() {
        assert_eq!(
            TrackContextActionId::GetInfo.detailed_action("track-context-17"),
            "app.get-info"
        );
        assert_eq!(
            TrackContextActionId::ShowInFolder.detailed_action("track-context-17"),
            "app.show-in-folder"
        );
        assert_eq!(
            TrackContextActionId::CopyFiles.detailed_action("track-context-17"),
            "track-context-17.copy-files"
        );
    }

    #[test]
    fn stale_popover_close_does_not_clear_the_newer_invocation() {
        let state = TrackContextInvocationState::default();
        let first = TrackActionInvocation {
            selected_track_ids: vec![TrackId::new(1).expect("positive track id")],
            displayed_track_ids: Vec::new(),
        };
        let second = TrackActionInvocation {
            selected_track_ids: vec![TrackId::new(2).expect("positive track id")],
            displayed_track_ids: Vec::new(),
        };

        let first_serial = state.activate(first);
        let second_serial = state.activate(second.clone());
        state.clear_if_active(first_serial);
        assert_eq!(
            state
                .current()
                .expect("newer invocation remains active")
                .selected_track_ids,
            second.selected_track_ids
        );

        state.clear_if_active(second_serial);
        assert!(state.current().is_none());
    }

    #[test]
    fn model_button_activates_anchor_scoped_action_after_popdown() {
        let ran = crate::test_support::with_gtk(|| {
            let calls = Rc::new(Cell::new(0));
            let calls_for_action = calls.clone();
            let actions = gtk::gio::SimpleActionGroup::new();
            let action = gtk::gio::SimpleAction::new("show-album", None);
            action.connect_activate(move |_action, _parameter| {
                calls_for_action.set(calls_for_action.get() + 1);
            });
            actions.add_action(&action);

            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            anchor.insert_action_group("track-context-1", Some(&actions));
            let menu = gtk::gio::Menu::new();
            menu.append(Some("Show Album"), Some("track-context-1.show-album"));
            let popover = gtk::PopoverMenu::from_model(None::<&gtk::gio::Menu>);

            let window = gtk::Window::new();
            window.set_child(Some(&anchor));
            popover.set_parent(&anchor);
            popover.set_menu_model(Some(&menu));
            let popover_for_close = popover.clone();
            popover.connect_closed(move |_| {
                let popover_for_close = popover_for_close.clone();
                gtk::glib::idle_add_local_once(move || popover_for_close.unparent());
            });
            window.present();
            popover.popup();

            let model_button = descendant_of_type(popover.upcast_ref(), "GtkModelButton")
                .expect("generated model button");
            assert!(model_button.activate(), "model button accepted activation");
            assert_eq!(
                calls.get(),
                1,
                "popover ancestry survived until GtkActionHelper activation"
            );

            let ctx = gtk::glib::MainContext::default();
            while ctx.iteration(false) {}
            assert!(
                popover.parent().is_none(),
                "idle cleanup unparented popover"
            );
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn track_context_popover_uses_nested_submenus() {
        let ran = crate::test_support::with_gtk(|| {
            let popover = track_context_popover();
            assert_eq!(
                popover.flags(),
                gtk::PopoverMenuFlags::NESTED,
                "track context menus should use desktop nested submenus, not sliding submenus"
            );
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn add_to_playlist_submenu_uses_parameterized_model_items() {
        let action = add_to_playlist_action(vec![
            add_to_playlist_entry(10, "Folder / Playlist"),
            add_to_playlist_entry(11, "Other"),
        ]);
        let action_group = gio::SimpleActionGroup::new();
        let selected_track_id = track_id(4);

        let menu = add_to_playlist_submenu_model(
            &action,
            &action_group,
            "track-context-7",
            &[selected_track_id],
        );

        assert_eq!(menu.n_items(), 2);
        assert_eq!(
            menu.item_attribute_value(0, "label", Some(glib::VariantTy::STRING))
                .and_then(|value| value.get::<String>()),
            Some("Folder / Playlist".to_owned())
        );
        assert_eq!(
            menu.item_attribute_value(0, "action", Some(glib::VariantTy::STRING))
                .and_then(|value| value.get::<String>()),
            Some("track-context-7.add-to-playlist".to_owned())
        );
        assert_eq!(
            menu.item_attribute_value(0, "target", Some(glib::VariantTy::INT64))
                .and_then(|value| value.get::<i64>()),
            Some(10)
        );
        assert!(
            menu.item_attribute_value(0, "custom", None).is_none(),
            "playlist submenu must not rely on GtkPopoverMenu custom children"
        );
    }

    #[test]
    fn add_to_playlist_action_invokes_callback_with_target_playlist() {
        let chosen = Rc::new(RefCell::new(None));
        let chosen_for_callback = chosen.clone();
        let action = AddToPlaylistAction {
            provider: Rc::new(|| vec![add_to_playlist_entry(10, "Playlist")]),
            callback: Rc::new(move |playlist_id, track_ids| {
                chosen_for_callback.replace(Some((playlist_id, track_ids)));
            }),
        };
        let action_group = gio::SimpleActionGroup::new();
        let selected_track_id = track_id(4);

        let _menu = add_to_playlist_submenu_model(
            &action,
            &action_group,
            "track-context-7",
            &[selected_track_id],
        );
        action_group.activate_action(ADD_TO_PLAYLIST_ACTION, Some(&10_i64.to_variant()));

        assert_eq!(
            *chosen.borrow(),
            Some((playlist_id(10), vec![selected_track_id]))
        );
    }

    #[test]
    fn track_context_popup_with_playlist_submenu_does_not_panic() {
        let ran = crate::test_support::with_gtk(|| {
            let window = gtk::Window::new();
            let anchor = gtk::Box::new(gtk::Orientation::Vertical, 0);
            window.set_child(Some(&anchor));
            window.present();

            let menu = TrackRowContextMenu::new(
                TrackContextActionSet::new(Vec::new()),
                window.clone(),
                TrackContextInvocationState::default(),
            )
            .with_add_to_playlist(
                Rc::new(|| vec![add_to_playlist_entry(10, "Playlist")]),
                Rc::new(|_playlist_id, _track_ids| {}),
            );

            menu.popup_at(vec![track_id(4)], vec![track_id(4)], &anchor, 0.0, 0.0);
            let popover = descendant_of_type(anchor.upcast_ref(), "GtkPopoverMenu")
                .expect("track context popover was parented to the anchor");
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
    fn duplicate_consolidation_requires_multiple_tracks() {
        let action = TrackContextAction::consolidate_duplicates(no_op_callback());
        let one = TrackId::new(1).expect("positive track id");
        let two = TrackId::new(2).expect("positive track id");

        assert!(!action.is_available(&[]));
        assert!(!action.is_available(&[one]));
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

    fn add_to_playlist_action(entries: Vec<AddToPlaylistEntry>) -> AddToPlaylistAction {
        AddToPlaylistAction {
            provider: Rc::new(move || entries.clone()),
            callback: Rc::new(|_playlist_id, _track_ids| {}),
        }
    }

    fn add_to_playlist_entry(id: i64, display_path: &str) -> AddToPlaylistEntry {
        AddToPlaylistEntry {
            playlist_id: playlist_id(id),
            display_path: display_path.to_owned(),
        }
    }

    fn track_id(value: i64) -> TrackId {
        TrackId::new(value).expect("positive track id")
    }

    fn playlist_id(value: i64) -> PlaylistId {
        PlaylistId::new(value).expect("positive playlist id")
    }

    fn always_visible() -> TrackActionVisibility {
        Rc::new(|_track_ids| true)
    }

    fn never_visible() -> TrackActionVisibility {
        Rc::new(|_track_ids| false)
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
