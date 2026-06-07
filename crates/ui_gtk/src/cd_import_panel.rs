// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The CD-import page shown in the main content column when an inserted
//! audio CD is selected in the DEVICES sidebar section (issue #25).
//!
//! Layout: a fixed-height top bar — the same height as the title bar, like
//! the Playlists header — carrying the release artwork on the left, the
//! release identity (title over artist/details) and an optional release
//! selector in the middle, and the action cluster (Encoding Settings, Eject,
//! Import CD) on the right. Below it the disc's audio tracks are listed in a
//! [`gtk::ColumnView`] built exactly like the Songs table: striped rows that
//! continue their zebra into the empty region below the last track via the
//! shared [`EmptyRowPainter`], proper column headers, and the same
//! click-an-already-selected-cell inline editing through the shared
//! [`InlineEditController`].
//!
//! The columns are: a leading rip-status column (empty / spinner while a
//! track is being ripped / green tick once done — issue #195), an import
//! tick, the track number, the editable title and artist, and the duration.
//! Editing title/artist lets a disc MusicBrainz could not identify be named
//! by hand; the typed value wins over the looked-up / generated one.
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
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gdk_pixbuf, gio, glib};

use sustain_app_runtime::{CdImportRequest, CdTrackOverride, DiscRelease, TocSnapshot};

use crate::track_table::{EditableField, EmptyRowPainter, InlineEditController};
use crate::{SharedRuntime, TITLEBAR_HEIGHT};

const FALLBACK_ALBUM: &str = "Audio CD";
const FALLBACK_ARTIST: &str = "Unknown Artist";
/// Edge length of the header artwork thumbnail. Sized to sit comfortably
/// inside the fixed [`TITLEBAR_HEIGHT`] top bar with a little vertical
/// breathing room.
const ARTWORK_SIZE: i32 = 56;

/// Fixed widths for the non-text columns. The status column matches the
/// Songs table's leading status column so the two views read identically.
const STATUS_COLUMN_WIDTH: i32 = 26;
const SELECT_COLUMN_WIDTH: i32 = 34;
const NUMBER_COLUMN_WIDTH: i32 = 48;
const TITLE_COLUMN_WIDTH: i32 = 280;
const ARTIST_COLUMN_WIDTH: i32 = 200;
const TIME_COLUMN_WIDTH: i32 = 70;
const STATUS_ICON_SIZE: i32 = 14;
/// The "done" tick glyph. Rendered as a filled green check disc via the
/// shared `device-analysis-badge`/`ok` classes — the exact same badge the
/// Smart Shuffle preferences tab uses for its "ready" state, so it reads
/// green in both light and dark themes and against the system accent.
const DONE_ICON: &str = "object-select-symbolic";

/// Fired when the user clicks `Import CD`. The main window wires this to
/// the runtime's prepare / background-worker / apply path.
pub(crate) type CdImportRequestedCallback = Rc<dyn Fn(CdImportRequest)>;

/// Fired when the user clicks the header `Eject` button. Carries the shown
/// disc so the main window can resolve and eject the drive.
pub(crate) type CdEjectRequestedCallback = Rc<dyn Fn(TocSnapshot)>;

/// Per-track rip status rendered in the leading status column (issue #195).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CdRipStatus {
    /// Not (yet) being ripped — the column is empty.
    Pending,
    /// Currently being read/encoded — an animated spinner.
    Ripping,
    /// Imported — a green tick.
    Done,
}

/// One disc track, the model item backing a [`gtk::ColumnView`] row. Mutable
/// per-row state (the import tick, the inline-edit overrides, the rip status)
/// lives behind interior mutability so the cell factories can read and write
/// it through the bound [`glib::BoxedAnyObject`] without a `&mut` model.
struct CdRow {
    number: u32,
    duration: Duration,
    /// The looked-up / generated title and artist for the selected release;
    /// recomputed when the chosen release changes.
    base_title: RefCell<String>,
    base_artist: RefCell<String>,
    /// User-typed overrides; a non-blank value wins over the base for both
    /// display and import. Kept across release switches.
    override_title: RefCell<Option<String>>,
    override_artist: RefCell<Option<String>>,
    /// Whether this track is ticked for import.
    selected: Cell<bool>,
    status: Cell<CdRipStatus>,
}

impl CdRow {
    fn display_title(&self) -> String {
        non_blank(self.override_title.borrow().as_deref())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.base_title.borrow().clone())
    }

    fn display_artist(&self) -> String {
        non_blank(self.override_artist.borrow().as_deref())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| self.base_artist.borrow().clone())
    }
}

/// A bound title/artist label, refreshed when the chosen release or a user
/// override changes its displayed value. Mirrors the Songs table's
/// `TextBindings`. Pruned on teardown.
struct CdTextBinding {
    list_item: gtk::ListItem,
    label: gtk::Label,
    field: EditableField,
}

#[derive(Clone, Default)]
struct CdTextBindings(Rc<RefCell<Vec<CdTextBinding>>>);

impl CdTextBindings {
    fn refresh(&self) {
        let mut bindings = self.0.borrow_mut();
        bindings.retain(|binding| binding.list_item.item().is_some());
        for binding in bindings.iter() {
            let field = binding.field;
            let text = list_item_cd_row(&binding.list_item, |row| match field {
                EditableField::Artist => row.display_artist(),
                _ => row.display_title(),
            })
            .unwrap_or_default();
            binding.label.set_text(&text);
        }
    }
}

/// A bound status cell, refreshed as a rip progresses. Pruned on teardown.
struct CdStatusBinding {
    list_item: gtk::ListItem,
    spinner: gtk::Spinner,
    icon: gtk::Image,
}

#[derive(Clone, Default)]
struct CdStatusBindings(Rc<RefCell<Vec<CdStatusBinding>>>);

impl CdStatusBindings {
    fn refresh(&self) {
        let mut bindings = self.0.borrow_mut();
        bindings.retain(|binding| binding.list_item.item().is_some());
        for binding in bindings.iter() {
            let status = list_item_cd_row(&binding.list_item, |row| row.status.get())
                .unwrap_or(CdRipStatus::Pending);
            render_cd_status(&binding.spinner, &binding.icon, status);
        }
    }
}

struct PanelState {
    snapshot: Option<TocSnapshot>,
    releases: Vec<DiscRelease>,
    selected_release: Option<usize>,
    cover: Option<Vec<u8>>,
    /// Sorted physical track numbers captured when an import starts, so the
    /// status column can mark the first `completed_tracks` of them done while
    /// the rest stay pending (issue #195).
    importing: Vec<u32>,
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
    store: gio::ListStore,
    text_bindings: CdTextBindings,
    status_bindings: CdStatusBindings,
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

        // --- Track table: a ColumnView built like the Songs table ---
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let table = gtk::ColumnView::new(None::<gtk::SelectionModel>);
        table.add_css_class("track-table");
        table.set_hexpand(true);
        table.set_vexpand(true);
        table.set_show_column_separators(false);
        table.set_show_row_separators(false);
        table.set_single_click_activate(false);
        table.set_reorderable(false);

        // A single selectable row drives the inline-edit interaction (click an
        // already-selected cell to edit); the per-row tick column owns the
        // separate "which tracks to import" choice.
        let selection = gtk::SingleSelection::new(Some(store.clone()));
        selection.set_autoselect(false);
        selection.set_can_unselect(true);

        let text_bindings = CdTextBindings::default();
        let status_bindings = CdStatusBindings::default();
        let suppress_check_toggle = Rc::new(Cell::new(false));

        let inline_edit = {
            let seed = Rc::new(move |object: &glib::Object, field: EditableField| {
                with_cd_row(object, |row| match field {
                    EditableField::Artist => row.display_artist(),
                    _ => row.display_title(),
                })
            });
            let commit_bindings = text_bindings.clone();
            let commit = Rc::new(
                move |object: &glib::Object, field: EditableField, new_text: String| {
                    let applied = with_cd_row(object, |row| match field {
                        EditableField::Artist => {
                            *row.override_artist.borrow_mut() = Some(new_text);
                        }
                        EditableField::Title => {
                            *row.override_title.borrow_mut() = Some(new_text);
                        }
                        _ => {}
                    });
                    if applied.is_none() {
                        return false;
                    }
                    commit_bindings.refresh();
                    true
                },
            );
            InlineEditController::new(crate::track_table::InlineEditHooks { seed, commit })
        };

        table.append_column(&fixed_column(
            None,
            STATUS_COLUMN_WIDTH,
            &build_cd_status_factory(status_bindings.clone()),
        ));
        table.append_column(&fixed_column(
            None,
            SELECT_COLUMN_WIDTH,
            &build_cd_check_factory(
                store.clone(),
                runtime.clone(),
                import_button.clone(),
                suppress_check_toggle,
            ),
        ));
        table.append_column(&fixed_column(
            Some("#"),
            NUMBER_COLUMN_WIDTH,
            &build_cd_number_factory(),
        ));
        table.append_column(&fixed_column(
            Some("Title"),
            TITLE_COLUMN_WIDTH,
            &build_cd_text_factory(
                EditableField::Title,
                text_bindings.clone(),
                inline_edit.clone(),
            ),
        ));
        table.append_column(&fixed_column(
            Some("Artist"),
            ARTIST_COLUMN_WIDTH,
            &build_cd_text_factory(
                EditableField::Artist,
                text_bindings.clone(),
                inline_edit.clone(),
            ),
        ));
        table.append_column(&fixed_column(
            Some("Time"),
            TIME_COLUMN_WIDTH,
            &build_cd_duration_factory(),
        ));
        let filler = gtk::ColumnViewColumn::new(None, Some(build_cd_filler_factory()));
        filler.set_expand(true);
        filler.set_resizable(false);
        table.append_column(&filler);

        table.set_model(Some(&selection));

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_hexpand(true);
        scroller.set_vexpand(true);
        scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroller.set_child(Some(&table));

        // Continue the zebra into the empty area below the last track, like
        // the library table. The painter reads the row count from the store
        // on every change.
        let painter = EmptyRowPainter::new(&scroller, &table);
        painter.set_row_count(store.n_items());
        let painter_for_store = painter.downgrade();
        store.connect_items_changed(move |store, _position, _removed, _added| {
            if let Some(painter) = painter_for_store.upgrade() {
                painter.set_row_count(store.n_items());
            }
        });
        root.append(&painter);

        let panel = Self {
            root,
            artwork_picture,
            artwork_icon,
            artwork_spinner,
            title_label,
            subtitle_label,
            release_selector,
            store,
            text_bindings,
            status_bindings,
            import_button,
            state: Rc::new(RefCell::new(PanelState {
                snapshot: None,
                releases: Vec::new(),
                selected_release: None,
                cover: None,
                importing: Vec::new(),
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
                panel.refresh_header();
                panel.recompute_row_base();
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
            // Record the import set (sorted, matching the worker's order) and
            // reset the status column so it tracks the rip (issue #195).
            let mut importing = request.selected_tracks.clone();
            importing.sort_unstable();
            panel.begin_import_display(importing);
            if let Some(callback) = panel.on_import_requested.borrow().as_ref() {
                callback(request);
            }
        });
    }

    /// Render the page for a freshly-probed disc: build the row model, show
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
            state.importing.clear();
        }
        self.suppress_toggles.set(true);
        self.release_selector.set_visible(false);
        self.suppress_toggles.set(false);
        self.set_artwork_placeholder();
        self.rebuild_rows(&snapshot);
        self.refresh_header();
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
        self.refresh_header();
        self.recompute_row_base();
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
            state.importing.clear();
        }
        self.store.remove_all();
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

    /// Reflect a rip-progress tick in the status column (issue #195): the
    /// first `completed` tracks of the import set are done, the
    /// `current` track (if any) shows a spinner, the rest stay pending.
    pub(crate) fn apply_import_progress(&self, completed: usize, current: Option<u32>) {
        let importing = self.state.borrow().importing.clone();
        let done: HashSet<u32> = importing.iter().take(completed).copied().collect();
        self.set_all_status(|number| {
            if done.contains(&number) {
                CdRipStatus::Done
            } else if current == Some(number) {
                CdRipStatus::Ripping
            } else {
                CdRipStatus::Pending
            }
        });
    }

    /// Settle the status column when the rip ends: the `imported` tracks of
    /// the import set are done, every other row clears (its spinner stops).
    pub(crate) fn finish_import_display(&self, imported: usize) {
        let importing = self.state.borrow().importing.clone();
        let done: HashSet<u32> = importing.iter().take(imported).copied().collect();
        self.set_all_status(|number| {
            if done.contains(&number) {
                CdRipStatus::Done
            } else {
                CdRipStatus::Pending
            }
        });
    }

    fn begin_import_display(&self, importing: Vec<u32>) {
        self.state.borrow_mut().importing = importing;
        self.set_all_status(|_| CdRipStatus::Pending);
    }

    fn set_all_status(&self, status_for: impl Fn(u32) -> CdRipStatus) {
        for index in 0..self.store.n_items() {
            let Some(object) = self.store.item(index) else {
                continue;
            };
            let _ = with_cd_row(&object, |row| row.status.set(status_for(row.number)));
        }
        self.status_bindings.refresh();
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

    fn refresh_header(&self) {
        let state = self.state.borrow();
        let release = state.selected_release();
        let title = release
            .and_then(|release| non_blank(release.title.as_deref()))
            .unwrap_or(FALLBACK_ALBUM);
        self.title_label.set_text(title);
        self.subtitle_label.set_text(&subtitle_text(release));
    }

    /// Recompute every row's base (looked-up) title/artist for the chosen
    /// release and refresh the visible labels. User overrides are untouched,
    /// so a hand-typed title survives a release switch.
    fn recompute_row_base(&self) {
        {
            let state = self.state.borrow();
            let Some(snapshot) = state.snapshot.as_ref() else {
                return;
            };
            let release = state.selected_release();
            for index in 0..self.store.n_items() {
                let Some(object) = self.store.item(index) else {
                    continue;
                };
                let _ = with_cd_row(&object, |row| {
                    let (title, artist) = track_display(snapshot, release, row.number);
                    *row.base_title.borrow_mut() = title;
                    *row.base_artist.borrow_mut() = artist;
                });
            }
        }
        self.text_bindings.refresh();
    }

    fn rebuild_rows(&self, snapshot: &TocSnapshot) {
        self.store.remove_all();
        for track in &snapshot.tracks {
            let (title, artist) = track_display(snapshot, None, track.number);
            let row = CdRow {
                number: track.number,
                duration: track.duration(),
                base_title: RefCell::new(title),
                base_artist: RefCell::new(artist),
                override_title: RefCell::new(None),
                override_artist: RefCell::new(None),
                selected: Cell::new(true),
                status: Cell::new(CdRipStatus::Pending),
            };
            self.store.append(&glib::BoxedAnyObject::new(row));
        }
    }

    fn current_request(&self) -> Option<CdImportRequest> {
        let state = self.state.borrow();
        let snapshot = state.snapshot.clone()?;
        let mut selected_tracks = Vec::new();
        let mut overrides = BTreeMap::new();
        for index in 0..self.store.n_items() {
            let Some(object) = self.store.item(index) else {
                continue;
            };
            let _ = with_cd_row(&object, |row| {
                if !row.selected.get() {
                    return;
                }
                selected_tracks.push(row.number);
                let track_override = CdTrackOverride {
                    title: row.override_title.borrow().clone(),
                    artist: row.override_artist.borrow().clone(),
                };
                if track_override.title.is_some() || track_override.artist.is_some() {
                    overrides.insert(row.number, track_override);
                }
            });
        }
        if selected_tracks.is_empty() {
            return None;
        }
        Some(CdImportRequest {
            snapshot,
            selected_tracks,
            release: state.selected_release().cloned(),
            cover: state.cover.clone(),
            overrides,
        })
    }

    fn update_import_sensitivity(&self) {
        self.import_button
            .set_sensitive(import_allowed(&self.store, &self.runtime));
    }

    /// Whether the page's own state permits an import: a disc is shown and at
    /// least one track is ticked. Separated from the runtime gates so it can
    /// be unit-tested without a configured library or CD backend.
    #[cfg(test)]
    fn panel_ready_for_import(&self) -> bool {
        self.state.borrow().snapshot.is_some() && any_selected(&self.store)
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

/// Whether the runtime currently permits starting a CD import for the rows in
/// `store`. Shared by the panel's button refresh and the per-row tick handler.
fn import_allowed(store: &gio::ListStore, runtime: &SharedRuntime) -> bool {
    if !any_selected(store) {
        return false;
    }
    let runtime = runtime.borrow();
    runtime.settings().library_path().is_some()
        && !runtime.background_task_status().is_running()
        && runtime.cd_import_unavailable_reason().is_none()
}

fn any_selected(store: &gio::ListStore) -> bool {
    (0..store.n_items()).any(|index| {
        store
            .item(index)
            .and_then(|object| with_cd_row(&object, |row| row.selected.get()))
            .unwrap_or(false)
    })
}

/// Build a fixed-width, non-resizable column from a factory. Mirrors the
/// Songs table's structural columns; the trailing filler column absorbs slack.
fn fixed_column(
    title: Option<&str>,
    width: i32,
    factory: &gtk::SignalListItemFactory,
) -> gtk::ColumnViewColumn {
    let column = gtk::ColumnViewColumn::new(title, Some(factory.clone()));
    column.set_resizable(false);
    column.set_expand(false);
    column.set_fixed_width(width);
    column
}

fn new_cell() -> gtk::Box {
    let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    cell.add_css_class("track-table-cell");
    cell.set_hexpand(true);
    cell.set_vexpand(true);
    cell.set_halign(gtk::Align::Fill);
    cell.set_valign(gtk::Align::Fill);
    cell
}

fn build_cd_status_factory(bindings: CdStatusBindings) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    let bindings_setup = bindings.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = new_cell();
        install_cd_cell_selection_sync(list_item, &cell);

        let spinner = gtk::Spinner::new();
        spinner.set_halign(gtk::Align::Center);
        spinner.set_valign(gtk::Align::Center);
        spinner.set_hexpand(true);
        spinner.set_size_request(STATUS_ICON_SIZE, STATUS_ICON_SIZE);
        spinner.set_visible(false);

        let icon = gtk::Image::from_icon_name(DONE_ICON);
        // Filled green check disc, identical to the Smart Shuffle "ready"
        // badge: the round fill comes from `device-analysis-badge`, the
        // success colour from `ok`, and the glyph is recoloured to the base.
        icon.add_css_class("device-analysis-badge");
        icon.add_css_class("ok");
        icon.set_pixel_size(STATUS_ICON_SIZE);
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);
        icon.set_hexpand(true);
        icon.set_visible(false);

        cell.append(&spinner);
        cell.append(&icon);
        list_item.set_child(Some(&cell));

        bindings_setup.0.borrow_mut().push(CdStatusBinding {
            list_item: list_item.clone(),
            spinner,
            icon,
        });
    });

    let bindings_teardown = bindings;
    factory.connect_teardown(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        bindings_teardown
            .0
            .borrow_mut()
            .retain(|binding| binding.list_item != *list_item);
    });

    factory.connect_unbind(move |_factory, item| {
        // Stop a recycled cell's spinner so it does not keep animating for a
        // row that is no longer visible.
        let Some((spinner, _icon)) = status_widgets_of(item) else {
            return;
        };
        spinner.stop();
        spinner.set_visible(false);
    });

    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item
            .child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        bind_cell_chrome(&cell, list_item);
        let Some((spinner, icon)) = status_widgets_of(item) else {
            return;
        };
        let status =
            list_item_cd_row(list_item, |row| row.status.get()).unwrap_or(CdRipStatus::Pending);
        render_cd_status(&spinner, &icon, status);
    });

    factory
}

fn build_cd_check_factory(
    store: gio::ListStore,
    runtime: SharedRuntime,
    import_button: gtk::Button,
    suppress: Rc<Cell<bool>>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    let suppress_setup = suppress.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = new_cell();
        install_cd_cell_selection_sync(list_item, &cell);

        let check = gtk::CheckButton::new();
        check.add_css_class("cd-import-track-check");
        check.set_halign(gtk::Align::Center);
        check.set_valign(gtk::Align::Center);
        check.set_hexpand(true);

        let list_item_for_toggle = list_item.clone();
        let store_for_toggle = store.clone();
        let runtime_for_toggle = runtime.clone();
        let button_for_toggle = import_button.clone();
        let suppress_for_toggle = suppress_setup.clone();
        check.connect_toggled(move |check| {
            if suppress_for_toggle.get() {
                return;
            }
            let _ = list_item_cd_row(&list_item_for_toggle, |row| {
                row.selected.set(check.is_active());
            });
            button_for_toggle.set_sensitive(import_allowed(&store_for_toggle, &runtime_for_toggle));
        });

        cell.append(&check);
        list_item.set_child(Some(&cell));
    });

    let suppress_bind = suppress;
    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item
            .child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        bind_cell_chrome(&cell, list_item);
        let Some(check) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::CheckButton>().ok())
        else {
            return;
        };
        let selected = list_item_cd_row(list_item, |row| row.selected.get()).unwrap_or(false);
        suppress_bind.set(true);
        check.set_active(selected);
        suppress_bind.set(false);
    });

    factory
}

fn build_cd_number_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = new_cell();
        install_cd_cell_selection_sync(list_item, &cell);
        let label = gtk::Label::new(None);
        label.add_css_class("dim-label");
        label.set_xalign(1.0);
        label.set_hexpand(true);
        label.set_valign(gtk::Align::Center);
        label.set_margin_start(8);
        label.set_margin_end(8);
        cell.append(&label);
        list_item.set_child(Some(&cell));
    });
    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item
            .child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        bind_cell_chrome(&cell, list_item);
        let Some(label) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        else {
            return;
        };
        let text =
            list_item_cd_row(list_item, |row| format!("{:02}", row.number)).unwrap_or_default();
        label.set_text(&text);
    });
    factory
}

fn build_cd_duration_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = new_cell();
        install_cd_cell_selection_sync(list_item, &cell);
        let label = gtk::Label::new(None);
        label.add_css_class("dim-label");
        label.set_xalign(1.0);
        label.set_hexpand(true);
        label.set_valign(gtk::Align::Center);
        label.set_margin_start(8);
        label.set_margin_end(8);
        cell.append(&label);
        list_item.set_child(Some(&cell));
    });
    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item
            .child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        bind_cell_chrome(&cell, list_item);
        let Some(label) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        else {
            return;
        };
        let text =
            list_item_cd_row(list_item, |row| format_duration(row.duration)).unwrap_or_default();
        label.set_text(&text);
    });
    factory
}

fn build_cd_text_factory(
    field: EditableField,
    bindings: CdTextBindings,
    inline_edit: InlineEditController,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    let bindings_setup = bindings.clone();
    let inline_setup = inline_edit.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = new_cell();
        install_cd_cell_selection_sync(list_item, &cell);

        let label = gtk::Label::new(None);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_hexpand(true);
        label.set_valign(gtk::Align::Center);
        label.set_margin_start(8);
        label.set_margin_end(8);
        label.set_xalign(0.0);
        cell.append(&label);
        list_item.set_child(Some(&cell));

        // Same inline-edit interaction as the Songs table: a click on an
        // editable cell of the already-selected row opens the entry.
        inline_setup.register_editable_cell(list_item, &cell, field);

        bindings_setup.0.borrow_mut().push(CdTextBinding {
            list_item: list_item.clone(),
            label,
            field,
        });
    });

    let bindings_teardown = bindings;
    factory.connect_teardown(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        bindings_teardown
            .0
            .borrow_mut()
            .retain(|binding| binding.list_item != *list_item);
    });

    // A cell about to be recycled to another row must not keep an open
    // editor: commit and close it first.
    let inline_unbind = inline_edit;
    factory.connect_unbind(move |_factory, item| {
        let Some(cell) = item
            .downcast_ref::<gtk::ListItem>()
            .and_then(|list_item| list_item.child())
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        inline_unbind.finish_if_editing_cell(&cell);
    });

    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item
            .child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        bind_cell_chrome(&cell, list_item);
        let Some(label) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        else {
            return;
        };
        let text = list_item_cd_row(list_item, |row| match field {
            EditableField::Artist => row.display_artist(),
            _ => row.display_title(),
        })
        .unwrap_or_default();
        label.set_text(&text);
    });

    factory
}

fn build_cd_filler_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let cell = new_cell();
        install_cd_cell_selection_sync(list_item, &cell);
        list_item.set_child(Some(&cell));
    });
    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item
            .child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        bind_cell_chrome(&cell, list_item);
    });
    factory
}

/// The (spinner, icon) of a status cell from its list item, if realized.
fn status_widgets_of(item: &glib::Object) -> Option<(gtk::Spinner, gtk::Image)> {
    let cell = item
        .downcast_ref::<gtk::ListItem>()?
        .child()?
        .downcast::<gtk::Box>()
        .ok()?;
    let spinner = cell.first_child()?.downcast::<gtk::Spinner>().ok()?;
    let icon = spinner.next_sibling()?.downcast::<gtk::Image>().ok()?;
    Some((spinner, icon))
}

fn render_cd_status(spinner: &gtk::Spinner, icon: &gtk::Image, status: CdRipStatus) {
    match status {
        CdRipStatus::Pending => {
            spinner.stop();
            spinner.set_visible(false);
            icon.set_visible(false);
        }
        CdRipStatus::Ripping => {
            icon.set_visible(false);
            spinner.set_visible(true);
            spinner.start();
        }
        CdRipStatus::Done => {
            spinner.stop();
            spinner.set_visible(false);
            icon.set_visible(true);
        }
    }
}

fn install_cd_cell_selection_sync(list_item: &gtk::ListItem, cell: &gtk::Box) {
    let cell_for_selection = cell.clone();
    list_item.connect_selected_notify(move |list_item| {
        sync_cd_selection_class(&cell_for_selection, list_item.is_selected());
    });
    sync_cd_selection_class(cell, list_item.is_selected());
}

fn bind_cell_chrome(cell: &gtk::Box, list_item: &gtk::ListItem) {
    apply_cd_row_tint(cell, list_item);
    sync_cd_selection_class(cell, list_item.is_selected());
}

fn apply_cd_row_tint(cell: &gtk::Box, list_item: &gtk::ListItem) {
    cell.remove_css_class("track-table-row-even");
    cell.remove_css_class("track-table-row-odd");
    if list_item.position() % 2 == 0 {
        cell.add_css_class("track-table-row-even");
    } else {
        cell.add_css_class("track-table-row-odd");
    }
}

fn sync_cd_selection_class(cell: &gtk::Box, selected: bool) {
    if selected {
        cell.add_css_class("track-table-row-selected");
    } else {
        cell.remove_css_class("track-table-row-selected");
    }
}

/// Borrow the [`CdRow`] behind a bound model object and run `f` over it.
/// `None` when the object is not a `CdRow` (it always is in this view).
fn with_cd_row<R>(object: &glib::Object, f: impl FnOnce(&CdRow) -> R) -> Option<R> {
    let boxed = object.downcast_ref::<glib::BoxedAnyObject>()?;
    let row = boxed.try_borrow::<CdRow>().ok()?;
    Some(f(&row))
}

/// Borrow the [`CdRow`] bound to a list item and run `f` over it.
fn list_item_cd_row<R>(list_item: &gtk::ListItem, f: impl FnOnce(&CdRow) -> R) -> Option<R> {
    let object = list_item.item()?;
    with_cd_row(&object, f)
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

    fn set_selected(panel: &CdImportPanel, index: u32, value: bool) {
        let object = panel.store.item(index).expect("row exists");
        with_cd_row(&object, |row| row.selected.set(value)).expect("CdRow");
    }

    fn row_status(panel: &CdImportPanel, number: u32) -> CdRipStatus {
        (0..panel.store.n_items())
            .find_map(|index| {
                let object = panel.store.item(index)?;
                with_cd_row(&object, |row| {
                    (row.number == number).then(|| row.status.get())
                })
                .flatten()
            })
            .expect("track present in the model")
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
            assert_eq!(panel.store.n_items(), 3);
            assert!(
                (0..panel.store.n_items()).all(|index| {
                    let object = panel.store.item(index).expect("row");
                    with_cd_row(&object, |row| row.selected.get()).unwrap_or(false)
                }),
                "every track is ticked on show"
            );
            assert!(panel.panel_ready_for_import());

            // Untick every row → not import-ready.
            for index in 0..panel.store.n_items() {
                set_selected(&panel, index, false);
            }
            assert!(!panel.panel_ready_for_import());

            // Re-tick one → ready again.
            set_selected(&panel, 0, true);
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
    fn import_progress_marks_done_ripping_and_pending_rows() {
        let ran = crate::test_support::with_gtk(|| {
            let panel = panel();
            panel.show_disc(snapshot("disc-a", 3));
            panel.begin_import_display(vec![1, 2, 3]);
            assert_eq!(row_status(&panel, 1), CdRipStatus::Pending);

            // One track done, the second ripping.
            panel.apply_import_progress(1, Some(2));
            assert_eq!(row_status(&panel, 1), CdRipStatus::Done);
            assert_eq!(row_status(&panel, 2), CdRipStatus::Ripping);
            assert_eq!(row_status(&panel, 3), CdRipStatus::Pending);

            // The run finished having imported two of the three.
            panel.finish_import_display(2);
            assert_eq!(row_status(&panel, 1), CdRipStatus::Done);
            assert_eq!(row_status(&panel, 2), CdRipStatus::Done);
            assert_eq!(row_status(&panel, 3), CdRipStatus::Pending);
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
