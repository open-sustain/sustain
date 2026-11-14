// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Inline cell editing for the Songs track table.
//!
//! A single click on an editable cell of the *already-selected* row opens
//! a small [`gtk::Entry`] seeded with the field's current value. Enter
//! commits, Escape cancels, Tab / Shift+Tab commit and hop to the
//! adjacent editable cell in the same row, and clicking away commits.
//! Commits travel the exact same write path as the File Info dialog — the
//! [`crate::metadata_diff`] helpers turn the typed string into a
//! [`MetadataChange`] which is dispatched as
//! [`sustain_app_runtime::ApplicationCommand::UpdateMetadata`], so SQLite
//! stays authoritative and the file tags are mirrored as a courtesy.
//!
//! ## Distinguishing the edit click from a play double-click
//!
//! A double-click plays the row (GTK's `ColumnView::activate`), so the
//! edit gesture must not fire on the first press of a double-click. The
//! only signal that separates "lone single click" from "first of a
//! double-click" is time, so the edit is *armed* on the single release
//! and only opens one double-click interval later; the second press of a
//! double-click cancels the armed open before it fires and lets playback
//! proceed. This is the standard desktop rename-vs-open disambiguation,
//! driven by the system's own `gtk-double-click-time`.

use std::{cell::RefCell, collections::VecDeque, rc::Rc, time::Duration};

use gtk::prelude::*;
use gtk::{gdk, glib, graphene};
use sustain_app_runtime::{MetadataChange, TrackMetadata};

use crate::cell_registry::{BindingRegistry, CellBinding};
use crate::metadata_diff::{bpm_diff, key_diff, number_diff, signed_number_diff, text_diff};
use crate::table_cell::TableCell;

/// Fallback double-click interval when the display has no settings object.
const DEFAULT_DOUBLE_CLICK_MS: i32 = 400;
/// Slack added to the double-click interval before an armed edit opens, so
/// the second press of a double-click (which lands *after* the first
/// release) always arrives in time to cancel the armed open.
const OPEN_DELAY_MARGIN_MS: u64 = 60;

/// A track-metadata field that can be edited directly in the table. Each
/// maps one column to one [`TrackMetadata`] field and knows how to read
/// its current value and diff a freshly-typed string against it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EditableField {
    Title,
    Artist,
    Album,
    Genre,
    Year,
    Bpm,
    Key,
    TrackNumber,
}

impl EditableField {
    /// The field's current value rendered as the text to seed the editor
    /// with. Read from the authoritative metadata rather than the table
    /// row so Title is seeded with the real (possibly empty) tag instead
    /// of the file-stem fallback the row column displays.
    pub(crate) fn seed_value(self, metadata: &TrackMetadata) -> String {
        match self {
            Self::Title => metadata.title.clone().unwrap_or_default(),
            Self::Artist => metadata.artist.clone().unwrap_or_default(),
            Self::Album => metadata.album.clone().unwrap_or_default(),
            Self::Genre => metadata.genre.clone().unwrap_or_default(),
            Self::Year => optional_to_string(metadata.year),
            Self::Bpm => optional_to_string(metadata.bpm),
            Self::Key => optional_to_string(metadata.key),
            Self::TrackNumber => optional_to_string(metadata.track_number),
        }
    }

    /// Build the [`MetadataChange`] for committing `new_text` against the
    /// track's current `initial` metadata. Uses the shared diff helpers so
    /// the result is byte-for-byte what the File Info dialog would produce
    /// for the same input (unchanged stays unchanged, emptied clears the
    /// tag, unparsable numbers are left unchanged).
    pub(crate) fn metadata_change(self, initial: &TrackMetadata, new_text: &str) -> MetadataChange {
        let mut change = MetadataChange::default();
        match self {
            Self::Title => change.title = text_diff(initial.title.as_deref(), new_text),
            Self::Artist => change.artist = text_diff(initial.artist.as_deref(), new_text),
            Self::Album => change.album = text_diff(initial.album.as_deref(), new_text),
            Self::Genre => change.genre = text_diff(initial.genre.as_deref(), new_text),
            Self::Year => change.year = signed_number_diff(initial.year, new_text),
            Self::Bpm => change.bpm = bpm_diff(initial.bpm, new_text),
            Self::Key => change.key = key_diff(initial.key, new_text),
            Self::TrackNumber => change.track_number = number_diff(initial.track_number, new_text),
        }
        change
    }
}

fn optional_to_string<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

/// An edit session on one row, bound to that row when the editor opens.
///
/// The commit closes over the row's identity (a library row's `TrackId`, a
/// CD-import row's own object), never over the cell's list item: a table may
/// move another row under an open editor (a re-sort, a search swapping
/// payloads in place), and the typed value must still land on the row it
/// was typed for.
pub(crate) struct EditSession {
    /// The field's current value, to seed the editor with.
    pub(crate) seed: String,
    /// Writes the typed value to the row the session was opened on.
    pub(crate) commit: Box<dyn FnOnce(String)>,
}

/// Opens an edit session for `field` on the row's bound model object, or
/// returns `None` if the row cannot be edited. The controller is
/// table-agnostic: it hands over the `ListItem`'s item and the callback
/// downcasts it to its own row type. Tables that do not opt in to inline
/// editing pass `None` instead of a callback.
pub(crate) type BeginEditCallback = Rc<dyn Fn(&glib::Object, EditableField) -> Option<EditSession>>;

/// What to do with the in-progress edit when it ends.
#[derive(Clone, Copy, Eq, PartialEq)]
enum FinishMode {
    Commit,
    Cancel,
}

/// The currently open edit session. Holds strong references to the cell
/// widgets for the (short) duration of the edit.
struct ActiveEdit {
    field: EditableField,
    commit: Box<dyn FnOnce(String)>,
    list_item: gtk::ListItem,
    cell: TableCell,
    entry: gtk::Entry,
}

/// A committed value waiting for its write.
struct PendingCommit {
    commit: Box<dyn FnOnce(String)>,
    text: String,
}

/// Where an ended edit session was open.
struct FinishedEdit {
    field: EditableField,
    list_item: gtk::ListItem,
}

/// A realized editable cell, used to find the row's other editable cells
/// when Tab hops between them. Pruned lazily when its widgets die.
struct EditableCellEntry {
    list_item: glib::WeakRef<gtk::ListItem>,
    cell: glib::WeakRef<TableCell>,
    field: EditableField,
}

impl CellBinding for EditableCellEntry {
    type Live = (gtk::ListItem, TableCell);

    fn upgrade(&self) -> Option<Self::Live> {
        Some((self.list_item.upgrade()?, self.cell.upgrade()?))
    }
}

/// The editable cell a press on an already-selected row landed on.
struct PressedCell {
    list_item: glib::WeakRef<gtk::ListItem>,
    cell: glib::WeakRef<TableCell>,
    field: EditableField,
}

/// An editable cell at a known horizontal offset, used to order a row's
/// editable cells left-to-right for Tab navigation (so the order follows
/// the user's current column arrangement and skips hidden columns).
struct OrderedCell {
    field: EditableField,
    list_item: gtk::ListItem,
    cell: TableCell,
    x: f32,
}

/// Coordinates inline editing across every editable cell of one table.
/// Cheaply cloneable — every field is shared state behind an `Rc`.
///
/// The controller is table-agnostic: it identifies a row by the
/// `gtk::ListItem`'s bound model object (recycle-safe by object identity)
/// and leaves interpreting that object to the [`BeginEditCallback`]. The same
/// machinery therefore drives the Songs/Playlist tables and the CD-import
/// table, so the macOS-style "click an already-selected cell to edit"
/// interaction stays identical everywhere.
#[derive(Clone)]
pub(crate) struct InlineEditController {
    begin_edit: BeginEditCallback,
    active: Rc<RefCell<Option<ActiveEdit>>>,
    pending_commits: Rc<RefCell<VecDeque<PendingCommit>>>,
    cells: BindingRegistry<EditableCellEntry>,
    pending_open: Rc<RefCell<Option<glib::SourceId>>>,
    double_click_ms: i32,
}

impl InlineEditController {
    /// The inline editing of `table`, whose editable cells register with
    /// [`Self::register_editable_cell`].
    pub(crate) fn new(table: &gtk::ColumnView, begin_edit: BeginEditCallback) -> Self {
        let double_click_ms = gtk::Settings::default()
            .map(|settings| settings.gtk_double_click_time())
            .unwrap_or(DEFAULT_DOUBLE_CLICK_MS);
        let controller = Self {
            begin_edit,
            active: Rc::new(RefCell::new(None)),
            pending_commits: Rc::default(),
            cells: BindingRegistry::default(),
            pending_open: Rc::new(RefCell::new(None)),
            double_click_ms,
        };
        controller.install_click_gesture(table);
        controller
    }

    /// Register a realized editable cell. Called once per cell widget at
    /// factory setup time. There is no unregister: the registry prunes
    /// dead entries lazily (see `cell_registry`), and an open editor on a
    /// cell leaving service is committed by the factory's unbind hook.
    pub(crate) fn register_editable_cell(
        &self,
        list_item: &gtk::ListItem,
        cell: &TableCell,
        field: EditableField,
    ) {
        self.cells.push(EditableCellEntry {
            list_item: list_item.downgrade(),
            cell: cell.downgrade(),
            field,
        });
    }

    /// The registered editable cell `cell`, with its row and field.
    fn editable_cell(&self, cell: &TableCell) -> Option<(gtk::ListItem, EditableField)> {
        self.cells.find_map_live(|entry, (list_item, registered)| {
            (registered == *cell).then_some((list_item, entry.field))
        })
    }

    fn install_click_gesture(&self, table: &gtk::ColumnView) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_PRIMARY);
        // Capture phase: the table sees the press before the row it lands
        // on, which selects itself only on release. Reading `is_selected()`
        // on press therefore reports whether the row was already selected
        // *before* this click — the signal the edit-start rule needs. The
        // gesture never claims, so selection and double-click activation
        // are untouched; a widget inside the cell that claims the press
        // (the open editor, a drag) cancels it before its release.
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);

        // The editable cell of an already-selected row a plain press landed
        // on, which its release arms an edit on.
        let pressed: Rc<RefCell<Option<PressedCell>>> = Rc::default();

        let pressed_on_press = pressed.clone();
        let controller_press = self.clone();
        gesture.connect_pressed(move |gesture, n_press, x, y| {
            pressed_on_press.borrow_mut().take();
            if n_press >= 2 {
                // Second press of a double-click: drop the armed edit and
                // let GTK activate the row (play).
                controller_press.cancel_pending();
                return;
            }
            let modifiers = gesture.current_event_state();
            if modifiers.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK)
            {
                return;
            }
            let Some(cell) = gesture
                .widget()
                .and_then(|table| TableCell::at(&table, x, y))
            else {
                return;
            };
            let Some((list_item, field)) = controller_press.editable_cell(&cell) else {
                return;
            };
            if list_item.is_selected() {
                *pressed_on_press.borrow_mut() = Some(PressedCell {
                    list_item: list_item.downgrade(),
                    cell: cell.downgrade(),
                    field,
                });
            }
        });

        let controller_release = self.clone();
        gesture.connect_released(move |_gesture, n_press, _x, _y| {
            let Some(pressed) = pressed.borrow_mut().take() else {
                return;
            };
            if n_press != 1 {
                return;
            }
            let (Some(list_item), Some(cell)) =
                (pressed.list_item.upgrade(), pressed.cell.upgrade())
            else {
                return;
            };
            controller_release.arm_open(&list_item, &cell, pressed.field);
        });

        table.add_controller(gesture);
    }

    /// Arm an edit to open after one double-click interval, cancelling any
    /// previously armed open. See the module docs for why the delay is
    /// required.
    fn arm_open(&self, list_item: &gtk::ListItem, cell: &TableCell, field: EditableField) {
        self.cancel_pending();

        // The row's bound item at arm time. GTK reuses `ListItem` slots and
        // cell widgets across rows, so by the time the timer fires this
        // slot may have been recycled to a different (still-selected) row
        // by a scroll. Re-checking the bound item by object identity then
        // keeps the editor from opening on the wrong row.
        let Some(armed_item) = list_item.item() else {
            return;
        };

        let controller = self.clone();
        let list_item = list_item.downgrade();
        let cell = cell.downgrade();
        let delay = self.double_click_ms.max(0) as u64 + OPEN_DELAY_MARGIN_MS;
        let source = glib::timeout_add_local_once(Duration::from_millis(delay), move || {
            controller.pending_open.borrow_mut().take();
            let (Some(list_item), Some(cell)) = (list_item.upgrade(), cell.upgrade()) else {
                return;
            };
            // The row must still be selected and still be the same item
            // when the timer fires; otherwise the user clicked elsewhere or
            // scrolled the slot onto a different row in the meantime.
            if !list_item.is_selected() || list_item.item().as_ref() != Some(&armed_item) {
                return;
            }
            // The click focused the table. If focus has left it since, the
            // user moved on (a shortcut opened the sidebar's rename entry,
            // a click reached the search field) and the editor would pull
            // focus, and their typing, back from there.
            if !focus_within_table(&cell) {
                return;
            }
            controller.open_edit(field, &cell, &list_item);
        });
        *self.pending_open.borrow_mut() = Some(source);
    }

    /// Commit and close the open edit if it belongs to `cell`. Called when
    /// a cell unbinds (scrolls off / is recycled) so an open editor is
    /// never left stranded in a cell about to be reused for another row.
    pub(crate) fn finish_if_editing_cell(&self, cell: &TableCell) {
        let editing_this = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|active| active.cell == *cell);
        if editing_this {
            self.finish_active(FinishMode::Commit);
        }
    }

    /// Commit and close any open edit and drop an armed one, ahead of the
    /// table swapping row payloads in place. A swap moves another row under
    /// the editor without unbinding its cell; the editor must not stay open
    /// over a row it was not opened on, and an armed open must not open on
    /// one.
    pub(crate) fn finish_before_payload_swap(&self) {
        self.cancel_pending();
        self.finish_active(FinishMode::Commit);
    }

    /// Commit any open edit and write every pending commit now. Call this
    /// from the window-close handler: the main loop that would run the
    /// writes stops with the window.
    pub(crate) fn flush(&self) {
        self.cancel_pending();
        self.finish_active(FinishMode::Commit);
        self.run_pending_commits();
    }

    fn cancel_pending(&self) {
        if let Some(source) = self.pending_open.borrow_mut().take() {
            source.remove();
        }
    }

    /// Open the editor on `cell` for `field`. Commits any other in-progress
    /// edit first, and is a no-op if this exact cell is already being
    /// edited.
    fn open_edit(&self, field: EditableField, cell: &TableCell, list_item: &gtk::ListItem) {
        let Some(item) = list_item.item() else {
            return;
        };
        if let Some(active) = self.active.borrow().as_ref()
            && active.field == field
            && active.list_item.item().as_ref() == Some(&item)
        {
            return;
        }
        self.finish_active(FinishMode::Commit);

        let Some(EditSession { seed, commit }) = (self.begin_edit)(&item, field) else {
            return;
        };

        let entry = gtk::Entry::new();
        entry.add_css_class("track-table-inline-edit");
        entry.set_text(&seed);
        gtk::prelude::EditableExt::set_alignment(&entry, cell.xalign());
        cell.append(&entry);

        // Install the entry's controllers — crucially the focus controller —
        // *before* the entry takes focus. `EventControllerFocus` only emits
        // `leave` for a focus transition it also saw enter on; grabbing focus
        // first would leave it initialised to "no focus", so the later leave
        // would be a no-op and the editor would never commit-and-close on a
        // click elsewhere.
        self.wire_entry(&entry);

        *self.active.borrow_mut() = Some(ActiveEdit {
            field,
            commit,
            list_item: list_item.clone(),
            cell: cell.clone(),
            entry: entry.clone(),
        });

        // Defer the focus grab to the next idle: a freshly-appended entry is
        // not mapped synchronously, so a same-tick `grab_focus` would lose the
        // race, and deferring also lets the focus controller (added above)
        // observe the enter transition. Mirrors the sidebar rename flow.
        glib::idle_add_local_once(move || {
            entry.grab_focus();
            entry.select_region(0, -1);
        });
    }

    fn wire_entry(&self, entry: &gtk::Entry) {
        let controller_activate = self.clone();
        entry.connect_activate(move |_| {
            controller_activate.finish_from_keyboard(FinishMode::Commit);
        });

        let key = gtk::EventControllerKey::new();
        let controller_key = self.clone();
        key.connect_key_pressed(move |_controller, keyval, _code, _modifiers| match keyval {
            gdk::Key::Escape => {
                controller_key.finish_from_keyboard(FinishMode::Cancel);
                glib::Propagation::Stop
            }
            gdk::Key::Tab | gdk::Key::KP_Tab => {
                controller_key.move_to_adjacent(true);
                glib::Propagation::Stop
            }
            gdk::Key::ISO_Left_Tab => {
                controller_key.move_to_adjacent(false);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        });
        entry.add_controller(key);

        // Focus leaving for any other reason (a click elsewhere) commits,
        // matching the dialog's "what you typed is kept" behaviour.
        //
        // The teardown is deferred to the next idle on purpose: removing the
        // entry from the cell *synchronously inside its own ::leave* would
        // unparent the widget GTK is mid-traversal over (the focus machinery
        // walks `get_parent` up from the widget losing focus), which corrupts
        // the walk and spams `gtk_widget_get_parent` criticals. By idle the
        // focus change has completed and the tree is safe to mutate.
        //
        // The deferred finish is scoped to *this* entry's session: a Tab hop
        // finishes synchronously and opens a new editor, and the old entry's
        // ::leave still fires — without the per-entry guard the idle would
        // then tear down the *new* session instead.
        let focus = gtk::EventControllerFocus::new();
        let controller_focus = self.clone();
        let entry_weak = entry.downgrade();
        focus.connect_leave(move |_| {
            let controller = controller_focus.clone();
            let entry_weak = entry_weak.clone();
            glib::idle_add_local_once(move || {
                controller.finish_active_if_entry(&entry_weak);
            });
        });
        entry.add_controller(focus);
    }

    /// Commit and close the active edit only if it is still the session for
    /// `entry`. Used by the deferred focus-leave teardown so a stale leave
    /// (e.g. from a Tab hop that already moved on) cannot close the wrong
    /// session.
    fn finish_active_if_entry(&self, entry: &glib::WeakRef<gtk::Entry>) {
        let is_current = match (self.active.borrow().as_ref(), entry.upgrade()) {
            (Some(active), Some(entry)) => active.entry == entry,
            _ => false,
        };
        if is_current {
            self.finish_active(FinishMode::Commit);
        }
    }

    /// End the active edit from the editor's own keys (Enter, Escape) and
    /// hand keyboard focus back to the edited row, so the arrow keys move on
    /// from it (#300). Removing the focused editor would otherwise leave
    /// focus on no widget at all. Other endings keep focus where it went: a
    /// click elsewhere moved it already, and Tab opens the next editor.
    ///
    /// The row is focused before the editor is removed, while the session
    /// still sits on the row it was opened on (a re-sort or payload swap
    /// ends the session first).
    fn finish_from_keyboard(&self, mode: FinishMode) {
        let row = self
            .active
            .borrow()
            .as_ref()
            .map(|active| (active.cell.clone(), active.list_item.position()));
        if let Some((cell, position)) = row
            && position != gtk::INVALID_LIST_POSITION
            && let Some(table) = cell
                .ancestor(gtk::ColumnView::static_type())
                .and_downcast::<gtk::ColumnView>()
        {
            table.scroll_to(position, None, gtk::ListScrollFlags::FOCUS, None);
        }
        self.finish_active(mode);
    }

    /// End the active edit (if any), restoring the cell's text and — for
    /// [`FinishMode::Commit`] — writing the typed value. Returns the ended
    /// session's cell so callers (Tab) can locate the row it belonged to.
    ///
    /// The write is queued and runs from the main loop (or [`Self::flush`]),
    /// never inside the call that ended the edit: an edit also ends inside
    /// model changes (a cell unbinding during a splice, a re-sort), whose
    /// callers may hold the runtime borrowed that the write goes through.
    /// The session is bound to its row, so the deferred write cannot land
    /// on another one.
    fn finish_active(&self, mode: FinishMode) -> Option<FinishedEdit> {
        let active = self.active.borrow_mut().take()?;
        active.cell.remove(&active.entry);
        if mode == FinishMode::Commit {
            self.pending_commits.borrow_mut().push_back(PendingCommit {
                commit: active.commit,
                text: active.entry.text().to_string(),
            });
            let controller = self.clone();
            glib::idle_add_local_once(move || controller.run_pending_commits());
        }
        Some(FinishedEdit {
            field: active.field,
            list_item: active.list_item,
        })
    }

    /// Write the queued commits, oldest first.
    fn run_pending_commits(&self) {
        loop {
            // Released before the write runs: a write may end another edit.
            let next = self.pending_commits.borrow_mut().pop_front();
            let Some(PendingCommit { commit, text }) = next else {
                break;
            };
            commit(text);
        }
    }

    /// Commit the current edit and open the next (Tab) or previous
    /// (Shift+Tab) editable cell in the same row, ordered by on-screen
    /// position. Stops at the row's ends.
    fn move_to_adjacent(&self, forward: bool) {
        let Some(finished) = self.finish_active(FinishMode::Commit) else {
            return;
        };
        let position = finished.list_item.position();
        if position == gtk::INVALID_LIST_POSITION {
            return;
        }
        let ordered = self.editable_cells_at(position);
        let Some(index) = ordered.iter().position(|cell| cell.field == finished.field) else {
            return;
        };
        let target = if forward {
            index.checked_add(1)
        } else {
            index.checked_sub(1)
        };
        let Some(next) = target.and_then(|index| ordered.get(index)) else {
            return;
        };
        self.open_edit(next.field, &next.cell, &next.list_item);
    }

    /// Every realized editable cell at `position`, ordered left-to-right by
    /// on-screen x. Hidden columns have no realized cell, so they fall out
    /// naturally.
    fn editable_cells_at(&self, position: u32) -> Vec<OrderedCell> {
        let mut ordered: Vec<OrderedCell> = Vec::new();
        self.cells.for_each_live(|entry, (list_item, cell)| {
            if list_item.position() != position {
                return;
            }
            let Some(x) = cell_origin_x(&cell) else {
                return;
            };
            ordered.push(OrderedCell {
                field: entry.field,
                list_item,
                cell,
                x,
            });
        });
        ordered.sort_by(|a, b| a.x.total_cmp(&b.x));
        ordered
    }
}

/// The cell's left edge in the toplevel's coordinate space, used only to
/// order sibling cells. `None` if the cell is not currently rooted/realized.
fn cell_origin_x(cell: &TableCell) -> Option<f32> {
    let root = cell.root()?;
    let point = cell.compute_point(
        root.upcast_ref::<gtk::Widget>(),
        &graphene::Point::new(0.0, 0.0),
    )?;
    Some(point.x())
}

/// Whether keyboard focus is inside the table that holds `cell`.
fn focus_within_table(cell: &TableCell) -> bool {
    let Some(table) = cell.ancestor(gtk::ColumnView::static_type()) else {
        return false;
    };
    cell.root()
        .and_then(|root| root.focus())
        .is_some_and(|focus| focus == table || focus.is_ancestor(&table))
}

#[cfg(test)]
mod tests {
    use super::super::TrackTableRow;
    use super::*;
    use sustain_app_runtime::{Bpm, TrackId};

    fn metadata() -> TrackMetadata {
        TrackMetadata {
            title: Some("Song".to_owned()),
            artist: Some("Band".to_owned()),
            year: Some(1998),
            bpm: Bpm::from_whole(120),
            track_number: Some(3),
            ..TrackMetadata::default()
        }
    }

    #[test]
    fn seed_value_reads_the_current_field() {
        let metadata = metadata();
        assert_eq!(EditableField::Title.seed_value(&metadata), "Song");
        assert_eq!(EditableField::Year.seed_value(&metadata), "1998");
        assert_eq!(EditableField::Bpm.seed_value(&metadata), "120");
        assert_eq!(EditableField::TrackNumber.seed_value(&metadata), "3");
        assert_eq!(EditableField::Album.seed_value(&metadata), "");
    }

    #[test]
    fn metadata_change_matches_the_dialog_diff_rules() {
        let metadata = metadata();
        assert_eq!(
            EditableField::Title.metadata_change(&metadata, "Song"),
            MetadataChange::default(),
            "re-typing the same value is a no-op"
        );
        assert_eq!(
            EditableField::Title.metadata_change(&metadata, "New").title,
            sustain_app_runtime::FieldChange::Set("New".to_owned())
        );
        assert_eq!(
            EditableField::Artist
                .metadata_change(&metadata, "   ")
                .artist,
            sustain_app_runtime::FieldChange::Clear,
            "emptying a populated field clears the tag"
        );
        assert_eq!(
            EditableField::Year.metadata_change(&metadata, "abc"),
            MetadataChange::default(),
            "an unparsable number leaves the field unchanged"
        );
        assert_eq!(
            EditableField::Year.metadata_change(&metadata, "2001").year,
            sustain_app_runtime::FieldChange::Set(2001)
        );
    }

    /// An armed edit opens only while focus stays in the table: once the
    /// user has moved focus elsewhere, their typing goes there.
    #[test]
    fn an_armed_edit_opens_only_while_focus_stays_in_the_table() {
        let ran = crate::test_support::with_gtk(|| {
            let begin_edit: BeginEditCallback = Rc::new(|_, _| {
                Some(EditSession {
                    seed: String::new(),
                    commit: Box::new(|_| {}),
                })
            });
            let table = super::super::build_track_table(
                vec![TrackTableRow::test_row(1)],
                None,
                None,
                None,
                None,
                Some(begin_edit),
            );
            let elsewhere = gtk::Entry::new();
            let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
            content.append(&table.widget());
            content.append(&elsewhere);
            let window = gtk::Window::new();
            window.set_default_size(700, 260);
            window.set_child(Some(&content));
            window.present();
            let context = glib::MainContext::default();
            for _ in 0..200 {
                if !context.iteration(false) {
                    break;
                }
            }
            table.selection.select_item(0, true);
            let controller = table.inline_edit.clone().expect("inline editing table");
            let (list_item, cell) = controller
                .cells
                .find_map_live(|entry, (list_item, cell)| {
                    (entry.field == EditableField::Title && list_item.position() == 0)
                        .then_some((list_item, cell))
                })
                .expect("a realized title cell on the first row");
            let opens_after_arming = |focus: &dyn Fn()| {
                // The click that arms an edit focuses its row.
                table
                    .table
                    .scroll_to(0, None, gtk::ListScrollFlags::FOCUS, None);
                controller.arm_open(&list_item, &cell, EditableField::Title);
                focus();
                while controller.pending_open.borrow().is_some() {
                    context.iteration(true);
                }
                let opened = controller.active.borrow().is_some();
                controller.finish_active(FinishMode::Cancel);
                opened
            };

            assert!(opens_after_arming(&|| {}));
            assert!(!opens_after_arming(&|| {
                elsewhere.grab_focus();
            }));
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    /// Enter and Escape hand focus back to the table, so the arrow keys keep
    /// moving through the rows after an inline edit (#300).
    #[test]
    fn ending_an_edit_from_the_keyboard_returns_focus_to_the_table() {
        let ran = crate::test_support::with_gtk(|| {
            let begin_edit: BeginEditCallback = Rc::new(|_, _| {
                Some(EditSession {
                    seed: String::new(),
                    commit: Box::new(|_| {}),
                })
            });
            let table = super::super::build_track_table(
                vec![TrackTableRow::test_row(1), TrackTableRow::test_row(2)],
                None,
                None,
                None,
                None,
                Some(begin_edit),
            );
            let window = gtk::Window::new();
            window.set_default_size(700, 260);
            window.set_child(Some(&table.widget()));
            window.present();
            let context = glib::MainContext::default();
            let settle = || {
                for _ in 0..200 {
                    if !context.iteration(false) {
                        break;
                    }
                }
            };
            settle();
            let controller = table.inline_edit.clone().expect("inline editing table");
            let (list_item, cell) = controller
                .cells
                .find_map_live(|entry, (list_item, cell)| {
                    (entry.field == EditableField::Title && list_item.position() == 0)
                        .then_some((list_item, cell))
                })
                .expect("a realized title cell on the first row");
            let focus_in_table = || focus_within_table(&cell);

            for end in [
                &(|entry: &gtk::Entry| entry.emit_activate()) as &dyn Fn(&gtk::Entry),
                &|_entry: &gtk::Entry| controller.finish_from_keyboard(FinishMode::Cancel),
            ] {
                controller.open_edit(EditableField::Title, &cell, &list_item);
                settle();
                let entry = controller
                    .active
                    .borrow()
                    .as_ref()
                    .map(|active| active.entry.clone())
                    .expect("an open editor");
                assert!(
                    entry
                        .root()
                        .and_then(|root| root.focus())
                        .is_some_and(|focus| focus == entry || focus.is_ancestor(&entry))
                );
                end(&entry);
                settle();
                assert!(controller.active.borrow().is_none(), "the edit ended");
                assert!(focus_in_table(), "focus is back in the table");
            }
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    /// The track each commit was bound to, and the committed text.
    type Commits = Rc<RefCell<Vec<(Option<TrackId>, String)>>>;

    #[test]
    fn a_re_sort_commits_the_open_edit_to_the_track_it_was_opened_on() {
        let ran = crate::test_support::with_gtk(|| {
            let commits: Commits = Rc::default();
            let commits_for_hook = commits.clone();
            let begin_edit: BeginEditCallback = Rc::new(move |item, _field| {
                let track_id = item
                    .downcast_ref::<glib::BoxedAnyObject>()
                    .and_then(|object| object.try_borrow::<TrackTableRow>().ok()?.track_id);
                let commits = commits_for_hook.clone();
                Some(EditSession {
                    seed: String::new(),
                    commit: Box::new(move |text| commits.borrow_mut().push((track_id, text))),
                })
            });
            let mut first = TrackTableRow::test_row(1);
            first.track_name_sort_key = "b".to_owned();
            let mut second = TrackTableRow::test_row(2);
            second.track_name_sort_key = "a".to_owned();
            let table = super::super::build_track_table(
                vec![first, second],
                None,
                None,
                None,
                None,
                Some(begin_edit),
            );
            let window = gtk::Window::new();
            window.set_default_size(700, 260);
            window.set_child(Some(&table.widget()));
            window.present();
            let context = glib::MainContext::default();
            for _ in 0..200 {
                if !context.iteration(false) {
                    break;
                }
            }

            // Open a title editor on track 1, the first row.
            let controller = table.inline_edit.clone().expect("inline editing table");
            let (list_item, cell) = controller
                .cells
                .find_map_live(|entry, (list_item, cell)| {
                    (entry.field == EditableField::Title && list_item.position() == 0)
                        .then_some((list_item, cell))
                })
                .expect("a realized title cell on the first row");
            controller.open_edit(EditableField::Title, &cell, &list_item);
            let entry = controller
                .active
                .borrow()
                .as_ref()
                .map(|active| active.entry.clone())
                .expect("an open editor");
            entry.set_text("Renamed");

            // Sorting by name moves track 2 into the edited row.
            let mut layout =
                super::super::read_current_layout(&table.table, &table.managed_columns);
            layout.sort = Some(sustain_app_runtime::TrackColumnSort {
                column_id: "track_name".to_owned(),
                ascending: true,
            });
            table.apply_layout(&layout);
            assert!(
                commits.borrow().is_empty(),
                "the write waits for the main loop, outside the re-sort"
            );
            controller.flush();

            assert_eq!(
                commits.borrow().as_slice(),
                [(TrackId::new(1), "Renamed".to_owned())],
                "the edit lands on the track it was opened on"
            );
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
