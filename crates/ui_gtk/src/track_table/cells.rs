// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gdk, glib, graphene};
use sustain_app_runtime::{Rating, TrackId};

use super::{
    RatingChangedCallback, RowDropPosition, RowReorderCallback, RowReorderDrop,
    columns::TrackTableColumn, inline_edit::InlineEditController, row::TrackTableRow,
};
use crate::sidebar::{parse_tracks_payload, tracks_drag_payload};
use crate::track_context::TrackRowContextMenu;
use crate::util::sync_rating_button;

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

const MAX_RATING: u8 = 5;
const STATUS_COLUMN_WIDTH: i32 = 26;
const STATUS_ICON_SIZE: i32 = 14;
const STATUS_ICON_PLAYING: &str = "audio-volume-high-symbolic";
const STATUS_ICON_MISSING: &str = "dialog-warning-symbolic";

#[derive(Clone)]
pub(super) struct TrackTableContextMenu {
    menu: TrackRowContextMenu,
    selection: gtk::MultiSelection,
    popover_parent: glib::WeakRef<gtk::ColumnView>,
    cells: Rc<RefCell<Vec<TrackTableContextCell>>>,
}

impl TrackTableContextMenu {
    pub(super) fn new(
        menu: TrackRowContextMenu,
        selection: gtk::MultiSelection,
        popover_parent: gtk::ColumnView,
    ) -> Self {
        Self {
            menu,
            selection,
            popover_parent: popover_parent.downgrade(),
            cells: Rc::new(RefCell::new(Vec::new())),
        }
    }

    pub(super) fn install_controller(&self) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);

        let context = self.clone();
        gesture.connect_released(move |gesture, _n_press, x, y| {
            let Some(popover_parent) = context.popover_parent.upgrade() else {
                return;
            };
            let Some(hit) = context.cell_at(popover_parent.upcast_ref(), x, y) else {
                return;
            };

            gesture.set_state(gtk::EventSequenceState::Claimed);
            let position = hit.list_item.position();
            if position == gtk::INVALID_LIST_POSITION {
                return;
            }
            if !context.selection.is_selected(position) {
                context.selection.select_item(position, true);
            }

            let track_ids = collect_selected_track_ids(&context.selection);
            if track_ids.is_empty() {
                return;
            }
            context.menu.popup_at_parent(
                track_ids,
                context.ordered_track_ids(),
                &popover_parent,
                &hit.widget,
                x,
                y,
            );
        });
        if let Some(popover_parent) = self.popover_parent.upgrade() {
            popover_parent.add_controller(gesture);
        }
    }

    fn register_cell(&self, list_item: &gtk::ListItem, cell: &gtk::Box) {
        self.cells.borrow_mut().push(TrackTableContextCell {
            widget: cell.clone().upcast::<gtk::Widget>().downgrade(),
            list_item: list_item.downgrade(),
        });
    }

    fn ordered_track_ids(&self) -> Vec<TrackId> {
        super::ordered_track_ids(&self.selection)
    }

    fn cell_at(&self, event_widget: &gtk::Widget, x: f64, y: f64) -> Option<TrackTableContextHit> {
        let mut current = event_widget.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(widget) = current {
            if let Some(hit) = self.list_item_for_widget(&widget) {
                return Some(hit);
            }
            current = widget.parent();
        }
        None
    }

    fn list_item_for_widget(&self, widget: &gtk::Widget) -> Option<TrackTableContextHit> {
        let mut cells = self.cells.borrow_mut();
        cells.retain(|cell| cell.widget.upgrade().is_some() && cell.list_item.upgrade().is_some());
        cells.iter().find_map(|cell| {
            let registered = cell.widget.upgrade()?;
            if registered == *widget {
                Some(TrackTableContextHit {
                    widget: registered,
                    list_item: cell.list_item.upgrade()?,
                })
            } else {
                None
            }
        })
    }
}

#[derive(Clone)]
struct TrackTableContextCell {
    widget: glib::WeakRef<gtk::Widget>,
    list_item: glib::WeakRef<gtk::ListItem>,
}

struct TrackTableContextHit {
    widget: gtk::Widget,
    list_item: gtk::ListItem,
}

struct StatusBinding {
    list_item: gtk::ListItem,
    icon: gtk::Image,
}

#[derive(Clone, Default)]
pub(super) struct StatusBindings(Rc<RefCell<Vec<StatusBinding>>>);

impl StatusBindings {
    pub(super) fn refresh(&self, playing_track_id: Option<TrackId>) {
        for binding in self.0.borrow().iter() {
            refresh_status_icon(&binding.list_item, &binding.icon, playing_track_id);
        }
    }
}

struct TextBinding {
    list_item: gtk::ListItem,
    label: gtk::Label,
    column: TrackTableColumn,
}

#[derive(Clone, Default)]
pub(super) struct TextBindings(Rc<RefCell<Vec<TextBinding>>>);

impl TextBindings {
    pub(super) fn refresh_track(&self, track_id: TrackId) {
        let mut bindings = self.0.borrow_mut();
        bindings.retain(|binding| binding.list_item.item().is_some());
        for binding in bindings.iter() {
            refresh_text_binding(binding, track_id);
        }
    }
}

struct RatingBinding {
    list_item: gtk::ListItem,
    rating_box: glib::WeakRef<gtk::Box>,
}

#[derive(Clone, Default)]
pub(super) struct RatingBindings(Rc<RefCell<Vec<RatingBinding>>>);

impl RatingBindings {
    fn register(&self, list_item: &gtk::ListItem, rating_box: &gtk::Box) {
        let mut bindings = self.0.borrow_mut();
        bindings.retain(|binding| binding.list_item != *list_item);
        bindings.push(RatingBinding {
            list_item: list_item.clone(),
            rating_box: rating_box.downgrade(),
        });
    }

    pub(super) fn refresh_track(&self, track_id: TrackId) {
        let mut bindings = self.0.borrow_mut();
        bindings.retain(|binding| {
            binding.list_item.item().is_some() && binding.rating_box.upgrade().is_some()
        });
        for binding in bindings.iter() {
            refresh_rating_binding(binding, track_id);
        }
    }
}

pub(super) fn build_status_column(
    playing_track_id: Rc<Cell<Option<TrackId>>>,
    bindings: StatusBindings,
    context_menu: Option<TrackTableContextMenu>,
    row_reorder: Option<RowReorderHooks>,
) -> gtk::ColumnViewColumn {
    let factory = build_status_cell_factory(playing_track_id, bindings, context_menu, row_reorder);
    let table_column = gtk::ColumnViewColumn::new(None, Some(factory));
    table_column.set_resizable(false);
    table_column.set_fixed_width(STATUS_COLUMN_WIDTH);
    table_column.set_visible(true);
    table_column
}

pub(super) fn build_text_cell_factory(
    column: TrackTableColumn,
    bindings: TextBindings,
    context_menu: Option<TrackTableContextMenu>,
    row_reorder: Option<RowReorderHooks>,
    inline_edit: Option<InlineEditController>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let context_for_setup = context_menu;
    let reorder_for_setup = row_reorder;
    let inline_edit_for_setup = inline_edit.clone();
    let editable_field = column.editable_field();
    let bindings_for_setup = bindings.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        cell.add_css_class("track-table-cell");
        cell.set_hexpand(true);
        cell.set_vexpand(true);
        cell.set_halign(gtk::Align::Fill);
        cell.set_valign(gtk::Align::Fill);
        install_cell_chrome(
            list_item,
            &cell,
            context_for_setup.as_ref(),
            reorder_for_setup.as_ref(),
        );

        let label = gtk::Label::new(None);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_hexpand(true);
        label.set_valign(gtk::Align::Center);
        label.set_margin_start(8);
        label.set_margin_end(8);
        label.set_xalign(column.xalign());

        cell.append(&label);
        list_item.set_child(Some(&cell));

        // Editable columns on an inline-editing table arm a click gesture
        // and join the row's editable-cell registry (used for Tab hops).
        if let (Some(controller), Some(field)) = (inline_edit_for_setup.as_ref(), editable_field) {
            controller.register_editable_cell(list_item, &cell, field);
        }

        bindings_for_setup.0.borrow_mut().push(TextBinding {
            list_item: list_item.clone(),
            label,
            column,
        });
    });

    let bindings_for_teardown = bindings;
    factory.connect_teardown(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        bindings_for_teardown
            .0
            .borrow_mut()
            .retain(|binding| binding.list_item != *list_item);
    });

    // A cell about to be recycled to another row must not keep an open
    // editor: commit and close it first so the entry is gone before the
    // slot is reused.
    let inline_edit_for_unbind = inline_edit;
    factory.connect_unbind(move |_factory, item| {
        let Some(controller) = inline_edit_for_unbind.as_ref() else {
            return;
        };
        let Some(cell) = item
            .downcast_ref::<gtk::ListItem>()
            .and_then(|list_item| list_item.child())
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        controller.finish_if_editing_cell(&cell);
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
        apply_bound_row_tint(&cell, list_item);
        sync_row_selection_class(&cell, list_item.is_selected());

        let Some(label) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::Label>().ok())
        else {
            return;
        };
        let Some(row_object) = list_item
            .item()
            .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
        else {
            return;
        };
        let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
            return;
        };

        label.set_text(&column.text(&row));
    });

    factory
}

pub(super) fn build_rating_cell_factory(
    bindings: RatingBindings,
    context_menu: Option<TrackTableContextMenu>,
    rating_changed: Option<RatingChangedCallback>,
    row_reorder: Option<RowReorderHooks>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let context_for_setup = context_menu;
    let rating_changed_for_bind = rating_changed;
    let reorder_for_setup = row_reorder;
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        cell.add_css_class("track-table-cell");
        cell.set_hexpand(true);
        cell.set_vexpand(true);
        cell.set_halign(gtk::Align::Fill);
        cell.set_valign(gtk::Align::Fill);
        install_cell_chrome(
            list_item,
            &cell,
            context_for_setup.as_ref(),
            reorder_for_setup.as_ref(),
        );
        list_item.set_child(Some(&cell));
    });

    let bindings_for_teardown = bindings.clone();
    factory.connect_teardown(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        bindings_for_teardown
            .0
            .borrow_mut()
            .retain(|binding| binding.list_item != *list_item);
    });

    let bindings_for_bind = bindings;
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
        apply_bound_row_tint(&cell, list_item);
        sync_row_selection_class(&cell, list_item.is_selected());
        clear_box_children(&cell);

        let Some(row_object) = list_item
            .item()
            .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
        else {
            return;
        };
        let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
            return;
        };
        let rating = row.rating;
        drop(row);

        let rating_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        rating_box.add_css_class("rating-stars");
        rating_box.set_margin_start(6);
        rating_box.set_margin_end(6);
        rating_box.set_halign(gtk::Align::End);
        rating_box.set_valign(gtk::Align::Center);

        for star in 1..=MAX_RATING {
            let button = gtk::Button::with_label("");
            button.add_css_class("flat");
            button.add_css_class("rating-star");
            sync_rating_button(&button, star, rating);

            let row_object_for_click = row_object.clone();
            let rating_box_for_click = rating_box.clone();
            let rating_changed_for_click = rating_changed_for_bind.clone();
            button.connect_clicked(move |_| {
                let Ok(row) = row_object_for_click.try_borrow::<TrackTableRow>() else {
                    return;
                };
                let Some(track_id) = row.track_id else {
                    return;
                };
                let new_rating = rating_after_click(row.rating, star);
                drop(row);

                let Some(rating) = Rating::new(new_rating) else {
                    return;
                };
                let Some(rating_changed) = rating_changed_for_click.as_ref() else {
                    return;
                };

                if rating_changed(track_id, rating) {
                    sync_rating_buttons(&rating_box_for_click, new_rating);
                }
            });

            rating_box.append(&button);
        }

        cell.append(&rating_box);
        bindings_for_bind.register(list_item, &rating_box);
    });

    factory
}

pub(super) fn build_filler_column(
    context_menu: Option<TrackTableContextMenu>,
    row_reorder: Option<RowReorderHooks>,
) -> gtk::ColumnViewColumn {
    let table_column =
        gtk::ColumnViewColumn::new(None, Some(build_filler_factory(context_menu, row_reorder)));
    table_column.set_expand(true);
    table_column.set_resizable(false);
    table_column.set_visible(true);
    table_column
}

fn build_status_cell_factory(
    playing_track_id: Rc<Cell<Option<TrackId>>>,
    bindings: StatusBindings,
    context_menu: Option<TrackTableContextMenu>,
    row_reorder: Option<RowReorderHooks>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    let bindings_for_setup = bindings.clone();
    let context_for_setup = context_menu;
    let reorder_for_setup = row_reorder;
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        cell.add_css_class("track-table-cell");
        cell.set_hexpand(true);
        cell.set_vexpand(true);
        cell.set_halign(gtk::Align::Fill);
        cell.set_valign(gtk::Align::Fill);
        install_cell_chrome(
            list_item,
            &cell,
            context_for_setup.as_ref(),
            reorder_for_setup.as_ref(),
        );

        let icon = gtk::Image::new();
        icon.set_pixel_size(STATUS_ICON_SIZE);
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);
        icon.set_hexpand(true);
        icon.add_css_class("track-table-status-icon");
        cell.append(&icon);

        list_item.set_child(Some(&cell));

        bindings_for_setup.0.borrow_mut().push(StatusBinding {
            list_item: list_item.clone(),
            icon,
        });
    });

    let bindings_for_teardown = bindings;
    factory.connect_teardown(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        bindings_for_teardown
            .0
            .borrow_mut()
            .retain(|binding| binding.list_item != *list_item);
    });

    let playing_for_bind = playing_track_id;
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
        apply_bound_row_tint(&cell, list_item);
        sync_row_selection_class(&cell, list_item.is_selected());

        let Some(icon) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::Image>().ok())
        else {
            return;
        };
        refresh_status_icon(list_item, &icon, playing_for_bind.get());
    });

    factory
}

fn build_filler_factory(
    context_menu: Option<TrackTableContextMenu>,
    row_reorder: Option<RowReorderHooks>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let context_for_setup = context_menu;
    let reorder_for_setup = row_reorder;
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        cell.add_css_class("track-table-cell");
        cell.set_hexpand(true);
        cell.set_vexpand(true);
        cell.set_halign(gtk::Align::Fill);
        cell.set_valign(gtk::Align::Fill);
        install_cell_chrome(
            list_item,
            &cell,
            context_for_setup.as_ref(),
            reorder_for_setup.as_ref(),
        );
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
        apply_bound_row_tint(&cell, list_item);
        sync_row_selection_class(&cell, list_item.is_selected());
    });

    factory
}

fn install_cell_chrome(
    list_item: &gtk::ListItem,
    cell: &gtk::Box,
    context_menu: Option<&TrackTableContextMenu>,
    row_reorder: Option<&RowReorderHooks>,
) {
    install_cell_selection_sync(list_item, cell);
    if let Some(menu) = context_menu {
        menu.register_cell(list_item, cell);
        install_cell_drag_source(list_item, cell, &menu.selection);
    }
    if let Some(hooks) = row_reorder {
        install_cell_drop_target(list_item, cell, hooks.clone());
    }
}

/// Drop target wired only when the parent table opts into row reordering
/// (currently just the regular-playlist track table). Each cell is its own
/// drop target — GTK's ColumnView does not surface a single per-row widget
/// without a custom row factory — but the drop-indicator stripe is painted
/// across **every** sibling cell in the target row via the shared
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
fn install_cell_drop_target(list_item: &gtk::ListItem, cell: &gtk::Box, hooks: RowReorderHooks) {
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

fn drop_position_from_offset(y: f64, height: i32) -> RowDropPosition {
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

fn install_cell_drag_source(
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
fn build_drag_paintable(
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

fn collect_selected_track_ids(selection: &gtk::MultiSelection) -> Vec<TrackId> {
    let bitset = selection.selection();
    let Some((iter, first)) = gtk::BitsetIter::init_first(&bitset) else {
        return Vec::new();
    };

    std::iter::once(first)
        .chain(iter)
        .filter_map(|position| row_track_id(selection.item(position)))
        .collect()
}

fn row_track_id(item: Option<glib::Object>) -> Option<TrackId> {
    let row_object = item?.downcast::<glib::BoxedAnyObject>().ok()?;
    let row = row_object.try_borrow::<TrackTableRow>().ok()?;
    row.track_id
}

fn refresh_text_binding(binding: &TextBinding, target_track_id: TrackId) {
    let Some(row_object) = binding
        .list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
    else {
        return;
    };
    let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
        return;
    };
    if row.track_id != Some(target_track_id) {
        return;
    }
    binding.label.set_text(&binding.column.text(&row));
}

fn refresh_rating_binding(binding: &RatingBinding, target_track_id: TrackId) {
    let Some(row_object) = binding
        .list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
    else {
        return;
    };
    let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
        return;
    };
    if row.track_id != Some(target_track_id) {
        return;
    }
    let rating = row.rating;
    drop(row);

    let Some(rating_box) = binding.rating_box.upgrade() else {
        return;
    };
    sync_rating_buttons(&rating_box, rating);
}

fn refresh_status_icon(
    list_item: &gtk::ListItem,
    icon: &gtk::Image,
    playing_track_id: Option<TrackId>,
) {
    let Some(row_object) = list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
    else {
        clear_status_icon(icon);
        return;
    };
    let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
        clear_status_icon(icon);
        return;
    };

    icon.remove_css_class("track-table-status-playing");
    icon.remove_css_class("track-table-status-missing");

    if row.is_missing {
        icon.set_icon_name(Some(STATUS_ICON_MISSING));
        icon.add_css_class("track-table-status-missing");
        icon.set_visible(true);
        return;
    }

    if matches!(
        (row.track_id, playing_track_id),
        (Some(track_id), Some(playing_id)) if track_id == playing_id
    ) {
        icon.set_icon_name(Some(STATUS_ICON_PLAYING));
        icon.add_css_class("track-table-status-playing");
        icon.set_visible(true);
        return;
    }

    clear_status_icon(icon);
}

fn clear_status_icon(icon: &gtk::Image) {
    icon.set_icon_name(None);
    icon.set_visible(false);
}

fn apply_bound_row_tint(cell: &gtk::Box, list_item: &gtk::ListItem) {
    cell.remove_css_class("track-table-row-even");
    cell.remove_css_class("track-table-row-odd");
    let group_band = list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
        .and_then(|row| {
            row.try_borrow::<TrackTableRow>()
                .ok()
                .and_then(|row| row.group_band)
        });
    if group_band.unwrap_or_else(|| list_item.position() % 2 == 0) {
        cell.add_css_class("track-table-row-even");
    } else {
        cell.add_css_class("track-table-row-odd");
    }
}

fn install_cell_selection_sync(list_item: &gtk::ListItem, cell: &gtk::Box) {
    let cell_for_selection = cell.clone();
    list_item.connect_selected_notify(move |list_item| {
        sync_row_selection_class(&cell_for_selection, list_item.is_selected());
    });
    sync_row_selection_class(cell, list_item.is_selected());
}

fn sync_row_selection_class(cell: &gtk::Box, selected: bool) {
    if selected {
        cell.add_css_class("track-table-row-selected");
    } else {
        cell.remove_css_class("track-table-row-selected");
    }
}

fn clear_box_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn sync_rating_buttons(rating_box: &gtk::Box, rating: u8) {
    let mut star = 1;
    let mut child = rating_box.first_child();
    while let Some(widget) = child {
        let next_child = widget.next_sibling();
        if let Ok(button) = widget.downcast::<gtk::Button>() {
            sync_rating_button(&button, star, rating);
            star += 1;
        }
        child = next_child;
    }
}

fn rating_after_click(current_rating: u8, clicked_star: u8) -> u8 {
    let clicked_star = clicked_star.min(MAX_RATING);
    if current_rating == clicked_star {
        0
    } else {
        clicked_star
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clicking_a_different_star_sets_that_rating() {
        assert_eq!(rating_after_click(2, 4), 4);
    }

    #[test]
    fn clicking_the_current_rating_clears_rating_to_zero() {
        assert_eq!(rating_after_click(4, 4), 0);
    }

    #[test]
    fn rating_clicks_are_clamped_to_five_stars() {
        assert_eq!(rating_after_click(0, 9), 5);
    }

    #[test]
    fn drop_position_splits_row_height_in_half() {
        assert_eq!(drop_position_from_offset(4.0, 40), RowDropPosition::Above);
        assert_eq!(drop_position_from_offset(20.0, 40), RowDropPosition::Below);
        assert_eq!(drop_position_from_offset(39.9, 40), RowDropPosition::Below);
    }

    #[test]
    fn drop_position_falls_back_to_above_for_zero_height() {
        // A cell with no measured height (transient layout state) cannot
        // produce a meaningful split; defaulting to Above keeps motion
        // deterministic until the row size is known.
        assert_eq!(drop_position_from_offset(10.0, 0), RowDropPosition::Above);
    }
}
