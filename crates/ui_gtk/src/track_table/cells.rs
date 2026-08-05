// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{cell::Cell, rc::Rc};

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{Rating, TrackId};

use super::drag_drop::{RowReorderHooks, install_cell_drag_source, install_cell_drop_target};
use super::{
    RatingChangedCallback, columns::TrackTableColumn, inline_edit::InlineEditController,
    row::TrackTableRow,
};
use crate::cell_registry::{BindingRegistry, CellBinding, list_item_key};
use crate::track_context::TrackRowContextMenu;
use crate::util::sync_rating_button;

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
    cells: BindingRegistry<TrackTableContextCell>,
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
            cells: BindingRegistry::default(),
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
        self.cells.push(TrackTableContextCell {
            key: list_item_key(list_item),
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
        self.cells.find_map_live(|cell| {
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

struct TrackTableContextCell {
    key: usize,
    widget: glib::WeakRef<gtk::Widget>,
    list_item: glib::WeakRef<gtk::ListItem>,
}

impl CellBinding for TrackTableContextCell {
    fn key(&self) -> usize {
        self.key
    }

    fn list_item(&self) -> Option<gtk::ListItem> {
        self.list_item.upgrade()
    }

    fn is_live(&self) -> bool {
        self.widget.upgrade().is_some() && self.list_item.upgrade().is_some()
    }
}

struct TrackTableContextHit {
    widget: gtk::Widget,
    list_item: gtk::ListItem,
}

struct StatusBinding {
    key: usize,
    list_item: glib::WeakRef<gtk::ListItem>,
    icon: glib::WeakRef<gtk::Image>,
}

impl CellBinding for StatusBinding {
    fn key(&self) -> usize {
        self.key
    }

    fn list_item(&self) -> Option<gtk::ListItem> {
        self.list_item.upgrade()
    }

    fn is_live(&self) -> bool {
        self.list_item.upgrade().is_some() && self.icon.upgrade().is_some()
    }
}

#[derive(Clone, Default)]
pub(super) struct StatusBindings(BindingRegistry<StatusBinding>);

impl StatusBindings {
    pub(super) fn refresh(&self, playing_track_id: Option<TrackId>) {
        self.0.for_each_live(|binding| {
            let (Some(list_item), Some(icon)) = (binding.list_item(), binding.icon.upgrade())
            else {
                return;
            };
            refresh_status_icon(&list_item, &icon, playing_track_id);
        });
    }
}

struct TextBinding {
    key: usize,
    list_item: glib::WeakRef<gtk::ListItem>,
    label: glib::WeakRef<gtk::Label>,
    column: TrackTableColumn,
}

impl CellBinding for TextBinding {
    fn key(&self) -> usize {
        self.key
    }

    fn list_item(&self) -> Option<gtk::ListItem> {
        self.list_item.upgrade()
    }

    fn is_live(&self) -> bool {
        self.list_item.upgrade().is_some() && self.label.upgrade().is_some()
    }
}

#[derive(Clone, Default)]
pub(super) struct TextBindings(BindingRegistry<TextBinding>);

impl TextBindings {
    pub(super) fn refresh_track(&self, track_id: TrackId) {
        self.0
            .for_each_live(|binding| refresh_text_binding(binding, track_id));
    }

    pub(super) fn refresh_all(&self) {
        self.0.for_each_live(refresh_text_binding_value);
    }
}

struct RatingBinding {
    key: usize,
    list_item: glib::WeakRef<gtk::ListItem>,
    rating_box: glib::WeakRef<gtk::Box>,
}

impl CellBinding for RatingBinding {
    fn key(&self) -> usize {
        self.key
    }

    fn list_item(&self) -> Option<gtk::ListItem> {
        self.list_item.upgrade()
    }

    fn is_live(&self) -> bool {
        self.list_item.upgrade().is_some() && self.rating_box.upgrade().is_some()
    }
}

#[derive(Clone, Default)]
pub(super) struct RatingBindings(BindingRegistry<RatingBinding>);

impl RatingBindings {
    fn register(&self, list_item: &gtk::ListItem, rating_box: &gtk::Box) {
        self.0.replace(RatingBinding {
            key: list_item_key(list_item),
            list_item: list_item.downgrade(),
            rating_box: rating_box.downgrade(),
        });
    }

    pub(super) fn refresh_track(&self, track_id: TrackId) {
        self.0
            .for_each_live(|binding| refresh_rating_binding(binding, track_id));
    }

    pub(super) fn refresh_all(&self) {
        self.0.for_each_live(refresh_rating_binding_value);
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
    let context_for_setup = context_menu.clone();
    let reorder_for_setup = row_reorder.clone();
    let inline_edit_for_setup = inline_edit.clone();
    let editable_field = column.editable_field();
    let bindings_for_setup = bindings.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_cell(
            list_item,
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

        bindings_for_setup.0.push(TextBinding {
            key: list_item_key(list_item),
            list_item: list_item.downgrade(),
            label: label.downgrade(),
            column,
        });
    });

    // A cell about to be recycled to another row must not keep an open
    // editor: commit and close it first so the entry is gone before the
    // slot is reused. Unbind also precedes teardown, so this hook covers a
    // cell being destroyed mid-edit too — there is deliberately no
    // teardown hook (see `cell_registry`).
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
    let context_for_setup = context_menu.clone();
    let rating_changed_for_setup = rating_changed;
    let reorder_for_setup = row_reorder.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_cell(
            list_item,
            context_for_setup.as_ref(),
            reorder_for_setup.as_ref(),
        );
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

            let list_item_for_click = list_item.clone();
            let rating_box_for_click = rating_box.clone();
            let rating_changed_for_click = rating_changed_for_setup.clone();
            button.connect_clicked(move |_| {
                let Some(row_object) = list_item_for_click
                    .item()
                    .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
                else {
                    return;
                };
                let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
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
        list_item.set_child(Some(&cell));
    });

    let bindings_for_bind = bindings.clone();
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
        let Some(rating_box) = cell
            .first_child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        sync_rating_buttons(&rating_box, rating);
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
    let context_for_setup = context_menu.clone();
    let reorder_for_setup = row_reorder.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_cell(
            list_item,
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

        bindings_for_setup.0.push(StatusBinding {
            key: list_item_key(list_item),
            list_item: list_item.downgrade(),
            icon: icon.downgrade(),
        });
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
    let context_for_setup = context_menu.clone();
    let reorder_for_setup = row_reorder.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_cell(
            list_item,
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

/// Builds the `gtk::Box` every track-table cell factory shares: the
/// `track-table-cell` styling, fill expansion, and the row chrome wired
/// through [`install_cell_chrome`] (selection sync, context-menu
/// registration, drag source, reorder drop target). Each factory then
/// appends its own content — a label, rating stars, a status icon, or
/// nothing for the filler column.
fn new_track_cell(
    list_item: &gtk::ListItem,
    context_menu: Option<&TrackTableContextMenu>,
    row_reorder: Option<&RowReorderHooks>,
) -> gtk::Box {
    let cell = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    cell.add_css_class("track-table-cell");
    cell.set_hexpand(true);
    cell.set_vexpand(false);
    cell.set_halign(gtk::Align::Fill);
    cell.set_valign(gtk::Align::Fill);
    install_cell_chrome(list_item, &cell, context_menu, row_reorder);
    cell
}

fn install_cell_chrome(
    list_item: &gtk::ListItem,
    cell: &gtk::Box,
    context_menu: Option<&TrackTableContextMenu>,
    row_reorder: Option<&RowReorderHooks>,
) {
    install_cell_row_tint_sync(list_item, cell);
    install_cell_selection_sync(list_item, cell);
    if let Some(menu) = context_menu {
        menu.register_cell(list_item, cell);
        install_cell_drag_source(list_item, cell, &menu.selection);
    }
    if let Some(hooks) = row_reorder {
        install_cell_drop_target(list_item, cell, hooks.clone());
    }
}

fn install_cell_row_tint_sync(list_item: &gtk::ListItem, cell: &gtk::Box) {
    let cell_for_position = cell.clone();
    list_item.connect_position_notify(move |list_item| {
        apply_bound_row_tint(&cell_for_position, list_item);
    });
}

/// Collect selected track ids from the table selection in model order.
/// Rows without a track id are ignored.
pub(super) fn collect_selected_track_ids(selection: &gtk::MultiSelection) -> Vec<TrackId> {
    let bitset = selection.selection();
    let Some((iter, first)) = gtk::BitsetIter::init_first(&bitset) else {
        return Vec::new();
    };

    std::iter::once(first)
        .chain(iter)
        .filter_map(|position| row_track_id(selection.item(position)))
        .collect()
}

pub(crate) fn row_track_id(item: Option<glib::Object>) -> Option<TrackId> {
    let row_object = item?.downcast::<glib::BoxedAnyObject>().ok()?;
    let row = row_object.try_borrow::<TrackTableRow>().ok()?;
    row.track_id
}

fn refresh_text_binding(binding: &TextBinding, target_track_id: TrackId) {
    let (Some(list_item), Some(label)) = (binding.list_item(), binding.label.upgrade()) else {
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
    if row.track_id != Some(target_track_id) {
        return;
    }
    label.set_text(&binding.column.text(&row));
}

fn refresh_text_binding_value(binding: &TextBinding) {
    let (Some(list_item), Some(label)) = (binding.list_item(), binding.label.upgrade()) else {
        return;
    };
    let Some(row_object) = list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
    else {
        label.set_text("");
        return;
    };
    let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
        return;
    };
    label.set_text(&binding.column.text(&row));
}

fn refresh_rating_binding(binding: &RatingBinding, target_track_id: TrackId) {
    let Some(list_item) = binding.list_item() else {
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
    if row.track_id != Some(target_track_id) {
        return;
    }
    let rating = row.rating;
    drop(row);
    refresh_rating_box(binding, rating);
}

fn refresh_rating_binding_value(binding: &RatingBinding) {
    let Some(list_item) = binding.list_item() else {
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
    let rating = row.rating;
    drop(row);
    refresh_rating_box(binding, rating);
}

fn refresh_rating_box(binding: &RatingBinding, rating: u8) {
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
    use super::super::{RowDropPosition, drag_drop::drop_position_from_offset};
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
