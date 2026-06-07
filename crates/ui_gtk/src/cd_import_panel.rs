// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The CD-import page shown in the main content column when an inserted
//! audio CD is selected in the DEVICES sidebar section (issue #25).
//!
//! Layout: a fixed-height top bar — the same height as the title bar, like
//! the Playlists header — carrying the release artwork on the left, the
//! release identity (title over artist/details) and an optional release
//! selector in the middle, and the `Import CD` button on the right. Below it
//! a song-table-style checklist of the disc's audio tracks (all ticked by
//! default), striped like the library table and continuing its zebra into
//! the empty region below the last track.
//!
//! The page renders a valid fallback identity immediately so an offline
//! import works; a MusicBrainz disc-id lookup refines it when it arrives,
//! and the artwork shows a spinner while that lookup (and the chosen
//! release's cover fetch) is in flight. All progress and outcomes flow
//! through the runtime's notification lane; the page never schedules timers
//! or pokes the status bar. When its disc is ejected the page is simply
//! left (the main window restores the prior selection), so it owns no
//! "disc removed" surface.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use std::collections::BTreeMap;

use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf, glib};

use sustain_app_runtime::{CdImportRequest, CdTrackOverride, DiscRelease, TocSnapshot};

use crate::{SharedRuntime, TITLEBAR_HEIGHT, track_table::EmptyRowPainter};

const FALLBACK_ALBUM: &str = "Audio CD";
const FALLBACK_ARTIST: &str = "Unknown Artist";
/// Edge length of the header artwork thumbnail. Sized to sit comfortably
/// inside the fixed [`TITLEBAR_HEIGHT`] top bar with a little vertical
/// breathing room.
const ARTWORK_SIZE: i32 = 56;

/// Fired when the user clicks `Import CD`. The main window wires this to
/// the runtime's prepare / background-worker / apply path.
pub(crate) type CdImportRequestedCallback = Rc<dyn Fn(CdImportRequest)>;

/// Fired when the user clicks the header `Eject` button. Carries the shown
/// disc so the main window can resolve and eject the drive.
pub(crate) type CdEjectRequestedCallback = Rc<dyn Fn(TocSnapshot)>;

/// A double-click-to-edit table cell: a label that swaps to an entry for
/// inline editing (used for the per-track title and artist, so a disc
/// MusicBrainz could not identify can be named by hand).
#[derive(Clone)]
struct EditableCell {
    stack: gtk::Stack,
    label: gtk::Label,
    entry: gtk::Entry,
}

/// Which track field an [`EditableCell`] edits.
#[derive(Clone, Copy)]
enum OverrideField {
    Title,
    Artist,
}

struct TrackRow {
    number: u32,
    check: gtk::CheckButton,
    title: EditableCell,
    artist: EditableCell,
}

struct PanelState {
    snapshot: Option<TocSnapshot>,
    releases: Vec<DiscRelease>,
    selected_release: Option<usize>,
    cover: Option<Vec<u8>>,
    rows: Vec<TrackRow>,
    /// Per-track user edits keyed by physical track number; these win over
    /// the looked-up / generated title and artist.
    overrides: BTreeMap<u32, CdTrackOverride>,
}

impl PanelState {
    fn selected_release(&self) -> Option<&DiscRelease> {
        self.selected_release
            .and_then(|index| self.releases.get(index))
    }
}

#[derive(Clone)]
pub(crate) struct CdImportPanel {
    root: gtk::Box,
    /// The artwork is an overlay holding three mutually-exclusive layers —
    /// the cover picture, the missing-artwork placeholder, and the
    /// "searching" spinner — clipped to a fixed square so a large cover can
    /// never grow the header.
    artwork_picture: gtk::Picture,
    artwork_icon: gtk::Image,
    artwork_spinner: gtk::Spinner,
    title_label: gtk::Label,
    subtitle_label: gtk::Label,
    release_selector: gtk::DropDown,
    checklist_box: gtk::Box,
    painter: EmptyRowPainter,
    import_button: gtk::Button,
    state: Rc<RefCell<PanelState>>,
    /// Suppresses the release-selector notify while a programmatic model
    /// swap / pre-selection is in flight.
    suppress_toggles: Rc<Cell<bool>>,
    runtime: SharedRuntime,
    on_import_requested: Rc<RefCell<Option<CdImportRequestedCallback>>>,
    on_eject_requested: Rc<RefCell<Option<CdEjectRequestedCallback>>>,
}

impl CdImportPanel {
    pub(crate) fn new(runtime: SharedRuntime) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.add_css_class("cd-import-panel");
        root.set_hexpand(true);
        root.set_vexpand(true);

        // --- Header: fixed height, like the Playlists top bar ---
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        header.add_css_class("cd-import-header");
        header.set_height_request(TITLEBAR_HEIGHT);
        header.set_hexpand(true);

        let (artwork, artwork_picture, artwork_icon, artwork_spinner) = build_artwork();
        header.append(&artwork);

        let identity = gtk::Box::new(gtk::Orientation::Vertical, 1);
        identity.set_hexpand(true);
        identity.set_valign(gtk::Align::Center);

        let title_label = gtk::Label::new(Some(FALLBACK_ALBUM));
        title_label.add_css_class("cd-import-title");
        title_label.set_xalign(0.0);
        title_label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let subtitle_label = gtk::Label::new(Some(FALLBACK_ARTIST));
        subtitle_label.add_css_class("cd-import-subtitle");
        subtitle_label.add_css_class("dim-label");
        subtitle_label.set_xalign(0.0);
        subtitle_label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        identity.append(&title_label);
        identity.append(&subtitle_label);

        let release_selector = gtk::DropDown::from_strings(&[]);
        release_selector.add_css_class("cd-import-release-selector");
        release_selector.set_valign(gtk::Align::Center);
        release_selector.set_visible(false);

        // Trailing action cluster: Encoding settings, Eject, Import.
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.add_css_class("cd-import-actions");
        actions.set_valign(gtk::Align::Center);

        let encoding_button = gtk::Button::with_label("Encoding Settings");
        encoding_button.add_css_class("cd-import-secondary-button");
        encoding_button.set_tooltip_text(Some("Choose the CD import format"));
        encoding_button.connect_clicked(|button| {
            // Open Preferences at the Encoding tab via the app action; any
            // widget in the window can reach the "app." action group.
            let _ = button.activate_action("app.preferences", Some(&"encoding".to_variant()));
        });

        let eject_button = gtk::Button::from_icon_name("media-eject-symbolic");
        eject_button.add_css_class("cd-import-secondary-button");
        eject_button.set_tooltip_text(Some("Eject"));

        let import_button = gtk::Button::with_label("Import CD");
        import_button.add_css_class("suggested-action");
        import_button.add_css_class("cd-import-button");

        actions.append(&encoding_button);
        actions.append(&eject_button);
        actions.append(&import_button);

        header.append(&identity);
        header.append(&release_selector);
        header.append(&actions);
        root.append(&header);

        // --- Track checklist: striped rows + filler stripes below ---
        let checklist_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        checklist_box.add_css_class("cd-import-checklist");
        let scroller = gtk::ScrolledWindow::new();
        scroller.set_hexpand(true);
        scroller.set_vexpand(true);
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_child(Some(&checklist_box));
        let painter = EmptyRowPainter::new_headerless(&scroller);
        root.append(&painter);

        let panel = Self {
            root,
            artwork_picture,
            artwork_icon,
            artwork_spinner,
            title_label,
            subtitle_label,
            release_selector,
            checklist_box,
            painter,
            import_button,
            state: Rc::new(RefCell::new(PanelState {
                snapshot: None,
                releases: Vec::new(),
                selected_release: None,
                cover: None,
                rows: Vec::new(),
                overrides: BTreeMap::new(),
            })),
            suppress_toggles: Rc::new(Cell::new(false)),
            runtime,
            on_import_requested: Rc::new(RefCell::new(None)),
            on_eject_requested: Rc::new(RefCell::new(None)),
        };
        panel.wire_signals();
        {
            let panel = panel.clone();
            eject_button.connect_clicked(move |_| {
                let Some(snapshot) = panel.current_snapshot() else {
                    return;
                };
                if let Some(callback) = panel.on_eject_requested.borrow().as_ref() {
                    callback(snapshot);
                }
            });
        }
        panel
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub(crate) fn set_import_requested_callback(&self, callback: CdImportRequestedCallback) {
        self.on_import_requested.replace(Some(callback));
    }

    pub(crate) fn set_eject_requested_callback(&self, callback: CdEjectRequestedCallback) {
        self.on_eject_requested.replace(Some(callback));
    }

    fn wire_signals(&self) {
        let panel = self.clone();
        self.release_selector
            .connect_selected_notify(move |selector| {
                if panel.suppress_toggles.get() {
                    return;
                }
                let selected = {
                    let mut state = panel.state.borrow_mut();
                    let index = selector.selected() as usize;
                    if index < state.releases.len() {
                        state.selected_release = Some(index);
                    }
                    state.selected_release().cloned()
                };
                panel.refresh_metadata();
                if let Some(release) = selected {
                    // Re-fetch the cover for the newly chosen release; show
                    // the spinner until that Cover event resolves it.
                    panel.set_artwork_searching();
                    panel.runtime.borrow().fetch_disc_cover(release);
                }
                panel.update_import_sensitivity();
            });

        let panel = self.clone();
        self.import_button.connect_clicked(move |_| {
            let Some(request) = panel.current_request() else {
                return;
            };
            if let Some(callback) = panel.on_import_requested.borrow().as_ref() {
                callback(request);
            }
        });
    }

    /// Render the page for a freshly-probed disc: build the checklist, show
    /// the fallback identity, and reset any prior lookup state. The artwork
    /// starts on the placeholder; [`Self::mark_lookup_started`] switches it
    /// to the spinner when a MusicBrainz lookup is actually in flight.
    pub(crate) fn show_disc(&self, snapshot: TocSnapshot) {
        {
            let mut state = self.state.borrow_mut();
            state.snapshot = Some(snapshot.clone());
            state.releases.clear();
            state.selected_release = None;
            state.cover = None;
            state.overrides.clear();
        }
        self.suppress_toggles.set(true);
        self.release_selector.set_visible(false);
        self.suppress_toggles.set(false);
        self.set_artwork_placeholder();
        self.rebuild_checklist(&snapshot);
        self.refresh_metadata();
        self.update_import_sensitivity();
    }

    /// Note that a MusicBrainz lookup has begun for the shown disc, so the
    /// artwork shows a spinner until the release candidates (and the chosen
    /// release's cover) arrive. Called only when a lookup actually started —
    /// without a remote service nothing would ever resolve the spinner.
    pub(crate) fn mark_lookup_started(&self) {
        self.set_artwork_searching();
    }

    /// Apply MusicBrainz release candidates. The most reasonable pressing is
    /// pre-selected; with more than one the selector is shown so the user can
    /// switch. `failed` distinguishes a lookup error from a genuine no-match
    /// (the main window reports the error once).
    pub(crate) fn apply_releases(&self, releases: Vec<DiscRelease>, _failed: bool) {
        let selected = most_reasonable_release_index(&releases);
        let chosen = {
            let mut state = self.state.borrow_mut();
            if state.snapshot.is_none() {
                return;
            }
            state.releases = releases;
            state.selected_release = selected;
            state.selected_release().cloned()
        };
        self.populate_release_selector();
        self.refresh_metadata();
        match chosen {
            Some(release) => {
                // A match was found; keep the spinner up while its cover is
                // fetched (a Cover event is guaranteed to follow), then it
                // resolves to the cover or the placeholder.
                self.set_artwork_searching();
                self.runtime.borrow().fetch_disc_cover(release);
            }
            // No match (or offline): nothing more will arrive, settle now.
            None => self.set_artwork_placeholder(),
        }
        self.update_import_sensitivity();
    }

    pub(crate) fn apply_cover(&self, cover: Option<Vec<u8>>) {
        if self.state.borrow().snapshot.is_none() {
            return;
        }
        self.state.borrow_mut().cover = cover.clone();
        match cover.as_deref().and_then(texture_from_bytes) {
            Some(texture) => self.set_artwork_texture(&texture),
            None => self.set_artwork_placeholder(),
        }
    }

    /// Clear the page when its disc is gone (ejected or replaced). The main
    /// window leaves the CD view when this happens, so the page only needs to
    /// drop its state — there is no lingering "disc removed" surface.
    pub(crate) fn forget_disc(&self) {
        {
            let mut state = self.state.borrow_mut();
            state.snapshot = None;
            state.releases.clear();
            state.selected_release = None;
            state.cover = None;
            state.rows.clear();
            state.overrides.clear();
        }
        while let Some(child) = self.checklist_box.first_child() {
            self.checklist_box.remove(&child);
        }
        self.painter.set_row_count(0);
        self.suppress_toggles.set(true);
        self.release_selector.set_visible(false);
        self.suppress_toggles.set(false);
        self.title_label.set_text(FALLBACK_ALBUM);
        self.subtitle_label.set_text(FALLBACK_ARTIST);
        self.set_artwork_placeholder();
        self.update_import_sensitivity();
    }

    /// The snapshot the page is currently showing, if any.
    pub(crate) fn current_snapshot(&self) -> Option<TocSnapshot> {
        self.state.borrow().snapshot.clone()
    }

    /// Recompute the Import button's sensitivity — called when a rip starts
    /// or finishes, since the button gates on the live background-task state.
    pub(crate) fn refresh_import_sensitivity(&self) {
        self.update_import_sensitivity();
    }

    fn populate_release_selector(&self) {
        let state = self.state.borrow();
        let labels: Vec<String> = state.releases.iter().map(release_selector_label).collect();
        let selected = state.selected_release.unwrap_or(0) as u32;
        let multiple = state.releases.len() > 1;
        drop(state);

        let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let model = gtk::StringList::new(&label_refs);
        self.suppress_toggles.set(true);
        self.release_selector.set_model(Some(&model));
        self.release_selector.set_selected(selected);
        self.suppress_toggles.set(false);
        // Only offer the selector when there is a genuine choice; with a
        // single (or no) release the pre-selected identity stands alone.
        self.release_selector.set_visible(multiple);
    }

    fn refresh_metadata(&self) {
        let state = self.state.borrow();
        let release = state.selected_release();
        let title = release
            .and_then(|release| non_blank(release.title.as_deref()))
            .unwrap_or(FALLBACK_ALBUM);
        self.title_label.set_text(title);
        self.subtitle_label.set_text(&subtitle_text(release));
        if let Some(snapshot) = state.snapshot.as_ref() {
            for row in &state.rows {
                let (mut track_title, mut track_artist) =
                    track_display(snapshot, release, row.number);
                // A user's inline edit wins over the looked-up value.
                if let Some(over) = state.overrides.get(&row.number) {
                    if let Some(title) = non_blank(over.title.as_deref()) {
                        track_title = title.to_owned();
                    }
                    if let Some(artist) = non_blank(over.artist.as_deref()) {
                        track_artist = artist.to_owned();
                    }
                }
                row.title.label.set_text(&track_title);
                row.artist.label.set_text(&track_artist);
            }
        }
    }

    /// Record a user's inline edit of a track field and refresh the labels
    /// so the edited value is shown (and used at import).
    fn set_track_override(&self, number: u32, field: OverrideField, value: String) {
        {
            let mut state = self.state.borrow_mut();
            let entry = state.overrides.entry(number).or_default();
            match field {
                OverrideField::Title => entry.title = Some(value),
                OverrideField::Artist => entry.artist = Some(value),
            }
        }
        self.refresh_metadata();
    }

    fn rebuild_checklist(&self, snapshot: &TocSnapshot) {
        while let Some(child) = self.checklist_box.first_child() {
            self.checklist_box.remove(&child);
        }
        let mut rows = Vec::with_capacity(snapshot.tracks.len());
        for (index, track) in snapshot.tracks.iter().enumerate() {
            let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
            row.add_css_class("cd-import-track-row");
            // Zebra parity matches the library table's `row_position % 2`
            // rule, so the filler painter's stripes line up below them.
            row.add_css_class(if index % 2 == 0 {
                "cd-import-track-row-even"
            } else {
                "cd-import-track-row-odd"
            });

            let check = gtk::CheckButton::new();
            check.set_active(true);
            check.add_css_class("cd-import-track-check");
            check.set_valign(gtk::Align::Center);

            let number = gtk::Label::new(Some(&format!("{:02}", track.number)));
            number.add_css_class("cd-import-track-number");
            number.add_css_class("dim-label");
            number.set_xalign(1.0);
            number.set_width_chars(2);

            let title = build_editable_cell("cd-import-track-title", false);
            let artist = build_editable_cell("cd-import-track-artist", true);

            let duration = gtk::Label::new(Some(&format_duration(track.duration())));
            duration.add_css_class("cd-import-track-duration");
            duration.add_css_class("dim-label");
            duration.set_xalign(1.0);
            // Fixed-width duration column so the title/artist split — and thus
            // the column boundaries — line up across every row.
            duration.set_width_chars(5);

            row.append(&check);
            row.append(&number);
            row.append(&title.stack);
            row.append(&artist.stack);
            row.append(&duration);
            self.checklist_box.append(&row);

            let panel = self.clone();
            check.connect_toggled(move |_| {
                panel.update_import_sensitivity();
            });

            let number_value = track.number;
            {
                let panel = self.clone();
                wire_cell_editing(&title, move |text| {
                    panel.set_track_override(number_value, OverrideField::Title, text);
                });
            }
            {
                let panel = self.clone();
                wire_cell_editing(&artist, move |text| {
                    panel.set_track_override(number_value, OverrideField::Artist, text);
                });
            }

            rows.push(TrackRow {
                number: track.number,
                check,
                title,
                artist,
            });
        }
        self.state.borrow_mut().rows = rows;
        self.painter
            .set_row_count(u32::try_from(snapshot.tracks.len()).unwrap_or(u32::MAX));
    }

    fn selected_track_numbers(&self) -> Vec<u32> {
        self.state
            .borrow()
            .rows
            .iter()
            .filter(|row| row.check.is_active())
            .map(|row| row.number)
            .collect()
    }

    fn current_request(&self) -> Option<CdImportRequest> {
        let state = self.state.borrow();
        let snapshot = state.snapshot.clone()?;
        let selected_tracks = self.selected_track_numbers();
        if selected_tracks.is_empty() {
            return None;
        }
        Some(CdImportRequest {
            snapshot,
            selected_tracks,
            release: state.selected_release().cloned(),
            cover: state.cover.clone(),
            overrides: state.overrides.clone(),
        })
    }

    fn update_import_sensitivity(&self) {
        self.import_button.set_sensitive(self.import_allowed());
    }

    /// Whether the page's own state permits an import: a disc is shown and at
    /// least one track is ticked. Separated from the runtime gates so it can
    /// be unit-tested without a configured library or CD backend.
    fn panel_ready_for_import(&self) -> bool {
        let state = self.state.borrow();
        state.snapshot.is_some() && state.rows.iter().any(|row| row.check.is_active())
    }

    fn import_allowed(&self) -> bool {
        if !self.panel_ready_for_import() {
            return false;
        }
        let runtime = self.runtime.borrow();
        runtime.settings().library_path().is_some()
            && !runtime.background_task_status().is_running()
            && runtime.cd_import_unavailable_reason().is_none()
    }

    fn set_artwork_searching(&self) {
        self.artwork_picture.set_visible(false);
        self.artwork_icon.set_visible(false);
        self.artwork_spinner.set_visible(true);
        self.artwork_spinner.start();
    }

    fn set_artwork_placeholder(&self) {
        self.artwork_spinner.stop();
        self.artwork_spinner.set_visible(false);
        self.artwork_picture.set_paintable(None::<&gdk::Texture>);
        self.artwork_picture.set_visible(false);
        self.artwork_icon.set_visible(true);
    }

    fn set_artwork_texture(&self, texture: &gdk::Texture) {
        self.artwork_spinner.stop();
        self.artwork_spinner.set_visible(false);
        self.artwork_icon.set_visible(false);
        self.artwork_picture.set_paintable(Some(texture));
        self.artwork_picture.set_visible(true);
    }
}

/// Build the fixed-size artwork overlay and return it with the three layers
/// the panel toggles between. The overlay has no main child — its size comes
/// solely from the square `size_request`, and every layer is excluded from
/// measurement (`set_measure_overlay(false)`) so an asynchronously loaded
/// cover can never grow the header.
fn build_artwork() -> (gtk::Overlay, gtk::Picture, gtk::Image, gtk::Spinner) {
    let artwork = gtk::Overlay::new();
    artwork.add_css_class("cd-import-artwork");
    artwork.set_size_request(ARTWORK_SIZE, ARTWORK_SIZE);
    artwork.set_halign(gtk::Align::Start);
    artwork.set_valign(gtk::Align::Center);
    // GTK has no CSS `overflow`; clip the cover to the rounded card here.
    artwork.set_overflow(gtk::Overflow::Hidden);

    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Cover);
    picture.set_can_shrink(true);
    picture.set_halign(gtk::Align::Fill);
    picture.set_valign(gtk::Align::Fill);
    picture.set_visible(false);
    artwork.add_overlay(&picture);
    artwork.set_clip_overlay(&picture, true);
    artwork.set_measure_overlay(&picture, false);

    let icon = gtk::Image::from_icon_name("image-missing-symbolic");
    icon.add_css_class("cd-import-artwork-placeholder");
    icon.add_css_class("dim-label");
    icon.set_pixel_size(ARTWORK_SIZE / 2);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    artwork.add_overlay(&icon);
    artwork.set_measure_overlay(&icon, false);

    let spinner = gtk::Spinner::new();
    spinner.set_halign(gtk::Align::Center);
    spinner.set_valign(gtk::Align::Center);
    spinner.set_size_request(ARTWORK_SIZE / 2, ARTWORK_SIZE / 2);
    spinner.set_visible(false);
    artwork.add_overlay(&spinner);
    artwork.set_measure_overlay(&spinner, false);

    (artwork, picture, icon, spinner)
}

/// Build a double-click-to-edit table cell. The label is shown normally; a
/// double-click swaps in the entry seeded with the current text. Editing is
/// committed on Enter or focus loss and cancelled on Escape.
fn build_editable_cell(label_css_class: &str, dim: bool) -> EditableCell {
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::None);
    // Size to the visible child so the compact label keeps the row at its
    // 28px pitch; the taller entry only grows the row while editing.
    stack.set_vhomogeneous(false);

    let label = gtk::Label::new(None);
    label.add_css_class(label_css_class);
    if dim {
        label.add_css_class("dim-label");
    }
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let entry = gtk::Entry::new();
    // Reuse the library table's inline-edit entry styling — same kind of
    // in-table cell editor, so it must look identical.
    entry.add_css_class("track-table-inline-edit");
    entry.set_hexpand(true);

    stack.add_named(&label, Some("label"));
    stack.add_named(&entry, Some("entry"));
    stack.set_visible_child_name("label");

    EditableCell {
        stack,
        label,
        entry,
    }
}

/// Wire a cell's edit lifecycle: double-click to begin, Enter / focus-out to
/// commit (calling `on_commit` with the typed text), Escape to cancel.
fn wire_cell_editing(cell: &EditableCell, on_commit: impl Fn(String) + 'static) {
    let on_commit = Rc::new(on_commit);

    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_PRIMARY);
    {
        let cell = cell.clone();
        gesture.connect_pressed(move |gesture, n_press, _x, _y| {
            if n_press < 2 {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            cell.entry.set_text(&cell.label.text());
            cell.stack.set_visible_child_name("entry");
            cell.entry.grab_focus();
            cell.entry.select_region(0, -1);
        });
    }
    cell.label.add_controller(gesture);

    {
        let cell = cell.clone();
        let on_commit = on_commit.clone();
        cell.entry.connect_activate(move |entry| {
            let text = entry.text().to_string();
            cell.stack.set_visible_child_name("label");
            on_commit(text);
        });
    }

    let focus = gtk::EventControllerFocus::new();
    {
        let cell = cell.clone();
        let on_commit = on_commit.clone();
        focus.connect_leave(move |_| {
            // Only commit if we are leaving an active edit (Escape already
            // restored the label, so this is a no-op then).
            if cell.stack.visible_child_name().as_deref() == Some("entry") {
                let text = cell.entry.text().to_string();
                cell.stack.set_visible_child_name("label");
                on_commit(text);
            }
        });
    }
    cell.entry.add_controller(focus);

    let keys = gtk::EventControllerKey::new();
    {
        let cell = cell.clone();
        keys.connect_key_pressed(move |_controller, key, _code, _modifiers| {
            if key == gdk::Key::Escape {
                // Cancel: drop the edit and restore the label unchanged.
                cell.stack.set_visible_child_name("label");
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        });
    }
    cell.entry.add_controller(keys);
}

fn texture_from_bytes(bytes: &[u8]) -> Option<gdk::Texture> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(bytes).ok()?;
    loader.close().ok()?;
    let pixbuf = loader.pixbuf()?;
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

/// Pick the most reasonable release to pre-select among the candidates
/// MusicBrainz returned (all already TOC-compatible). Prefers a non-
/// compilation pressing, then the most complete metadata (year / label /
/// country), then the earliest year (the original release over a reissue).
/// Returns the first candidate on a tie, or `None` for an empty list.
fn most_reasonable_release_index(releases: &[DiscRelease]) -> Option<usize> {
    let mut best_index = 0;
    let mut best_rank = release_rank(releases.first()?);
    for (index, release) in releases.iter().enumerate().skip(1) {
        let rank = release_rank(release);
        if rank > best_rank {
            best_index = index;
            best_rank = rank;
        }
    }
    Some(best_index)
}

/// Sort key for [`most_reasonable_release_index`]; larger is better. The
/// year term maps an earlier year to a larger value so the original pressing
/// wins, with a missing year ranking last.
fn release_rank(release: &DiscRelease) -> (u8, u8, u32) {
    let not_compilation = u8::from(!release.is_compilation);
    let mut completeness = 0u8;
    if release.year.is_some() {
        completeness += 1;
    }
    if non_blank(release.label.as_deref()).is_some() {
        completeness += 1;
    }
    if non_blank(release.country.as_deref()).is_some() {
        completeness += 1;
    }
    let earliest = release
        .year
        .map(|year| u32::MAX - year.max(0) as u32)
        .unwrap_or(0);
    (not_compilation, completeness, earliest)
}

fn release_selector_label(release: &DiscRelease) -> String {
    let title = non_blank(release.title.as_deref()).unwrap_or(FALLBACK_ALBUM);
    match release.year {
        Some(year) => format!("{title} ({year})"),
        None => title.to_owned(),
    }
}

/// The header's second line: the release artist, with the release details
/// (year / label / country) appended after a separator when present.
fn subtitle_text(release: Option<&DiscRelease>) -> String {
    let artist = release
        .and_then(|release| non_blank(release.artist_credit.as_deref()))
        .unwrap_or(FALLBACK_ARTIST);
    let details = release.map(release_details).unwrap_or_default();
    if details.is_empty() {
        artist.to_owned()
    } else {
        format!("{artist} · {details}")
    }
}

fn release_details(release: &DiscRelease) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(year) = release.year {
        parts.push(year.to_string());
    }
    if let Some(label) = non_blank(release.label.as_deref()) {
        parts.push(label.to_owned());
    }
    if let Some(country) = non_blank(release.country.as_deref()) {
        parts.push(country.to_owned());
    }
    parts.join(" · ")
}

/// Display title/artist for one physical track under the selected release,
/// mapped by ordered position (the runtime applies the same mapping when it
/// actually tags the file).
fn track_display(
    snapshot: &TocSnapshot,
    release: Option<&DiscRelease>,
    number: u32,
) -> (String, String) {
    let ordered_index = snapshot
        .tracks
        .iter()
        .position(|track| track.number == number);
    let disc_track = release.and_then(|release| ordered_index.and_then(|i| release.tracks.get(i)));
    let title = disc_track
        .and_then(|track| non_blank(track.title.as_deref()))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("Track {number:02}"));
    let artist = disc_track
        .and_then(|track| non_blank(track.artist_credit.as_deref()))
        .or_else(|| release.and_then(|release| non_blank(release.artist_credit.as_deref())))
        .unwrap_or("")
        .to_owned();
    (title, artist)
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|inner| !inner.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use sustain_app_runtime::{ApplicationRuntime, DiscTrack, RawTocTrack, TocSnapshot};

    use super::*;

    fn snapshot(disc_id: &str, tracks: usize) -> TocSnapshot {
        let raw: Vec<RawTocTrack> = (1..=tracks as i32)
            .map(|number| RawTocTrack {
                number,
                offset: 150 + number * 10_000,
                sectors: 13_350,
            })
            .collect();
        TocSnapshot::from_raw(
            std::path::PathBuf::from("/dev/sr0"),
            disc_id.to_owned(),
            String::new(),
            &raw,
        )
    }

    fn release_named(title: &str, tracks: usize) -> DiscRelease {
        release_with(title, tracks, false, Some(2000))
    }

    fn release_with(
        title: &str,
        tracks: usize,
        is_compilation: bool,
        year: Option<i32>,
    ) -> DiscRelease {
        DiscRelease {
            release_mbid: format!("rel-{title}"),
            release_group_mbid: None,
            title: Some(title.to_owned()),
            artist_credit: Some("The Band".to_owned()),
            year,
            date: year.map(|y| y.to_string()),
            country: None,
            label: None,
            format: Some("CD".to_owned()),
            disc_number: Some(1),
            disc_total: None,
            track_total: tracks as u32,
            is_compilation,
            tracks: (1..=tracks as u32)
                .map(|position| DiscTrack {
                    position,
                    title: Some(format!("{title} track {position}")),
                    artist_credit: None,
                    duration_ms: Some(180_000),
                    recording_mbid: None,
                })
                .collect(),
        }
    }

    fn panel() -> CdImportPanel {
        let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
        CdImportPanel::new(runtime)
    }

    #[test]
    fn most_reasonable_prefers_album_then_earliest_pressing() {
        // Nothing to pick from.
        assert_eq!(most_reasonable_release_index(&[]), None);

        // A compilation loses to a real album even if the album is older.
        let compilation = release_with("Hits", 3, true, Some(2010));
        let album = release_with("Album", 3, false, Some(1999));
        assert_eq!(
            most_reasonable_release_index(&[compilation, album]),
            Some(1)
        );

        // Two equal albums: the earlier (original) pressing wins.
        let reissue = release_with("Album", 3, false, Some(2015));
        let original = release_with("Album", 3, false, Some(1995));
        assert_eq!(most_reasonable_release_index(&[reissue, original]), Some(1));

        // A full tie keeps the first candidate.
        let first = release_with("Album", 3, false, Some(2000));
        let second = release_with("Album", 3, false, Some(2000));
        assert_eq!(most_reasonable_release_index(&[first, second]), Some(0));
    }

    #[test]
    fn all_tracks_ticked_by_default_and_unticking_all_blocks_import() {
        let ran = crate::test_support::with_gtk(|| {
            let panel = panel();
            panel.show_disc(snapshot("disc-a", 3));
            assert_eq!(panel.state.borrow().rows.len(), 3);
            assert!(
                panel
                    .state
                    .borrow()
                    .rows
                    .iter()
                    .all(|row| row.check.is_active()),
                "every track is ticked on show"
            );
            assert!(panel.panel_ready_for_import());

            // Untick every row → not import-ready.
            for row in &panel.state.borrow().rows {
                row.check.set_active(false);
            }
            assert!(!panel.panel_ready_for_import());

            // Re-tick one → ready again.
            panel.state.borrow().rows[0].check.set_active(true);
            assert!(panel.panel_ready_for_import());
        });
        if !ran {
            eprintln!("SMOKE: no display, skipping");
        }
    }

    #[test]
    fn multiple_releases_preselect_the_most_reasonable_and_show_the_selector() {
        let ran = crate::test_support::with_gtk(|| {
            let panel = panel();
            panel.show_disc(snapshot("disc-a", 3));
            // No releases yet (fallback) → import-ready on the panel's terms.
            assert!(panel.panel_ready_for_import());

            // Two albums; the earlier pressing (row 1) is most reasonable.
            let later = release_with("First", 3, false, Some(2010));
            let earlier = release_with("Second", 3, false, Some(1990));
            panel.apply_releases(vec![later, earlier], false);
            assert!(panel.release_selector.is_visible());
            assert_eq!(panel.release_selector.selected(), 1, "earlier pressing");
            assert!(panel.panel_ready_for_import());
            assert_eq!(panel.title_label.text(), "Second");
        });
        if !ran {
            eprintln!("SMOKE: no display, skipping");
        }
    }

    #[test]
    fn single_release_auto_selects_and_hides_the_selector() {
        let ran = crate::test_support::with_gtk(|| {
            let panel = panel();
            panel.show_disc(snapshot("disc-a", 2));
            panel.apply_releases(vec![release_named("Only", 2)], false);
            assert!(!panel.release_selector.is_visible());
            assert!(panel.panel_ready_for_import());
            assert_eq!(panel.title_label.text(), "Only");
        });
        if !ran {
            eprintln!("SMOKE: no display, skipping");
        }
    }

    #[test]
    fn forgetting_the_disc_clears_the_page_and_disables_import() {
        let ran = crate::test_support::with_gtk(|| {
            let panel = panel();
            panel.show_disc(snapshot("disc-a", 2));
            // A default runtime has no library path, so the button is disabled
            // even though the panel itself is ready.
            assert!(panel.panel_ready_for_import());
            assert!(
                !panel.import_button.is_sensitive(),
                "no configured library path keeps Import disabled"
            );

            panel.forget_disc();
            assert!(panel.current_snapshot().is_none());
            assert!(!panel.panel_ready_for_import());
            assert!(!panel.import_button.is_sensitive());
        });
        if !ran {
            eprintln!("SMOKE: no display, skipping");
        }
    }
}
