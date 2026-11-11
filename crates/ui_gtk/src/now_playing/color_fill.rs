// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A plain fill of one color, or nothing until one is set.
//!
//! The artwork tile takes the color of its cover. A CSS rule could set
//! it, but only through a provider for the whole display, and changing a
//! provider restyles every node of every window: thousands in the track
//! table, on each change of track. The fill draws the color itself.

use std::cell::Cell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ColorFill {
        pub(super) color: Cell<Option<gdk::RGBA>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ColorFill {
        const NAME: &'static str = "SustainColorFill";
        type Type = super::ColorFill;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("color-fill");
        }
    }

    impl ObjectImpl for ColorFill {}

    impl WidgetImpl for ColorFill {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let Some(color) = self.color.get() else {
                return;
            };
            let widget = self.obj();
            let bounds =
                graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32);
            snapshot.append_color(&color, &bounds);
        }
    }
}

glib::wrapper! {
    pub struct ColorFill(ObjectSubclass<imp::ColorFill>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ColorFill {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    /// Fill with `color`, or with nothing.
    pub(crate) fn set_color(&self, color: Option<gdk::RGBA>) {
        if self.imp().color.replace(color) != color {
            self.queue_draw();
        }
    }
}
