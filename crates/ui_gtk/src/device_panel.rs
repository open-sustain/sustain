// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The device-sync panel shown in the main content column when a device
//! is selected in the DEVICES sidebar section (issues #23 / #24).
//!
//! Layout: a header whose left side identifies the device (name + mount
//! path) and whose right corner holds the configuration options
//! (on-drive format, per-layout settings); below it the ticked-playlist
//! list fills the remaining height in its own contrasting container; and
//! a fixed bottom bar carries the disk-occupation bar (how much of the
//! device the ticked playlists would occupy) alongside the `Forget
//! device` / `Sync` actions. All mutations go through the command
//! controller; all progress flows through the runtime's notification
//! lane, so the panel never schedules its own timers or pokes the status
//! bar.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};

use sustain_app_runtime::{
    ApplicationCommand, ConnectedDevice, DeviceAnalysisReadiness, DeviceLayout,
    DeviceSyncPlanState, FilesPerFolderCap, PlaylistItem, SyncPlan,
};

use crate::SharedRuntime;
use crate::command_controller::SharedCommandController;

/// Recomputes the status-bar track/duration/size summary for the device
/// view. Supplied by the main window, which knows the status bar.
type SummaryRefresh = Rc<dyn Fn()>;

#[derive(Clone)]
pub(crate) struct DeviceSyncPanel {
    root: gtk::Box,
    /// Header + playlist container; fills the height above the bottom bar.
    body: gtk::Box,
    /// Fixed footer: occupation bar + Forget / Sync. Lives outside the
    /// body and is re-filled in place when the selection changes.
    bottom_bar: gtk::Box,
    /// The device currently rendered, so the status-bar summary can be
    /// computed for it while the device view is visible.
    current_device: Rc<RefCell<Option<ConnectedDevice>>>,
    current_device_connected: Rc<Cell<bool>>,
    /// Recomputes the status-bar track/duration/size summary. Fired on
    /// show, selection toggle, and forget so the summary tracks the
    /// device's selected content instead of a stale earlier view.
    on_summary_refresh: Rc<RefCell<Option<SummaryRefresh>>>,
    /// The Pioneer "Analysis" readiness box, when the current view shows a
    /// Pioneer device. Held so a background analysis completion can refill
    /// the BPM/Key/Waveform rows in place without rebuilding the panel.
    /// `None` for other layouts (or no device shown).
    readiness_section: Rc<RefCell<Option<gtk::Box>>>,
    /// Set while an analysis-driven readiness refresh is already queued for
    /// the next idle, so a burst of per-track completions collapses into a
    /// single recompute instead of one O(selection) pass per track.
    readiness_refresh_queued: Rc<Cell<bool>>,
    runtime: SharedRuntime,
    command_controller: SharedCommandController,
}

impl DeviceSyncPanel {
    pub(crate) fn new(runtime: SharedRuntime, command_controller: SharedCommandController) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("device-sync-panel");
        root.set_hexpand(true);
        root.set_vexpand(true);

        // The playlist container inside the body fills the leftover height
        // and scrolls its own checklist, so the body itself does not
        // scroll. Children carry their own 18px margins to match the
        // bottom bar's padding.
        let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
        body.set_hexpand(true);
        body.set_vexpand(true);
        root.append(&body);

        let bottom_bar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        bottom_bar.add_css_class("device-sync-bottom-bar");
        root.append(&bottom_bar);

        Self {
            root,
            body,
            bottom_bar,
            current_device: Rc::new(RefCell::new(None)),
            current_device_connected: Rc::new(Cell::new(false)),
            on_summary_refresh: Rc::new(RefCell::new(None)),
            readiness_section: Rc::new(RefCell::new(None)),
            readiness_refresh_queued: Rc::new(Cell::new(false)),
            runtime,
            command_controller,
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// Shared handle to the currently-rendered device, so the status-bar
    /// summary callback can resolve its selected tracks while the device
    /// view is visible.
    pub(crate) fn current_device_cell(&self) -> Rc<RefCell<Option<ConnectedDevice>>> {
        self.current_device.clone()
    }

    /// Install the callback that recomputes the status-bar summary. The
    /// panel fires it whenever the device's selected content changes.
    pub(crate) fn set_summary_refresh(&self, refresh: SummaryRefresh) {
        self.on_summary_refresh.replace(Some(refresh));
    }

    fn fire_summary_refresh(&self) {
        if let Some(refresh) = self.on_summary_refresh.borrow().as_ref() {
            refresh();
        }
    }

    /// Render the panel for `device`, ensuring its saved-config row
    /// exists first so configuration commands have something to update.
    /// Call only once the content stack is already showing the device
    /// page, so the summary refresh resolves to the device's content.
    pub(crate) fn show_device(&self, device: ConnectedDevice) {
        let _ = self.runtime.borrow().ensure_device_config(&device);
        self.runtime.borrow_mut().request_device_sync_plan(&device);
        self.current_device.replace(Some(device.clone()));
        self.current_device_connected.set(true);
        self.rebuild(&device);
        self.fire_summary_refresh();
    }

    pub(crate) fn connected_devices_changed(&self, devices: &[ConnectedDevice]) {
        let Some(current) = self.current_device.borrow().clone() else {
            return;
        };
        let connected = devices.iter().find(|device| device.id == current.id);
        match connected {
            Some(device)
                if !self.current_device_connected.get()
                    || device.target != current.target
                    || device.volume_id != current.volume_id =>
            {
                self.show_device(device.clone());
            }
            Some(_) => {}
            None if self.current_device_connected.replace(false) => {
                self.runtime.borrow_mut().invalidate_device_sync_plan();
                self.refresh_bottom_bar(&current);
            }
            None => {}
        }
    }

    pub(crate) fn refresh_plan(&self) {
        if !self.root.is_mapped() || !self.current_device_connected.get() {
            return;
        }
        if let Some(device) = self.current_device.borrow().clone() {
            self.refresh_bottom_bar(&device);
        }
    }

    fn request_plan(&self, device: &ConnectedDevice) {
        self.runtime.borrow_mut().request_device_sync_plan(device);
    }

    fn clear(container: &gtk::Box) {
        while let Some(child) = container.first_child() {
            container.remove(&child);
        }
    }

    fn rebuild(&self, device: &ConnectedDevice) {
        Self::clear(&self.body);
        // The readiness box belongs to the view we are about to discard;
        // `append_pioneer_readiness` re-establishes it for Pioneer layouts.
        self.readiness_section.replace(None);

        let config = self
            .runtime
            .borrow()
            .device_config(&device.id)
            .unwrap_or_else(|| sustain_app_runtime::SyncDevice {
                id: device.id.clone(),
                label: device.label.clone(),
                kind: device.kind,
                layout: DeviceLayout::M3u,
                sub_path: sustain_app_runtime::DeviceRelativePath::root(),
                files_per_folder_cap: FilesPerFolderCap::Unlimited,
                volume_id: device.volume_id.clone(),
            });

        // --- Header: device identity, full width ---
        let header = gtk::Box::new(gtk::Orientation::Vertical, 1);
        header.set_margin_top(18);
        header.set_margin_start(18);
        header.set_margin_end(18);
        let title = gtk::Label::new(Some(&config.label));
        title.add_css_class("device-sync-title");
        title.set_xalign(0.0);
        header.append(&title);
        let mount = gtk::Label::new(Some(&device.target.location_label()));
        mount.add_css_class("dim-label");
        mount.set_xalign(0.0);
        header.append(&mount);
        self.body.append(&header);

        // --- Two columns: playlists (left) | settings (right) ---
        let columns = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        columns.set_margin_top(12);
        columns.set_margin_start(18);
        columns.set_margin_end(18);
        columns.set_margin_bottom(18);
        columns.set_vexpand(true);

        let playlists_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        playlists_column.set_hexpand(true);
        playlists_column.set_vexpand(true);
        playlists_column.append(&section_label("Playlists to sync"));
        playlists_column.append(&self.build_playlist_container(device));
        columns.append(&playlists_column);

        let settings_column = gtk::Box::new(gtk::Orientation::Vertical, 6);
        settings_column.set_valign(gtk::Align::Start);
        settings_column.set_size_request(SETTINGS_COLUMN_WIDTH, -1);
        settings_column.append(&section_label("On-drive format"));
        let format_group = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let mut anchor: Option<gtk::CheckButton> = None;
        for layout in DeviceLayout::ALL {
            let radio = gtk::CheckButton::with_label(layout.label());
            match &anchor {
                Some(first) => radio.set_group(Some(first)),
                None => anchor = Some(radio.clone()),
            }
            // Set state before wiring the handler so this construction-time
            // activation does not dispatch a redundant command.
            radio.set_active(layout == config.layout);
            {
                let panel = self.clone();
                let device = device.clone();
                radio.connect_toggled(move |radio| {
                    // A group emits two toggles per switch (off + on); act
                    // only on the one that became active.
                    if radio.is_active() {
                        if panel.command_controller.dispatch_succeeded(
                            ApplicationCommand::SetDeviceLayout {
                                device_id: device.id.clone(),
                                layout,
                            },
                        ) {
                            panel.request_plan(&device);
                        }
                        panel.rebuild(&device);
                    }
                });
            }
            format_group.append(&radio);
        }
        settings_column.append(&format_group);
        match config.layout {
            DeviceLayout::FolderPerPlaylist => {
                self.append_folder_cap(&settings_column, device, config.files_per_folder_cap)
            }
            DeviceLayout::Pioneer => self.append_pioneer_readiness(&settings_column, device),
            DeviceLayout::M3u => {}
        }
        columns.append(&settings_column);
        self.body.append(&columns);

        // --- Bottom bar (occupation + actions) ---
        self.refresh_bottom_bar(device);
    }

    fn append_folder_cap(
        &self,
        container: &gtk::Box,
        device: &ConnectedDevice,
        current: FilesPerFolderCap,
    ) {
        container.append(&section_label("Files per folder"));
        let caption = gtk::Label::new(Some("Before chunking into numbered subfolders."));
        caption.set_xalign(0.0);
        caption.set_wrap(true);
        caption.add_css_class("dim-label");
        container.append(&caption);

        let cap_group = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let mut anchor: Option<gtk::CheckButton> = None;
        for cap in FilesPerFolderCap::ALL {
            let radio = gtk::CheckButton::with_label(cap.label());
            match &anchor {
                Some(first) => radio.set_group(Some(first)),
                None => anchor = Some(radio.clone()),
            }
            // Set state before wiring the handler so this construction-time
            // activation does not dispatch a redundant command.
            radio.set_active(cap == current);
            {
                let panel = self.clone();
                let device = device.clone();
                radio.connect_toggled(move |radio| {
                    // A group emits two toggles per switch (off + on); act
                    // only on the one that became active.
                    if radio.is_active()
                        && panel.command_controller.dispatch_succeeded(
                            ApplicationCommand::SetDeviceFilesPerFolderCap {
                                device_id: device.id.clone(),
                                cap,
                            },
                        )
                    {
                        panel.request_plan(&device);
                        panel.refresh_selection_derived(&device);
                    }
                });
            }
            cap_group.append(&radio);
        }
        container.append(&cap_group);
    }

    fn append_pioneer_readiness(&self, container: &gtk::Box, device: &ConnectedDevice) {
        let section = gtk::Box::new(gtk::Orientation::Vertical, 6);
        self.populate_pioneer_readiness(&section, device);
        container.append(&section);
        // Retain the box so a background analysis completion can refill it
        // in place (see [`refresh_readiness`]).
        self.readiness_section.replace(Some(section));
    }

    /// Fill (or refill) the Pioneer "Analysis" section: a status row per
    /// metric — BPM / Key / Waveform. We surface only what is complete vs
    /// missing; a total count is noise here. The Analyse action lives in
    /// the bottom bar (see [`Self::refresh_bottom_bar`]).
    fn populate_pioneer_readiness(&self, section: &gtk::Box, device: &ConnectedDevice) {
        Self::clear(section);
        let readiness = self.runtime.borrow().device_analysis_readiness(&device.id);
        section.append(&section_label("Analysis"));
        section.append(&analysis_status_row("BPM", readiness.missing_bpm));
        section.append(&analysis_status_row("Key", readiness.missing_key));
        section.append(&analysis_status_row("Waveform", readiness.missing_waveform));
    }

    /// Refill every part of the view that derives from the current
    /// selection and analysis state: the Pioneer readiness rows (when a
    /// Pioneer device is shown) and the bottom bar (occupation meter +
    /// pre-sync warning, both of which read the live counts). One entry
    /// point so a selection toggle and a background analysis completion
    /// can never refresh one fragment and forget the other.
    fn refresh_selection_derived(&self, device: &ConnectedDevice) {
        if let Some(section) = self.readiness_section.borrow().clone() {
            self.populate_pioneer_readiness(&section, device);
        }
        self.refresh_bottom_bar(device);
    }

    /// Recompute readiness after a background analysis run touches a track,
    /// so the BPM/Key/Waveform rows and the pre-sync warning track reality
    /// as the run progresses. A cheap no-op unless a Pioneer device view is
    /// actually on screen. The recompute is coalesced onto the next idle so
    /// a sweep of many completions costs one O(selection) pass, not one per
    /// track.
    pub(crate) fn refresh_readiness(&self) {
        if !self.root.is_mapped() || self.readiness_section.borrow().is_none() {
            return;
        }
        if self.readiness_refresh_queued.replace(true) {
            return;
        }
        let panel = self.clone();
        glib::idle_add_local_once(move || {
            panel.readiness_refresh_queued.set(false);
            // The view may have changed between scheduling and now.
            if !panel.root.is_mapped() || panel.readiness_section.borrow().is_none() {
                return;
            }
            if let Some(device) = panel.current_device.borrow().clone() {
                panel.refresh_selection_derived(&device);
            }
        });
    }

    fn build_playlist_container(&self, device: &ConnectedDevice) -> gtk::Box {
        // The enclosing column owns the outer spacing; this is just the
        // contrasting card that fills the column height.
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.add_css_class("device-sync-playlist-container");
        container.set_hexpand(true);
        container.set_vexpand(true);

        let runtime = self.runtime.borrow();
        let selected: std::collections::HashSet<PlaylistItem> =
            runtime.device_selection(&device.id).into_iter().collect();

        let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let mut entries: Vec<(PlaylistItem, gtk::CheckButton)> = Vec::new();
        for playlist in runtime.playlists() {
            let item = PlaylistItem::Playlist(playlist.id);
            let check = playlist_check(&playlist.name, PLAYLIST_ICON, selected.contains(&item));
            list.append(&check);
            entries.push((item, check));
        }
        for smart in runtime.smart_playlists() {
            let item = PlaylistItem::SmartPlaylist(smart.id);
            let check = playlist_check(&smart.name, SMART_PLAYLIST_ICON, selected.contains(&item));
            list.append(&check);
            entries.push((item, check));
        }
        drop(runtime);

        if entries.is_empty() {
            let empty = gtk::Label::new(Some("No playlists yet. Create a playlist to sync it."));
            empty.set_xalign(0.0);
            empty.set_valign(gtk::Align::Start);
            empty.add_css_class("dim-label");
            container.append(&empty);
            return container;
        }

        let entries = Rc::new(entries);
        for (_, check) in entries.iter() {
            let panel = self.clone();
            let device = device.clone();
            let entries = entries.clone();
            check.connect_toggled(move |_| {
                let selection: Vec<PlaylistItem> = entries
                    .iter()
                    .filter(|(_, check)| check.is_active())
                    .map(|(item, _)| *item)
                    .collect();
                if panel.command_controller.dispatch_succeeded(
                    ApplicationCommand::SetDeviceSelection {
                        device_id: device.id.clone(),
                        selection,
                    },
                ) {
                    panel.request_plan(&device);
                }
                // The new selection changes occupation, the Sync action,
                // the Pioneer readiness counts and the status-bar summary —
                // refresh all of them in place, never touching this
                // checklist.
                panel.refresh_selection_derived(&device);
                panel.fire_summary_refresh();
            });
        }

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_vexpand(true);
        scroller.set_child(Some(&list));
        container.append(&scroller);
        container
    }

    /// (Re)fill the bottom bar from the current plan + device capacity.
    /// Safe to call from a playlist toggle: it only touches the bottom
    /// bar, never the checklist the toggle came from.
    fn refresh_bottom_bar(&self, device: &ConnectedDevice) {
        Self::clear(&self.bottom_bar);

        let runtime = self.runtime.borrow();
        let plan_state = runtime.device_sync_plan_state(device);
        // Pioneer hardware reads BPM/key/waveforms from the export, so warn
        // before syncing a selection that has gaps. Other layouts carry no
        // such data, so the readiness is irrelevant and left at zero.
        let is_pioneer = runtime
            .device_config(&device.id)
            .map(|config| config.layout == DeviceLayout::Pioneer)
            .unwrap_or(false);
        let readiness = if is_pioneer {
            runtime.device_analysis_readiness(&device.id)
        } else {
            DeviceAnalysisReadiness::default()
        };
        drop(runtime);

        let connected = self.current_device_connected.get();
        let (plan, capacity, pending) = match plan_state {
            DeviceSyncPlanState::Ready(snapshot) if connected => {
                (snapshot.plan, Some(snapshot.capacity), false)
            }
            DeviceSyncPlanState::Pending if connected => (None, None, true),
            DeviceSyncPlanState::Unavailable
            | DeviceSyncPlanState::Pending
            | DeviceSyncPlanState::Ready(_) => (None, None, false),
        };
        let selected_bytes = plan.as_ref().map(|p| p.bytes_total).unwrap_or(0);
        let total_bytes = capacity.map(|c| c.total_bytes).unwrap_or(0);
        let available_bytes = capacity.map(|c| c.available_bytes).unwrap_or(0);
        let needed_bytes = plan.as_ref().map(|p| p.bytes_to_copy).unwrap_or(0);
        let over_capacity = total_bytes > 0 && needed_bytes > available_bytes;

        let (fraction, text) = if !connected {
            (0.0, "Device disconnected".to_owned())
        } else if pending {
            (0.0, "Calculating device sync plan...".to_owned())
        } else if total_bytes == 0 {
            (0.0, "Device capacity unavailable".to_owned())
        } else if over_capacity {
            (
                1.0,
                format!(
                    "Not enough space — {} needed, {} free",
                    human_bytes(needed_bytes),
                    human_bytes(available_bytes)
                ),
            )
        } else {
            (
                selected_bytes as f64 / total_bytes as f64,
                format!(
                    "{} of {}",
                    human_bytes(selected_bytes),
                    human_bytes(total_bytes)
                ),
            )
        };

        // Under capacity the meter stacks the selection by genre; over
        // capacity (or with no capacity reading) it falls back to a single
        // accent/error fill spanning `fraction`.
        let segments = if over_capacity || total_bytes == 0 {
            vec![BarSegment {
                fraction,
                tint: SegmentTint::Plain,
                label: None,
            }]
        } else {
            plan.as_ref()
                .map(|plan| genre_segments(plan, total_bytes))
                .unwrap_or_default()
        };
        let bar = occupation_bar(segments, &text, over_capacity);
        self.bottom_bar.append(&bar);

        // --- Actions (extreme right) ---
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);

        let forget = gtk::Button::with_label("Forget device");
        {
            let panel = self.clone();
            let device_id = device.id.clone();
            forget.connect_clicked(move |_| {
                panel
                    .command_controller
                    .dispatch_succeeded(ApplicationCommand::ForgetDevice {
                        device_id: device_id.clone(),
                    });
                Self::clear(&panel.body);
                Self::clear(&panel.bottom_bar);
                let note = gtk::Label::new(Some(
                    "Device forgotten. Its saved playlists and sync history were cleared.",
                ));
                note.set_xalign(0.0);
                note.set_valign(gtk::Align::Start);
                note.set_margin_top(18);
                note.set_margin_start(18);
                note.set_margin_end(18);
                panel.body.append(&note);
                // The selection is gone; the summary now reads empty.
                panel.current_device.replace(None);
                panel.current_device_connected.set(false);
                panel.fire_summary_refresh();
            });
        }
        actions.append(&forget);

        // Analyse the selection's missing BPM/key/waveforms. Only shown
        // when there is something analysable (always zero off Pioneer).
        if readiness.analyzable > 0 {
            let analyse = gtk::Button::with_label(&format!(
                "Analyse {} missing {}",
                readiness.analyzable,
                if readiness.analyzable == 1 {
                    "track"
                } else {
                    "tracks"
                },
            ));
            {
                let panel = self.clone();
                let device_id = device.id.clone();
                analyse.connect_clicked(move |_| {
                    panel.command_controller.dispatch_succeeded(
                        ApplicationCommand::AnalyzeDeviceTracks {
                            device_id: device_id.clone(),
                        },
                    );
                });
            }
            actions.append(&analyse);
        }

        let sync = gtk::Button::with_label("Sync");
        sync.add_css_class("suggested-action");
        sync.set_sensitive(connected && !pending && plan.is_some() && !over_capacity);
        {
            let command_controller = self.command_controller.clone();
            let root = self.root.clone();
            let device_id = device.id.clone();
            let remove_count = plan.as_ref().map(|p| p.to_remove.len()).unwrap_or(0);
            let warnings = PreSyncWarnings {
                missing_bpm: readiness.missing_bpm,
                missing_key: readiness.missing_key,
                missing_waveform: readiness.missing_waveform,
                remove_count,
            };
            sync.connect_clicked(move |_| {
                let dispatch = {
                    let command_controller = command_controller.clone();
                    let device_id = device_id.clone();
                    move |remove_stale: bool| {
                        command_controller.dispatch_succeeded(ApplicationCommand::SyncDevice {
                            device_id: device_id.clone(),
                            remove_stale,
                        });
                    }
                };
                // Confirm before a sync that would delete stale tracks
                // (destructive) or leave analysis gaps on Pioneer hardware.
                if warnings.needs_dialog() {
                    match root.root().and_downcast::<gtk::Window>() {
                        Some(parent) => {
                            confirm_sync(&parent, warnings, move || dispatch(true));
                        }
                        // No window to host the dialog (should not happen
                        // while visible): proceed, but keep stale files
                        // rather than deleting without consent.
                        None => dispatch(warnings.remove_count == 0),
                    }
                } else {
                    dispatch(true);
                }
            });
        }
        actions.append(&sync);
        self.bottom_bar.append(&actions);
    }
}

fn section_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("device-sync-section");
    label.set_xalign(0.0);
    label.set_margin_top(8);
    label
}

/// One per-metric analysis-coverage row for the Pioneer export panel:
/// the metric name, a round status badge — a green tick when complete or
/// an amber mark when not — and the matching caption. Pioneer hardware
/// reads BPM, key and waveforms from the export, so a gap here is a gap
/// on the player.
fn analysis_status_row(name: &str, missing: usize) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let name_label = gtk::Label::new(Some(name));
    name_label.set_xalign(0.0);
    name_label.set_width_chars(9);
    name_label.add_css_class("dim-label");
    row.append(&name_label);

    let (icon, badge_class, text, text_class) = if missing == 0 {
        (
            "object-select-symbolic",
            "ok",
            "Complete".to_owned(),
            "device-analysis-ok",
        )
    } else {
        (
            "emblem-important-symbolic",
            "warn",
            format!("{missing} missing"),
            "device-analysis-warn",
        )
    };

    let badge = gtk::Image::from_icon_name(icon);
    badge.set_pixel_size(12);
    badge.add_css_class("device-analysis-badge");
    badge.add_css_class(badge_class);
    row.append(&badge);

    let status = gtk::Label::new(Some(&text));
    status.set_xalign(0.0);
    status.add_css_class(text_class);
    row.append(&status);

    row
}

/// Width of the right-hand settings column.
const SETTINGS_COLUMN_WIDTH: i32 = 260;

/// Sidebar-consistent icons for the two playlist kinds in the checklist.
const PLAYLIST_ICON: &str = "view-list-symbolic";
const SMART_PLAYLIST_ICON: &str = "emblem-system-symbolic";

/// A checklist row: a check button whose child is the playlist's icon
/// (normal vs smart, matching the sidebar) followed by its name.
fn playlist_check(name: &str, icon: &str, active: bool) -> gtk::CheckButton {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::Image::from_icon_name(icon));
    let label = gtk::Label::new(Some(name));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&label);

    let check = gtk::CheckButton::new();
    check.set_child(Some(&row));
    check.set_active(active);
    check
}

/// One coloured run of the occupation bar, as a fraction of the device
/// capacity. The tint is resolved at draw time so the meter tracks the
/// live theme accent and light/dark appearance. `label`, when present,
/// is the hover tooltip naming the slice (e.g. its genre and size).
#[derive(Clone)]
struct BarSegment {
    fraction: f64,
    tint: SegmentTint,
    label: Option<String>,
}

/// How a segment is coloured, resolved against the drawing area's live
/// colour (the theme accent, or the error colour when over capacity) so
/// nothing is baked in and the meter follows the system accent in both
/// light and dark themes.
#[derive(Clone, Copy)]
enum SegmentTint {
    /// The accent (or error colour) verbatim — the single fill shown when
    /// over capacity or when the device capacity is unknown.
    Plain,
    /// The accent rotated this many degrees around the hue wheel, for the
    /// genre palette. `0.0` is the largest genre; each further genre steps
    /// one palette slot.
    AccentHue(f64),
    /// A neutral grey for the aggregated "other genres" tail.
    Muted,
}

/// The genre palette size: the largest 8 genres get distinct accent-
/// derived hues; everything past that aggregates into one grey segment.
const GENRE_PALETTE_CAP: usize = 8;

/// Resolve a [`SegmentTint`] to an opaque `(r, g, b)` against the live
/// `accent` colour. Hues are spread from the accent so the palette
/// honours the user's system accent; the muted tail is a theme-neutral
/// grey ([`crate::chart::MUTED_GREY`]).
fn resolve_tint(tint: SegmentTint, accent: gdk::RGBA) -> (f64, f64, f64) {
    match tint {
        SegmentTint::Plain => (
            accent.red() as f64,
            accent.green() as f64,
            accent.blue() as f64,
        ),
        // The shared categorical palette (also used by the Statistics
        // donuts): the accent rotated around the hue wheel, kept vivid and
        // legible at this meter's translucency.
        SegmentTint::AccentHue(degrees) => crate::chart::categorical_color(accent, degrees),
        SegmentTint::Muted => crate::chart::MUTED_GREY,
    }
}

/// Build the occupation bar's per-genre segments from a sync plan. The
/// largest [`GENRE_PALETTE_CAP`] genres each get an accent-derived hue
/// (largest first); any remainder collapses into one trailing grey
/// "other" segment. Each fraction is the genre's share of the whole
/// device, so the stack spans exactly the `selected / total` width it
/// replaces (the breakdown sums to the plan's `bytes_total`).
fn genre_segments(plan: &SyncPlan, total_bytes: u64) -> Vec<BarSegment> {
    if total_bytes == 0 {
        return Vec::new();
    }
    let hue_step = 360.0 / GENRE_PALETTE_CAP as f64;
    let mut segments = Vec::new();
    let mut other_bytes = 0u64;
    let mut other_count = 0usize;
    for (rank, genre) in plan.genre_bytes.iter().enumerate() {
        if rank < GENRE_PALETTE_CAP {
            let name = genre.genre.as_deref().unwrap_or("Unknown");
            segments.push(BarSegment {
                fraction: genre.bytes as f64 / total_bytes as f64,
                tint: SegmentTint::AccentHue(rank as f64 * hue_step),
                label: Some(format!("{name} — {}", human_bytes(genre.bytes))),
            });
        } else {
            other_bytes += genre.bytes;
            other_count += 1;
        }
    }
    if other_bytes > 0 {
        segments.push(BarSegment {
            fraction: other_bytes as f64 / total_bytes as f64,
            tint: SegmentTint::Muted,
            label: Some(format!(
                "{other_count} other {} — {}",
                if other_count == 1 { "genre" } else { "genres" },
                human_bytes(other_bytes)
            )),
        });
    }
    segments
}

/// The disk-occupation meter: a button-height, button-radius pill that
/// paints `segments` left-to-right over a faint trough and centres `text`
/// over them. Trough/fill colours and the rounded clip come from CSS so
/// the meter tracks the theme and matches the adjacent buttons; over
/// capacity it switches to the theme's error colour. Returned as a plain
/// widget so the bottom bar's horizontal box stretches it to the button
/// height.
fn occupation_bar(segments: Vec<BarSegment>, text: &str, over_capacity: bool) -> gtk::Widget {
    let frame = gtk::Overlay::new();
    frame.add_css_class("device-occupation-bar");
    frame.set_hexpand(true);
    // Clip the fill to the CSS border-radius.
    frame.set_overflow(gtk::Overflow::Hidden);

    let fill = gtk::DrawingArea::new();
    fill.add_css_class("device-occupation-fill");
    if over_capacity {
        frame.add_css_class("over-capacity");
        fill.add_css_class("over-capacity");
    }
    // hexpand only: the bar fills the row's height through the default
    // valign=Fill, NOT vexpand — vexpand would propagate up through the
    // Overlay to the bottom bar and make the whole footer claim vertical
    // space, ballooning it (and the buttons) far past the button height.
    fill.set_hexpand(true);
    let draw_segments = segments.clone();
    fill.set_draw_func(move |area, cr, width, height| {
        let w = width as f64;
        let h = height as f64;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let accent = area.color();
        let mut x = 0.0;
        for segment in &draw_segments {
            let segment_width = (segment.fraction.clamp(0.0, 1.0) * w).min(w - x);
            if segment_width <= 0.0 {
                continue;
            }
            let (r, g, b) = resolve_tint(segment.tint, accent);
            cr.set_source_rgba(r, g, b, 0.45);
            cr.rectangle(x, 0.0, segment_width, h);
            let _ = cr.fill();
            x += segment_width;
        }
    });
    frame.set_child(Some(&fill));

    let label = gtk::Label::new(Some(text));
    label.add_css_class("device-occupation-label");
    label.set_hexpand(true);
    label.set_halign(gtk::Align::Fill);
    label.set_xalign(0.5);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_margin_start(10);
    label.set_margin_end(10);
    frame.add_overlay(&label);

    // Name the genre slice under the pointer with a popover whose arrow
    // points at that slice — unlike a plain tooltip (which floats by the
    // cursor with no anchor), it tracks the segment it describes. The fill
    // and label are made input-transparent so the frame is the unambiguous
    // hover target. Only built when some segment carries a label; segments
    // with none (the single over-capacity fill) pop it back down.
    if segments.iter().any(|s| s.label.is_some()) {
        fill.set_can_target(false);
        label.set_can_target(false);

        let tip_label = gtk::Label::new(None);
        tip_label.set_margin_top(4);
        tip_label.set_margin_bottom(4);
        tip_label.set_margin_start(8);
        tip_label.set_margin_end(8);
        let popover = gtk::Popover::new();
        popover.set_autohide(false);
        popover.set_position(gtk::PositionType::Top);
        popover.set_child(Some(&tip_label));
        popover.set_parent(&frame);
        // A popover attached with `set_parent` must be unparented before
        // its parent is finalized, or GTK warns and leaks the surface. The
        // bottom bar rebuilds this frame on every selection change. Guard
        // against a double unparent in case dispose already detached it.
        {
            let popover = popover.clone();
            frame.connect_destroy(move |_| {
                if popover.parent().is_some() {
                    popover.unparent();
                }
            });
        }

        let motion = gtk::EventControllerMotion::new();
        // Index of the slice the popover currently points at, so moving
        // within one slice doesn't re-pop it; `usize::MAX` means hidden.
        let shown = Rc::new(Cell::new(usize::MAX));
        {
            // Weak frame ref: capturing it strongly here would cycle
            // (frame → controller → closure → frame) and leak the bar.
            let frame = frame.downgrade();
            let segments = segments.clone();
            let popover = popover.clone();
            let shown = shown.clone();
            motion.connect_motion(move |_, x, _y| {
                let Some(frame) = frame.upgrade() else {
                    return;
                };
                let w = frame.width() as f64;
                let height = frame.height();
                if w <= 0.0 {
                    return;
                }
                let mut start = 0.0;
                for (index, segment) in segments.iter().enumerate() {
                    let segment_width = (segment.fraction.clamp(0.0, 1.0) * w).min(w - start);
                    if segment_width <= 0.0 {
                        continue;
                    }
                    if x >= start && x < start + segment_width {
                        match &segment.label {
                            Some(text) if shown.replace(index) != index => {
                                tip_label.set_text(text);
                                popover.set_pointing_to(Some(&gdk::Rectangle::new(
                                    start as i32,
                                    0,
                                    segment_width as i32,
                                    height,
                                )));
                                popover.popup();
                            }
                            Some(_) => {}
                            None => {
                                shown.set(usize::MAX);
                                popover.popdown();
                            }
                        }
                        return;
                    }
                    start += segment_width;
                }
                shown.set(usize::MAX);
                popover.popdown();
            });
        }
        {
            let popover = popover.clone();
            motion.connect_leave(move |_| {
                shown.set(usize::MAX);
                popover.popdown();
            });
        }
        frame.add_controller(motion);
    }

    frame.upcast()
}

/// What the user should be told before a sync proceeds. A removal count
/// (stale tracks that would be deleted — destructive) and the Pioneer
/// analysis gaps (BPM/key/waveform the hardware will show as missing).
/// Zero across the board means no dialog is needed.
#[derive(Clone, Copy)]
struct PreSyncWarnings {
    missing_bpm: usize,
    missing_key: usize,
    missing_waveform: usize,
    remove_count: usize,
}

impl PreSyncWarnings {
    /// Whether any selected track lacks BPM, key or waveform analysis.
    fn analysis_incomplete(&self) -> bool {
        self.missing_bpm + self.missing_key + self.missing_waveform > 0
    }

    /// Whether the sync warrants a confirmation modal at all.
    fn needs_dialog(&self) -> bool {
        self.remove_count > 0 || self.analysis_incomplete()
    }

    /// The caution sentence about analysis gaps, or `None` when complete.
    fn analysis_sentence(&self) -> Option<String> {
        if !self.analysis_incomplete() {
            return None;
        }
        let mut parts = Vec::new();
        if self.missing_bpm > 0 {
            parts.push(format!("{} missing BPM", self.missing_bpm));
        }
        if self.missing_key > 0 {
            parts.push(format!("{} missing key", self.missing_key));
        }
        if self.missing_waveform > 0 {
            parts.push(format!("{} missing waveform", self.missing_waveform));
        }
        Some(format!(
            "Some selected tracks are not fully analysed ({}). They will still sync, \
             but that information will be missing on Pioneer players. Run Analyse first \
             to fill the gaps.",
            parts.join(", "),
        ))
    }

    /// The destructive sentence about stale removals, or `None`.
    fn removal_sentence(&self) -> Option<String> {
        if self.remove_count == 0 {
            return None;
        }
        Some(format!(
            "Syncing will remove {} {} from this device that {} no longer in your selected playlists.",
            self.remove_count,
            if self.remove_count == 1 {
                "track"
            } else {
                "tracks"
            },
            if self.remove_count == 1 { "is" } else { "are" },
        ))
    }
}

/// Confirm a sync that has something the user should weigh first: stale
/// removals (destructive) and/or Pioneer analysis gaps (a caution).
/// Mirrors the trash-confirmation dialog: a small modal with Cancel as
/// the default. `on_confirm` fires only on the proceed button.
fn confirm_sync(parent: &gtk::Window, warnings: PreSyncWarnings, on_confirm: impl Fn() + 'static) {
    let window = gtk::Window::builder()
        .title("Sync device")
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

    // Analysis caution first (informational), then the removal warning
    // (destructive) closest to the action buttons.
    for sentence in [warnings.analysis_sentence(), warnings.removal_sentence()]
        .into_iter()
        .flatten()
    {
        let detail = gtk::Label::new(Some(&sentence));
        detail.add_css_class("dim-label");
        detail.set_xalign(0.0);
        detail.set_wrap(true);
        content.append(&detail);
    }

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);

    let cancel = gtk::Button::with_label("Cancel");
    // Removals delete files, so that path keeps the destructive styling and
    // wording; an analysis-only caution is a plain "proceed anyway".
    let confirm = if warnings.remove_count > 0 {
        let button = gtk::Button::with_label("Sync and remove");
        button.add_css_class("destructive-action");
        button
    } else {
        let button = gtk::Button::with_label("Sync anyway");
        button.add_css_class("suggested-action");
        button
    };

    {
        let window = window.clone();
        cancel.connect_clicked(move |_| window.close());
    }
    {
        let window = window.clone();
        confirm.connect_clicked(move |_| {
            on_confirm();
            window.close();
        });
    }

    buttons.append(&cancel);
    buttons.append(&confirm);
    content.append(&buttons);
    window.set_child(Some(&content));
    window.set_default_widget(Some(&cancel));

    let key_controller = gtk::EventControllerKey::new();
    {
        let window = window.clone();
        key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
            if key == gdk::Key::Escape {
                window.close();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    window.add_controller(key_controller);

    window.present();
    cancel.grab_focus();
}

/// Format a byte count in SI units (powers of 1000), matching how the
/// desktop reports disk sizes so "14.9 GB" lines up with the file
/// manager.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
