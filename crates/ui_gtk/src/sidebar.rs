// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gdk, glib};

use sustain_app_runtime::{
    AnalysisCapability, AnalysisRunRequest, ConnectedDevice, DeviceKind, OnlineRunRequest,
    PlaylistItem, SmartPlaylistId, TocSnapshot, TrackId,
};

use super::{
    SIDEBAR_DEFAULT_WIDTH, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, SharedRuntime,
    sidebar_context::SidebarContextMenu,
};

mod drag_drop;
mod model;
mod row_context;

pub(crate) use drag_drop::{DropPosition, parse_tracks_payload, tracks_drag_payload};
use drag_drop::{SharedDropIndicator, attach_drag_and_drop};
use model::{SidebarItem, build_tree_model, select_item, selected_item};
use row_context::{
    SidebarRowContext, attach_rename_entry_signals, attach_row_context_menu, begin_rename,
};

pub(crate) type SidebarSelectionChangedCallback = Rc<dyn Fn(Option<SidebarSelection>)>;
pub(crate) type SidebarMoveCallback = Rc<dyn Fn(PlaylistItem, PlaylistItem, DropPosition)>;
pub(crate) type SidebarRenameCallback = Rc<dyn Fn(PlaylistItem, String)>;
pub(crate) type SidebarDeleteCallback = Rc<dyn Fn(PlaylistItem)>;
pub(crate) type SidebarTracksDropCallback = Rc<dyn Fn(PlaylistItem, Vec<TrackId>)>;
pub(crate) type SidebarEditSmartPlaylistCallback = Rc<dyn Fn(SmartPlaylistId)>;
/// Invoked when the user picks any item inside the "Analyze"
/// submenu on a playlist or smart playlist sidebar row. The
/// `AnalysisRunRequest` carries either a single capability or the
/// `All` bundle, matching exactly which menu entry was clicked.
pub(crate) type SidebarAnalysisRunCallback = Rc<dyn Fn(PlaylistItem, AnalysisRunRequest)>;
/// Invoked when the user picks any item inside the "Retrieve"
/// submenu on a playlist or smart playlist sidebar row.
pub(crate) type SidebarOnlineRunCallback = Rc<dyn Fn(PlaylistItem, OnlineRunRequest)>;
/// Invoked when the user clicks a removable-sync-target row under the
/// Devices section. Carries the connected device so the panel can render
/// and sync it.
pub(crate) type SidebarDeviceSelectedCallback = Rc<dyn Fn(ConnectedDevice)>;
/// Invoked when the user clicks an inserted-audio-CD row under the Devices
/// section. Carries the probed disc snapshot so the CD-import page can
/// render it.
pub(crate) type SidebarCdSelectedCallback = Rc<dyn Fn(TocSnapshot)>;
/// Invoked when the user clicks the eject control on an audio-CD row.
/// Carries the disc snapshot so the caller can resolve and eject the drive.
pub(crate) type SidebarCdEjectCallback = Rc<dyn Fn(TocSnapshot)>;
pub(crate) type SidebarDuplicatesSelectedCallback = Rc<dyn Fn()>;

/// A transient entry shown under the Devices section. The two kinds open
/// different pages — a removable sync target opens the device-sync panel; an
/// inserted audio CD opens the CD-import page — so the row model is typed
/// rather than forcing one shape onto both.
#[derive(Clone)]
pub(crate) enum SidebarDeviceEntry {
    /// A mounted, writable sync target (USB stick, SD card, Android phone).
    SyncTarget(ConnectedDevice),
    /// An inserted, readable audio CD.
    AudioCd(TocSnapshot),
}
/// Queries whether a given analysis capability is enabled globally
/// (i.e. covered by the background sweep). The sidebar renders the
/// matching submenu item insensitive whenever this returns `true`.
pub(crate) type SidebarAnalysisEnabledQuery = Rc<dyn Fn(AnalysisCapability) -> bool>;
/// Queries whether the online retrieval process is running right now.
/// The sidebar renders the Retrieve submenu entries insensitive while
/// it returns `true`; when idle they are clickable regardless of the
/// background toggle so a manual retrieval can re-contact tracks that
/// previously found nothing (issue #61).
pub(crate) type SidebarOnlineBusyQuery = Rc<dyn Fn() -> bool>;

/// The sidebar's top-level selection targets.
///
/// The sidebar drives every top-level navigation choice:
/// - `Music` — the Library → Music row, the whole-library track table.
/// - `Albums` — the Library → Albums row, the album-cover grid.
/// - `Statistics` — the Library → Statistics row, the library-wide
///   diagnostic charts.
/// - `Item` — a row under the Playlists section (regular playlist,
///   smart playlist, or folder).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SidebarSelection {
    Music,
    Albums,
    Statistics,
    Item(PlaylistItem),
}

type MoveCallbackHolder = Rc<RefCell<Option<SidebarMoveCallback>>>;
type RenameCallbackHolder = Rc<RefCell<Option<SidebarRenameCallback>>>;
type DeleteCallbackHolder = Rc<RefCell<Option<SidebarDeleteCallback>>>;
type TracksDropCallbackHolder = Rc<RefCell<Option<SidebarTracksDropCallback>>>;
type EditSmartPlaylistCallbackHolder = Rc<RefCell<Option<SidebarEditSmartPlaylistCallback>>>;
type AnalysisRunCallbackHolder = Rc<RefCell<Option<SidebarAnalysisRunCallback>>>;
type OnlineRunCallbackHolder = Rc<RefCell<Option<SidebarOnlineRunCallback>>>;
type AnalysisEnabledQueryHolder = Rc<RefCell<Option<SidebarAnalysisEnabledQuery>>>;
type OnlineBusyQueryHolder = Rc<RefCell<Option<SidebarOnlineBusyQuery>>>;

/// Which row under the Library section is currently active. Mutually
/// exclusive with a playlist selection in the list view.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LibraryRowState {
    #[default]
    Music,
    Albums,
    Statistics,
    /// A playlist (or no row at all) is selected — no library row paints
    /// itself active.
    None,
}

#[derive(Clone)]
pub(crate) struct PlaylistSidebar {
    root: gtk::Box,
    selection: gtk::SingleSelection,
    music_row: gtk::Box,
    albums_row: gtk::Box,
    duplicates_row: gtk::Box,
    statistics_row: gtk::Box,
    library_state: Rc<Cell<LibraryRowState>>,
    runtime: SharedRuntime,
    on_selection_changed: Rc<RefCell<Option<SidebarSelectionChangedCallback>>>,
    on_move: MoveCallbackHolder,
    on_rename: RenameCallbackHolder,
    on_delete: DeleteCallbackHolder,
    on_tracks_drop: TracksDropCallbackHolder,
    on_edit_smart_playlist: EditSmartPlaylistCallbackHolder,
    on_analysis_run: AnalysisRunCallbackHolder,
    on_online_run: OnlineRunCallbackHolder,
    analysis_enabled_query: AnalysisEnabledQueryHolder,
    online_busy_query: OnlineBusyQueryHolder,
    pending_rename: Rc<RefCell<Option<PlaylistItem>>>,
    /// Fold state of the Library disclosure section. Read back at
    /// shutdown so the choice persists across launches.
    library_section_collapsed: Rc<Cell<bool>>,
    /// Fold state of the Playlists disclosure section.
    playlists_section_collapsed: Rc<Cell<bool>>,
    /// Container holding the dynamically-rebuilt Devices rows.
    devices_body: gtk::Box,
    on_device_selected: Rc<RefCell<Option<SidebarDeviceSelectedCallback>>>,
    on_cd_selected: Rc<RefCell<Option<SidebarCdSelectedCallback>>>,
    on_cd_eject: Rc<RefCell<Option<SidebarCdEjectCallback>>>,
    on_duplicates_selected: Rc<RefCell<Option<SidebarDuplicatesSelectedCallback>>>,
    /// The currently highlighted *transient* row (a device today, the
    /// future Duplicates scan), for single-selection CSS. Mutually
    /// exclusive with the persistent highlight.
    active_transient_row: Rc<RefCell<Option<gtk::Widget>>>,
    /// The persistent selection live just before a transient view took
    /// over. While a transient view is active this is what gets persisted
    /// and restored — never the transient view itself, which could fail
    /// or be costly to re-open at launch.
    persistent_selection: Rc<Cell<SidebarSelection>>,
}

impl PlaylistSidebar {
    pub(crate) fn new(
        runtime: SharedRuntime,
        library_collapsed: bool,
        playlists_collapsed: bool,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("playlist-sidebar");
        root.set_vexpand(true);
        root.set_size_request(SIDEBAR_MIN_WIDTH, -1);

        let (library_header, library_caret) = build_section_header("Library");
        root.append(&library_header);

        let music_row = build_standalone_sidebar_row("Music", "audio-x-generic-symbolic");
        let albums_row = build_standalone_sidebar_row("Albums", "media-optical-symbolic");
        let duplicates_row = build_standalone_sidebar_row("Duplicates", "edit-copy-symbolic");
        let statistics_row =
            build_standalone_sidebar_row("Statistics", "sustain-statistics-symbolic");
        // Duplicates and Statistics are optional library rows, toggled in
        // the Views preferences (both shown by default).
        {
            let runtime_ref = runtime.borrow();
            let ui = &runtime_ref.settings().ui;
            duplicates_row.set_visible(ui.sidebar_show_duplicates);
            statistics_row.set_visible(ui.sidebar_show_statistics);
        }
        root.append(&music_row);
        root.append(&albums_row);
        root.append(&duplicates_row);
        root.append(&statistics_row);

        // Devices section: connected USB sticks / SD cards. Sits between
        // Library and Playlists; the playlist list view below keeps
        // vexpand, so this group stays a fixed-height block pinned under
        // the library rows while the playlists absorb the remaining
        // space. Rows are rebuilt dynamically from device discovery,
        // which runs after first-frame to keep startup cheap. Folded
        // state is not persisted — the section defaults to expanded.
        let (devices_header, devices_caret) = build_section_header("Devices");
        root.append(&devices_header);
        let devices_body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        devices_body.add_css_class("playlist-sidebar-devices");
        root.append(&devices_body);
        let devices_section_collapsed = Rc::new(Cell::new(false));
        connect_section_toggle(
            &devices_header,
            &devices_caret,
            devices_section_collapsed,
            vec![devices_body.clone().upcast::<gtk::Widget>()],
        );
        let on_device_selected: Rc<RefCell<Option<SidebarDeviceSelectedCallback>>> =
            Rc::new(RefCell::new(None));
        let on_cd_selected: Rc<RefCell<Option<SidebarCdSelectedCallback>>> =
            Rc::new(RefCell::new(None));
        let on_cd_eject: Rc<RefCell<Option<SidebarCdEjectCallback>>> = Rc::new(RefCell::new(None));
        let on_duplicates_selected: Rc<RefCell<Option<SidebarDuplicatesSelectedCallback>>> =
            Rc::new(RefCell::new(None));
        let active_transient_row: Rc<RefCell<Option<gtk::Widget>>> = Rc::new(RefCell::new(None));
        let persistent_selection = Rc::new(Cell::new(SidebarSelection::Music));

        let (playlists_header, playlists_caret) = build_section_header("Playlists");
        root.append(&playlists_header);

        let tree_model = build_tree_model(&runtime.borrow());
        let selection = gtk::SingleSelection::new(Some(tree_model));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let on_move: MoveCallbackHolder = Rc::new(RefCell::new(None));
        let on_rename: RenameCallbackHolder = Rc::new(RefCell::new(None));
        let on_delete: DeleteCallbackHolder = Rc::new(RefCell::new(None));
        let on_tracks_drop: TracksDropCallbackHolder = Rc::new(RefCell::new(None));
        let on_edit_smart_playlist: EditSmartPlaylistCallbackHolder = Rc::new(RefCell::new(None));
        let on_analysis_run: AnalysisRunCallbackHolder = Rc::new(RefCell::new(None));
        let on_online_run: OnlineRunCallbackHolder = Rc::new(RefCell::new(None));
        let analysis_enabled_query: AnalysisEnabledQueryHolder = Rc::new(RefCell::new(None));
        let online_busy_query: OnlineBusyQueryHolder = Rc::new(RefCell::new(None));
        let pending_rename: Rc<RefCell<Option<PlaylistItem>>> = Rc::new(RefCell::new(None));
        let list_view = gtk::ListView::new(
            Some(selection.clone()),
            Some(build_row_factory(
                on_move.clone(),
                on_rename.clone(),
                on_delete.clone(),
                on_tracks_drop.clone(),
                on_edit_smart_playlist.clone(),
                on_analysis_run.clone(),
                on_online_run.clone(),
                analysis_enabled_query.clone(),
                online_busy_query.clone(),
                pending_rename.clone(),
            )),
        );
        list_view.add_css_class("playlist-sidebar-list");
        list_view.set_vexpand(true);
        list_view.set_single_click_activate(false);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_vexpand(true);
        scroller.set_hexpand(true);
        scroller.set_child(Some(&list_view));
        root.append(&scroller);

        // Wire the two disclosure sections. Folding hides the section's
        // rows and flips the caret; the state lives in a cell read back
        // at shutdown so the choice persists across launches. The
        // sections are independent.
        //
        // Library folds away its fixed-height rows, so collapsing it
        // lets Playlists rise to fill the freed space. Playlists folds the
        // list view itself rather than the enclosing scroller: the scroller
        // keeps `vexpand`, so the now-empty area is held by the scroller
        // instead of letting the list float up under the header.
        let library_section_collapsed = Rc::new(Cell::new(library_collapsed));
        let playlists_section_collapsed = Rc::new(Cell::new(playlists_collapsed));
        connect_section_toggle(
            &library_header,
            &library_caret,
            library_section_collapsed.clone(),
            vec![
                music_row.clone().upcast::<gtk::Widget>(),
                albums_row.clone().upcast::<gtk::Widget>(),
                duplicates_row.clone().upcast::<gtk::Widget>(),
                statistics_row.clone().upcast::<gtk::Widget>(),
            ],
        );
        connect_section_toggle(
            &playlists_header,
            &playlists_caret,
            playlists_section_collapsed.clone(),
            vec![list_view.clone().upcast::<gtk::Widget>()],
        );

        let library_state = Rc::new(Cell::new(LibraryRowState::Music));
        let on_selection_changed: Rc<RefCell<Option<SidebarSelectionChangedCallback>>> =
            Rc::new(RefCell::new(None));

        connect_library_row(
            &music_row,
            LibraryRowState::Music,
            &music_row,
            &albums_row,
            &statistics_row,
            &library_state,
            &selection,
            on_selection_changed.clone(),
        );
        connect_library_row(
            &albums_row,
            LibraryRowState::Albums,
            &music_row,
            &albums_row,
            &statistics_row,
            &library_state,
            &selection,
            on_selection_changed.clone(),
        );
        connect_library_row(
            &statistics_row,
            LibraryRowState::Statistics,
            &music_row,
            &albums_row,
            &statistics_row,
            &library_state,
            &selection,
            on_selection_changed.clone(),
        );
        connect_selection_signal(
            &selection,
            &music_row,
            &albums_row,
            &statistics_row,
            &library_state,
            on_selection_changed.clone(),
        );

        // Music is the default landing entry for a fresh session.
        // The SingleSelection's default behavior could otherwise pick
        // the first row of the playlist tree as "selected", which
        // would render two rows highlighted simultaneously (Music via
        // its CSS class and a playlist via the list selection). Force
        // an empty playlist selection up front; the Library state is
        // the only thing live at this point.
        music_row.add_css_class("selected");
        selection.set_selected(gtk::INVALID_LIST_POSITION);

        let duplicates_gesture = gtk::GestureClick::new();
        duplicates_gesture.set_button(gdk::BUTTON_PRIMARY);
        let duplicates_sidebar_row = duplicates_row.clone().upcast::<gtk::Widget>();
        let sidebar_active_transient_row = active_transient_row.clone();
        let persistent_selection_for_duplicates = persistent_selection.clone();
        let library_state_for_duplicates = library_state.clone();
        let selection_for_duplicates = selection.clone();
        let music_row_for_duplicates = music_row.clone();
        let albums_row_for_duplicates = albums_row.clone();
        let statistics_row_for_duplicates = statistics_row.clone();
        let on_selection_changed_for_duplicates = on_selection_changed.clone();
        let on_duplicates_selected_for_gesture = on_duplicates_selected.clone();
        duplicates_gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            activate_transient_view_state(
                &duplicates_sidebar_row,
                &sidebar_active_transient_row,
                &persistent_selection_for_duplicates,
                &library_state_for_duplicates,
                &selection_for_duplicates,
                &on_selection_changed_for_duplicates,
                &music_row_for_duplicates,
                &albums_row_for_duplicates,
                &statistics_row_for_duplicates,
            );
            if let Some(callback) = on_duplicates_selected_for_gesture.borrow().as_ref() {
                callback();
            }
        });
        duplicates_row.add_controller(duplicates_gesture);

        Self {
            root,
            selection,
            music_row,
            albums_row,
            duplicates_row,
            statistics_row,
            library_state,
            runtime,
            on_selection_changed,
            on_move,
            on_rename,
            on_delete,
            on_tracks_drop,
            on_edit_smart_playlist,
            on_analysis_run,
            on_online_run,
            analysis_enabled_query,
            online_busy_query,
            pending_rename,
            library_section_collapsed,
            playlists_section_collapsed,
            devices_body,
            on_device_selected,
            on_cd_selected,
            on_cd_eject,
            on_duplicates_selected,
            active_transient_row,
            persistent_selection,
        }
    }

    /// Whether the Library disclosure section is currently folded shut.
    /// Read at shutdown to persist the fold state.
    pub(crate) fn library_section_collapsed(&self) -> bool {
        self.library_section_collapsed.get()
    }

    /// Whether the Playlists disclosure section is currently folded shut.
    pub(crate) fn playlists_section_collapsed(&self) -> bool {
        self.playlists_section_collapsed.get()
    }

    pub(crate) fn widget(&self) -> gtk::Box {
        self.root.clone()
    }

    /// Show or hide the optional Library rows per the Views preferences.
    /// Called live when the toggles change so the sidebar updates without
    /// a relaunch.
    pub(crate) fn set_view_row_visibility(&self, show_duplicates: bool, show_statistics: bool) {
        self.duplicates_row.set_visible(show_duplicates);
        self.statistics_row.set_visible(show_statistics);
    }

    pub(crate) fn set_selection_changed(&self, callback: SidebarSelectionChangedCallback) {
        let initial = callback.clone();
        self.on_selection_changed.replace(Some(callback));
        initial(self.current_selection());
    }

    pub(crate) fn set_move_callback(&self, callback: SidebarMoveCallback) {
        self.on_move.replace(Some(callback));
    }

    pub(crate) fn set_rename_callback(&self, callback: SidebarRenameCallback) {
        self.on_rename.replace(Some(callback));
    }

    pub(crate) fn set_delete_callback(&self, callback: SidebarDeleteCallback) {
        self.on_delete.replace(Some(callback));
    }

    pub(crate) fn set_tracks_drop_callback(&self, callback: SidebarTracksDropCallback) {
        self.on_tracks_drop.replace(Some(callback));
    }

    pub(crate) fn set_edit_smart_playlist_callback(
        &self,
        callback: SidebarEditSmartPlaylistCallback,
    ) {
        self.on_edit_smart_playlist.replace(Some(callback));
    }

    pub(crate) fn set_analysis_run_callback(&self, callback: SidebarAnalysisRunCallback) {
        self.on_analysis_run.replace(Some(callback));
    }

    pub(crate) fn set_online_run_callback(&self, callback: SidebarOnlineRunCallback) {
        self.on_online_run.replace(Some(callback));
    }

    pub(crate) fn set_analysis_enabled_query(&self, query: SidebarAnalysisEnabledQuery) {
        self.analysis_enabled_query.replace(Some(query));
    }

    pub(crate) fn set_online_busy_query(&self, query: SidebarOnlineBusyQuery) {
        self.online_busy_query.replace(Some(query));
    }

    pub(crate) fn set_device_selected_callback(&self, callback: SidebarDeviceSelectedCallback) {
        self.on_device_selected.replace(Some(callback));
    }

    pub(crate) fn set_cd_selected_callback(&self, callback: SidebarCdSelectedCallback) {
        self.on_cd_selected.replace(Some(callback));
    }

    pub(crate) fn set_cd_eject_callback(&self, callback: SidebarCdEjectCallback) {
        self.on_cd_eject.replace(Some(callback));
    }

    /// Re-activate the persistent selection that preceded a transient view.
    /// Used when a transient view's backing hardware goes away (an audio CD
    /// is ejected, a sync device is unplugged) while that view is on screen:
    /// the page is no longer meaningful, so we fall back to the library row
    /// or playlist the user was on before. Re-selecting fires the normal
    /// selection-changed path, which switches the content area and clears the
    /// (already-stale) transient highlight.
    pub(crate) fn restore_persistent_selection(&self) {
        match self.persistent_selection.get() {
            SidebarSelection::Music => self.select_music(),
            SidebarSelection::Albums => self.select_albums(),
            SidebarSelection::Statistics => self.select_statistics(),
            // `select_item` falls back to Music if the playlist is gone.
            SidebarSelection::Item(item) => self.select_item(item),
        }
    }

    pub(crate) fn set_duplicates_selected_callback(
        &self,
        callback: SidebarDuplicatesSelectedCallback,
    ) {
        self.on_duplicates_selected.replace(Some(callback));
    }

    /// Rebuild the Devices section from the current set of transient
    /// entries — removable sync targets and inserted audio CDs. Clears the
    /// previous rows and any highlight.
    pub(crate) fn set_devices(&self, entries: &[SidebarDeviceEntry]) {
        while let Some(child) = self.devices_body.first_child() {
            self.devices_body.remove(&child);
        }
        self.active_transient_row.replace(None);

        if entries.is_empty() {
            let empty = gtk::Label::new(Some("No devices connected"));
            empty.add_css_class("playlist-sidebar-devices-empty");
            empty.add_css_class("dim-label");
            empty.set_xalign(0.0);
            self.devices_body.append(&empty);
            return;
        }

        for entry in entries {
            let row = match entry {
                SidebarDeviceEntry::SyncTarget(device) => {
                    let icon = match device.kind {
                        DeviceKind::Android => "phone-symbolic",
                        DeviceKind::UsbDrive => "drive-removable-media-symbolic",
                    };
                    let row = build_standalone_sidebar_row(&device.label, icon);
                    let sidebar = self.clone();
                    let row_widget = row.clone().upcast::<gtk::Widget>();
                    let device = device.clone();
                    attach_transient_row_gesture(
                        &row,
                        move |sidebar_row| {
                            sidebar.activate_transient_view(sidebar_row);
                            if let Some(callback) = sidebar.on_device_selected.borrow().as_ref() {
                                callback(device.clone());
                            }
                        },
                        row_widget,
                    );
                    row
                }
                SidebarDeviceEntry::AudioCd(snapshot) => {
                    let row_parts =
                        build_standalone_sidebar_row_parts("Audio CD", "media-optical-symbolic");
                    let row = row_parts.row;
                    // Trailing eject control. A flat icon button so it reads
                    // as a per-row affordance, not a primary action. Clicking
                    // it claims the press (GtkButton handles the click), so the
                    // row's select gesture does not also fire.
                    let eject = gtk::Button::from_icon_name("media-eject-symbolic");
                    eject.add_css_class("flat");
                    eject.add_css_class("playlist-sidebar-cd-eject");
                    eject.set_tooltip_text(Some("Eject"));
                    eject.set_valign(gtk::Align::Center);
                    // Drive the eject from a CAPTURE-phase gesture that claims
                    // the press before it reaches the row's bubble-phase select
                    // gesture — otherwise the click falls through and merely
                    // re-opens the CD page instead of ejecting.
                    {
                        let gesture = gtk::GestureClick::new();
                        gesture.set_button(gdk::BUTTON_PRIMARY);
                        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
                        let sidebar = self.clone();
                        let snapshot = snapshot.clone();
                        gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
                            gesture.set_state(gtk::EventSequenceState::Claimed);
                            if let Some(callback) = sidebar.on_cd_eject.borrow().as_ref() {
                                callback(snapshot.clone());
                            }
                        });
                        eject.add_controller(gesture);
                    }
                    row_parts.content.append(&eject);
                    let sidebar = self.clone();
                    let row_widget = row.clone().upcast::<gtk::Widget>();
                    let snapshot = snapshot.clone();
                    attach_transient_row_gesture(
                        &row,
                        move |sidebar_row| {
                            sidebar.activate_transient_view(sidebar_row);
                            if let Some(callback) = sidebar.on_cd_selected.borrow().as_ref() {
                                callback(snapshot.clone());
                            }
                        },
                        row_widget,
                    );
                    row
                }
            };
            self.devices_body.append(&row);
        }
    }

    /// Make `row` the sidebar's sole active highlight on behalf of a
    /// *transient* view — a connected device or the deferred Duplicates
    /// scan. The persistent selection
    /// (Music / Albums / a playlist) that was live is visually
    /// deactivated but remembered, so it — not this transient view — is
    /// what [`Self::persisted_selection`] reports and what is restored on
    /// the next launch. Transient views are never persisted: re-opening
    /// one at startup could fail (the device is gone) or be expensive.
    pub(crate) fn activate_transient_view(&self, row: &gtk::Widget) {
        activate_transient_view_state(
            row,
            &self.active_transient_row,
            &self.persistent_selection,
            &self.library_state,
            &self.selection,
            &self.on_selection_changed,
            &self.music_row,
            &self.albums_row,
            &self.statistics_row,
        );
    }

    /// Drop the transient-view highlight — called when a persistent
    /// sidebar surface (a library row or a playlist) becomes active.
    pub(crate) fn clear_transient_highlight(&self) {
        if let Some(previous) = self.active_transient_row.borrow_mut().take() {
            previous.remove_css_class("selected");
        }
    }

    /// The selection to persist and restore on launch. While a transient
    /// view is active this is the persistent selection that preceded it
    /// (never the transient view itself); otherwise it is the live
    /// selection.
    pub(crate) fn persisted_selection(&self) -> Option<SidebarSelection> {
        if self.active_transient_row.borrow().is_some() {
            Some(self.persistent_selection.get())
        } else {
            self.current_selection()
        }
    }

    /// Arm an inline rename for `item` on the next bind that matches it.
    /// Designed for the "create then immediately name" flow: callers set this
    /// before calling [`Self::refresh`], so the new row enters edit mode the moment
    /// it is bound.
    pub(crate) fn arm_pending_rename(&self, item: PlaylistItem) {
        *self.pending_rename.borrow_mut() = Some(item);
    }

    pub(crate) fn current_selection(&self) -> Option<SidebarSelection> {
        current_selection(self.library_state.get(), &self.selection)
    }

    pub(crate) fn select_music(&self) {
        self.activate_library_row(LibraryRowState::Music);
    }

    pub(crate) fn select_albums(&self) {
        self.activate_library_row(LibraryRowState::Albums);
    }

    pub(crate) fn select_statistics(&self) {
        self.activate_library_row(LibraryRowState::Statistics);
    }

    pub(crate) fn select_item(&self, item: PlaylistItem) {
        self.library_state.set(LibraryRowState::None);
        self.paint_library_rows();
        if !select_item(&self.selection, item) {
            // The playlist no longer exists (e.g. deleted between
            // sessions). Fall back to the default landing surface.
            self.activate_library_row(LibraryRowState::Music);
        }
    }

    fn activate_library_row(&self, target: LibraryRowState) {
        self.library_state.set(target);
        self.paint_library_rows();
        self.selection.set_selected(gtk::INVALID_LIST_POSITION);
        if let Some(callback) = self.on_selection_changed.borrow().as_ref() {
            callback(self.current_selection());
        }
    }

    /// Paint the `.selected` CSS class onto exactly the Library row that
    /// matches the current [`LibraryRowState`], clearing it from the
    /// others. Centralised so adding a row (Statistics) does not multiply
    /// the per-call-site class juggling.
    fn paint_library_rows(&self) {
        let state = self.library_state.get();
        set_row_selected(&self.music_row, state == LibraryRowState::Music);
        set_row_selected(&self.albums_row, state == LibraryRowState::Albums);
        set_row_selected(&self.statistics_row, state == LibraryRowState::Statistics);
    }

    pub(crate) fn install_context_menu(&self, menu: SidebarContextMenu) {
        menu.install_on(self.root.upcast_ref::<gtk::Widget>());
    }

    pub(crate) fn refresh(&self) {
        self.refresh_with_selection_notification(true);
    }

    /// Rebuild the tree after deferred startup hydration without firing the
    /// Music selection callback. Songs rows and their aggregate summary are
    /// already being published incrementally; re-running that callback would
    /// rematerialize the full table on the GTK thread.
    pub(crate) fn refresh_silently(&self) {
        self.refresh_with_selection_notification(false);
    }

    fn refresh_with_selection_notification(&self, notify_selection: bool) {
        let transient_active = self.active_transient_row.borrow().is_some();
        let previous = self.current_selection();
        let tree_model = build_tree_model(&self.runtime.borrow());
        // Suppress the selection callback while we swap the model and
        // restore the previous selection. `set_model` and `select_item`
        // each emit `selected_notify` synchronously, and the connected
        // notify handler would otherwise fire the callback up to twice
        // — each call rebuilds the playlists table, which dominates
        // refresh cost on a large library. Suspend, do the surgery,
        // restore, and fire the callback exactly once with the final
        // selection.
        let suspended = self.on_selection_changed.borrow_mut().take();
        self.selection.set_model(Some(&tree_model));
        match previous {
            Some(SidebarSelection::Item(item)) => {
                self.library_state.set(LibraryRowState::None);
                self.paint_library_rows();
                if !select_item(&self.selection, item) {
                    self.library_state.set(LibraryRowState::Music);
                    self.paint_library_rows();
                }
            }
            Some(SidebarSelection::Music)
            | Some(SidebarSelection::Albums)
            | Some(SidebarSelection::Statistics)
            | None => {
                // The library-row CSS state was left untouched; only
                // ensure the playlist list view shows no selection.
                self.selection.set_selected(gtk::INVALID_LIST_POSITION);
            }
        }
        *self.on_selection_changed.borrow_mut() = suspended;
        if transient_active || !notify_selection {
            // A transient view owns the content area, or deferred startup
            // explicitly requested a quiet model refresh. Keep the current
            // page shown rather than firing a selection callback.
            return;
        }
        if let Some(callback) = self.on_selection_changed.borrow().as_ref() {
            callback(self.current_selection());
        }
    }
}

/// Attach the primary-click gesture that activates a transient Devices row.
/// Shared by the sync-target and audio-CD rows: both claim the gesture,
/// take over the sidebar highlight, and fire their kind's callback.
fn attach_transient_row_gesture(
    row: &gtk::Box,
    on_pressed: impl Fn(&gtk::Widget) + 'static,
    row_widget: gtk::Widget,
) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        on_pressed(&row_widget);
    });
    row.add_controller(gesture);
}

/// Builds a standalone top-level sidebar row (Music, Albums, Devices, …).
///
/// Rendered with the same `.playlist-sidebar-row` styling as a playlist
/// row so the Library, Devices, and Playlists sections present a uniform list of
/// selectable items.
fn build_standalone_sidebar_row(label_text: &str, icon_name: &str) -> gtk::Box {
    build_standalone_sidebar_row_parts(label_text, icon_name).row
}

struct StandaloneSidebarRowParts {
    row: gtk::Box,
    content: gtk::Box,
}

fn build_standalone_sidebar_row_parts(
    label_text: &str,
    icon_name: &str,
) -> StandaloneSidebarRowParts {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    row.add_css_class("playlist-sidebar-row");

    let indent_slot = gtk::Image::from_icon_name(SIDEBAR_TREE_INDENT_ICON);
    indent_slot.set_opacity(0.0);
    indent_slot.set_can_target(false);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.set_hexpand(true);

    let icon = gtk::Image::from_icon_name(icon_name);
    icon.add_css_class("playlist-sidebar-icon");

    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    content.append(&icon);
    content.append(&label);
    row.append(&indent_slot);
    row.append(&content);

    StandaloneSidebarRowParts { row, content }
}

/// Builds a clickable disclosure header for a sidebar section — the small
/// labels that introduce the Library, Devices, and Playlists groups, each
/// prefixed by a caret that reflects (and toggles) whether
/// the section is folded. Returns the header row and its caret image;
/// the caller wires the fold behaviour via [`connect_section_toggle`].
///
/// The whole row is the click/focus target. It is focusable so the
/// Left/Right arrow keys can drive the fold once the header is focused.
fn build_section_header(text: &str) -> (gtk::Box, gtk::Image) {
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    header.add_css_class("playlist-sidebar-section-header-row");
    header.set_focusable(true);

    let caret = gtk::Image::from_icon_name(SECTION_EXPANDED_ICON);
    caret.add_css_class("playlist-sidebar-section-caret");

    let label = gtk::Label::new(Some(text));
    label.add_css_class("playlist-sidebar-section-header");
    label.set_xalign(0.0);
    label.set_hexpand(true);

    header.append(&caret);
    header.append(&label);
    (header, caret)
}

/// Caret icon for an expanded (open) disclosure section — points down.
const SECTION_EXPANDED_ICON: &str = "pan-down-symbolic";
/// Caret icon for a collapsed (folded) disclosure section — points to
/// the inline-end edge (right in LTR).
const SECTION_COLLAPSED_ICON: &str = "pan-end-symbolic";
/// Invisible slot used by standalone top-level rows to align their content
/// with depth-0 playlist rows, whose real [`gtk::TreeExpander`] prepends an
/// expander or indent node before the row content.
const SIDEBAR_TREE_INDENT_ICON: &str = "pan-end-symbolic";

/// Paints a section's current fold state: swaps the caret glyph and
/// shows or hides every body widget belonging to the section.
fn apply_section_visual(caret: &gtk::Image, bodies: &[gtk::Widget], collapsed: bool) {
    caret.set_icon_name(Some(if collapsed {
        SECTION_COLLAPSED_ICON
    } else {
        SECTION_EXPANDED_ICON
    }));
    for body in bodies {
        body.set_visible(!collapsed);
    }
}

/// Wires a disclosure header so that clicking it (or pressing
/// Enter/Space while it is focused) toggles the section, while Left
/// folds and Right unfolds it. `collapsed` is the persisted fold cell;
/// `bodies` are the widgets shown only while the section is open. The
/// initial visual state is applied immediately from `collapsed`.
fn connect_section_toggle(
    header: &gtk::Box,
    caret: &gtk::Image,
    collapsed: Rc<Cell<bool>>,
    bodies: Vec<gtk::Widget>,
) {
    apply_section_visual(caret, &bodies, collapsed.get());

    let set_collapsed: Rc<dyn Fn(bool)> = {
        let caret = caret.clone();
        let collapsed = collapsed.clone();
        Rc::new(move |want: bool| {
            if collapsed.get() == want {
                return;
            }
            collapsed.set(want);
            apply_section_visual(&caret, &bodies, want);
        })
    };

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    {
        let header = header.clone();
        let set_collapsed = set_collapsed.clone();
        let collapsed = collapsed.clone();
        gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            // Take focus so the arrow-key controller below acts on this
            // header immediately after a click.
            header.grab_focus();
            set_collapsed(!collapsed.get());
        });
    }
    header.add_controller(gesture);

    let keys = gtk::EventControllerKey::new();
    {
        let set_collapsed = set_collapsed.clone();
        keys.connect_key_pressed(move |_controller, key, _code, _modifiers| match key {
            gdk::Key::Left => {
                set_collapsed(true);
                glib::Propagation::Stop
            }
            gdk::Key::Right => {
                set_collapsed(false);
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => {
                set_collapsed(!collapsed.get());
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
    }
    header.add_controller(keys);
}

/// Toggle the `.selected` highlight on a single Library row.
fn set_row_selected(row: &gtk::Box, selected: bool) {
    if selected {
        row.add_css_class("selected");
    } else {
        row.remove_css_class("selected");
    }
}

/// Paint the `.selected` highlight onto the row matching `state`, clearing
/// it from the others. The free-function counterpart of
/// [`PlaylistSidebar::paint_library_rows`], used by the per-row click
/// gesture and the selection-change signal, which hold the rows directly
/// rather than `&self`.
fn paint_library_rows(
    state: LibraryRowState,
    music_row: &gtk::Box,
    albums_row: &gtk::Box,
    statistics_row: &gtk::Box,
) {
    set_row_selected(music_row, state == LibraryRowState::Music);
    set_row_selected(albums_row, state == LibraryRowState::Albums);
    set_row_selected(statistics_row, state == LibraryRowState::Statistics);
}

/// Map a Library row's state to the selection it represents. Returns
/// `None` for [`LibraryRowState::None`], which is not a row.
fn library_row_selection(state: LibraryRowState) -> Option<SidebarSelection> {
    match state {
        LibraryRowState::Music => Some(SidebarSelection::Music),
        LibraryRowState::Albums => Some(SidebarSelection::Albums),
        LibraryRowState::Statistics => Some(SidebarSelection::Statistics),
        LibraryRowState::None => None,
    }
}

fn current_selection(
    library_state: LibraryRowState,
    selection: &gtk::SingleSelection,
) -> Option<SidebarSelection> {
    library_row_selection(library_state)
        .or_else(|| selected_item(selection).map(SidebarSelection::Item))
}

#[allow(clippy::too_many_arguments)]
fn activate_transient_view_state(
    row: &gtk::Widget,
    active_transient_row: &Rc<RefCell<Option<gtk::Widget>>>,
    persistent_selection: &Rc<Cell<SidebarSelection>>,
    library_state: &Rc<Cell<LibraryRowState>>,
    selection: &gtk::SingleSelection,
    on_selection_changed: &Rc<RefCell<Option<SidebarSelectionChangedCallback>>>,
    music_row: &gtk::Box,
    albums_row: &gtk::Box,
    statistics_row: &gtk::Box,
) {
    // Entering the transient state from a persistent one: snapshot the
    // persistent selection. Switching transient-to-transient keeps the
    // original snapshot.
    if active_transient_row.borrow().is_none()
        && let Some(selection) = current_selection(library_state.get(), selection)
    {
        persistent_selection.set(selection);
    }
    if let Some(previous) = active_transient_row.borrow_mut().take() {
        previous.remove_css_class("selected");
    }
    // Clear the persistent highlight without firing the selection callback:
    // that would switch the content area back to a library or playlist view.
    let suspended = on_selection_changed.borrow_mut().take();
    library_state.set(LibraryRowState::None);
    paint_library_rows(LibraryRowState::None, music_row, albums_row, statistics_row);
    selection.set_selected(gtk::INVALID_LIST_POSITION);
    *on_selection_changed.borrow_mut() = suspended;

    row.add_css_class("selected");
    active_transient_row.replace(Some(row.clone()));
}

#[allow(clippy::too_many_arguments)]
fn connect_library_row(
    target_row: &gtk::Box,
    target_state: LibraryRowState,
    music_row: &gtk::Box,
    albums_row: &gtk::Box,
    statistics_row: &gtk::Box,
    library_state: &Rc<Cell<LibraryRowState>>,
    selection: &gtk::SingleSelection,
    on_selection_changed: Rc<RefCell<Option<SidebarSelectionChangedCallback>>>,
) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);

    let music_row = music_row.clone();
    let albums_row = albums_row.clone();
    let statistics_row = statistics_row.clone();
    let library_state = library_state.clone();
    let selection = selection.clone();
    gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let already_active = library_state.get() == target_state;
        if !already_active {
            library_state.set(target_state);
            paint_library_rows(target_state, &music_row, &albums_row, &statistics_row);
            selection.set_selected(gtk::INVALID_LIST_POSITION);
        }
        if let Some(callback) = on_selection_changed.borrow().as_ref()
            && let Some(selection) = library_row_selection(target_state)
        {
            callback(Some(selection));
        }
    });
    target_row.add_controller(gesture);
}

fn connect_selection_signal(
    selection: &gtk::SingleSelection,
    music_row: &gtk::Box,
    albums_row: &gtk::Box,
    statistics_row: &gtk::Box,
    library_state: &Rc<Cell<LibraryRowState>>,
    on_selection_changed: Rc<RefCell<Option<SidebarSelectionChangedCallback>>>,
) {
    let selection_clone = selection.clone();
    let music_row = music_row.clone();
    let albums_row = albums_row.clone();
    let statistics_row = statistics_row.clone();
    let library_state = library_state.clone();
    selection.connect_selected_notify(move |_selection| {
        let item = selected_item(&selection_clone);
        let new_selection = if let Some(item) = item {
            // A playlist row was just picked. Any active Library row
            // loses its highlight.
            if library_state.get() != LibraryRowState::None {
                library_state.set(LibraryRowState::None);
                paint_library_rows(
                    LibraryRowState::None,
                    &music_row,
                    &albums_row,
                    &statistics_row,
                );
            }
            Some(SidebarSelection::Item(item))
        } else {
            library_row_selection(library_state.get())
        };
        if let Some(callback) = on_selection_changed.borrow().as_ref() {
            callback(new_selection);
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn build_row_factory(
    on_move: MoveCallbackHolder,
    on_rename: RenameCallbackHolder,
    on_delete: DeleteCallbackHolder,
    on_tracks_drop: TracksDropCallbackHolder,
    on_edit_smart_playlist: EditSmartPlaylistCallbackHolder,
    on_analysis_run: AnalysisRunCallbackHolder,
    on_online_run: OnlineRunCallbackHolder,
    analysis_enabled_query: AnalysisEnabledQueryHolder,
    online_busy_query: OnlineBusyQueryHolder,
    pending_rename: Rc<RefCell<Option<PlaylistItem>>>,
) -> gtk::SignalListItemFactory {
    let current_indicator: SharedDropIndicator = Rc::new(RefCell::new(None));
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_factory, list_item| {
        let Some(list_item) = list_item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        // Inner Box no longer carries the visual frame class; the frame
        // (padding, border-radius, drop indicators) lives on the TreeExpander
        // below so it spans the full row width.

        let icon = gtk::Image::new();
        icon.add_css_class("playlist-sidebar-icon");

        let name_stack = gtk::Stack::new();
        name_stack.set_hexpand(true);
        name_stack.set_transition_type(gtk::StackTransitionType::None);
        // Without this, the Stack reserves the tall rename Entry's natural
        // height even when the compact Label is the visible child, padding
        // every sidebar row to ~32px. Sizing to the visible child keeps the
        // row tight in display mode and lets the entry briefly grow when
        // rename starts.
        name_stack.set_vhomogeneous(false);

        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_hexpand(true);

        let entry = gtk::Entry::new();
        entry.set_hexpand(true);
        entry.add_css_class("playlist-sidebar-rename-entry");

        name_stack.add_named(&label, Some("label"));
        name_stack.add_named(&entry, Some("entry"));
        name_stack.set_visible_child_name("label");

        row.append(&icon);
        row.append(&name_stack);

        let expander = gtk::TreeExpander::new();
        expander.add_css_class("playlist-sidebar-row");
        expander.set_child(Some(&row));
        list_item.set_child(Some(&expander));
    });

    factory.connect_bind(move |_factory, list_item| {
        let Some(list_item) = list_item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(expander_widget) = list_item.child() else {
            return;
        };
        let Ok(expander) = expander_widget.downcast::<gtk::TreeExpander>() else {
            return;
        };
        let Some(item_object) = list_item.item() else {
            return;
        };
        let Ok(tree_row) = item_object.downcast::<gtk::TreeListRow>() else {
            return;
        };
        expander.set_list_row(Some(&tree_row));

        let Some(inner_object) = tree_row.item() else {
            return;
        };
        let Some(boxed) = inner_object.downcast_ref::<glib::BoxedAnyObject>() else {
            return;
        };
        let Ok(sidebar_item) = boxed.try_borrow::<SidebarItem>() else {
            return;
        };
        let playlist_item = sidebar_item.item;
        let label_text = sidebar_item.name.clone();
        let icon_name = sidebar_item.icon_name();
        drop(sidebar_item);

        let Some(row_widget) = expander.child() else {
            return;
        };
        let Ok(row) = row_widget.downcast::<gtk::Box>() else {
            return;
        };
        let Some(icon_widget) = row.first_child() else {
            return;
        };
        let Ok(icon) = icon_widget.downcast::<gtk::Image>() else {
            return;
        };
        let Some(stack_widget) = icon.next_sibling() else {
            return;
        };
        let Ok(name_stack) = stack_widget.downcast::<gtk::Stack>() else {
            return;
        };
        let Some(label) = name_stack
            .child_by_name("label")
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        else {
            return;
        };
        let Some(entry) = name_stack
            .child_by_name("entry")
            .and_then(|child| child.downcast::<gtk::Entry>().ok())
        else {
            return;
        };

        icon.set_icon_name(Some(icon_name));
        label.set_text(&label_text);
        entry.set_text(&label_text);
        name_stack.set_visible_child_name("label");

        attach_drag_and_drop(
            expander.upcast_ref::<gtk::Widget>(),
            playlist_item,
            on_move.clone(),
            on_tracks_drop.clone(),
            current_indicator.clone(),
        );
        attach_row_context_menu(
            expander.upcast_ref::<gtk::Widget>(),
            SidebarRowContext {
                item: playlist_item,
                current_name: label_text.clone(),
                name_stack: name_stack.clone(),
                label: label.clone(),
                entry: entry.clone(),
                on_delete: on_delete.clone(),
                on_edit_smart_playlist: on_edit_smart_playlist.clone(),
                on_analysis_run: on_analysis_run.clone(),
                on_online_run: on_online_run.clone(),
                analysis_enabled_query: analysis_enabled_query.clone(),
                online_busy_query: online_busy_query.clone(),
            },
        );
        attach_rename_entry_signals(
            &entry,
            &name_stack,
            &label,
            playlist_item,
            on_rename.clone(),
        );

        // If a caller armed `arm_pending_rename` for this item, immediately
        // enter rename mode now that the row is bound and its widgets exist.
        let should_rename = {
            let mut pending = pending_rename.borrow_mut();
            if pending.as_ref() == Some(&playlist_item) {
                *pending = None;
                true
            } else {
                false
            }
        };
        if should_rename {
            begin_rename(&name_stack, &label, &entry);
        }
    });

    factory
}

/// Assembles the horizontal split between the sidebar and the main
/// content column. The Paned is the single layout authority for the
/// sidebar's width: drag-resize stays clamped between
/// [`SIDEBAR_MIN_WIDTH`] and [`SIDEBAR_MAX_WIDTH`], and the
/// collapse-toggle animation operates by tweening the Paned's
/// position rather than by hiding the sidebar widget.
pub(crate) fn build_content_area(sidebar: &gtk::Box, main_content: &gtk::Box) -> gtk::Paned {
    let content_area = gtk::Paned::new(gtk::Orientation::Horizontal);
    content_area.set_hexpand(true);
    content_area.set_vexpand(true);
    content_area.set_wide_handle(false);
    content_area.set_resize_start_child(false);
    content_area.set_shrink_start_child(true);
    content_area.set_resize_end_child(true);
    content_area.set_shrink_end_child(false);
    content_area.set_start_child(Some(sidebar));
    content_area.set_end_child(Some(main_content));
    content_area.set_position(SIDEBAR_DEFAULT_WIDTH);
    content_area.connect_position_notify(clamp_sidebar_width);
    content_area
}

/// Clamps a user drag-resize back into the [`SIDEBAR_MIN_WIDTH`] /
/// [`SIDEBAR_MAX_WIDTH`] band. Position `0` is treated as the
/// collapsed state and is allowed through — the collapse-toggle
/// animation drives the Paned through zero on its way to collapsed,
/// and clamping that to MIN_WIDTH would visibly snap the sidebar back
/// open mid-animation.
fn clamp_sidebar_width(content_area: &gtk::Paned) {
    let current_width = content_area.position();
    if current_width == 0 {
        return;
    }
    let clamped_width = current_width.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
    if clamped_width != current_width {
        content_area.set_position(clamped_width);
    }
}
