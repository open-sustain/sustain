// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The box every track-table column puts in its rows.
//!
//! A cell knows the list item it belongs to, so the table's own pointer
//! controllers — context menu, drag source, reorder drop target — resolve
//! the row under the pointer from the widget GTK picks there. No cell
//! carries a controller of its own for them: the table builds and drops
//! cells by the hundred on every playlist switch, and per-cell
//! controllers made up a large share of that cost.

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TrackCell {
        pub(super) list_item: glib::WeakRef<gtk::ListItem>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TrackCell {
        const NAME: &'static str = "SustainTrackCell";
        type Type = super::TrackCell;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for TrackCell {}
    impl WidgetImpl for TrackCell {}
    impl BoxImpl for TrackCell {}
}

glib::wrapper! {
    pub struct TrackCell(ObjectSubclass<imp::TrackCell>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl TrackCell {
    /// A cell for `list_item`, which owns it once set as its child. The
    /// cell holds the list item weakly, so the two do not keep each other
    /// alive.
    pub(super) fn new(list_item: &gtk::ListItem) -> Self {
        let cell: Self = glib::Object::new();
        cell.imp().list_item.set(Some(list_item));
        cell.add_css_class("track-table-cell");
        cell.set_hexpand(true);
        cell.set_vexpand(false);
        cell.set_halign(gtk::Align::Fill);
        cell.set_valign(gtk::Align::Fill);
        cell
    }

    pub(super) fn list_item(&self) -> Option<gtk::ListItem> {
        self.imp().list_item.upgrade()
    }

    /// The cell under `(x, y)` in `table` coordinates, if the point lies
    /// on a row rather than on a header, a scrollbar or the empty space
    /// below the last row.
    pub(super) fn at(table: &gtk::ColumnView, x: f64, y: f64) -> Option<Self> {
        let mut current = table.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(widget) = current {
            if let Some(cell) = widget.downcast_ref::<Self>() {
                return Some(cell.clone());
            }
            current = widget.parent();
        }
        None
    }

    /// The table row this cell sits in: the `row` node GTK documents for
    /// a column view's rows (`columnview > listview > row > cell`).
    pub(super) fn row(&self) -> Option<gtk::Widget> {
        let mut current = self.parent();
        while let Some(widget) = current {
            if widget.css_name() == "row" {
                return Some(widget);
            }
            current = widget.parent();
        }
        None
    }
}
