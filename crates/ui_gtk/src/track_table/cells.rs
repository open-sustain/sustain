// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{cell::Cell, collections::HashSet, rc::Rc};

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{Rating, TrackId};

use super::{
    RatingChangedCallback, columns::TrackTableColumn, inline_edit::InlineEditController,
    row::TrackTableRow,
};
use crate::cell_registry::{BindingRegistry, CellBinding};
use crate::rating_stars::RatingStars;
use crate::table_cell::TableCell;
use crate::track_context::TrackRowContextMenu;
use crate::util::set_exclusive_css_class;

const STATUS_COLUMN_WIDTH: i32 = 26;
const STATUS_ICON_SIZE: i32 = 14;
const STATUS_ICON_PLAYING: &str = "audio-volume-high-symbolic";
const STATUS_ICON_MISSING: &str = "dialog-warning-symbolic";

/// Pop `menu` up for the selected tracks on a secondary click on a row of
/// `table`, first selecting the clicked row if it is not part of the
/// selection.
pub(super) fn install_context_menu(
    table: &gtk::ColumnView,
    menu: TrackRowContextMenu,
    selection: gtk::MultiSelection,
) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);

    gesture.connect_released(move |gesture, _n_press, x, y| {
        let Some(table) = gesture.widget().and_downcast::<gtk::ColumnView>() else {
            return;
        };
        let Some(cell) = TableCell::at(&table, x, y) else {
            return;
        };
        let Some(list_item) = cell.list_item() else {
            return;
        };

        gesture.set_state(gtk::EventSequenceState::Claimed);
        let position = list_item.position();
        if position == gtk::INVALID_LIST_POSITION {
            return;
        }
        if !selection.is_selected(position) {
            selection.select_item(position, true);
        }

        let track_ids = collect_selected_track_ids(&selection);
        if track_ids.is_empty() {
            return;
        }
        menu.popup_at_parent(
            track_ids,
            super::ordered_track_ids(&selection),
            &table,
            &cell,
            x,
            y,
        );
    });
    table.add_controller(gesture);
}

/// Which bound cells a refresh re-derives from their row payload.
#[derive(Clone, Copy)]
pub(super) enum RefreshScope<'a> {
    /// Every bound cell: payloads moved between list items — a re-sort, a
    /// replacement swap — so any cell may now show another row.
    AllRows,
    /// The cells showing one of these tracks, whose payloads were updated
    /// in place. No other cell's content changed.
    Tracks(&'a HashSet<TrackId>),
}

impl RefreshScope<'_> {
    fn includes(self, list_item: &gtk::ListItem) -> bool {
        match self {
            Self::AllRows => true,
            Self::Tracks(track_ids) => {
                row_track_id(list_item.item()).is_some_and(|track_id| track_ids.contains(&track_id))
            }
        }
    }
}

struct StatusBinding {
    list_item: glib::WeakRef<gtk::ListItem>,
    icon: glib::WeakRef<gtk::Image>,
}

impl CellBinding for StatusBinding {
    type Live = (gtk::ListItem, gtk::Image);

    fn upgrade(&self) -> Option<Self::Live> {
        Some((self.list_item.upgrade()?, self.icon.upgrade()?))
    }
}

#[derive(Clone, Default)]
pub(super) struct StatusBindings(BindingRegistry<StatusBinding>);

impl StatusBindings {
    /// Re-derive every bound status icon: the playing track or a missing
    /// flag changed, the row payloads did not.
    pub(super) fn refresh_icons(&self, playing_track_id: Option<TrackId>) {
        self.0.for_each_live(|_, (list_item, icon)| {
            refresh_status_icon(&list_item, &icon, playing_track_id);
        });
    }

    /// Re-derive the status cells in `scope` from their row payload: the
    /// icon and the row tint.
    pub(super) fn refresh(&self, scope: RefreshScope<'_>, playing_track_id: Option<TrackId>) {
        self.0.for_each_live(|_, (list_item, icon)| {
            if scope.includes(&list_item) {
                refresh_status_icon(&list_item, &icon, playing_track_id);
                retint_cell_of(&icon, &list_item);
            }
        });
    }
}

struct TextBinding {
    list_item: glib::WeakRef<gtk::ListItem>,
    cell: glib::WeakRef<TableCell>,
    column: TrackTableColumn,
}

impl CellBinding for TextBinding {
    type Live = (gtk::ListItem, TableCell);

    fn upgrade(&self) -> Option<Self::Live> {
        Some((self.list_item.upgrade()?, self.cell.upgrade()?))
    }
}

#[derive(Clone, Default)]
pub(super) struct TextBindings(BindingRegistry<TextBinding>);

impl TextBindings {
    /// The list item of the row whose text cells sit inside `widget`. GTK
    /// does not expose which row holds keyboard focus; asking with the
    /// focused row widget answers that.
    pub(super) fn row_within(&self, widget: &gtk::Widget) -> Option<gtk::ListItem> {
        self.0
            .find_map_live(|_, (list_item, cell)| cell.is_ancestor(widget).then_some(list_item))
    }

    /// Re-derive the text cells in `scope` from their row payload.
    pub(super) fn refresh(&self, scope: RefreshScope<'_>) {
        self.0.for_each_live(|binding, (list_item, cell)| {
            if scope.includes(&list_item) {
                refresh_text_cell(&list_item, &cell, binding.column);
            }
        });
    }
}

struct RatingBinding {
    list_item: glib::WeakRef<gtk::ListItem>,
    stars: glib::WeakRef<RatingStars>,
}

impl CellBinding for RatingBinding {
    type Live = (gtk::ListItem, RatingStars);

    fn upgrade(&self) -> Option<Self::Live> {
        Some((self.list_item.upgrade()?, self.stars.upgrade()?))
    }
}

#[derive(Clone, Default)]
pub(super) struct RatingBindings(BindingRegistry<RatingBinding>);

impl RatingBindings {
    /// Re-derive the rating cells in `scope` from their row payload.
    pub(super) fn refresh(&self, scope: RefreshScope<'_>) {
        self.0.for_each_live(|_, (list_item, stars)| {
            if scope.includes(&list_item) {
                refresh_rating_cell(&list_item, &stars);
            }
        });
    }
}

pub(super) fn build_status_column(
    playing_track_id: Rc<Cell<Option<TrackId>>>,
    bindings: StatusBindings,
) -> gtk::ColumnViewColumn {
    let factory = build_status_cell_factory(playing_track_id, bindings);
    let table_column = gtk::ColumnViewColumn::new(None, Some(factory));
    table_column.set_resizable(false);
    table_column.set_fixed_width(STATUS_COLUMN_WIDTH);
    table_column.set_visible(true);
    table_column
}

pub(super) fn build_text_cell_factory(
    column: TrackTableColumn,
    bindings: TextBindings,
    inline_edit: Option<InlineEditController>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let inline_edit_for_setup = inline_edit.clone();
    let editable_field = column.editable_field();
    let bindings_for_setup = bindings.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_text_cell(list_item, column.xalign());
        list_item.set_child(Some(&cell));

        // Editable columns on an inline-editing table register their cells,
        // which the table's edit click and Tab hops look up.
        if let (Some(controller), Some(field)) = (inline_edit_for_setup.as_ref(), editable_field) {
            controller.register_editable_cell(list_item, &cell, field);
        }

        bindings_for_setup.0.push(TextBinding {
            list_item: list_item.downgrade(),
            cell: cell.downgrade(),
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
            .and_downcast::<TableCell>()
        else {
            return;
        };
        controller.finish_if_editing_cell(&cell);
    });

    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item.child().and_downcast::<TableCell>() else {
            return;
        };
        apply_bound_row_tint(&cell, list_item);

        let Some(row_object) = list_item
            .item()
            .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
        else {
            return;
        };
        let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
            return;
        };

        cell.set_text(&column.text(&row));
    });

    factory
}

pub(super) fn build_rating_cell_factory(
    bindings: RatingBindings,
    rating_changed: Option<RatingChangedCallback>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let rating_changed_for_setup = rating_changed;
    let bindings_for_setup = bindings;
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_cell(list_item);
        let stars = RatingStars::new();
        stars.set_margin_start(6);
        stars.set_margin_end(6);
        stars.set_halign(gtk::Align::End);
        stars.set_valign(gtk::Align::Center);
        if let Some(rating_changed) = rating_changed_for_setup.clone() {
            let list_item = list_item.downgrade();
            stars.connect_rating_chosen(move |stars, rating| {
                let Some(track_id) = list_item
                    .upgrade()
                    .and_then(|list_item| row_track_id(list_item.item()))
                else {
                    return;
                };
                if rating_changed(track_id, rating) {
                    stars.set_rating(rating);
                }
            });
        }

        cell.append(&stars);
        list_item.set_child(Some(&cell));
        bindings_for_setup.0.push(RatingBinding {
            list_item: list_item.downgrade(),
            stars: stars.downgrade(),
        });
    });

    factory.connect_bind(|_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item.child().and_downcast::<TableCell>() else {
            return;
        };
        apply_bound_row_tint(&cell, list_item);
        if let Some(stars) = cell.first_child().and_downcast::<RatingStars>() {
            stars.set_rating(row_rating(list_item.item()));
        }
    });

    factory
}

pub(super) fn build_filler_column() -> gtk::ColumnViewColumn {
    let table_column = gtk::ColumnViewColumn::new(None, Some(build_filler_factory()));
    table_column.set_expand(true);
    table_column.set_resizable(false);
    table_column.set_visible(true);
    table_column
}

fn build_status_cell_factory(
    playing_track_id: Rc<Cell<Option<TrackId>>>,
    bindings: StatusBindings,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    let bindings_for_setup = bindings.clone();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_cell(list_item);

        let icon = gtk::Image::new();
        icon.set_pixel_size(STATUS_ICON_SIZE);
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);
        icon.set_hexpand(true);
        icon.add_css_class("track-table-status-icon");
        cell.append(&icon);

        list_item.set_child(Some(&cell));

        bindings_for_setup.0.push(StatusBinding {
            list_item: list_item.downgrade(),
            icon: icon.downgrade(),
        });
    });

    let playing_for_bind = playing_track_id;
    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item.child().and_downcast::<TableCell>() else {
            return;
        };
        apply_bound_row_tint(&cell, list_item);

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

fn build_filler_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let cell = new_track_cell(list_item);
        list_item.set_child(Some(&cell));
    });

    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(cell) = list_item.child().and_downcast::<TableCell>() else {
            return;
        };
        apply_bound_row_tint(&cell, list_item);
    });

    factory
}

/// A track-table cell that shows the widget its factory appends — rating
/// stars, a status icon, or nothing for the filler column.
fn new_track_cell(list_item: &gtk::ListItem) -> TableCell {
    track_cell(list_item, TableCell::new(list_item))
}

/// A track-table cell that shows a text aligned to `xalign`.
fn new_track_text_cell(list_item: &gtk::ListItem, xalign: f32) -> TableCell {
    track_cell(list_item, TableCell::with_text(list_item, xalign))
}

/// `cell`, kept tinted along with its row. A selected row needs nothing
/// from its cells: the stylesheet reads the row's own `:selected` state.
fn track_cell(list_item: &gtk::ListItem, cell: TableCell) -> TableCell {
    cell.add_css_class("track-table-cell");
    install_cell_row_tint_sync(list_item, &cell);
    cell
}

fn install_cell_row_tint_sync(list_item: &gtk::ListItem, cell: &TableCell) {
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

fn refresh_text_cell(list_item: &gtk::ListItem, cell: &TableCell, column: TrackTableColumn) {
    let Some(row_object) = list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
    else {
        cell.set_text("");
        return;
    };
    let Ok(row) = row_object.try_borrow::<TrackTableRow>() else {
        return;
    };
    cell.set_text(&column.text(&row));
    drop(row);
    apply_bound_row_tint(cell, list_item);
}

fn row_rating(item: Option<glib::Object>) -> Rating {
    item.and_downcast::<glib::BoxedAnyObject>()
        .and_then(|row_object| {
            let row = row_object.try_borrow::<TrackTableRow>().ok()?;
            Rating::new(row.rating)
        })
        .unwrap_or_else(Rating::unrated)
}

fn refresh_rating_cell(list_item: &gtk::ListItem, stars: &RatingStars) {
    stars.set_rating(row_rating(list_item.item()));
    retint_cell_of(stars, list_item);
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

    let shown = if row.is_missing {
        Some((STATUS_ICON_MISSING, "track-table-status-missing"))
    } else if matches!(
        (row.track_id, playing_track_id),
        (Some(track_id), Some(playing_id)) if track_id == playing_id
    ) {
        Some((STATUS_ICON_PLAYING, "track-table-status-playing"))
    } else {
        None
    };
    drop(row);
    match shown {
        Some((icon_name, class)) => show_status_icon(icon, icon_name, class),
        None => clear_status_icon(icon),
    }
}

const STATUS_ICON_CLASSES: [&str; 2] = ["track-table-status-missing", "track-table-status-playing"];

/// Show `icon_name` styled by `class`. Only what differs from the icon's
/// current state is touched: setting an icon name re-resolves the icon
/// and resizes the image even when the name is unchanged.
fn show_status_icon(icon: &gtk::Image, icon_name: &str, class: &str) {
    for other in STATUS_ICON_CLASSES
        .into_iter()
        .filter(|other| *other != class)
    {
        icon.remove_css_class(other);
    }
    icon.add_css_class(class);
    if icon.icon_name().as_deref() != Some(icon_name) {
        icon.set_icon_name(Some(icon_name));
    }
    icon.set_visible(true);
}

fn clear_status_icon(icon: &gtk::Image) {
    for class in STATUS_ICON_CLASSES {
        icon.remove_css_class(class);
    }
    if icon.icon_name().is_some() {
        icon.set_icon_name(None);
    }
    icon.set_visible(false);
}

/// Re-apply the row tint to the track cell holding `content`, whose row
/// payload may have changed in place (a Duplicates group band, say).
fn retint_cell_of(content: &impl IsA<gtk::Widget>, list_item: &gtk::ListItem) {
    if let Some(cell) = content.parent().and_downcast::<TableCell>() {
        apply_bound_row_tint(&cell, list_item);
    }
}

fn apply_bound_row_tint(cell: &TableCell, list_item: &gtk::ListItem) {
    let group_band = list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
        .and_then(|row| {
            row.try_borrow::<TrackTableRow>()
                .ok()
                .and_then(|row| row.placement.group_band)
        });
    if group_band.unwrap_or_else(|| list_item.position().is_multiple_of(2)) {
        set_exclusive_css_class(cell, "track-table-row-even", "track-table-row-odd");
    } else {
        set_exclusive_css_class(cell, "track-table-row-odd", "track-table-row-even");
    }
}

#[cfg(test)]
mod tests {
    use super::super::{RowDropPosition, drag_drop::drop_position_from_offset};

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
