// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{cell::RefCell, rc::Rc};

use gtk::prelude::*;
use gtk::{gdk, glib, graphene};

use super::cells::{collect_selected_track_ids, row_track_id};
use super::track_cell::TrackCell;
use super::{RowDropPosition, RowReorderCallback, RowReorderDrop};
use crate::sidebar::{parse_tracks_payload, tracks_drag_payload};

const ROW_DROP_ABOVE_CSS_CLASS: &str = "track-row-drop-above";
const ROW_DROP_BELOW_CSS_CLASS: &str = "track-row-drop-below";

/// What the playlist track table needs to reorder its rows by drag and
/// drop: the runtime command a drop dispatches, and the play-order column,
/// whose sort is the only one a drop position means anything in. `None`
/// on tables that don't reorder their rows (Songs view, Albums tracklist).
pub(super) struct RowReorderHooks {
    pub(super) drop: RowReorderCallback,
    pub(super) play_order_column: gtk::ColumnViewColumn,
}

impl RowReorderHooks {
    /// Whether the rows show in play order. Under any other sort the row
    /// order no longer matches the playlist's, so a drop between two rows
    /// has no consistent insertion point.
    fn is_play_order_active(&self, table: &gtk::ColumnView) -> bool {
        table
            .sorter()
            .and_downcast::<gtk::ColumnViewSorter>()
            .and_then(|sorter| sorter.primary_sort_column())
            .is_some_and(|active| active == self.play_order_column)
    }
}

/// The row a drop would land next to, marked by a stripe along its top or
/// bottom edge. The mark is a CSS class on the row widget, which every
/// cell of the row picks up (see `app.css`).
///
/// Drag motion arrives for every pixel the pointer moves, so the marked
/// row and edge are kept to skip restyling while they stay the same.
#[derive(Default)]
struct RowDropIndicator {
    marked: RefCell<Option<(glib::WeakRef<gtk::Widget>, RowDropPosition)>>,
}

impl RowDropIndicator {
    fn mark(&self, row: &gtk::Widget, position: RowDropPosition) {
        let mut marked = self.marked.borrow_mut();
        if let Some((marked_row, marked_position)) = marked.as_ref()
            && marked_row.upgrade().as_ref() == Some(row)
            && *marked_position == position
        {
            return;
        }
        if let Some((marked_row, _)) = marked.take() {
            clear_row_drop_classes(marked_row.upgrade().as_ref());
        }
        let (class, other) = match position {
            RowDropPosition::Above => (ROW_DROP_ABOVE_CSS_CLASS, ROW_DROP_BELOW_CSS_CLASS),
            RowDropPosition::Below => (ROW_DROP_BELOW_CSS_CLASS, ROW_DROP_ABOVE_CSS_CLASS),
        };
        row.remove_css_class(other);
        row.add_css_class(class);
        *marked = Some((row.downgrade(), position));
    }

    fn clear(&self) {
        if let Some((marked_row, _)) = self.marked.take() {
            clear_row_drop_classes(marked_row.upgrade().as_ref());
        }
    }
}

fn clear_row_drop_classes(row: Option<&gtk::Widget>) {
    if let Some(row) = row {
        row.remove_css_class(ROW_DROP_ABOVE_CSS_CLASS);
        row.remove_css_class(ROW_DROP_BELOW_CSS_CLASS);
    }
}

/// Where a drop on `table` at `(x, y)` would land: the cell under the
/// pointer, and whether the pointer is in the upper or lower half of it.
fn drop_point(table: &gtk::ColumnView, x: f64, y: f64) -> Option<(TrackCell, RowDropPosition)> {
    let cell = TrackCell::at(table, x, y)?;
    let point = table.compute_point(&cell, &graphene::Point::new(x as f32, y as f32))?;
    let position = drop_position_from_offset(f64::from(point.y()), cell.height());
    Some((cell, position))
}

/// Make `table` take drops of its own rows, dispatched to
/// [`RowReorderHooks::drop`] with the row the pointer is on and the half
/// of it, and marked while the drag moves by a stripe on that row.
///
/// The drop is parsed from the same `tracks:<ids>` payload
/// [`tracks_drag_payload`] emits. A drag from another table (e.g. the
/// Songs view into a selected playlist) carries the same payload; the
/// reorder callback (see `main_window::playlist_row_reorder_callback`)
/// authoritatively guards on the sidebar's current selection.
///
/// The table rejects the drag — no stripe, no drop — while any column but
/// the play-order one sorts it: the user returns to play order by clicking
/// that column's header before reordering. This mirrors the iTunes 11
/// contract: drag-reorder is only meaningful in manual-order view.
pub(super) fn install_row_reorder_drop_target(table: &gtk::ColumnView, hooks: RowReorderHooks) {
    // The drag source advertises COPY (see install_tracks_drag_source), and
    // a drop target must accept at least one action the source offers —
    // only the intersection of source/target masks actually fires
    // connect_drop. Accepting MOVE | COPY keeps us forward-compatible if a
    // future caller switches the drag source to MOVE.
    let drop_target = gtk::DropTarget::new(
        glib::Type::STRING,
        gdk::DragAction::MOVE | gdk::DragAction::COPY,
    );
    drop_target.set_preload(true);

    let hooks = Rc::new(hooks);
    let indicator = Rc::new(RowDropIndicator::default());

    let hooks_for_motion = hooks.clone();
    let indicator_for_motion = indicator.clone();
    drop_target.connect_motion(move |target, x, y| {
        let Some(table) = target.widget().and_downcast::<gtk::ColumnView>() else {
            return gdk::DragAction::empty();
        };
        let marked_row = hooks_for_motion
            .is_play_order_active(&table)
            .then(|| drop_point(&table, x, y))
            .flatten()
            .and_then(|(cell, position)| {
                let list_item = cell.list_item()?;
                if drop_would_self_target(target, &list_item) {
                    return None;
                }
                Some((cell.row()?, position))
            });
        match marked_row {
            Some((row, position)) => {
                indicator_for_motion.mark(&row, position);
                gdk::DragAction::COPY
            }
            None => {
                indicator_for_motion.clear();
                gdk::DragAction::empty()
            }
        }
    });

    let indicator_for_leave = indicator.clone();
    drop_target.connect_leave(move |_target| {
        indicator_for_leave.clear();
    });

    drop_target.connect_drop(move |target, value, x, y| {
        indicator.clear();
        let Some(table) = target.widget().and_downcast::<gtk::ColumnView>() else {
            return false;
        };
        if !hooks.is_play_order_active(&table) {
            return false;
        }
        let Ok(text) = value.get::<String>() else {
            return false;
        };
        let Some(dragged_track_ids) = parse_tracks_payload(&text) else {
            return false;
        };
        let Some((cell, position)) = drop_point(&table, x, y) else {
            return false;
        };
        let Some(target_track_id) = cell
            .list_item()
            .and_then(|list_item| row_track_id(list_item.item()))
        else {
            return false;
        };
        if dragged_track_ids.contains(&target_track_id) {
            // Dropping onto a source row would be either a no-op or a
            // logically ambiguous move; reject it so GTK does not report
            // a successful drag and the runtime never receives a pointless
            // command.
            return false;
        }
        (hooks.drop)(RowReorderDrop {
            dragged_track_ids,
            target_track_id,
            position,
        })
    });

    table.add_controller(drop_target);
}

pub(super) fn drop_position_from_offset(y: f64, height: i32) -> RowDropPosition {
    if height <= 0 {
        return RowDropPosition::Above;
    }
    if y < f64::from(height) / 2.0 {
        RowDropPosition::Above
    } else {
        RowDropPosition::Below
    }
}

/// True when the in-flight drag's payload contains the target row's own
/// track id — i.e. the user is dragging a row over itself. Using the
/// drop target's preloaded value avoids dispatching motion-driven visual
/// state for impossible drops.
fn drop_would_self_target(target: &gtk::DropTarget, list_item: &gtk::ListItem) -> bool {
    let Some(value) = target.value() else {
        return false;
    };
    let Ok(text) = value.get::<String>() else {
        return false;
    };
    let Some(track_ids) = parse_tracks_payload(&text) else {
        return false;
    };
    let Some(target_track_id) = row_track_id(list_item.item()) else {
        return false;
    };
    track_ids.contains(&target_track_id)
}

/// Make `table`'s rows draggable: a drag from a row carries the selected
/// tracks when the row is selected, and the row's own track otherwise.
pub(super) fn install_tracks_drag_source(table: &gtk::ColumnView, selection: &gtk::MultiSelection) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gdk::DragAction::COPY);
    drag_source.set_button(gdk::BUTTON_PRIMARY);

    let selection = selection.clone();
    drag_source.connect_prepare(move |source, x, y| {
        let table = source.widget().and_downcast::<gtk::ColumnView>()?;
        let Some(cell) = TrackCell::at(&table, x, y) else {
            // A press off the rows — on a header, a scrollbar — never
            // drags tracks; stop asking for the rest of this press.
            source.set_state(gtk::EventSequenceState::Denied);
            return None;
        };
        prepare_tracks_drag(source, &cell, &selection)
    });
    table.add_controller(drag_source);
}

fn prepare_tracks_drag(
    source: &gtk::DragSource,
    cell: &TrackCell,
    selection: &gtk::MultiSelection,
) -> Option<gdk::ContentProvider> {
    let list_item = cell.list_item()?;
    let position = list_item.position();
    let row_track_id = row_track_id(list_item.item())?;

    let track_ids = if position != gtk::INVALID_LIST_POSITION && selection.is_selected(position) {
        let mut selected = collect_selected_track_ids(selection);
        if !selected.contains(&row_track_id) {
            selected.push(row_track_id);
        }
        selected
    } else {
        vec![row_track_id]
    };

    if track_ids.is_empty() {
        return None;
    }

    if let Some(paintable) = build_drag_paintable(cell, position, selection) {
        source.set_icon(Some(&paintable), 0, 0);
    }

    Some(gdk::ContentProvider::for_value(
        &tracks_drag_payload(&track_ids).to_value(),
    ))
}

/// Build the drag image. Single-track drags use a [`gtk::WidgetPaintable`] of
/// the originating row so the row image follows the cursor. Multi-track drags
/// composite the visible selected rows into a stacked snapshot via
/// `gtk::Snapshot::to_paintable`.
///
/// The multi-row composite leans on three implicit GTK4 invariants. If any of
/// them ever stops holding, the originating row's plain [`gtk::WidgetPaintable`]
/// is returned as a graceful fallback (the icon will still follow the cursor;
/// it just won't show the stack).
///
/// 1. [`TrackCell::row`] assumes the ColumnView's row container has the CSS
///    node name `row`. GTK documents that node, but if a future GTK
///    reworks the listview hierarchy, `TrackCell::row` would return `None`
///    and we fall straight back to a missing-icon `None` here.
/// 2. [`visible_selected_row_widgets`] assumes sibling order in the row
///    container matches position order. True for `ListBase` virtualization in
///    GTK4, but there is no public per-widget position API to verify it; if
///    GTK ever recycles widgets out of order, we may stack the wrong rows.
/// 3. The composite calls `gdk::Paintable::snapshot` on a
///    [`gtk::WidgetPaintable`] wrapping a widget still parented inside the
///    live listview. WidgetPaintable is designed for this, but if a future
///    GTK refuses to paint widgets mid-virtualization, the composite returns
///    `None` and we fall back to the single-row paintable below.
fn build_drag_paintable(
    cell: &TrackCell,
    originating_position: u32,
    selection: &gtk::MultiSelection,
) -> Option<gdk::Paintable> {
    let origin_row = cell.row()?;

    if originating_position == gtk::INVALID_LIST_POSITION
        || !selection.is_selected(originating_position)
    {
        return Some(gtk::WidgetPaintable::new(Some(&origin_row)).upcast());
    }

    let selected_rows = visible_selected_row_widgets(&origin_row, originating_position, selection);

    if selected_rows.len() <= 1 {
        return Some(gtk::WidgetPaintable::new(Some(&origin_row)).upcast());
    }

    compose_stacked_row_paintable(&selected_rows)
        .or_else(|| Some(gtk::WidgetPaintable::new(Some(&origin_row)).upcast()))
}

/// Walk the row container's children in both directions from `origin`, gathering
/// row widgets whose positions belong to the current selection. Sibling order
/// matches position order in GTK4 `ListBase`, so we infer each sibling's position
/// from its offset relative to the originating row instead of asking the
/// widget — there's no public per-widget position API today.
///
/// Risk: if GTK ever recycles row widgets out of position order (e.g. as part
/// of an aggressive virtualization rework), our computed positions would be
/// wrong and we'd stack the wrong rows in the drag icon. The drag payload
/// (which is built independently from the selection model) is unaffected.
fn visible_selected_row_widgets(
    origin: &gtk::Widget,
    origin_position: u32,
    selection: &gtk::MultiSelection,
) -> Vec<gtk::Widget> {
    let mut collected: Vec<(u32, gtk::Widget)> = vec![(origin_position, origin.clone())];

    let mut position = origin_position;
    let mut current = origin.next_sibling();
    while let Some(sibling) = current {
        position = position.saturating_add(1);
        if selection.is_selected(position) {
            collected.push((position, sibling.clone()));
        }
        current = sibling.next_sibling();
    }

    let mut position = origin_position;
    let mut current = origin.prev_sibling();
    while let Some(sibling) = current {
        if position == 0 {
            break;
        }
        position -= 1;
        if selection.is_selected(position) {
            collected.push((position, sibling.clone()));
        }
        current = sibling.prev_sibling();
    }

    collected.sort_by_key(|(p, _)| *p);
    collected.into_iter().map(|(_, widget)| widget).collect()
}

/// Compose `rows` into a single vertically stacked paintable.
///
/// Risk: each row is painted via a [`gtk::WidgetPaintable`] that still
/// references the live row widget inside the listview. WidgetPaintable is
/// designed for exactly this use, but if a future GTK refuses to paint
/// widgets mid-virtualization the call quietly produces a blank or `None`
/// paintable. `build_drag_paintable` falls back to the originating row's
/// single paintable in that case, so the drag still shows *something*.
///
/// Returns `None` if any dimension is zero (rows weren't laid out yet) so
/// the caller can fall back instead of producing an invalid icon.
fn compose_stacked_row_paintable(rows: &[gtk::Widget]) -> Option<gdk::Paintable> {
    let widths: Vec<f32> = rows.iter().map(|row| row.width() as f32).collect();
    let heights: Vec<f32> = rows.iter().map(|row| row.height() as f32).collect();
    let total_width = widths.iter().copied().fold(0.0_f32, f32::max);
    let total_height: f32 = heights.iter().sum();
    if total_width <= 0.0 || total_height <= 0.0 {
        return None;
    }

    let snapshot = gtk::Snapshot::new();
    let mut y_offset = 0.0_f32;
    for (row, height) in rows.iter().zip(heights.iter().copied()) {
        let width = row.width() as f64;
        let paintable = gtk::WidgetPaintable::new(Some(row));
        snapshot.translate(&graphene::Point::new(0.0, y_offset));
        paintable.snapshot(snapshot.upcast_ref::<gdk::Snapshot>(), width, height as f64);
        snapshot.translate(&graphene::Point::new(0.0, -y_offset));
        y_offset += height;
    }

    snapshot.to_paintable(Some(&graphene::Size::new(total_width, total_height)))
}
