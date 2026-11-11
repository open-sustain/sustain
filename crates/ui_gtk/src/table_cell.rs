// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The widget a list table puts in each of its cells.
//!
//! A table realizes a couple of hundred rows of a dozen cells each, and
//! builds or drops them by the hundred on every playlist switch, so one
//! widget does everything a cell needs:
//!
//! - It draws its text itself. Its size follows its font, never its text,
//!   so a new text — a search re-texting every realized cell — only
//!   redraws it. The text is laid out at its first draw, so the rows
//!   realized off screen never shape any.
//! - It holds the widgets of the cells that show something else — a status
//!   icon, rating stars, a check box — and the inline editor while the
//!   user edits its text. Children cover the whole cell and replace the
//!   text. It also hosts the row's context menu, which covers nothing.
//! - It knows the list item it belongs to, so a table's own pointer
//!   controllers resolve the row under the pointer from the widget GTK
//!   picks there, and no cell carries controllers of its own for them.
//! - It shades its row's band of a striped table itself. A band is no CSS
//!   class: scrolling rebinds the recycled rows to new positions, and a
//!   class flipped on every cell of each of them restyles them all, frame
//!   after frame. A band flipped on a cell only redraws it.
//!
//! The text takes the cell's CSS `color` (node name `table-cell`).

use std::cell::{Cell, RefCell};

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, pango};

/// Space between a text and the cell's edges.
const TEXT_MARGIN: i32 = 8;

/// Characters a text cell is always wide enough for.
const MIN_TEXT_CHARS: i32 = 3;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TableCell {
        pub(super) list_item: glib::WeakRef<gtk::ListItem>,
        /// Where the text sits between the cell's edges; `None` on a cell
        /// that holds widgets rather than text.
        pub(super) xalign: Cell<Option<f32>>,
        pub(super) text: RefCell<String>,
        /// The text laid out for drawing, from the first draw on. It is
        /// made from the cell's own Pango context, which GTK keeps in step
        /// with the cell's font and display, and the layout follows every
        /// change of that context.
        pub(super) layout: RefCell<Option<pango::Layout>>,
        pub(super) banded: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TableCell {
        const NAME: &'static str = "SustainTableCell";
        type Type = super::TableCell;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("table-cell");
        }
    }

    impl ObjectImpl for TableCell {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for TableCell {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let cell = self.obj();
            if cell.shows_children() {
                return cell.measure_children(orientation, for_size);
            }
            match self.xalign.get() {
                Some(_) => cell.measure_text(orientation),
                None => (0, 0, -1, -1),
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let mut child = self.obj().first_child();
            while let Some(widget) = child {
                // A popover — the context menu of the row — is no part of
                // the cell's layout, but follows it when it moves.
                if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
                    popover.present();
                } else if widget.should_layout() {
                    widget.allocate(width, height, baseline, None);
                }
                child = widget.next_sibling();
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let cell = self.obj();
            cell.snapshot_band(snapshot);
            if !cell.shows_children() {
                cell.snapshot_text(snapshot);
            }
            self.parent_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct TableCell(ObjectSubclass<imp::TableCell>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl TableCell {
    /// A cell for `list_item` that shows the widgets appended to it. The
    /// list item owns the cell once it is set as its child; the cell holds
    /// the list item weakly, so the two do not keep each other alive.
    pub(crate) fn new(list_item: &gtk::ListItem) -> Self {
        let cell: Self = glib::Object::builder().property("hexpand", true).build();
        cell.imp().list_item.set(Some(list_item));
        cell
    }

    /// A cell for `list_item` that shows a text, placed between its edges
    /// by `xalign` (0 at the start, 1 at the end).
    pub(crate) fn with_text(list_item: &gtk::ListItem, xalign: f32) -> Self {
        let cell: Self = glib::Object::builder()
            .property("accessible-role", gtk::AccessibleRole::Label)
            .property("hexpand", true)
            .build();
        cell.imp().list_item.set(Some(list_item));
        cell.imp().xalign.set(Some(xalign));
        cell
    }

    pub(crate) fn list_item(&self) -> Option<gtk::ListItem> {
        self.imp().list_item.upgrade()
    }

    /// Where the text sits between the cell's edges, 0 at the start and 1
    /// at the end; 0 on a cell that shows widgets rather than text.
    pub(crate) fn xalign(&self) -> f32 {
        self.imp().xalign.get().unwrap_or(0.0)
    }

    pub(crate) fn set_text(&self, text: &str) {
        let imp = self.imp();
        if *imp.text.borrow() == text {
            return;
        }
        text.clone_into(&mut imp.text.borrow_mut());
        if let Some(layout) = imp.layout.borrow().as_ref() {
            layout.set_text(text);
        }
        if imp.xalign.get().is_some() {
            self.update_property(&[gtk::accessible::Property::Label(text)]);
        }
        self.queue_draw();
    }

    /// Shade the cell as part of a band of its striped table, or not.
    pub(crate) fn set_banded(&self, banded: bool) {
        if self.imp().banded.replace(banded) != banded {
            self.queue_draw();
        }
    }

    #[cfg(test)]
    pub(crate) fn is_banded(&self) -> bool {
        self.imp().banded.get()
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.imp().text.borrow().clone()
    }

    /// Lay `child` over the whole cell, in place of its text.
    pub(crate) fn append(&self, child: &impl IsA<gtk::Widget>) {
        child.set_parent(self);
    }

    /// Take `child` out of the cell, if it is one of its children; the
    /// text shows again once no child covers the cell.
    pub(crate) fn remove(&self, child: &impl IsA<gtk::Widget>) {
        if child.parent().as_ref() == Some(self.upcast_ref()) {
            child.unparent();
        }
    }

    /// The cell under `(x, y)` in `table` coordinates, if the point lies
    /// on a row rather than on a header, a scrollbar or the empty space
    /// below the last row.
    pub(crate) fn at(table: &impl IsA<gtk::Widget>, x: f64, y: f64) -> Option<Self> {
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
    pub(crate) fn row(&self) -> Option<gtk::Widget> {
        let mut current = self.parent();
        while let Some(widget) = current {
            if widget.css_name() == "row" {
                return Some(widget);
            }
            current = widget.parent();
        }
        None
    }

    /// Whether a child covers the cell, in place of its text. A popover
    /// parented to the cell or a hidden child covers nothing.
    fn shows_children(&self) -> bool {
        let mut child = self.first_child();
        while let Some(widget) = child {
            if widget.should_layout() {
                return true;
            }
            child = widget.next_sibling();
        }
        false
    }

    fn measure_children(
        &self,
        orientation: gtk::Orientation,
        for_size: i32,
    ) -> (i32, i32, i32, i32) {
        let (mut minimum, mut natural) = (0, 0);
        let mut child = self.first_child();
        while let Some(widget) = child {
            if widget.should_layout() {
                let (child_minimum, child_natural, _, _) = widget.measure(orientation, for_size);
                minimum = minimum.max(child_minimum);
                natural = natural.max(child_natural);
            }
            child = widget.next_sibling();
        }
        (minimum, natural, -1, -1)
    }

    /// A text cell's size, from its font alone: one line high, and wide
    /// enough for a few characters.
    fn measure_text(&self, orientation: gtk::Orientation) -> (i32, i32, i32, i32) {
        let metrics = self.pango_context().metrics(None, None);
        let size = match orientation {
            gtk::Orientation::Horizontal => {
                let char_width = metrics
                    .approximate_char_width()
                    .max(metrics.approximate_digit_width());
                pango_units_to_pixels_ceil(MIN_TEXT_CHARS * char_width) + 2 * TEXT_MARGIN
            }
            _ => pango_units_to_pixels_ceil(metrics.ascent() + metrics.descent()),
        };
        (size, size, -1, -1)
    }

    /// The band is the row's: it takes the row's foreground, whatever
    /// colour the cell gives its own text, and a selected row shows the
    /// selection colour its stylesheet gives its cells, unbanded.
    fn snapshot_band(&self, snapshot: &gtk::Snapshot) {
        if !self.imp().banded.get() {
            return;
        }
        let Some(row) = self.row() else {
            return;
        };
        if row.state_flags().contains(gtk::StateFlags::SELECTED) {
            return;
        }
        snapshot.append_color(
            &band_color(&row),
            &graphene::Rect::new(0.0, 0.0, self.width() as f32, self.height() as f32),
        );
    }

    fn snapshot_text(&self, snapshot: &gtk::Snapshot) {
        let imp = self.imp();
        let Some(xalign) = imp.xalign.get() else {
            return;
        };
        let text = imp.text.borrow();
        let text_width = self.width() - 2 * TEXT_MARGIN;
        if text.is_empty() || text_width <= 0 {
            return;
        }

        let mut layout_slot = imp.layout.borrow_mut();
        let layout = layout_slot.get_or_insert_with(|| {
            let layout = pango::Layout::new(&self.pango_context());
            layout.set_text(&text);
            layout.set_ellipsize(pango::EllipsizeMode::End);
            layout.set_single_paragraph_mode(true);
            layout
        });
        layout.set_width(text_width * pango::SCALE);
        layout.set_alignment(text_alignment(xalign, self.direction()));
        let (_, logical) = layout.pixel_extents();
        let y = ((self.height() - logical.height()) / 2).max(0);

        snapshot.push_clip(&graphene::Rect::new(
            0.0,
            0.0,
            self.width() as f32,
            self.height() as f32,
        ));
        snapshot.save();
        snapshot.translate(&graphene::Point::new(TEXT_MARGIN as f32, y as f32));
        snapshot.append_layout(layout, &self.color());
        snapshot.restore();
        snapshot.pop();
    }
}

/// The shade of a striped table's bands: `widget`'s foreground at 2.5%,
/// which follows the theme between light and dark.
pub(crate) fn band_color(widget: &gtk::Widget) -> gdk::RGBA {
    let fg = widget.color();
    gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), fg.alpha() * 0.025)
}

/// The Pango alignment that places a line at `xalign` between the start
/// (0) and the end (1) of a cell in `direction`.
fn text_alignment(xalign: f32, direction: gtk::TextDirection) -> pango::Alignment {
    let rtl = direction == gtk::TextDirection::Rtl;
    if xalign < 0.33 {
        if rtl {
            pango::Alignment::Right
        } else {
            pango::Alignment::Left
        }
    } else if xalign < 0.67 {
        pango::Alignment::Center
    } else if rtl {
        pango::Alignment::Left
    } else {
        pango::Alignment::Right
    }
}

fn pango_units_to_pixels_ceil(units: i32) -> i32 {
    (units + pango::SCALE - 1) / pango::SCALE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_lines_up_with_its_xalign_in_either_direction() {
        let ltr = gtk::TextDirection::Ltr;
        let rtl = gtk::TextDirection::Rtl;
        assert_eq!(text_alignment(0.0, ltr), pango::Alignment::Left);
        assert_eq!(text_alignment(1.0, ltr), pango::Alignment::Right);
        assert_eq!(text_alignment(0.5, ltr), pango::Alignment::Center);
        assert_eq!(text_alignment(0.0, rtl), pango::Alignment::Right);
        assert_eq!(text_alignment(1.0, rtl), pango::Alignment::Left);
    }

    #[test]
    fn children_replace_the_text() {
        let ran = crate::test_support::with_gtk(|| {
            let list_item = glib::Object::new::<gtk::ListItem>();
            let cell = TableCell::with_text(&list_item, 0.0);
            cell.set_text("Title");
            let entry = gtk::Entry::new();

            cell.append(&entry);
            assert_eq!(cell.first_child().as_ref(), Some(entry.upcast_ref()));
            cell.remove(&entry);

            assert!(cell.first_child().is_none());
            assert_eq!(cell.text(), "Title");
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
