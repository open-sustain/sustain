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
pub(crate) use cells::row_track_id;
use cells::{
    RatingBindings, StatusBindings, TextBindings, TrackTableContextMenu, build_filler_column,
    build_rating_cell_factory, build_status_column, build_text_cell_factory,
};
use columns::{TRACK_TABLE_COLUMNS, TrackTableColumn};
use drag_drop::{RowDropCellRegistry, RowReorderHooks};
pub(crate) use empty_row_painter::EmptyRowPainter;
pub(crate) use inline_edit::{EditableField, InlineEditController, InlineEditHooks};
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

/// Row-count drop above which [`TrackTable::replace_rows`] hides the
/// `ColumnView` around the shrink splice.
///
/// 512 is an empirically tuned value, not a round number. It was settled while
/// fixing the multi-second playlist-switch freeze (#226): below it the
/// unmap/remap costs more than the GTK reconciliation it saves, above it the
/// synchronous teardown of GTK's realized-cell cache dominates the switch.
/// Re-measure a large→small playlist switch before changing it.
const LARGE_SHRINK_UNMAP_THRESHOLD: u32 = 512;

impl TrackTable {
    pub(crate) fn widget(&self) -> gtk::Widget {
        self.painter.clone().upcast()
    }

    /// Swap the entire visible row set for `rows`, reusing row objects where
    /// possible.
    ///
    /// PERFORMANCE-CRITICAL — this is the playlist-switch hot path. Selecting a
    /// playlist used to freeze the UI for ~10 s (#226). The body below is the
    /// end product of extensive trial-and-error profiling; every step earns its
    /// keep against a measured stall, not abstract tidiness:
    ///
    /// - reuse the boxed row objects for the common prefix, so GTK only does
    ///   structural work for the length delta instead of tearing down and
    ///   rebinding every realized cell even when the next playlist is tiny;
    /// - splice only the surplus/deficit, under a single `items-changed`;
    /// - unmap the view around a large shrink (see
    ///   [`LARGE_SHRINK_UNMAP_THRESHOLD`]).
    ///
    /// Any refactor here must re-measure a large→small playlist switch and not
    /// regress it. "Cleaner" code that reintroduces a full remove+insert, a
    /// per-row append loop, or a mapped large-shrink splice will bring the
    /// freeze back.
    pub(crate) fn replace_rows(&self, rows: Vec<TrackTableRow>) {
        self.bump_row_replacement_generation();
        self.selection.unselect_all();

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

        // GTK's ColumnView has no public "batch model changes" API. For a
        // large collapse, keeping the mapped view attached makes ListBase
        // synchronously reconcile thousands of removed rows against its
        // realized cell cache. Unmapping the view around that single splice
        // bounds the work to one teardown/remap before the next frame can be
        // drawn; small shrinks stay mapped because unmapping costs more than
        // it saves there.
        let unmap_for_large_shrink = old_len.saturating_sub(new_len) > LARGE_SHRINK_UNMAP_THRESHOLD
            && self.table.is_mapped();
        let restore_focus = unmap_for_large_shrink && self.table.has_focus();
        if unmap_for_large_shrink {
            self.table.set_visible(false);
        }

        if old_len > new_len {
            let no_additions: &[glib::BoxedAnyObject] = &[];
            self.store.splice(new_len, old_len - new_len, no_additions);
        } else if new_len > old_len {
            let additions: Vec<glib::BoxedAnyObject> =
                rows.map(glib::BoxedAnyObject::new).collect();
            self.store.splice(old_len, 0, &additions);
        }

        if unmap_for_large_shrink {
            self.table.set_visible(true);
            if restore_focus {
                let _ = self.table.grab_focus();
            }
        }

        self.refresh_after_in_place_replacement();
    }

    /// Refresh cell contents after the in-place payload swap in
    /// [`Self::replace_rows`].
    ///
    /// Mutating the boxed row payloads in place (rather than replacing the
    /// objects) means GTK sees no `items-changed` for the reused prefix, so it
    /// rebinds none of those cells on its own. We therefore nudge the sorter
    /// and refresh the visible bindings by hand. This is the correctness half
    /// of the #226 playlist-switch fix — it is what makes the cheap in-place
    /// swap show the right data — so do not drop these refreshes when reworking
    /// `replace_rows`.
    fn refresh_after_in_place_replacement(&self) {
        let _guard = ApplyLayoutGuard::enter(self.applying_layout.clone());
        if let Some(sorter) = self.table.sorter() {
            let active_column_sort = sorter
                .downcast_ref::<gtk::ColumnViewSorter>()
                .and_then(gtk::ColumnViewSorter::primary_sort_column)
                .is_some();
            if active_column_sort {
                sorter.changed(gtk::SorterChange::Different);
            }
        }
        self.text_bindings.refresh_all();
        self.rating_bindings.refresh_all();
        self.status_bindings.refresh(self.playing_track_id.get());
        self.scroll_to_top();
    }

    pub(crate) fn summary_values(&self) -> (usize, u64, u64) {
        let mut track_count = 0usize;
        let mut duration_seconds = 0u64;
        let mut size_bytes = 0u64;
        for position in 0..self.store.n_items() {
            let Some(row_object) = self
                .store
                .item(position)
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                continue;
            };
            let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
                continue;
            };
            track_count += 1;
            duration_seconds += row.duration_seconds;
            size_bytes += row.file_size_bytes;
        }
        (track_count, duration_seconds, size_bytes)
    }

    /// Append structurally new rows without replacing the existing model.
    ///
    /// Import completion uses this path so freshly-added tracks can appear in
    /// Songs while the current scroll/focus/selection state survives. The
    /// wrapping `SortListModel` still places the new source rows according to
    /// the active column sort, but GTK receives an insertion event instead of
    /// a full remove-and-readd cycle.
    pub(crate) fn append_rows(&self, rows: Vec<TrackTableRow>) {
        if rows.is_empty() {
            return;
        }
        let additions: Vec<glib::BoxedAnyObject> =
            rows.into_iter().map(glib::BoxedAnyObject::new).collect();
        self.store.splice(self.store.n_items(), 0, &additions);
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
    ///
    /// Splices the whole batch under a single `items-changed` rather than
    /// appending row-by-row. When a column sort is active the wrapping
    /// `SortListModel` answers every `items-changed` by merging the new rows
    /// into sorted position — O(n) per event — so a per-row loop is O(n²) and
    /// stalls for seconds while the initial library publishes on a 10k
    /// library (the trap `replace_rows` documents). One splice per batch keeps
    /// the cost at one merge per batch instead of one per row.
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

    /// Updates every matching cached row in one store pass, without emitting
    /// `gio::ListModel::items-changed`.
    ///
    /// Batch metadata edits can touch dozens or hundreds of rows at once. A
    /// per-track loop over [`Self::update_row`] would rescan the visible model
    /// and refresh bound cells once per track; this keeps the same
    /// scroll-preserving mutation strategy while bounding the table work to one
    /// scan and one visible-cell refresh.
    pub(crate) fn update_rows(&self, new_rows: &HashMap<TrackId, TrackTableRow>) -> usize {
        if new_rows.is_empty() {
            return 0;
        }

        let mut updated = 0usize;
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
            let Some(new_row) = track_id.and_then(|track_id| new_rows.get(&track_id)) else {
                continue;
            };
            *row_object.borrow_mut::<TrackTableRow>() = new_row.clone();
            updated += 1;
        }

        if updated > 0 {
            self.text_bindings.refresh_all();
            self.rating_bindings.refresh_all();
            self.status_bindings.refresh(self.playing_track_id.get());
        }
        updated
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

    /// Anchor the viewport at the first row and move the keyboard cursor to
    /// it. No-op on an empty model.
    ///
    /// The initial library publish streams rows in id order with the sort
    /// detached, so while the list fills, track id 1 sits at the top and GTK
    /// adopts it as the viewport's scroll anchor. When the sort is reapplied at
    /// the end, the `ColumnView` keeps that anchored row in view and follows it
    /// to wherever it now sorts — a deterministic mid-list jump on every launch
    /// (#201). Calling this *after* the reapply re-anchors to row 0, so the
    /// view settles at the top intentionally instead of chasing track id 1.
    /// Issued as the last scroll request in the publish tick, it wins over GTK's
    /// implicit follow-the-anchor behaviour on the next layout pass.
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

    /// Detach the active sort ahead of a progressive bulk load, returning it
    /// for [`Self::reapply_sort`] to restore once every row is in place.
    ///
    /// Streaming thousands of rows into a sorted model merges each batch into
    /// position and visibly reshuffles the list as it fills. Detaching first
    /// lets the rows append at the tail, then a single re-sort settles them
    /// once. Guarded so neither the clear nor the later restore schedules a
    /// layout save.
    pub(crate) fn detach_sort_for_bulk_load(&self) -> Option<TrackColumnSort> {
        let _guard = ApplyLayoutGuard::enter(self.applying_layout.clone());
        let sort = read_current_sort(&self.table, &self.managed_columns);
        self.table
            .sort_by_column(None::<&gtk::ColumnViewColumn>, gtk::SortType::Ascending);
        sort
    }

    /// Restore a sort detached by [`Self::detach_sort_for_bulk_load`] in one
    /// pass over the now-populated model.
    pub(crate) fn reapply_sort(&self, sort: Option<TrackColumnSort>) {
        let _guard = ApplyLayoutGuard::enter(self.applying_layout.clone());
        self.apply_sort(sort.as_ref());
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

    // Sorting is deliberately synchronous — `SortListModel`'s `incremental`
    // mode stays off. The playlist table briefly opted in during the #226
    // freeze fix, but an incremental model presents a temporarily-unsorted
    // item order while the deferred sort runs, and `replace_rows`' in-place
    // payload swap depends on the post-swap sorter nudge for the displayed
    // order to be correct at all. The combination drew every playlist switch
    // as the OLD playlist's permutation over the new rows, then visibly
    // shuffled them into place across the following frames. A synchronous
    // resort settles inside the same main-loop dispatch, so the switch
    // renders atomically; it is affordable because row sort keys are stored
    // pre-collated and the play-order sorter compares plain integers — the
    // Songs view already takes this exact synchronous path on every column-
    // header click over the full library. `replace_rows_presents_play_order_
    // synchronously` pins the atomicity; re-measure a large playlist switch
    // before reintroducing any deferred sorting here.
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

#[cfg(test)]
mod tests {
    use sustain_app_runtime::TrackId;

    use super::*;

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
            // In the initial sort, the first three store objects occupy only
            // even display positions. Search reuses those objects for a
            // three-row result whose display positions are 0, 1, and 2.
            // GTK updates each ListItem's position without necessarily
            // rebinding its cell, so tinting only from `bind` leaves all three
            // rows carrying their pre-search even stripe.
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
                    cell_containing_label(&root, text).expect("visible search-result cell exists");
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

            assert_eq!(table.summary_values(), (2, 183, 3_500));
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
    fn superseded_hydration_reattaches_the_sort_without_persisting() {
        // Pins the load-bearing half of `publish_initial_library_rows`: the
        // deferred hydration detaches the sorter while it streams rows, and it
        // MUST hand the sorter back when it exits — even when a search has
        // superseded it. `replace_rows` (the search path) deliberately never
        // touches the sorter, so it relies on the steady-state invariant that a
        // sorter is always live; if the superseded hydration skipped the
        // reapply, the search's results would render unsorted and the user's
        // active sort would be silently dropped for the rest of the session.
        // Every step here is programmatic and guarded, so none of it may arm a
        // persistence write.
        crate::test_support::with_gtk(|| {
            let table = build_track_table(vec![row(1), row(2)], None, None, None, None, None);
            let sort = TrackColumnSort {
                column_id: "date_added".to_owned(),
                ascending: false,
            };
            let mut layout = read_current_layout(&table.table, &table.managed_columns);
            layout.sort = Some(sort.clone());
            table.apply_layout(&layout);

            // From here on, any attempt to persist is observable. The
            // `ApplyLayoutGuard` is the only thing that should keep the
            // programmatic detach/reapply below from arming a save.
            let saves = Rc::new(Cell::new(0u32));
            let saves_for_cb = saves.clone();
            table.set_layout_changed_callback(Rc::new(move |_layout: TrackColumnLayout| {
                saves_for_cb.set(saves_for_cb.get() + 1);
            }));

            // Hydration borrows the sorter and starts a progressive publish.
            let detached = table.detach_sort_for_bulk_load();
            assert_eq!(detached, Some(sort.clone()));
            assert!(
                read_current_sort(&table.table, &table.managed_columns).is_none(),
                "the sorter must be unset while rows stream in"
            );
            let generation = table.begin_progressive_replace();
            assert!(table.append_progressive_rows(generation, vec![row(1)]));

            // A search supersedes the load: `replace_rows` bumps the generation
            // and only asks an already-attached sorter to re-evaluate changed
            // row payloads. While hydration has the sorter detached, it stays
            // detached.
            table.replace_rows(vec![row(3), row(4)]);
            assert!(
                !table.append_progressive_rows(generation, vec![row(2)]),
                "the superseded load must report it no longer owns the model"
            );

            // The hydration loop exits and reapplies the sort unconditionally,
            // reattaching it over the search's own rows.
            table.reapply_sort(detached);
            assert_eq!(
                read_current_sort(&table.table, &table.managed_columns),
                Some(sort),
                "supersession must still reattach the sorter over the new rows"
            );

            // None of that guarded lifecycle may schedule a layout save.
            assert!(
                table.pending_save.borrow().is_none(),
                "the guarded detach/reapply must not arm a debounced save"
            );
            table.flush_pending_layout_save();
            assert_eq!(
                saves.get(),
                0,
                "programmatic sort restore must never persist"
            );
        });
    }

    /// Regression for the messy playlist-open rendering: with the play-order
    /// sort active, `replace_rows` must leave the sorted selection model
    /// presenting the new rows in their final order before it returns — i.e.
    /// before GTK can draw a frame. The in-place payload swap deliberately
    /// emits no `items-changed` for the reused prefix, so the wrapping
    /// `SortListModel` keeps the OLD playlist's permutation until the sorter
    /// nudge re-sorts; if that re-sort is deferred (incremental sorting), the
    /// switch draws the new rows scrambled and shuffles them into place over
    /// the following frames.
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

    fn row(id: i64) -> TrackTableRow {
        TrackTableRow {
            track_id: TrackId::new(id),
            track_name: format!("Track {id}"),
            artist: String::new(),
            album: String::new(),
            genre: String::new(),
            has_lyrics: false,
            // Sort keys are stored pre-collated (`normalize_sort_text`).
            track_name_sort_key: format!("track {id}"),
            artist_sort_key: String::new(),
            album_sort_key: String::new(),
            year: None,
            bpm: None,
            music_key: None,
            bitrate_kbps: None,
            file_type: row::AudioFileType::Unknown,
            duration_seconds: 0,
            rating: 0,
            plays: 0,
            skips: 0,
            last_played: None,
            last_skipped: None,
            date_added: None,
            track_number: None,
            file_size_bytes: 0,
            is_missing: false,
            playlist_position: None,
            group_band: None,
        }
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

    fn cell_containing_label(root: &gtk::Widget, text: &str) -> Option<gtk::Widget> {
        if let Some(label) = root.downcast_ref::<gtk::Label>()
            && label.text() == text
        {
            return root.parent();
        }
        let mut child = root.first_child();
        while let Some(widget) = child {
            if let Some(cell) = cell_containing_label(&widget, text) {
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
