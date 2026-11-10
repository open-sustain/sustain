// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::glib::variant::ToVariant;
use gtk::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, RefCell};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use sustain_app_runtime::{Rating, TrackColumnEntry, TrackColumnLayout, TrackColumnSort, TrackId};

use super::track_context::TrackRowContextMenu;
use crate::track_summary::TrackSummary;
pub(crate) use cells::row_track_id;
use cells::{
    RatingBindings, RefreshScope, StatusBindings, TextBindings, build_filler_column,
    build_rating_cell_factory, build_status_column, build_text_cell_factory, install_context_menu,
};
use columns::{TRACK_TABLE_COLUMNS, TrackTableColumn};
use drag_drop::{RowReorderHooks, install_row_reorder_drop_target, install_tracks_drag_source};
pub(crate) use empty_row_painter::EmptyRowPainter;
pub(crate) use inline_edit::{BeginEditCallback, EditSession, EditableField, InlineEditController};
use order::{RowOrder, SortKey, apply_permutation, compare_playlist_positions};
pub(crate) use row::TrackTableRow;

mod cells;
mod columns;
mod drag_drop;
mod empty_row_painter;
mod inline_edit;
mod order;
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
    kind: TrackTableColumn,
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
    inline_edit: Option<InlineEditController>,
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

    /// Swap the entire visible row set for `rows`, presented in the active
    /// sort order, reusing row objects where possible.
    ///
    /// PERFORMANCE-CRITICAL — this is the search and playlist-switch hot path
    /// (#226). Every step earns its keep against a measured stall:
    ///
    /// - order the rows here, in Rust (see the `order` module), instead of
    ///   letting a sort model reconcile and re-sort the store afterwards;
    /// - reuse the boxed row objects for the common prefix, so GTK only does
    ///   structural work for the length delta instead of tearing down and
    ///   rebinding every realized cell even when the next row set is tiny;
    /// - splice only the surplus/deficit, under a single `items-changed`.
    ///
    /// Any refactor here must re-measure a whole-library search and a
    /// large→small playlist switch and not regress them. "Cleaner" code that
    /// reintroduces a full remove+insert, a per-row append loop, or a sort
    /// model over the store will bring the freeze back.
    pub(crate) fn replace_rows(&self, mut rows: Vec<TrackTableRow>) {
        self.finish_edit_before_payload_swap();
        self.bump_row_replacement_generation();
        self.selection.unselect_all();
        self.order_rows(&mut rows);

        // Reuse the existing boxed row objects for the common prefix: updating
        // the payloads in place keeps the model identity stable and limits
        // structural model work to the length delta.
        let old_len = self.store.n_items();
        let new_len = u32::try_from(rows.len()).unwrap_or(u32::MAX);
        let common_len = old_len.min(new_len);
        let mut rows = rows.into_iter();
        for position in 0..common_len {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            if let Some(new_row) = rows.next() {
                *row_object.borrow_mut::<TrackTableRow>() = new_row;
            }
        }

        if old_len > new_len {
            let no_additions: &[glib::BoxedAnyObject] = &[];
            self.store.splice(new_len, old_len - new_len, no_additions);
        } else if new_len > old_len {
            let additions: Vec<glib::BoxedAnyObject> =
                rows.map(glib::BoxedAnyObject::new).collect();
            self.store.splice(old_len, 0, &additions);
        }

        self.refresh_bound_cells();
        self.scroll_to_top();
    }

    fn finish_edit_before_payload_swap(&self) {
        if let Some(inline_edit) = &self.inline_edit {
            inline_edit.finish_before_payload_swap();
        }
    }

    /// Re-derive every realized cell from its row payload.
    ///
    /// Payloads swapped in place (by [`Self::replace_rows`] or a re-sort)
    /// emit no `items-changed`, so GTK rebinds none of those cells on its
    /// own. This is the correctness half of the in-place swap — it is what
    /// makes the cheap swap show the right data — so do not drop it when
    /// reworking either caller.
    fn refresh_bound_cells(&self) {
        self.refresh_cells(RefreshScope::AllRows);
    }

    fn refresh_cells(&self, scope: RefreshScope<'_>) {
        self.text_bindings.refresh(scope);
        self.rating_bindings.refresh(scope);
        self.status_bindings
            .refresh(scope, self.playing_track_id.get());
    }

    pub(crate) fn summary(&self) -> TrackSummary {
        let mut summary = TrackSummary::default();
        for position in 0..self.store.n_items() {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            if let Ok(row) = row_object.try_borrow::<TrackTableRow>() {
                summary.add_row(&row);
            }
        }
        summary
    }

    /// Insert structurally new rows at their sorted positions without
    /// replacing the existing model.
    ///
    /// Import completion uses this path so freshly-added tracks can appear in
    /// Songs while the current scroll/focus/selection state survives: GTK
    /// receives insertion events instead of a full remove-and-readd cycle.
    /// Rows that land next to each other go in under one splice, and a new
    /// row follows the existing rows it ties with.
    pub(crate) fn append_rows(&self, mut rows: Vec<TrackTableRow>) {
        if rows.is_empty() {
            return;
        }
        let order = self.row_order();
        order.sort(&mut rows);
        let mut insertions: Vec<(u32, Vec<glib::BoxedAnyObject>)> = Vec::new();
        for row in rows {
            let position = self.insertion_position(&order, &row);
            let object = glib::BoxedAnyObject::new(row);
            match insertions.last_mut() {
                Some((last_position, objects)) if *last_position == position => {
                    objects.push(object);
                }
                _ => insertions.push((position, vec![object])),
            }
        }
        // Positions refer to the store before any insertion, so splice from
        // the back: each splice leaves the positions before it untouched.
        for (position, objects) in insertions.into_iter().rev() {
            self.store.splice(position, 0, &objects);
        }
    }

    /// Where `row` belongs in the store: after every row that does not sort
    /// after it.
    fn insertion_position(&self, order: &RowOrder, row: &TrackTableRow) -> u32 {
        let (mut low, mut high) = (0, self.store.n_items());
        if order.is_unsorted() {
            return high;
        }
        while low < high {
            let middle = low + (high - low) / 2;
            let sorts_after = self
                .store
                .item(middle)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
                .and_then(|object| {
                    let existing = object.try_borrow::<TrackTableRow>().ok()?;
                    Some(order.compare(&existing, row).is_gt())
                })
                .unwrap_or(true);
            if sorts_after {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        low
    }

    /// Clear the store and return a token for a bounded idle-batch rebuild.
    /// Any later full replacement invalidates the token, allowing a search
    /// rebuild to supersede startup publication without interleaving rows.
    ///
    /// The batches must arrive in display order: order the whole row set
    /// with [`Self::order_rows`] first, then stream it.
    pub(crate) fn begin_progressive_replace(&self) -> u64 {
        let generation = self.bump_row_replacement_generation();
        self.store.remove_all();
        generation
    }

    /// Append one idle-sized batch when no newer rebuild superseded it.
    ///
    /// Splices the whole batch under a single `items-changed` rather than
    /// appending row-by-row, so the view does its bookkeeping once per batch.
    pub(crate) fn append_progressive_rows(
        &self,
        generation: u64,
        rows: Vec<TrackTableRow>,
    ) -> bool {
        if self.row_replacement_generation.get() != generation {
            return false;
        }
        let additions: Vec<glib::BoxedAnyObject> =
            rows.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.store.splice(self.store.n_items(), 0, &additions);
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
    /// the cell sets its stars once the change is accepted). The visual
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
        self.update_rows(&HashMap::from([(track_id, new_row)])) > 0
    }

    /// Updates every matching cached row in one store pass, without emitting
    /// `gio::ListModel::items-changed`.
    ///
    /// Batch metadata edits can touch dozens or hundreds of rows at once, so
    /// the table work is bounded to one store scan. Only the cells showing an
    /// updated track are re-derived: a background analysis result updates
    /// one track at a time, and must not redraw every realized row.
    pub(crate) fn update_rows(&self, new_rows: &HashMap<TrackId, TrackTableRow>) -> usize {
        if new_rows.is_empty() {
            return 0;
        }

        let mut updated = 0usize;
        let mut updated_track_ids = HashSet::new();
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
            let Some((track_id, new_row)) =
                track_id.and_then(|track_id| new_rows.get_key_value(&track_id))
            else {
                continue;
            };
            row_object
                .borrow_mut::<TrackTableRow>()
                .update_track_data(new_row);
            updated += 1;
            updated_track_ids.insert(*track_id);
        }

        if updated > 0 {
            self.refresh_cells(RefreshScope::Tracks(&updated_track_ids));
        }
        updated
    }

    /// Removes the rows of `track_ids`, one `items-changed` per contiguous
    /// run, leaving every other row, the selection around them, and the
    /// scroll position in place.
    ///
    /// Returns how many rows were removed.
    pub(crate) fn remove_rows(&self, track_ids: &HashSet<TrackId>) -> u32 {
        if track_ids.is_empty() {
            return 0;
        }
        self.remove_rows_where(|position| {
            row_track_id(self.store.item(position)).is_some_and(|id| track_ids.contains(&id))
        })
    }

    /// Bring the table to exactly `rows` while keeping the user's place: a
    /// row already shown takes its new data where it stands, a row no longer
    /// listed leaves as under [`Self::remove_rows`], and a new row goes in at
    /// its sorted position as under [`Self::append_rows`]. Unlike
    /// [`Self::replace_rows`], the selection on the rows that stay and the
    /// scroll position survive.
    ///
    /// A row is the one already shown when it shows the same track at the
    /// same playlist position, since a regular playlist may list a track
    /// twice. As under [`Self::update_rows`], a row that stays does not move
    /// when its new data would sort it elsewhere.
    pub(crate) fn reconcile_rows(&self, rows: Vec<TrackTableRow>) {
        // A progressive rebuild still streaming would add its remaining rows
        // a second time; they go in below with the other new rows instead.
        self.bump_row_replacement_generation();
        let index_by_identity: HashMap<_, _> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| (row.identity(), index))
            .collect();
        let mut incoming: Vec<Option<TrackTableRow>> = rows.into_iter().map(Some).collect();

        let mut leaving = HashSet::new();
        for position in 0..self.store.n_items() {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            let Ok(identity) = row_object
                .try_borrow::<TrackTableRow>()
                .map(|row| row.identity())
            else {
                continue;
            };
            match index_by_identity
                .get(&identity)
                .and_then(|&index| incoming[index].take())
            {
                Some(row) => *row_object.borrow_mut::<TrackTableRow>() = row,
                None => {
                    leaving.insert(position);
                }
            }
        }

        if !leaving.is_empty() {
            self.remove_rows_where(|position| leaving.contains(&position));
        }
        self.append_rows(incoming.into_iter().flatten().collect());
        self.refresh_bound_cells();
    }

    /// Removes the rows at the positions `removes` picks, one
    /// `items-changed` per contiguous run, keeping focus, the selection
    /// around them and the scroll position as [`Self::remove_rows`]
    /// describes.
    fn remove_rows_where(&self, removes: impl Fn(u32) -> bool) -> u32 {
        self.finish_edit_before_payload_swap();
        self.move_focus_off_rows(&removes);
        let no_additions: &[glib::BoxedAnyObject] = &[];
        let mut removed = 0;
        // Walk from the back, so each splice leaves the positions before it
        // untouched.
        let mut run_end = self.store.n_items();
        while run_end > 0 {
            if !removes(run_end - 1) {
                run_end -= 1;
                continue;
            }
            let mut run_start = run_end - 1;
            while run_start > 0 && removes(run_start - 1) {
                run_start -= 1;
            }
            self.store
                .splice(run_start, run_end - run_start, no_additions);
            removed += run_end - run_start;
            run_end = run_start;
        }
        removed
    }

    /// Mirror each row's play-order position from `positions` after the
    /// displayed playlist renumbered its entries. Renumbering keeps the
    /// entries' relative order, so no row moves and none is redrawn.
    pub(crate) fn set_playlist_positions(&self, positions: &HashMap<TrackId, u32>) {
        for position in 0..self.store.n_items() {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            let mut row = row_object.borrow_mut::<TrackTableRow>();
            row.placement.playlist_position =
                row.track_id.and_then(|id| positions.get(&id).copied());
        }
    }

    /// Hand keyboard focus to the nearest row `removes` spares — the next
    /// one, else the previous one — when the focused row is about to go.
    ///
    /// GTK unparents a removed row widget; when that widget held focus, the
    /// window refocuses the list from its start and scrolls it to the top.
    /// Moving focus first keeps the user's place. The spared row above is
    /// then pinned where it stands, so the gap closes from below, as in a
    /// file manager.
    fn move_focus_off_rows(&self, removes: impl Fn(u32) -> bool) {
        let Some(focus) = self.table.root().and_then(|root| root.focus()) else {
            return;
        };
        let Some(focused) = self.text_bindings.row_within(&focus) else {
            return;
        };
        let focused = focused.position();
        if focused == gtk::INVALID_LIST_POSITION || !removes(focused) {
            return;
        }
        let below = (focused + 1..self.store.n_items()).find(|&position| !removes(position));
        let above = (0..focused).rev().find(|&position| !removes(position));
        let Some(next_focus) = below.or(above) else {
            return;
        };
        self.table
            .scroll_to(next_focus, None, gtk::ListScrollFlags::FOCUS, None);
        if let (Some(_), Some(above)) = (below, above) {
            self.table
                .scroll_to(above, None, gtk::ListScrollFlags::NONE, None);
        }
    }

    /// Returns the subset of `candidates` currently represented in the table's
    /// underlying store. Performs one model scan regardless of candidate count.
    pub(crate) fn contained_track_ids(&self, candidates: &HashSet<TrackId>) -> HashSet<TrackId> {
        if candidates.is_empty() {
            return HashSet::new();
        }

        let mut present = HashSet::new();
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
                && let Some(track_id) = row.track_id
                && candidates.contains(&track_id)
            {
                present.insert(track_id);
            }
        }
        present
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
        self.status_bindings
            .refresh_icons(self.playing_track_id.get());
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
        self.status_bindings.refresh_icons(playing_track_id);
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

    /// Anchor the viewport at the first row and move the keyboard cursor to
    /// it. No-op on an empty model.
    ///
    /// A payload swap leaves GTK anchored on whichever position it tracked
    /// before, now showing some other row; a replaced row set starts from the
    /// top instead.
    pub(crate) fn scroll_to_top(&self) {
        if self.selection.n_items() == 0 {
            return;
        }
        self.table.scroll_to(
            0,
            None,
            gtk::ListScrollFlags::FOCUS,
            Some(vertical_scroll_info()),
        );
    }

    /// Apply a persisted layout: reorder columns, set visibility, set widths.
    /// Any managed column missing from `layout` keeps its factory defaults and
    /// is appended after the explicit entries.
    ///
    /// The [`Self::applying_layout`] guard is set for the duration so the resulting
    /// `notify::*` and `items-changed` signals do not loop back into a save.
    pub(crate) fn apply_layout(&self, layout: &TrackColumnLayout) {
        // Skip the column reshuffle when the table already matches. Sidebar
        // selection re-applies the layout on every playlist switch, and most
        // switches share an identical layout; running the full insert_column
        // cascade each time was avoidable work the #226 freeze fix removed from
        // the hot path. Do not drop this guard without re-measuring switches.
        if read_current_layout(&self.table, &self.managed_columns) == *layout {
            return;
        }
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

        self.apply_sort(layout.sort.as_ref());
    }

    /// Make the table's active sort match `sort` exactly: point the
    /// `ColumnView` at the persisted column and direction, or clear sorting
    /// when `sort` is `None` (or names a column this table does not have).
    /// Authoritative on purpose — a view switch must not inherit the previous
    /// selection's sort. Runs under the [`ApplyLayoutGuard`] set by
    /// [`Self::apply_layout`], so the resulting `changed` signal does not loop
    /// back into a save.
    fn apply_sort(&self, sort: Option<&TrackColumnSort>) {
        let target = sort.and_then(|sort| {
            self.managed_columns
                .iter()
                .find(|managed| managed.column_id == sort.column_id.as_str())
                .map(|managed| (&managed.column, sort.ascending))
        });
        match target {
            Some((column, ascending)) => {
                let direction = if ascending {
                    gtk::SortType::Ascending
                } else {
                    gtk::SortType::Descending
                };
                self.table.sort_by_column(Some(column), direction);
            }
            None => self
                .table
                .sort_by_column(None::<&gtk::ColumnViewColumn>, gtk::SortType::Ascending),
        }
    }

    /// Put `rows` in the table's active sort order.
    pub(crate) fn order_rows(&self, rows: &mut [TrackTableRow]) {
        self.row_order().sort(rows);
    }

    /// The active sort, read from the `ColumnViewSorter`'s chain of sort
    /// columns, primary first.
    fn row_order(&self) -> RowOrder {
        let mut order = RowOrder::default();
        let Some(sorter) = self
            .table
            .sorter()
            .and_then(|sorter| sorter.downcast::<gtk::ColumnViewSorter>().ok())
        else {
            return order;
        };
        for position in 0..sorter.n_sort_columns() {
            let (Some(column), sort_type) = sorter.nth_sort_column(position) else {
                continue;
            };
            let key = if column == self.status_column {
                SortKey::PlaylistPosition
            } else if let Some(managed) = self
                .managed_columns
                .iter()
                .find(|managed| managed.column == column)
            {
                SortKey::Column(managed.kind)
            } else {
                continue;
            };
            order.push(key, sort_type == gtk::SortType::Ascending);
        }
        order
    }

    /// Re-sort the rows already in the table after the active sort changed.
    ///
    /// Payloads move between the existing row objects, so GTK does no
    /// structural work, and the selection moves with its rows. The view then
    /// follows the first selected row, or returns to the top when nothing is
    /// selected.
    pub(crate) fn sort_current_rows(&self) {
        let order = self.row_order();
        if order.is_unsorted() {
            return;
        }
        self.finish_edit_before_payload_swap();
        let Some(objects) = (0..self.store.n_items())
            .map(|position| {
                self.store
                    .item(position)
                    .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            })
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        let permutation = {
            let Ok(rows) = objects
                .iter()
                .map(|object| object.try_borrow::<TrackTableRow>())
                .collect::<Result<Vec<_>, _>>()
            else {
                return;
            };
            order.permutation(&rows)
        };
        if permutation
            .iter()
            .enumerate()
            .all(|(position, &source)| position == source)
        {
            return;
        }

        let previous_selection = self.selection.selection();
        let mut new_positions = vec![0u32; permutation.len()];
        for (position, &source) in permutation.iter().enumerate() {
            new_positions[source] = position as u32;
        }
        let selection = gtk::Bitset::new_empty();
        if let Some((rest, first)) = gtk::BitsetIter::init_first(&previous_selection) {
            for source in std::iter::once(first).chain(rest) {
                if let Some(&position) = new_positions.get(source as usize) {
                    selection.add(position);
                }
            }
        }

        apply_permutation(&permutation, |left, right| {
            std::mem::swap(
                &mut *objects[left].borrow_mut::<TrackTableRow>(),
                &mut *objects[right].borrow_mut::<TrackTableRow>(),
            );
        });
        self.selection
            .set_selection(&selection, &gtk::Bitset::new_range(0, self.store.n_items()));
        self.refresh_bound_cells();
        if selection.is_empty() {
            self.scroll_to_top();
        } else {
            self.table.scroll_to(
                selection.minimum(),
                None,
                gtk::ListScrollFlags::FOCUS,
                Some(vertical_scroll_info()),
            );
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
        // Already on the play-order sort: bail before touching the sorter.
        // Every playlist selection calls this, and re-issuing sort_by_column
        // forces a full re-sort plus selection reconcile even when nothing
        // changed — avoidable work the #226 freeze fix removed from the hot
        // path. Do not drop this guard without re-measuring playlist switches.
        if status_sort_is_active(&self.table, &self.status_column) {
            return;
        }
        // Programmatic like apply_layout, so suppress the resulting sorter
        // `changed` from scheduling a layout save on every playlist selection.
        let _guard = ApplyLayoutGuard::enter(self.applying_layout.clone());
        self.table
            .sort_by_column(Some(&self.status_column), gtk::SortType::Ascending);
    }

    /// Writes the open inline edit and any edit still waiting for its
    /// write. Call this from the window-close handler, like
    /// [`Self::flush_pending_layout_save`].
    pub(crate) fn flush_pending_edits(&self) {
        if let Some(inline_edit) = &self.inline_edit {
            inline_edit.flush();
        }
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
    let sort = read_current_sort(table, managed_columns);
    TrackColumnLayout { entries, sort }
}

/// Read the table's active sort as a persistable [`TrackColumnSort`], or
/// `None` when nothing is sorted or the primary sort column is not one of the
/// managed columns (the playlist status/manual-order column has no persisted
/// id and intentionally maps to `None`).
fn read_current_sort(
    table: &gtk::ColumnView,
    managed_columns: &[ManagedColumn],
) -> Option<TrackColumnSort> {
    let column_sorter = table.sorter()?.downcast::<gtk::ColumnViewSorter>().ok()?;
    let active = column_sorter.primary_sort_column()?;
    let column_id = managed_columns
        .iter()
        .find(|managed| managed.column.as_ptr() as *const () == active.as_ptr() as *const ())?
        .column_id;
    Some(TrackColumnSort {
        column_id: column_id.to_owned(),
        ascending: matches!(column_sorter.primary_sort_order(), gtk::SortType::Ascending),
    })
}

/// Whether `table` shows play order, first track first: its primary sort is
/// the status column, ascending.
fn status_sort_is_active(table: &gtk::ColumnView, status_column: &gtk::ColumnViewColumn) -> bool {
    let Some(column_sorter) = table
        .sorter()
        .and_then(|sorter| sorter.downcast::<gtk::ColumnViewSorter>().ok())
    else {
        return false;
    };
    let Some(active_column) = column_sorter.primary_sort_column() else {
        return false;
    };

    active_column.as_ptr() as *const () == status_column.as_ptr() as *const ()
        && matches!(column_sorter.primary_sort_order(), gtk::SortType::Ascending)
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
    inline_edit: Option<BeginEditCallback>,
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
    // One controller per table coordinates the edit click on its editable
    // cells and the single open edit session. `None` on tables that did
    // not opt into inline editing (everything but the Songs view today).
    let inline_edit = inline_edit.map(|begin_edit| InlineEditController::new(&table, begin_edit));

    // The store holds the rows in display order; see the `order` module for
    // why no `SortListModel` sits in between.
    let selection = gtk::MultiSelection::new(Some(store.clone()));

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_hexpand(true);

    // A table offering track actions also lets its tracks be dragged out.
    if let Some(menu) = context_menu {
        install_context_menu(&table, menu, selection.clone());
        install_tracks_drag_source(&table, &selection);
    }

    let status_column = build_status_column(playing_track_id.clone(), status_bindings.clone());
    if let Some(drop) = row_reorder {
        // The status column doubles as the play-order sort. Installing the
        // CustomSorter here makes clicking its header equivalent to "sort
        // by manual order" — matching the iTunes 11 leftmost column — and
        // gives apply_playlist_default_sort() a column to point at.
        let playlist_position_sorter = gtk::CustomSorter::new(compare_playlist_position_objects);
        status_column.set_sorter(Some(&playlist_position_sorter));
        install_row_reorder_drop_target(
            &table,
            RowReorderHooks {
                drop,
                play_order_column: status_column.clone(),
            },
        );
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
            rating_changed.clone(),
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
            kind: column,
            column: table_column,
        });
    }
    table.append_column(&build_filler_column());

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

    let track_table = TrackTable {
        painter,
        table,
        store,
        selection,
        playing_track_id,
        status_bindings,
        text_bindings,
        rating_bindings,
        inline_edit,
        status_column,
        managed_columns,
        applying_layout,
        layout_changed,
        pending_save,
        row_replacement_generation: Rc::new(Cell::new(0)),
    };
    // Header clicks, restored layouts, and the play-order sort all change
    // the sorter; the rows follow.
    if let Some(sorter) = track_table.table.sorter() {
        let table_for_sort = track_table.clone();
        sorter.connect_changed(move |_sorter, _change| {
            table_for_sort.sort_current_rows();
        });
    }
    track_table
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

    // Clicking a column header re-points the ColumnView's sorter, which emits
    // `changed`. Persisting the new sort is the whole point of this listener;
    // restoring a sort under apply_layout's guard is suppressed by `schedule`.
    if let Some(sorter) = table.sorter() {
        let schedule_for_sort = schedule.clone();
        sorter.connect_changed(move |_sorter, _change| {
            schedule_for_sort();
        });
    }

    let schedule_for_reorder = schedule;
    table
        .columns()
        .connect_items_changed(move |_model, _position, _removed, _added| {
            schedule_for_reorder();
        });
}

fn build_table_column(
    column: TrackTableColumn,
    header_menu: &gio::Menu,
    text_bindings: TextBindings,
    rating_bindings: RatingBindings,
    rating_changed: Option<RatingChangedCallback>,
    inline_edit: Option<InlineEditController>,
) -> gtk::ColumnViewColumn {
    let factory = if column == TrackTableColumn::Rating {
        build_rating_cell_factory(rating_bindings, rating_changed)
    } else {
        build_text_cell_factory(column, text_bindings, inline_edit)
    };
    let table_column = gtk::ColumnViewColumn::new(Some(column.title()), Some(factory));
    table_column.set_resizable(true);
    table_column.set_expand(column.expands());
    table_column.set_fixed_width(column.default_width());
    table_column.set_visible(column.default_visible());
    table_column.set_header_menu(Some(header_menu));

    // A column is sortable from its header only when it has a sorter. The
    // table orders its own rows with the same comparison (see the `order`
    // module); this keeps the `ColumnViewSorter` a faithful `Sorter` too.
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
/// present. Orders by [`row::RowPlacement::playlist_position`] so rows from a
/// regular playlist lay out in the playlist's authoritative
/// [`sustain_app_runtime::PlaylistEntry::position`] order. Rows without a
/// playlist position (Songs / Library / Smart Playlist views) compare equal
/// to each other and sort after positioned rows, leaving non-playlist tables
/// undisturbed when the column header is clicked.
fn compare_playlist_position_objects(left: &glib::Object, right: &glib::Object) -> gtk::Ordering {
    to_gtk_ordering(compare_playlist_positions(
        playlist_position_from_object(left),
        playlist_position_from_object(right),
    ))
}

fn playlist_position_from_object(object: &glib::Object) -> Option<u32> {
    let row_object = object.downcast_ref::<glib::BoxedAnyObject>()?;
    let row = row_object.try_borrow::<TrackTableRow>().ok()?;
    row.placement.playlist_position
}

#[cfg(test)]
mod tests {
    use sustain_app_runtime::TrackId;

    use super::*;
    use crate::table_cell::TableCell;
    use crate::track_context::{TrackContextActionSet, TrackContextInvocationState};

    #[test]
    fn append_rows_adds_to_existing_store_without_replacement() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            table.replace_rows(vec![row(1)]);
            let first_object = table.store.item(0);

            table.append_rows(vec![row(2), row(3)]);

            assert_eq!(table.store.n_items(), 3);
            assert_eq!(table.store.item(0), first_object);
        });
    }

    #[test]
    fn replace_rows_reuses_common_row_objects() {
        crate::test_support::with_gtk(|| {
            let table =
                build_track_table(vec![row(1), row(2), row(3)], None, None, None, None, None);
            let first_object = store_object_key(&table, 0);
            let second_object = store_object_key(&table, 1);

            table.replace_rows(vec![row(4), row(5)]);

            assert_eq!(table.store.n_items(), 2);
            assert_eq!(store_object_key(&table, 0), first_object);
            assert_eq!(store_object_key(&table, 1), second_object);
            assert_eq!(store_track_id(&table, 0), TrackId::new(4));
            assert_eq!(store_track_id(&table, 1), TrackId::new(5));
        });
    }

    #[test]
    fn search_replacement_restripes_rows_in_their_new_sorted_positions() {
        let ran = crate::test_support::with_gtk(|| {
            // In the initial sort, rows A, C, and E sit at even positions.
            // Search swaps new payloads into the existing row objects, so
            // GTK rebinds no cell; striping must still follow each row's
            // position after the swap.
            let table = build_track_table(
                vec![
                    named_row(1, "A"),
                    named_row(2, "C"),
                    named_row(3, "E"),
                    named_row(4, "B"),
                    named_row(5, "D"),
                ],
                None,
                None,
                None,
                None,
                None,
            );
            let mut layout = read_current_layout(&table.table, &table.managed_columns);
            layout.sort = Some(TrackColumnSort {
                column_id: "track_name".to_owned(),
                ascending: true,
            });
            table.apply_layout(&layout);

            let window = gtk::Window::new();
            window.set_default_size(700, 260);
            window.set_child(Some(&table.widget()));
            window.present();
            drain_main_context();

            table.replace_rows(vec![
                named_row(1, "A"),
                named_row(2, "B"),
                named_row(3, "C"),
            ]);
            drain_main_context();

            let root = window
                .child()
                .expect("window retains the mapped track table");
            for (position, text) in ["A", "B", "C"].into_iter().enumerate() {
                let cell =
                    cell_containing_text(&root, text).expect("visible search-result cell exists");
                assert_eq!(
                    cell.has_css_class("track-table-row-even"),
                    position % 2 == 0,
                    "row {text} must be striped from visible position {position}"
                );
                assert_eq!(
                    cell.has_css_class("track-table-row-odd"),
                    position % 2 != 0,
                    "row {text} must carry exactly one parity class"
                );
            }

            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn in_place_replacement_repaints_group_bands() {
        let ran = crate::test_support::with_gtk(|| {
            // Duplicates bands rows by candidate group. A rescan swaps new
            // payloads into the realized rows, so the band must follow the
            // payload rather than whatever the cell was bound with.
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            table.disable_sorting_and_column_reordering();
            table.replace_rows(vec![
                named_row(1, "A").with_group_band(true),
                named_row(2, "B").with_group_band(true),
            ]);

            let window = gtk::Window::new();
            window.set_default_size(700, 260);
            window.set_child(Some(&table.widget()));
            window.present();
            drain_main_context();

            table.replace_rows(vec![
                named_row(1, "A").with_group_band(false),
                named_row(2, "B").with_group_band(false),
            ]);
            drain_main_context();

            let root = window
                .child()
                .expect("window retains the mapped track table");
            for text in ["A", "B"] {
                let cell = cell_containing_text(&root, text).expect("visible cell exists");
                assert!(
                    cell.has_css_class("track-table-row-odd")
                        && !cell.has_css_class("track-table-row-even"),
                    "row {text} must carry its new group band"
                );
            }

            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn update_rows_redraws_only_the_cells_of_updated_tracks() {
        let ran = crate::test_support::with_gtk(|| {
            let table = build_track_table(
                vec![named_row(1, "A"), named_row(2, "B")],
                None,
                None,
                None,
                None,
                None,
            );
            let window = gtk::Window::new();
            window.set_default_size(700, 260);
            window.set_child(Some(&table.widget()));
            window.present();
            drain_main_context();
            let root = window
                .child()
                .expect("window retains the mapped track table");
            // A probe: re-deriving row B's cell would overwrite it.
            let untouched =
                cell_containing_text(&root, "B").expect("row B's name cell is realized");
            untouched.set_text("probe");

            table.update_rows(&HashMap::from([(
                TrackId::new(1).expect("positive track id"),
                named_row(1, "A2"),
            )]));

            assert!(
                cell_containing_text(&root, "A2").is_some(),
                "the updated track's cell shows its new data"
            );
            assert_eq!(untouched.text(), "probe");

            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn remove_rows_keeps_the_other_rows_and_their_selection() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table((1..=6).map(row).collect(), None, None, None, None, None);
            table.selection.select_item(1, false);
            table.selection.select_item(4, false);
            let ids = |ids: &[i64]| {
                ids.iter()
                    .map(|id| TrackId::new(*id).expect("positive track id"))
                    .collect::<Vec<_>>()
            };

            let removed = table.remove_rows(&ids(&[1, 3, 4, 6]).into_iter().collect());

            assert_eq!(removed, 4);
            assert_eq!(table.ordered_track_ids(), ids(&[2, 5]));
            assert_eq!(table.selected_track_ids(), ids(&[2, 5]));
        });
    }

    #[test]
    fn reconcile_rows_keeps_the_rows_that_stay_and_their_selection() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            sort_by(&table, "track_name", true);
            table.replace_rows(vec![
                named_row(1, "A"),
                named_row(2, "C"),
                named_row(3, "E"),
                named_row(4, "G"),
            ]);
            table.selection.select_item(1, false);
            table.selection.select_item(2, false);
            let kept_object = store_object_key(&table, 1);

            table.reconcile_rows(vec![
                named_row(2, "Z"),
                named_row(3, "E"),
                named_row(5, "F"),
                named_row(6, "B"),
            ]);

            assert_eq!(
                table.ordered_track_ids(),
                track_ids(&[6, 2, 3, 5]),
                "a row that stays keeps its place; a new one goes in sorted"
            );
            assert_eq!(table.selected_track_ids(), track_ids(&[2, 3]));
            assert_eq!(store_object_key(&table, 1), kept_object);
            assert_eq!(store_track_name(&table, 1).as_deref(), Some("Z"));
        });
    }

    #[test]
    fn reconcile_rows_tells_apart_the_entries_of_a_track_listed_twice() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            table.apply_playlist_default_sort();
            let entry = |id: i64, position: u32| row(id).with_playlist_position(Some(position));
            table.replace_rows(vec![entry(1, 0), entry(2, 1), entry(1, 2)]);
            table.selection.select_item(0, false);

            // The second entry of track 1 left the playlist.
            table.reconcile_rows(vec![entry(1, 0), entry(2, 1)]);

            assert_eq!(table.ordered_track_ids(), track_ids(&[1, 2]));
            assert_eq!(table.selected_track_ids(), track_ids(&[1]));
        });
    }

    #[test]
    fn summary_values_reads_the_current_store_rows() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            let mut first = row(1);
            first.duration_seconds = 61;
            first.file_size_bytes = 1_000;
            let mut second = row(2);
            second.duration_seconds = 122;
            second.file_size_bytes = 2_500;

            table.replace_rows(vec![first, second]);

            assert_eq!(
                table.summary(),
                TrackSummary {
                    track_count: 2,
                    duration_seconds: 183,
                    size_bytes: 3_500,
                }
            );
        });
    }

    #[test]
    fn update_rows_patches_matching_rows_without_replacing_objects() {
        crate::test_support::with_gtk(|| {
            let table =
                build_track_table(vec![row(1), row(2), row(3)], None, None, None, None, None);
            let first_object = store_object_key(&table, 0);
            let third_object = store_object_key(&table, 2);
            let mut replacements = HashMap::new();
            let mut first = row(1);
            first.track_name = "Updated 1".to_owned();
            let mut third = row(3);
            third.track_name = "Updated 3".to_owned();
            replacements.insert(TrackId::new(1).expect("positive track id"), first);
            replacements.insert(TrackId::new(3).expect("positive track id"), third);

            assert_eq!(table.update_rows(&replacements), 2);

            assert_eq!(store_object_key(&table, 0), first_object);
            assert_eq!(store_object_key(&table, 2), third_object);
            assert_eq!(store_track_name(&table, 0).as_deref(), Some("Updated 1"));
            assert_eq!(store_track_name(&table, 1).as_deref(), Some("Track 2"));
            assert_eq!(store_track_name(&table, 2).as_deref(), Some("Updated 3"));
        });
    }

    #[test]
    fn apply_layout_round_trips_the_persisted_sort() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(vec![row(1), row(2)], None, None, None, None, None);

            // A freshly built table carries no column sort.
            assert!(
                read_current_layout(&table.table, &table.managed_columns)
                    .sort
                    .is_none()
            );

            let mut layout = read_current_layout(&table.table, &table.managed_columns);
            layout.sort = Some(TrackColumnSort {
                column_id: "date_added".to_owned(),
                ascending: false,
            });
            table.apply_layout(&layout);

            assert_eq!(
                read_current_layout(&table.table, &table.managed_columns).sort,
                Some(TrackColumnSort {
                    column_id: "date_added".to_owned(),
                    ascending: false,
                })
            );

            // Applying a layout with no sort authoritatively clears it.
            layout.sort = None;
            table.apply_layout(&layout);
            assert!(
                read_current_layout(&table.table, &table.managed_columns)
                    .sort
                    .is_none()
            );
        });
    }

    #[test]
    fn replace_rows_presents_rows_in_the_active_sort_order() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            sort_by(&table, "track_name", false);

            table.replace_rows(vec![
                named_row(1, "B"),
                named_row(2, "C"),
                named_row(3, "A"),
            ]);

            assert_eq!(table.ordered_track_ids(), track_ids(&[2, 1, 3]));
        });
    }

    #[test]
    fn changing_the_sort_reorders_rows_and_keeps_the_selection_on_its_rows() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(
                vec![named_row(1, "B"), named_row(2, "C"), named_row(3, "A")],
                None,
                None,
                None,
                None,
                None,
            );
            let row_objects: HashSet<Option<usize>> = (0..3)
                .map(|position| store_object_key(&table, position))
                .collect();
            table.selection.select_item(1, true);

            sort_by(&table, "track_name", true);

            assert_eq!(table.ordered_track_ids(), track_ids(&[3, 1, 2]));
            assert_eq!(table.selected_track_ids(), track_ids(&[2]));
            let reordered_objects: HashSet<Option<usize>> = (0..3)
                .map(|position| store_object_key(&table, position))
                .collect();
            assert_eq!(
                reordered_objects, row_objects,
                "a re-sort moves payloads between the existing row objects"
            );
        });
    }

    #[test]
    fn append_rows_inserts_each_row_at_its_sorted_position() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            sort_by(&table, "track_name", true);
            table.replace_rows(vec![named_row(1, "B"), named_row(2, "D")]);
            let first_object = store_object_key(&table, 0);

            table.append_rows(vec![
                named_row(3, "E"),
                named_row(4, "A"),
                named_row(5, "C"),
            ]);

            assert_eq!(table.ordered_track_ids(), track_ids(&[4, 1, 5, 2, 3]));
            assert_eq!(
                store_object_key(&table, 1),
                first_object,
                "existing rows keep their objects"
            );
        });
    }

    #[test]
    fn rows_streamed_in_order_need_no_settling() {
        crate::test_support::with_gtk(|| {
            let table = build_track_table(Vec::new(), None, None, None, None, None);
            sort_by(&table, "track_name", true);
            let mut rows = vec![named_row(1, "C"), named_row(2, "A"), named_row(3, "B")];
            table.order_rows(&mut rows);

            let generation = table.begin_progressive_replace();
            let mut rows = rows.into_iter();
            assert!(table.append_progressive_rows(generation, rows.by_ref().take(2).collect()));
            assert!(table.append_progressive_rows(generation, rows.collect()));
            assert_eq!(table.ordered_track_ids(), track_ids(&[2, 3, 1]));

            // A search supersedes the load and presents its own rows sorted.
            table.replace_rows(vec![named_row(4, "Z"), named_row(5, "Y")]);
            assert!(!table.append_progressive_rows(generation, vec![named_row(6, "X")]));
            assert_eq!(table.ordered_track_ids(), track_ids(&[5, 4]));
        });
    }

    /// Regression for the messy playlist-open rendering: with the play-order
    /// sort active, `replace_rows` must leave the selection model presenting
    /// the new rows in their final order before it returns — i.e. before GTK
    /// can draw a frame. A deferred sort draws the new rows scrambled and
    /// shuffles them into place over the following frames.
    #[test]
    fn replace_rows_presents_play_order_synchronously() {
        crate::test_support::with_gtk(|| {
            // The playlists-table configuration: a row-reorder hook is what
            // installs the play-order sorter on the status column.
            let reorder: RowReorderCallback = Rc::new(|_| false);
            let table = build_track_table(Vec::new(), None, None, None, Some(reorder), None);
            table.apply_playlist_default_sort();

            // "Old playlist": play order follows track id.
            let first: Vec<TrackTableRow> = (1..=600)
                .map(|id| row(id).with_playlist_position(Some(id as u32)))
                .collect();
            table.replace_rows(first);

            // "New playlist": same length, reversed play order, so the
            // payload swap invalidates every previously sorted position.
            let second: Vec<TrackTableRow> = (1..=600)
                .map(|id| row(id).with_playlist_position(Some(601 - id as u32)))
                .collect();
            table.replace_rows(second);

            let presented = ordered_track_ids(&table.selection);
            let expected: Vec<TrackId> = (1..=600).rev().filter_map(TrackId::new).collect();
            assert_eq!(
                presented, expected,
                "the sorted view must present the new play order synchronously"
            );
        });
    }

    /// A track's data changing in place — a rating, a background BPM — must
    /// not cost its row the place it holds in the displayed playlist, or
    /// returning to play order would sink the row to the bottom.
    #[test]
    fn update_rows_keeps_each_row_in_its_playlist_place() {
        crate::test_support::with_gtk(|| {
            let reorder: RowReorderCallback = Rc::new(|_| false);
            let table = build_track_table(Vec::new(), None, None, None, Some(reorder), None);
            table.replace_rows(
                (1..=3)
                    .map(|id| row(id).with_playlist_position(Some(id as u32)))
                    .collect(),
            );
            sort_by(&table, "track_name", false);
            let mut rated = row(1);
            rated.rating = 5;

            table.update_rows(&HashMap::from([(
                TrackId::new(1).expect("positive track id"),
                rated,
            )]));
            table.apply_playlist_default_sort();

            assert_eq!(table.ordered_track_ids(), track_ids(&[1, 2, 3]));
        });
    }

    /// A playlist renumbers its entries when some leave. The rows that stay
    /// must take the new numbers, or a track appended afterwards would sort
    /// above a row still holding a higher, stale number.
    #[test]
    fn rows_left_by_a_removal_take_the_playlist_renumbering() {
        crate::test_support::with_gtk(|| {
            let reorder: RowReorderCallback = Rc::new(|_| false);
            let table = build_track_table(Vec::new(), None, None, None, Some(reorder), None);
            table.apply_playlist_default_sort();
            table.replace_rows(
                (1..=4)
                    .map(|id| row(id).with_playlist_position(Some(id as u32 - 1)))
                    .collect(),
            );

            table.remove_rows(&track_ids(&[1, 2]).into_iter().collect());
            table.set_playlist_positions(&track_ids(&[3, 4]).into_iter().zip(0..).collect());
            table.append_rows(vec![row(5).with_playlist_position(Some(2))]);

            assert_eq!(table.ordered_track_ids(), track_ids(&[3, 4, 5]));
        });
    }

    /// A drag reorder renumbers the playlist; the rows follow in place and
    /// the dragged rows stay selected where they were dropped.
    #[test]
    fn rows_follow_a_playlist_reorder_with_their_selection() {
        crate::test_support::with_gtk(|| {
            let reorder: RowReorderCallback = Rc::new(|_| false);
            let table = build_track_table(Vec::new(), None, None, None, Some(reorder), None);
            table.apply_playlist_default_sort();
            table.replace_rows(
                (1..=4)
                    .map(|id| row(id).with_playlist_position(Some(id as u32 - 1)))
                    .collect(),
            );
            table.selection.select_item(0, true);

            // Track 1 dropped after track 3.
            table.set_playlist_positions(&track_ids(&[2, 3, 1, 4]).into_iter().zip(0..).collect());
            table.sort_current_rows();

            assert_eq!(table.ordered_track_ids(), track_ids(&[2, 3, 1, 4]));
            assert_eq!(table.selected_track_ids(), track_ids(&[1]));
        });
    }

    /// Only play order, first track first, takes row drops: reversed, a
    /// drop above a row would land below it.
    #[test]
    fn only_forward_play_order_counts_as_play_order() {
        crate::test_support::with_gtk(|| {
            let reorder: RowReorderCallback = Rc::new(|_| false);
            let table = build_track_table(Vec::new(), None, None, None, Some(reorder), None);
            let shows_play_order = || status_sort_is_active(&table.table, &table.status_column);

            table.apply_playlist_default_sort();
            assert!(shows_play_order());

            table
                .table
                .sort_by_column(Some(&table.status_column), gtk::SortType::Descending);
            assert!(!shows_play_order(), "reversed play order");

            sort_by(&table, "track_name", true);
            assert!(!shows_play_order(), "another column");
        });
    }

    fn sort_by(table: &TrackTable, column_id: &str, ascending: bool) {
        let mut layout = read_current_layout(&table.table, &table.managed_columns);
        layout.sort = Some(TrackColumnSort {
            column_id: column_id.to_owned(),
            ascending,
        });
        table.apply_layout(&layout);
    }

    fn track_ids(ids: &[i64]) -> Vec<TrackId> {
        ids.iter().filter_map(|&id| TrackId::new(id)).collect()
    }

    /// A row's cells must go with the row. Cells carry controllers and
    /// signal handlers — rating clicks, inline edit clicks, the row's tint
    /// and selection — and none of them may hold the cell or its list
    /// item, or every row a shorter list drops stays alive behind the
    /// table.
    #[test]
    fn cells_of_rows_that_leave_the_table_are_released() {
        let ran = crate::test_support::with_gtk(|| {
            let window = gtk::Window::new();
            let context_menu = TrackRowContextMenu::new(
                TrackContextActionSet::new(Vec::new()),
                window.clone(),
                TrackContextInvocationState::default(),
            );
            let rating_changed: RatingChangedCallback = Rc::new(|_, _| true);
            let reorder: RowReorderCallback = Rc::new(|_| false);
            let begin_edit: BeginEditCallback = Rc::new(|_, _| None);
            let table = build_track_table(
                (1..=300).map(row).collect(),
                None,
                Some(context_menu),
                Some(rating_changed),
                Some(reorder),
                Some(begin_edit),
            );
            window.set_default_size(700, 400);
            window.set_child(Some(&table.widget()));
            window.present();
            drain_main_context();

            let table_root: &gtk::Widget = table.table.upcast_ref();
            let mut widgets = Vec::new();
            collect_descendants(table_root, &mut widgets);
            assert!(
                widgets.len() > 300,
                "the table built cell widgets for its visible rows"
            );

            table.replace_rows(vec![row(1)]);
            drain_main_context();

            let detached_alive = widgets
                .iter()
                .filter_map(glib::WeakRef::upgrade)
                .filter(|widget| !widget.is_ancestor(table_root))
                .count();
            assert_eq!(
                detached_alive, 0,
                "widgets of the rows that left the table must be released"
            );
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    fn collect_descendants(root: &gtk::Widget, widgets: &mut Vec<glib::WeakRef<gtk::Widget>>) {
        let mut child = root.first_child();
        while let Some(widget) = child {
            widgets.push(widget.downgrade());
            collect_descendants(&widget, widgets);
            child = widget.next_sibling();
        }
    }

    fn row(id: i64) -> TrackTableRow {
        TrackTableRow::test_row(id)
    }

    fn named_row(id: i64, track_name: &str) -> TrackTableRow {
        let mut row = row(id);
        row.track_name = track_name.to_owned();
        row.track_name_sort_key = track_name.to_lowercase();
        row
    }

    fn drain_main_context() {
        let context = glib::MainContext::default();
        let mut iterations = 0;
        while context.iteration(false) && iterations < 200 {
            iterations += 1;
        }
    }

    fn cell_containing_text(root: &gtk::Widget, text: &str) -> Option<TableCell> {
        if let Some(cell) = root.downcast_ref::<TableCell>()
            && cell.text() == text
        {
            return Some(cell.clone());
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(cell) = cell_containing_text(&widget, text) {
                return Some(cell);
            }
            child = widget.next_sibling();
        }
        None
    }

    fn store_object_key(table: &TrackTable, position: u32) -> Option<usize> {
        table
            .store
            .item(position)
            .map(|object| object.as_ptr() as usize)
    }

    fn store_track_id(table: &TrackTable, position: u32) -> Option<TrackId> {
        let row_object = table
            .store
            .item(position)?
            .downcast::<glib::BoxedAnyObject>()
            .ok()?;
        let row = row_object.try_borrow::<TrackTableRow>().ok()?;
        row.track_id
    }

    fn store_track_name(table: &TrackTable, position: u32) -> Option<String> {
        let row_object = table
            .store
            .item(position)?
            .downcast::<glib::BoxedAnyObject>()
            .ok()?;
        let row = row_object.try_borrow::<TrackTableRow>().ok()?;
        Some(row.track_name.clone())
    }
}
