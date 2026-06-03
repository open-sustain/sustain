// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::glib::variant::ToVariant;
use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering as CmpOrdering;
use std::collections::HashSet;
use std::rc::Rc;
use sustain_app_runtime::{Rating, TrackColumnEntry, TrackColumnLayout, TrackId};

use super::track_context::TrackRowContextMenu;
use cells::{
    RatingBindings, StatusBindings, TextBindings, TrackTableContextMenu, build_filler_column,
    build_rating_cell_factory, build_status_column, build_text_cell_factory,
};
use columns::{TRACK_TABLE_COLUMNS, TrackTableColumn};
use drag_drop::{RowDropCellRegistry, RowReorderHooks};
use empty_row_painter::EmptyRowPainter;
use inline_edit::InlineEditController;
pub(crate) use inline_edit::{EditableField, InlineEditHooks};
pub(crate) use row::TrackTableRow;

mod cells;
mod columns;
mod drag_drop;
mod empty_row_painter;
mod inline_edit;
mod row;

pub(crate) type TrackActivatedCallback = Rc<dyn Fn(TrackId)>;
pub(crate) type RatingChangedCallback = Rc<dyn Fn(TrackId, Rating) -> bool>;
pub(crate) type LayoutChangedCallback = Rc<dyn Fn(TrackColumnLayout)>;

/// Outcome handler for a within-table row drop. Wired only on tables that
/// own an authoritative row order — currently just the playlist track table.
/// Returns `true` when the drop was accepted and dispatched, `false` to
/// reject (so GTK reports the drop as failed and the source row stays put).
pub(crate) type RowReorderCallback = Rc<dyn Fn(RowReorderDrop) -> bool>;

/// Per-drop information delivered to a [`RowReorderCallback`].
#[derive(Clone, Debug)]
pub(crate) struct RowReorderDrop {
    /// The track ids that came in on the drag payload, in payload order
    /// (which is the source row's display order — not necessarily the
    /// playlist's logical order).
    pub dragged_track_ids: Vec<TrackId>,
    /// The track id of the row the drop landed on.
    pub target_track_id: TrackId,
    /// Whether the drop landed in the top or bottom half of the target row.
    pub position: RowDropPosition,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RowDropPosition {
    Above,
    Below,
}

/// A track column that participates in the persisted layout. Status and
/// filler columns are intentionally structural — they never appear in a
/// [`TrackColumnLayout`] and never move.
#[derive(Clone)]
struct ManagedColumn {
    column_id: &'static str,
    column: gtk::ColumnViewColumn,
}

#[derive(Clone)]
pub(crate) struct TrackTable {
    painter: EmptyRowPainter,
    table: gtk::ColumnView,
    store: gio::ListStore,
    selection: gtk::MultiSelection,
    playing_track_id: Rc<Cell<Option<TrackId>>>,
    status_bindings: StatusBindings,
    text_bindings: TextBindings,
    rating_bindings: RatingBindings,
    status_column: gtk::ColumnViewColumn,
    managed_columns: Rc<Vec<ManagedColumn>>,
    applying_layout: Rc<Cell<bool>>,
    layout_changed: Rc<RefCell<Option<LayoutChangedCallback>>>,
    pending_save: Rc<RefCell<Option<glib::SourceId>>>,
    row_replacement_generation: Rc<Cell<u64>>,
}

/// Debounce window for coalescing column-layout changes into a single save.
///
/// `notify::fixed-width` fires repeatedly while the user drags a column
/// boundary, so we must NOT write to SQLite on every property tick. 250 ms
/// is long enough to swallow a continuous drag (motion events keep the timer
/// resetting) yet short enough that a single visibility toggle feels
/// instantaneous and a pending save survives realistic close-window races
/// when [`TrackTable::flush_pending_layout_save`] is invoked on shutdown.
const LAYOUT_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

impl TrackTable {
    pub(crate) fn widget(&self) -> gtk::Widget {
        self.painter.clone().upcast()
    }

    pub(crate) fn replace_rows(&self, rows: Vec<TrackTableRow>) {
        self.bump_row_replacement_generation();
        // One `splice` instead of `remove_all` + N `append`s. Each append
        // fires its own `items-changed`, which the wrapping `SortListModel`
        // answers with a binary-insert-and-shift (O(n)) and the
        // `MultiSelection` with its own bookkeeping — so a per-row rebuild is
        // O(n²) and visibly stalls for seconds on a 10k library or a large
        // playlist (#157). `splice` removes every old row and inserts every
        // new one under a single `items-changed`, so the sorter re-sorts and
        // the selection reconciles exactly once.
        let additions: Vec<glib::BoxedAnyObject> =
            rows.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.store.splice(0, self.store.n_items(), &additions);
    }

    /// Clear the store and return a token for a bounded idle-batch rebuild.
    /// Any later full replacement invalidates the token, allowing a search
    /// rebuild to supersede startup publication without interleaving rows.
    pub(crate) fn begin_progressive_replace(&self) -> u64 {
        let generation = self.bump_row_replacement_generation();
        self.store.remove_all();
        generation
    }

    /// Append one idle-sized batch when no newer rebuild superseded it.
    pub(crate) fn append_progressive_rows(
        &self,
        generation: u64,
        rows: Vec<TrackTableRow>,
    ) -> bool {
        if self.row_replacement_generation.get() != generation {
            return false;
        }
        for row in rows {
            self.store.append(&glib::BoxedAnyObject::new(row));
        }
        true
    }

    fn bump_row_replacement_generation(&self) -> u64 {
        let generation = self.row_replacement_generation.get().wrapping_add(1);
        self.row_replacement_generation.set(generation);
        generation
    }

    /// Updates the cached [`TrackTableRow`] for `track_id` in place,
    /// without emitting a `gio::ListModel::items-changed` signal.
    ///
    /// Used by single-track mutations whose cell widgets already update
    /// themselves on click (the rating stars are the canonical case —
    /// see `sync_rating_buttons` in the cells module). The visual
    /// update has already happened on the rendered widget; this call
    /// just keeps the row data the cell factory will re-bind to (when
    /// the user scrolls away and back, or when GTK re-binds for any
    /// other reason) in sync with the new state.
    ///
    /// Crucially, this does **not** splice or otherwise restructure the
    /// store. A splice would trigger `items-changed`, which the
    /// `ColumnView` treats as a structural event — focus is dropped
    /// and the scroll position resets to the top of the list. For a
    /// one-field change initiated by a click in the row itself, that
    /// is unacceptable UX.
    ///
    /// Trade-off: if the current sort is by the field that changed
    /// (e.g. Rating column sorted, then user re-rates the row), the
    /// row stays in its now-incorrect sorted position until the next
    /// full reflow. We accept that — losing the user's scroll/focus
    /// would be worse.
    ///
    /// Returns `true` when a matching row was found and updated.
    pub(crate) fn update_row(&self, track_id: TrackId, new_row: TrackTableRow) -> bool {
        let n_items = self.store.n_items();
        for position in 0..n_items {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            let matches = row_object
                .try_borrow::<TrackTableRow>()
                .map(|row| row.track_id == Some(track_id))
                .unwrap_or(false);
            if !matches {
                continue;
            }
            // `BoxedAnyObject::borrow_mut` takes `&self`; the local
            // `row_object` is shadowed-immutable, but the inner
            // `RefCell` borrow is what we actually need.
            let mut row = row_object.borrow_mut::<TrackTableRow>();
            *row = new_row;
            drop(row);
            self.text_bindings.refresh_track(track_id);
            self.rating_bindings.refresh_track(track_id);
            self.status_bindings.refresh(self.playing_track_id.get());
            return true;
        }
        false
    }

    /// Whether the underlying store currently holds a row for
    /// `track_id`. Walks the store but performs no mutation — used by
    /// the smart-playlist refresh path to decide between an in-place
    /// row update and a full rebuild without going through
    /// [`Self::update_row`]'s side-effecting code path.
    pub(crate) fn contains_track(&self, track_id: TrackId) -> bool {
        let n_items = self.store.n_items();
        for position in 0..n_items {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            if let Ok(row) = row_object.try_borrow::<TrackTableRow>()
                && row.track_id == Some(track_id)
            {
                return true;
            }
        }
        false
    }

    /// Walk the underlying store once, refreshing each row's
    /// `is_missing` flag from `lookup`, then repaint the status icon
    /// on whichever rows are currently bound to a visible cell. Never
    /// emits `gio::ListModel::items-changed`: scroll position, focus,
    /// and selection survive a missing-flag flip — the table did not
    /// restructure, only the per-row status-column rendering needs to
    /// change. The same pattern the rating cell uses for its own
    /// click-driven updates (see `update_row`).
    ///
    /// Cost is bounded by `store.n_items()` for the data sync plus
    /// the visible-row count for the icon repaint, never library-wide
    /// re-bind cost. Callers pass `lookup` returning `None` for ids
    /// they don't know about; those rows are left untouched.
    pub(crate) fn refresh_missing_flags(&self, lookup: &dyn Fn(TrackId) -> Option<bool>) {
        let n_items = self.store.n_items();
        for position in 0..n_items {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            let track_id = match row_object.try_borrow::<TrackTableRow>() {
                Ok(row) => row.track_id,
                Err(_) => continue,
            };
            let Some(track_id) = track_id else { continue };
            let Some(now_missing) = lookup(track_id) else {
                continue;
            };
            let mut row = row_object.borrow_mut::<TrackTableRow>();
            if row.is_missing != now_missing {
                row.is_missing = now_missing;
            }
        }
        self.status_bindings.refresh(self.playing_track_id.get());
    }

    /// Returns the [`TrackId`]s of the currently selected rows, in the
    /// table's current sort order. Empty when the table is empty or no
    /// rows are selected.
    ///
    /// Used by global keyboard shortcuts (Get Info, Show in Folder) that
    /// must operate on whichever view the user is looking at; the caller
    /// picks the right table based on the visible content stack.
    pub(crate) fn selected_track_ids(&self) -> Vec<TrackId> {
        let bitset = self.selection.selection();
        let Some((iter, first)) = gtk::BitsetIter::init_first(&bitset) else {
            return Vec::new();
        };
        std::iter::once(first)
            .chain(iter)
            .filter_map(|position| {
                let row_object = self
                    .selection
                    .item(position)?
                    .downcast::<glib::BoxedAnyObject>()
                    .ok()?;
                let row = row_object.try_borrow::<TrackTableRow>().ok()?;
                row.track_id
            })
            .collect()
    }

    /// Select every displayed row through the model's bitset-backed bulk
    /// operation. Avoid walking rows in the UI layer: GTK can update the
    /// selection compactly and repaint only the list items it has realized.
    pub(crate) fn select_all(&self) {
        self.selection.select_all();
    }

    /// Returns every displayed track id in the table's current sort order.
    pub(crate) fn ordered_track_ids(&self) -> Vec<TrackId> {
        ordered_track_ids(&self.selection)
    }

    pub(crate) fn set_playing_track_id(&self, playing_track_id: Option<TrackId>) {
        if self.playing_track_id.get() == playing_track_id {
            return;
        }
        self.playing_track_id.set(playing_track_id);
        self.status_bindings.refresh(playing_track_id);
    }

    /// Finds the row whose track matches `track_id` in the current sort order,
    /// selects it (clearing any prior selection), and scrolls it into the
    /// viewport. Returns `false` when no row matches — callers use that as the
    /// signal to fall back to a different view (Songs is the fallback for
    /// Ctrl-L when the playing track is not in the current view's contents).
    pub(crate) fn reveal_track(&self, track_id: TrackId) -> bool {
        let n_items = self.selection.n_items();
        for position in 0..n_items {
            let Some(row_object) = self
                .selection
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
                continue;
            };
            let is_target = row.track_id == Some(track_id);
            drop(row);

            if !is_target {
                continue;
            }
            self.table.scroll_to(
                position,
                None,
                gtk::ListScrollFlags::SELECT | gtk::ListScrollFlags::FOCUS,
                Some(vertical_scroll_info()),
            );
            return true;
        }
        false
    }

    /// Apply a persisted layout: reorder columns, set visibility, set widths.
    /// Any managed column missing from `layout` keeps its factory defaults and
    /// is appended after the explicit entries.
    ///
    /// The [`Self::applying_layout`] guard is set for the duration so the resulting
    /// `notify::*` and `items-changed` signals do not loop back into a save.
    pub(crate) fn apply_layout(&self, layout: &TrackColumnLayout) {
        let _guard = ApplyLayoutGuard::enter(self.applying_layout.clone());
        let mut applied: HashSet<&'static str> = HashSet::new();
        // Position 0 is the status column; managed columns start at 1, and the
        // filler column is pushed to the end by the cascade of insert calls.
        let mut insert_at: u32 = 1;
        for entry in &layout.entries {
            if let Some(managed) = self
                .managed_columns
                .iter()
                .find(|managed| managed.column_id == entry.column_id.as_str())
            {
                managed.column.set_visible(entry.visible);
                managed
                    .column
                    .set_fixed_width(i32::try_from(entry.width_px).unwrap_or(i32::MAX));
                self.table.insert_column(insert_at, &managed.column);
                insert_at += 1;
                applied.insert(managed.column_id);
            }
        }
        for managed in self.managed_columns.iter() {
            if applied.contains(managed.column_id) {
                continue;
            }
            self.table.insert_column(insert_at, &managed.column);
            insert_at += 1;
        }
    }

    pub(crate) fn set_layout_changed_callback(&self, callback: LayoutChangedCallback) {
        *self.layout_changed.borrow_mut() = Some(callback);
    }

    /// Keep a derived grouped view in its caller-provided order. Duplicates
    /// rows must stay adjacent by candidate group, so header sorting and
    /// column reordering are intentionally disabled there.
    pub(crate) fn disable_sorting_and_column_reordering(&self) {
        self.table.set_reorderable(false);
        let columns = self.table.columns();
        for index in 0..columns.n_items() {
            let Some(column) = columns
                .item(index)
                .and_then(|item| item.downcast::<gtk::ColumnViewColumn>().ok())
            else {
                continue;
            };
            column.set_sorter(None::<&gtk::Sorter>);
        }
    }

    /// Activate the play-order sort on the status column.
    ///
    /// Per iTunes 11 semantics, a regular playlist is always sorted by some
    /// column; "manual order" is the sort represented by the leftmost
    /// (status) column. Callers invoke this when a regular playlist becomes
    /// the active selection so newly-displayed rows lay out in
    /// `PlaylistEntry::position` order, and so the within-playlist drag-
    /// reorder gate (which only accepts drops while this sort is active)
    /// is satisfied without the user having to click any header first.
    pub(crate) fn apply_playlist_default_sort(&self) {
        self.table
            .sort_by_column(Some(&self.status_column), gtk::SortType::Ascending);
    }

    /// Synchronously fires any pending debounced save. Call this from the
    /// window-close handler so a column tweak made within
    /// [`LAYOUT_SAVE_DEBOUNCE`] of shutdown is not lost.
    pub(crate) fn flush_pending_layout_save(&self) {
        let Some(source_id) = self.pending_save.borrow_mut().take() else {
            return;
        };
        source_id.remove();
        let Some(callback) = self.layout_changed.borrow().as_ref().cloned() else {
            return;
        };
        callback(read_current_layout(&self.table, &self.managed_columns));
    }
}

fn ordered_track_ids(selection: &gtk::MultiSelection) -> Vec<TrackId> {
    (0..selection.n_items())
        .filter_map(|position| {
            let row_object = selection
                .item(position)?
                .downcast::<glib::BoxedAnyObject>()
                .ok()?;
            let row = row_object.try_borrow::<TrackTableRow>().ok()?;
            row.track_id
        })
        .collect()
}

struct ApplyLayoutGuard {
    applying: Rc<Cell<bool>>,
}

impl ApplyLayoutGuard {
    fn enter(applying: Rc<Cell<bool>>) -> Self {
        applying.set(true);
        Self { applying }
    }
}

impl Drop for ApplyLayoutGuard {
    fn drop(&mut self) {
        self.applying.set(false);
    }
}

fn read_current_layout(
    table: &gtk::ColumnView,
    managed_columns: &[ManagedColumn],
) -> TrackColumnLayout {
    let columns_model = table.columns();
    let mut entries = Vec::with_capacity(managed_columns.len());
    for index in 0..columns_model.n_items() {
        let Some(item) = columns_model.item(index) else {
            continue;
        };
        let Ok(column) = item.downcast::<gtk::ColumnViewColumn>() else {
            continue;
        };
        let Some(managed) = managed_columns
            .iter()
            .find(|managed| managed.column.as_ptr() as *const () == column.as_ptr() as *const ())
        else {
            continue;
        };
        entries.push(TrackColumnEntry {
            column_id: managed.column_id.to_owned(),
            visible: managed.column.is_visible(),
            width_px: managed.column.fixed_width().max(0) as u32,
        });
    }
    TrackColumnLayout::new(entries)
}

fn vertical_scroll_info() -> gtk::ScrollInfo {
    let scroll_info = gtk::ScrollInfo::new();
    scroll_info.set_enable_horizontal(false);
    scroll_info.set_enable_vertical(true);
    scroll_info
}

pub(crate) fn build_track_table(
    rows: Vec<TrackTableRow>,
    track_activated: Option<TrackActivatedCallback>,
    context_menu: Option<TrackRowContextMenu>,
    rating_changed: Option<RatingChangedCallback>,
    row_reorder: Option<RowReorderCallback>,
    inline_edit: Option<InlineEditHooks>,
) -> TrackTable {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    for row in rows {
        store.append(&glib::BoxedAnyObject::new(row));
    }

    let table = gtk::ColumnView::new(None::<gtk::SelectionModel>);
    table.add_css_class("track-table");
    table.set_hexpand(true);
    table.set_vexpand(true);
    table.set_reorderable(true);
    table.set_show_column_separators(false);
    table.set_show_row_separators(false);
    table.set_single_click_activate(false);

    let playing_track_id: Rc<Cell<Option<TrackId>>> = Rc::new(Cell::new(None));
    let status_bindings = StatusBindings::default();
    let text_bindings = TextBindings::default();
    let rating_bindings = RatingBindings::default();
    // One controller per table coordinates every editable cell's click
    // gesture and the single open edit session. `None` on tables that did
    // not opt into inline editing (everything but the Songs view today).
    let inline_edit = inline_edit.map(InlineEditController::new);

    let sorted_rows = gtk::SortListModel::new(Some(store.clone()), table.sorter());
    let selection = gtk::MultiSelection::new(Some(sorted_rows));

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_hexpand(true);

    let context_menu =
        context_menu.map(|menu| TrackTableContextMenu::new(menu, selection.clone(), table.clone()));
    if let Some(context_menu) = &context_menu {
        context_menu.install_controller();
    }

    // Late-bound back reference broken on purpose: the hooks' play-order-
    // sort predicate needs to compare against the status column, and the
    // status column's cell factories need the hooks installed at setup time
    // (so each status cell becomes a drop target). The predicate reads
    // through this RefCell, which we populate the instant build_status_column
    // returns. Cell setup runs strictly after build_track_table finishes
    // installing the selection model below, so the predicate is never asked
    // before the cell is filled.
    let status_column_for_pred: Rc<RefCell<Option<gtk::ColumnViewColumn>>> =
        Rc::new(RefCell::new(None));

    let row_reorder_hooks = row_reorder.map(|callback| {
        let table_for_pred = table.clone();
        let status_column_for_pred = status_column_for_pred.clone();
        let is_play_order_active: Rc<dyn Fn() -> bool> = Rc::new(move || {
            let Some(sorter) = table_for_pred.sorter() else {
                return false;
            };
            let Some(column_sorter) = sorter.downcast_ref::<gtk::ColumnViewSorter>() else {
                return false;
            };
            let Some(active) = column_sorter.primary_sort_column() else {
                return false;
            };
            status_column_for_pred
                .borrow()
                .as_ref()
                .is_some_and(|target| &active == target)
        });
        RowReorderHooks {
            drop: callback,
            is_play_order_active,
            cells: RowDropCellRegistry::default(),
        }
    });

    let status_column = build_status_column(
        playing_track_id.clone(),
        status_bindings.clone(),
        context_menu.clone(),
        row_reorder_hooks.clone(),
    );
    *status_column_for_pred.borrow_mut() = Some(status_column.clone());
    if row_reorder_hooks.is_some() {
        // The status column doubles as the play-order sort. Installing the
        // CustomSorter here makes clicking its header equivalent to "sort
        // by manual order" — matching the iTunes 11 leftmost column — and
        // gives apply_playlist_default_sort() a column to point at.
        let playlist_position_sorter = gtk::CustomSorter::new(compare_playlist_position_objects);
        status_column.set_sorter(Some(&playlist_position_sorter));
    }
    table.append_column(&status_column);

    let header_menu = build_column_visibility_menu();
    let column_actions = gio::SimpleActionGroup::new();
    let mut managed_columns: Vec<ManagedColumn> = Vec::with_capacity(TRACK_TABLE_COLUMNS.len());

    for column in TRACK_TABLE_COLUMNS.iter().copied() {
        let table_column = build_table_column(
            column,
            &header_menu,
            text_bindings.clone(),
            rating_bindings.clone(),
            context_menu.clone(),
            rating_changed.clone(),
            row_reorder_hooks.clone(),
            inline_edit.clone(),
        );
        let action = gio::SimpleAction::new_stateful(
            column.action_name(),
            None,
            &column.default_visible().to_variant(),
        );
        let column_for_action = table_column.clone();
        action.connect_activate(move |_action, _parameter| {
            let visible = !column_for_action.is_visible();
            column_for_action.set_visible(visible);
        });
        // Keep the menu checkmark in sync whenever the column's visibility
        // changes — whether the user toggled the action, dragged a separator,
        // or apply_layout() set it programmatically.
        let action_for_sync = action.clone();
        table_column.connect_notify_local(Some("visible"), move |column, _spec| {
            action_for_sync.set_state(&column.is_visible().to_variant());
        });
        column_actions.add_action(&action);
        table.append_column(&table_column);
        managed_columns.push(ManagedColumn {
            column_id: column.action_name(),
            column: table_column,
        });
    }
    table.append_column(&build_filler_column(
        context_menu.clone(),
        row_reorder_hooks.clone(),
    ));

    table.insert_action_group("columns", Some(&column_actions));

    let managed_columns: Rc<Vec<ManagedColumn>> = Rc::new(managed_columns);
    let applying_layout: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let layout_changed: Rc<RefCell<Option<LayoutChangedCallback>>> = Rc::new(RefCell::new(None));
    let pending_save: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    install_layout_change_listeners(
        &table,
        managed_columns.clone(),
        applying_layout.clone(),
        layout_changed.clone(),
        pending_save.clone(),
    );

    if let Some(track_activated) = track_activated {
        let selection_for_activate = selection.clone();
        table.connect_activate(move |_table, position| {
            let Some(track_id) = selection_for_activate
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
                .and_then(|row_object| {
                    row_object
                        .try_borrow::<TrackTableRow>()
                        .ok()
                        .and_then(|row| row.track_id)
                })
            else {
                return;
            };

            track_activated(track_id);
        });
    }
    table.set_model(Some(&selection));

    scroller.set_child(Some(&table));

    // Wrap the scroller in a custom widget that paints empty striped rows
    // below the last real row when the table does not fill the viewport.
    // The painter owns no model state; it only needs the row count, which
    // we seed now and refresh on every `gio::ListModel::items-changed`
    // from the underlying store. The store is the truth here — no filter
    // chain sits between it and the selection model, so `n_items` matches
    // what the user sees on screen.
    let painter = EmptyRowPainter::new(&scroller, &table);
    painter.set_row_count(store.n_items());
    let painter_for_store = painter.downgrade();
    store.connect_items_changed(move |store, _position, _removed, _added| {
        if let Some(painter) = painter_for_store.upgrade() {
            painter.set_row_count(store.n_items());
        }
    });

    TrackTable {
        painter,
        table,
        store,
        selection,
        playing_track_id,
        status_bindings,
        text_bindings,
        rating_bindings,
        status_column,
        managed_columns,
        applying_layout,
        layout_changed,
        pending_save,
        row_replacement_generation: Rc::new(Cell::new(0)),
    }
}

fn install_layout_change_listeners(
    table: &gtk::ColumnView,
    managed_columns: Rc<Vec<ManagedColumn>>,
    applying_layout: Rc<Cell<bool>>,
    layout_changed: Rc<RefCell<Option<LayoutChangedCallback>>>,
    pending_save: Rc<RefCell<Option<glib::SourceId>>>,
) {
    // Debounced scheduler. Each call cancels any prior pending save and queues
    // a new one LAYOUT_SAVE_DEBOUNCE in the future, so a continuous resize
    // drag (which fires notify::fixed-width per pixel) collapses to a single
    // SQLite write when the drag stops.
    let schedule: Rc<dyn Fn()> = {
        let table = table.clone();
        let managed_columns = managed_columns.clone();
        let applying_layout = applying_layout.clone();
        let layout_changed = layout_changed.clone();
        let pending_save = pending_save.clone();
        Rc::new(move || {
            if applying_layout.get() {
                return;
            }
            if let Some(previous) = pending_save.borrow_mut().take() {
                previous.remove();
            }
            let table = table.clone();
            let managed_columns = managed_columns.clone();
            let layout_changed = layout_changed.clone();
            let pending_save_clear = pending_save.clone();
            let source_id = glib::timeout_add_local_once(LAYOUT_SAVE_DEBOUNCE, move || {
                // The timer has now fired; release our handle to it before
                // doing work so flush_pending_layout_save() can run cleanly
                // even if the callback ends up triggering more changes.
                pending_save_clear.borrow_mut().take();
                let Some(callback) = layout_changed.borrow().as_ref().cloned() else {
                    return;
                };
                callback(read_current_layout(&table, &managed_columns));
            });
            *pending_save.borrow_mut() = Some(source_id);
        })
    };

    for managed in managed_columns.iter() {
        let schedule_for_width = schedule.clone();
        managed
            .column
            .connect_notify_local(Some("fixed-width"), move |_column, _spec| {
                schedule_for_width();
            });
        let schedule_for_visible = schedule.clone();
        managed
            .column
            .connect_notify_local(Some("visible"), move |_column, _spec| {
                schedule_for_visible();
            });
    }

    let schedule_for_reorder = schedule;
    table
        .columns()
        .connect_items_changed(move |_model, _position, _removed, _added| {
            schedule_for_reorder();
        });
}

#[allow(clippy::too_many_arguments)]
fn build_table_column(
    column: TrackTableColumn,
    header_menu: &gio::Menu,
    text_bindings: TextBindings,
    rating_bindings: RatingBindings,
    context_menu: Option<TrackTableContextMenu>,
    rating_changed: Option<RatingChangedCallback>,
    row_reorder: Option<RowReorderHooks>,
    inline_edit: Option<InlineEditController>,
) -> gtk::ColumnViewColumn {
    let factory = if column == TrackTableColumn::Rating {
        build_rating_cell_factory(rating_bindings, context_menu, rating_changed, row_reorder)
    } else {
        build_text_cell_factory(
            column,
            text_bindings,
            context_menu,
            row_reorder,
            inline_edit,
        )
    };
    let table_column = gtk::ColumnViewColumn::new(Some(column.title()), Some(factory));
    table_column.set_resizable(true);
    table_column.set_expand(column.expands());
    table_column.set_fixed_width(column.default_width());
    table_column.set_visible(column.default_visible());
    table_column.set_header_menu(Some(header_menu));

    let sorter =
        gtk::CustomSorter::new(move |left, right| compare_track_objects(column, left, right));
    table_column.set_sorter(Some(&sorter));

    table_column
}

fn build_column_visibility_menu() -> gio::Menu {
    let menu = gio::Menu::new();
    let columns = gio::Menu::new();
    for column in TRACK_TABLE_COLUMNS {
        columns.append(
            Some(column.title()),
            Some(&format!("columns.{}", column.action_name())),
        );
    }
    menu.append_section(Some("Columns"), &columns);
    menu
}

fn compare_track_objects(
    column: TrackTableColumn,
    left: &glib::Object,
    right: &glib::Object,
) -> gtk::Ordering {
    let Some(left) = left.downcast_ref::<glib::BoxedAnyObject>() else {
        return gtk::Ordering::Equal;
    };
    let Some(right) = right.downcast_ref::<glib::BoxedAnyObject>() else {
        return gtk::Ordering::Equal;
    };
    let Ok(left) = left.try_borrow::<TrackTableRow>() else {
        return gtk::Ordering::Equal;
    };
    let Ok(right) = right.try_borrow::<TrackTableRow>() else {
        return gtk::Ordering::Equal;
    };

    to_gtk_ordering(column.compare(&left, &right))
}

fn to_gtk_ordering(ordering: CmpOrdering) -> gtk::Ordering {
    match ordering {
        CmpOrdering::Less => gtk::Ordering::Smaller,
        CmpOrdering::Equal => gtk::Ordering::Equal,
        CmpOrdering::Greater => gtk::Ordering::Larger,
    }
}

/// Sort comparator wired to the status column when a row-reorder hook is
/// present. Orders by [`TrackTableRow::playlist_position`] so rows from a
/// regular playlist lay out in the playlist's authoritative
/// [`sustain_app_runtime::PlaylistEntry::position`] order. Rows without a
/// playlist position (Songs / Library / Smart Playlist views) compare equal
/// to each other and sort after positioned rows, leaving non-playlist tables
/// undisturbed when the column header is clicked.
fn compare_playlist_position_objects(left: &glib::Object, right: &glib::Object) -> gtk::Ordering {
    let left_position = playlist_position_from_object(left);
    let right_position = playlist_position_from_object(right);
    to_gtk_ordering(match (left_position, right_position) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => CmpOrdering::Less,
        (None, Some(_)) => CmpOrdering::Greater,
        (None, None) => CmpOrdering::Equal,
    })
}

fn playlist_position_from_object(object: &glib::Object) -> Option<u32> {
    let row_object = object.downcast_ref::<glib::BoxedAnyObject>()?;
    let row = row_object.try_borrow::<TrackTableRow>().ok()?;
    row.playlist_position
}
