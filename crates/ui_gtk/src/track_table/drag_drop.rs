// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gdk, glib, graphene};

use super::cells::{collect_selected_track_ids, row_track_id};
use super::{RowDropPosition, RowReorderCallback, RowReorderDrop};
use crate::sidebar::{parse_tracks_payload, tracks_drag_payload};

const ROW_DROP_ABOVE_CSS_CLASS: &str = "track-row-drop-above";
const ROW_DROP_BELOW_CSS_CLASS: &str = "track-row-drop-below";

/// Bundle the playlist track table threads through every cell so cell-level
/// drop targets can dispatch the runtime command ([`RowReorderHooks::drop`]),
/// gate themselves on the play-order sort being active
/// ([`RowReorderHooks::is_play_order_active`]), and coordinate a single
/// row-spanning visual indicator across all cells in the target row
/// ([`RowReorderHooks::cells`]). Empty / `None` on tables that don't opt
/// into within-table row reordering (Songs view, Albums tracklist).
#[derive(Clone)]
pub(super) struct RowReorderHooks {
    pub(super) drop: RowReorderCallback,
    pub(super) is_play_order_active: Rc<dyn Fn() -> bool>,
    pub(super) cells: RowDropCellRegistry,
}

/// Registry of every drop-target cell currently realised in the playlist
/// track table, used so a motion event on one cell can paint the indicator
/// stripe across all sibling cells in the same row. `ListItem::position`
/// (re-read live, not cached at register time) identifies which cells belong
/// to the target row; entries with a dead widget or list_item weak ref are
/// pruned on the next walk.
///
/// Painting is deduped: GTK fires `connect_motion` for every pixel of cursor
/// movement, so without a guard a steady cursor over one row would still
/// trigger O(cells) CSS class mutations per pixel, and GTK's style engine
/// would re-cascade every cell on every change. The `current_target` cell
/// records the last `(row_position, drop_position)` painted; repeated calls
/// for the same target return early without touching CSS.
#[derive(Clone, Default)]
pub(super) struct RowDropCellRegistry {
    cells: Rc<RefCell<Vec<RowDropCellEntry>>>,
    current_target: Rc<Cell<Option<(u32, RowDropPosition)>>>,
}

struct RowDropCellEntry {
    widget: glib::WeakRef<gtk::Widget>,
    list_item: glib::WeakRef<gtk::ListItem>,
}

impl RowDropCellRegistry {
    fn register(&self, list_item: &gtk::ListItem, cell: &gtk::Box) {
        self.cells.borrow_mut().push(RowDropCellEntry {
            widget: cell.clone().upcast::<gtk::Widget>().downgrade(),
            list_item: list_item.downgrade(),
        });
    }

    pub(super) fn unregister(&self, list_item: &gtk::ListItem) {
        self.cells.borrow_mut().retain(|entry| {
            entry
                .widget
                .upgrade()
                .inspect(|widget| {
                    widget.remove_css_class(ROW_DROP_ABOVE_CSS_CLASS);
                    widget.remove_css_class(ROW_DROP_BELOW_CSS_CLASS);
                })
                .is_some()
                && entry
                    .list_item
                    .upgrade()
                    .is_some_and(|registered| registered != *list_item)
        });
        self.current_target.set(None);
    }

    fn clear_all(&self) {
        if self.current_target.get().is_none() {
            return;
        }
        self.current_target.set(None);
        let mut cells = self.cells.borrow_mut();
        cells.retain(|entry| {
            entry.widget.upgrade().is_some() && entry.list_item.upgrade().is_some()
        });
        for entry in cells.iter() {
            let Some(widget) = entry.widget.upgrade() else {
                continue;
            };
            widget.remove_css_class(ROW_DROP_ABOVE_CSS_CLASS);
            widget.remove_css_class(ROW_DROP_BELOW_CSS_CLASS);
        }
    }

    fn paint_row(&self, row_position: u32, drop: RowDropPosition) {
        if self.current_target.get() == Some((row_position, drop)) {
            return;
        }
        // The previous target — if any — must shed its class before the new
        // one gets painted. Doing this here (instead of leaning on
        // `clear_all` to walk every cell again) keeps motion to a single
        // pass over the registry.
        let previous_class =
            self.current_target
                .get()
                .map(|(_, previous_drop)| match previous_drop {
                    RowDropPosition::Above => ROW_DROP_ABOVE_CSS_CLASS,
                    RowDropPosition::Below => ROW_DROP_BELOW_CSS_CLASS,
                });
        let new_class = match drop {
            RowDropPosition::Above => ROW_DROP_ABOVE_CSS_CLASS,
            RowDropPosition::Below => ROW_DROP_BELOW_CSS_CLASS,
        };
        let mut cells = self.cells.borrow_mut();
        cells.retain(|entry| {
            entry.widget.upgrade().is_some() && entry.list_item.upgrade().is_some()
        });
        for entry in cells.iter() {
            let (Some(widget), Some(list_item)) =
                (entry.widget.upgrade(), entry.list_item.upgrade())
            else {
                continue;
            };
            if let Some(previous) = previous_class {
                widget.remove_css_class(previous);
            }
            if list_item.position() == row_position {
                widget.add_css_class(new_class);
            }
        }
        self.current_target.set(Some((row_position, drop)));
    }
}

/// [`RowDropCellRegistry`] in [`RowReorderHooks`], so the visual matches
/// what the user expects.
///
/// The drop is parsed from the same `tracks:<ids>` payload
/// [`tracks_drag_payload`] emits, so cross-source drags (e.g. dragging from
/// the Songs view into a selected playlist) cannot accidentally enter this
/// path — they originate from a different table whose drag source does not
/// stamp the playlist context anyway, and the reorder callback (see
/// `main_window::playlist_row_reorder_callback`) authoritatively guards on
/// the sidebar's current selection.
///
/// Motion and drop are additionally gated on
/// [`RowReorderHooks::is_play_order_active`]: when the user has clicked any
/// header other than the status column, the table's row order no longer
/// matches `PlaylistEntry::position` and a drag-reorder gesture would have
/// no consistent insertion point. In that state the target rejects the
/// drag (no indicator, no drop event) — the user must click the status
/// column header to return to play-order before reordering. This mirrors
/// the iTunes 11 contract: drag-reorder is only meaningful in manual-order
/// view.
pub(super) fn install_cell_drop_target(
    list_item: &gtk::ListItem,
    cell: &gtk::Box,
    hooks: RowReorderHooks,
) {
    hooks.cells.register(list_item, cell);

    // The drag source advertises COPY (see install_cell_drag_source), and a
    // drop target must accept at least one action the source offers — only
    // the intersection of source/target masks actually fires connect_drop.
    // Accepting MOVE | COPY keeps us forward-compatible if a future caller
    // switches the drag source to MOVE.
    let drop_target = gtk::DropTarget::new(
        glib::Type::STRING,
        gdk::DragAction::MOVE | gdk::DragAction::COPY,
    );
    drop_target.set_preload(true);

    let list_item_for_motion = list_item.downgrade();
    let cell_for_motion = cell.clone();
    let hooks_for_motion = hooks.clone();
    drop_target.connect_motion(move |target, _x, y| {
        let Some(list_item) = list_item_for_motion.upgrade() else {
            return gdk::DragAction::empty();
        };
        if !(hooks_for_motion.is_play_order_active)() {
            hooks_for_motion.cells.clear_all();
            return gdk::DragAction::empty();
        }
        if drop_would_self_target(target, &list_item) {
            hooks_for_motion.cells.clear_all();
            return gdk::DragAction::empty();
        }
        let position = drop_position_from_offset(y, cell_for_motion.height());
        hooks_for_motion
            .cells
            .paint_row(list_item.position(), position);
        gdk::DragAction::COPY
    });

    let hooks_for_leave = hooks.clone();
    drop_target.connect_leave(move |_target| {
        hooks_for_leave.cells.clear_all();
    });

    let list_item_for_drop = list_item.downgrade();
    let cell_for_drop = cell.clone();
    let hooks_for_drop = hooks;
    drop_target.connect_drop(move |_target, value, _x, y| {
        hooks_for_drop.cells.clear_all();
        if !(hooks_for_drop.is_play_order_active)() {
            return false;
        }
        let Ok(text) = value.get::<String>() else {
            return false;
        };
        let Some(dragged_track_ids) = parse_tracks_payload(&text) else {
            return false;
        };
        let Some(list_item) = list_item_for_drop.upgrade() else {
            return false;
        };
        let Some(target_track_id) = row_track_id(list_item.item()) else {
            return false;
        };
        if dragged_track_ids.contains(&target_track_id) {
            // Dropping onto a source row would be either a no-op or a
            // logically ambiguous move; reject it so GTK does not report
            // a successful drag and the runtime never receives a pointless
            // command.
            return false;
        }
        let position = drop_position_from_offset(y, cell_for_drop.height());
        (hooks_for_drop.drop)(RowReorderDrop {
            dragged_track_ids,
            target_track_id,
            position,
        })
    });

    cell.add_controller(drop_target);
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

pub(super) fn install_cell_drag_source(
    list_item: &gtk::ListItem,
    cell: &gtk::Box,
    selection: &gtk::MultiSelection,
) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gdk::DragAction::COPY);
    drag_source.set_button(gdk::BUTTON_PRIMARY);

    let list_item = list_item.clone();
    let selection = selection.clone();
    let cell_for_prepare = cell.clone();
    drag_source.connect_prepare(move |source, _x, _y| {
        let position = list_item.position();
        let row_track_id = row_track_id(list_item.item())?;

        let track_ids = if position != gtk::INVALID_LIST_POSITION && selection.is_selected(position)
        {
            let mut selected = collect_selected_track_ids(&selection);
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

        if let Some(paintable) = build_drag_paintable(&cell_for_prepare, position, &selection) {
            source.set_icon(Some(&paintable), 0, 0);
        }

        Some(gdk::ContentProvider::for_value(
            &tracks_drag_payload(&track_ids).to_value(),
        ))
    });
    cell.add_controller(drag_source);
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
/// 1. [`find_listview_row`] assumes the ColumnView's row container has the CSS
///    node name `row`. Stable in GTK4 today but not a contract — if a future
///    GTK reworks the listview hierarchy, `find_listview_row` would return
///    `None` and we fall straight back to a missing-icon `None` here.
/// 2. [`visible_selected_row_widgets`] assumes sibling order in the row
///    container matches position order. True for `ListBase` virtualization in
///    GTK4, but there is no public per-widget position API to verify it; if
///    GTK ever recycles widgets out of order, we may stack the wrong rows.
/// 3. The composite calls `gdk::Paintable::snapshot` on a
///    [`gtk::WidgetPaintable`] wrapping a widget still parented inside the
///    live listview. WidgetPaintable is designed for this, but if a future
///    GTK refuses to paint widgets mid-virtualization, the composite returns
///    `None` and we fall back to the single-row paintable below.
pub(super) fn build_drag_paintable(
    cell: &gtk::Box,
    originating_position: u32,
    selection: &gtk::MultiSelection,
) -> Option<gdk::Paintable> {
    let origin_row = find_listview_row(cell)?;

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

/// Walk up the cell's parent chain to the ColumnView row container.
///
/// Risk: this depends on GTK4's convention that the row container has CSS
/// node name `row`. If a future GTK renames or restructures the listview
/// hierarchy this returns `None` and the drag falls back to no icon (cursor
/// without preview). Caller is responsible for the fallback.
fn find_listview_row(cell: &gtk::Box) -> Option<gtk::Widget> {
    let mut current: Option<gtk::Widget> = cell.parent();
    while let Some(widget) = current {
        if widget.css_name() == "row" {
            return Some(widget);
        }
        current = widget.parent();
    }
    None
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
