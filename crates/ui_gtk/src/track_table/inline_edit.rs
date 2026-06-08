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

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::prelude::*;
use gtk::{gdk, glib, graphene};
use sustain_app_runtime::{MetadataChange, TrackMetadata};

use crate::metadata_diff::{number_diff, signed_number_diff, text_diff};

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
            Self::Key => metadata.key.clone().unwrap_or_default(),
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
            Self::Bpm => change.bpm = number_diff(initial.bpm, new_text),
            Self::Key => change.key = text_diff(initial.key.as_deref(), new_text),
            Self::TrackNumber => change.track_number = number_diff(initial.track_number, new_text),
        }
        change
    }
}

fn optional_to_string<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

/// Reads the current value of an editable field for the row's bound model
/// object, returning `None` if it cannot be resolved. The controller is
/// table-agnostic: it hands back the `ListItem`'s item and the caller
/// downcasts it to its own row type — a library row keyed by `TrackId`, a
/// CD-import row keyed by track number, and so on.
pub(crate) type CellEditSeedCallback = Rc<dyn Fn(&glib::Object, EditableField) -> Option<String>>;
/// Commits an edited field for the row's bound model object. Returns `true`
/// when the editor should close (the value was written, or it was a no-op),
/// `false` to keep it open (a dispatch failure).
pub(crate) type CellEditCommitCallback = Rc<dyn Fn(&glib::Object, EditableField, String) -> bool>;

/// The two callbacks a table needs to support inline editing. Tables that
/// do not opt in pass `None`.
#[derive(Clone)]
pub(crate) struct InlineEditHooks {
    pub(crate) seed: CellEditSeedCallback,
    pub(crate) commit: CellEditCommitCallback,
}

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
    list_item: gtk::ListItem,
    cell: gtk::Box,
    label: gtk::Label,
    entry: gtk::Entry,
}

/// A realized editable cell, used to find the row's other editable cells
/// when Tab hops between them. Pruned lazily when its widgets die.
struct EditableCellEntry {
    list_item: glib::WeakRef<gtk::ListItem>,
    cell: glib::WeakRef<gtk::Box>,
    field: EditableField,
}

/// An editable cell at a known horizontal offset, used to order a row's
/// editable cells left-to-right for Tab navigation (so the order follows
/// the user's current column arrangement and skips hidden columns).
struct OrderedCell {
    field: EditableField,
    list_item: gtk::ListItem,
    cell: gtk::Box,
    x: f32,
}

/// Coordinates inline editing across every editable cell of one table.
/// Cheaply cloneable — every field is shared state behind an `Rc`.
///
/// The controller is table-agnostic: it identifies a row by the
/// `gtk::ListItem`'s bound model object (recycle-safe by object identity)
/// and leaves interpreting that object to the [`InlineEditHooks`]. The same
/// machinery therefore drives the Songs/Playlist tables and the CD-import
/// table, so the macOS-style "click an already-selected cell to edit"
/// interaction stays identical everywhere.
#[derive(Clone)]
pub(crate) struct InlineEditController {
    hooks: InlineEditHooks,
    active: Rc<RefCell<Option<ActiveEdit>>>,
    cells: Rc<RefCell<Vec<EditableCellEntry>>>,
    pending_open: Rc<RefCell<Option<glib::SourceId>>>,
    double_click_ms: i32,
}

impl InlineEditController {
    pub(crate) fn new(hooks: InlineEditHooks) -> Self {
        let double_click_ms = gtk::Settings::default()
            .map(|settings| settings.gtk_double_click_time())
            .unwrap_or(DEFAULT_DOUBLE_CLICK_MS);
        Self {
            hooks,
            active: Rc::new(RefCell::new(None)),
            cells: Rc::new(RefCell::new(Vec::new())),
            pending_open: Rc::new(RefCell::new(None)),
            double_click_ms,
        }
    }

    /// Register a realized editable cell and install its click gesture.
    /// Called once per cell widget at factory setup time.
    pub(crate) fn register_editable_cell(
        &self,
        list_item: &gtk::ListItem,
        cell: &gtk::Box,
        field: EditableField,
    ) {
        self.cells.borrow_mut().push(EditableCellEntry {
            list_item: list_item.downgrade(),
            cell: cell.downgrade(),
            field,
        });
        self.install_click_gesture(list_item, cell, field);
    }

    pub(crate) fn unregister_editable_cell(&self, list_item: &gtk::ListItem) {
        let editing_this_item = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|active| active.list_item == *list_item);
        if editing_this_item {
            self.finish_active(FinishMode::Commit);
        }

        self.cells.borrow_mut().retain(|entry| {
            entry.cell.upgrade().is_some()
                && entry
                    .list_item
                    .upgrade()
                    .is_some_and(|registered| registered != *list_item)
        });
    }

    fn install_click_gesture(
        &self,
        list_item: &gtk::ListItem,
        cell: &gtk::Box,
        field: EditableField,
    ) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_PRIMARY);
        // Default (bubble) phase. The cell sits *inside* GTK's per-row
        // selection widget, and bubble events travel leaf→root, so this
        // descendant handler runs before the row selects itself. Reading
        // `is_selected()` in `pressed` therefore reports whether the row
        // was already selected *before* this click — the signal the
        // edit-start rule needs. We never claim the gesture, so selection
        // and double-click activation are untouched.
        let was_already_selected = Rc::new(Cell::new(false));

        let was_selected_press = was_already_selected.clone();
        let list_item_press = list_item.downgrade();
        let controller_press = self.clone();
        gesture.connect_pressed(move |gesture, n_press, _x, _y| {
            if n_press >= 2 {
                // Second press of a double-click: drop the armed edit and
                // let GTK activate the row (play).
                controller_press.cancel_pending();
                was_selected_press.set(false);
                return;
            }
            let modifiers = gesture.current_event_state();
            let plain = !modifiers.contains(gdk::ModifierType::CONTROL_MASK)
                && !modifiers.contains(gdk::ModifierType::SHIFT_MASK);
            let already_selected = list_item_press
                .upgrade()
                .is_some_and(|item| item.is_selected());
            was_selected_press.set(plain && already_selected);
        });

        let was_selected_release = was_already_selected;
        let list_item_release = list_item.downgrade();
        let cell_release = cell.downgrade();
        let controller_release = self.clone();
        gesture.connect_released(move |_gesture, n_press, _x, _y| {
            if n_press != 1 || !was_selected_release.get() {
                return;
            }
            let (Some(list_item), Some(cell)) =
                (list_item_release.upgrade(), cell_release.upgrade())
            else {
                return;
            };
            controller_release.arm_open(&list_item, &cell, field);
        });

        cell.add_controller(gesture);
    }

    /// Arm an edit to open after one double-click interval, cancelling any
    /// previously armed open. See the module docs for why the delay is
    /// required.
    fn arm_open(&self, list_item: &gtk::ListItem, cell: &gtk::Box, field: EditableField) {
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
            controller.open_edit(field, &cell, &list_item);
        });
        *self.pending_open.borrow_mut() = Some(source);
    }

    /// Commit and close the open edit if it belongs to `cell`. Called when
    /// a cell unbinds (scrolls off / is recycled) so an open editor is
    /// never left stranded in a cell about to be reused for another row.
    pub(crate) fn finish_if_editing_cell(&self, cell: &gtk::Box) {
        let editing_this = self
            .active
            .borrow()
            .as_ref()
            .is_some_and(|active| active.cell == *cell);
        if editing_this {
            self.finish_active(FinishMode::Commit);
        }
    }

    fn cancel_pending(&self) {
        if let Some(source) = self.pending_open.borrow_mut().take() {
            source.remove();
        }
    }

    /// Open the editor on `cell` for `field`. Commits any other in-progress
    /// edit first, and is a no-op if this exact cell is already being
    /// edited.
    fn open_edit(&self, field: EditableField, cell: &gtk::Box, list_item: &gtk::ListItem) {
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

        let Some(seed) = (self.hooks.seed)(&item, field) else {
            return;
        };
        let Some(label) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        else {
            return;
        };

        let entry = gtk::Entry::new();
        entry.add_css_class("track-table-inline-edit");
        entry.set_hexpand(true);
        entry.set_text(&seed);
        gtk::prelude::EditableExt::set_alignment(&entry, label.xalign());

        label.set_visible(false);
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
            list_item: list_item.clone(),
            cell: cell.clone(),
            label,
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
            controller_activate.finish_active(FinishMode::Commit);
        });

        let key = gtk::EventControllerKey::new();
        let controller_key = self.clone();
        key.connect_key_pressed(move |_controller, keyval, _code, _modifiers| match keyval {
            gdk::Key::Escape => {
                controller_key.finish_active(FinishMode::Cancel);
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

    /// End the active edit (if any), restoring the cell's label and — for
    /// [`FinishMode::Commit`] — writing the typed value. Returns the ended
    /// session so callers (Tab) can locate the row it belonged to.
    fn finish_active(&self, mode: FinishMode) -> Option<ActiveEdit> {
        let active = self.active.borrow_mut().take()?;
        if active.entry.parent().is_some() {
            active.cell.remove(&active.entry);
        }
        active.label.set_visible(true);
        if mode == FinishMode::Commit
            && let Some(item) = active.list_item.item()
        {
            let text = active.entry.text().to_string();
            (self.hooks.commit)(&item, active.field, text);
        }
        Some(active)
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
        let mut cells = self.cells.borrow_mut();
        cells.retain(|entry| entry.list_item.upgrade().is_some() && entry.cell.upgrade().is_some());
        let mut ordered: Vec<OrderedCell> = Vec::new();
        for entry in cells.iter() {
            let (Some(list_item), Some(cell)) = (entry.list_item.upgrade(), entry.cell.upgrade())
            else {
                continue;
            };
            if list_item.position() != position {
                continue;
            }
            let Some(x) = cell_origin_x(&cell) else {
                continue;
            };
            ordered.push(OrderedCell {
                field: entry.field,
                list_item,
                cell,
                x,
            });
        }
        ordered.sort_by(|a, b| a.x.total_cmp(&b.x));
        ordered
    }
}

/// The cell's left edge in the toplevel's coordinate space, used only to
/// order sibling cells. `None` if the cell is not currently rooted/realized.
fn cell_origin_x(cell: &gtk::Box) -> Option<f32> {
    let root = cell.root()?;
    let point = cell.compute_point(
        root.upcast_ref::<gtk::Widget>(),
        &graphene::Point::new(0.0, 0.0),
    )?;
    Some(point.x())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata() -> TrackMetadata {
        TrackMetadata {
            title: Some("Song".to_owned()),
            artist: Some("Band".to_owned()),
            year: Some(1998),
            bpm: Some(120),
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
}
