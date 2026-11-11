// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! One child, given the width it takes when there is room apart from the
//! narrowest it may get when there is not.
//!
//! A size request or a CSS `min-width` sets a widget's minimum, and the
//! natural width only follows from its content. A top-bar control that
//! should keep its width in a wide window, yet give way in a narrow one,
//! needs the two set apart: its natural width is the size it is designed
//! at, its minimum how far it may shrink.

use std::cell::Cell;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FlexibleWidth {
        pub(super) minimum: Cell<i32>,
        pub(super) natural: Cell<i32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FlexibleWidth {
        const NAME: &'static str = "SustainFlexibleWidth";
        type Type = super::FlexibleWidth;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("flexible-width");
        }
    }

    impl ObjectImpl for FlexibleWidth {
        fn dispose(&self) {
            if let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for FlexibleWidth {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.obj()
                .first_child()
                .map_or(gtk::SizeRequestMode::ConstantSize, |child| {
                    child.request_mode()
                })
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.obj().first_child() else {
                return (0, 0, -1, -1);
            };
            let measured = child.measure(orientation, for_size);
            if orientation == gtk::Orientation::Vertical {
                return measured;
            }
            // The child never gets less than it needs, whatever the bounds.
            let (child_minimum, _, _, _) = measured;
            let minimum = self.minimum.get().max(child_minimum);
            (minimum, self.natural.get().max(minimum), -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.obj().first_child() {
                child.allocate(width, height, baseline, None);
            }
        }
    }
}

glib::wrapper! {
    pub struct FlexibleWidth(ObjectSubclass<imp::FlexibleWidth>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl FlexibleWidth {
    /// `child`, `natural` pixels wide when there is room and down to
    /// `minimum` when there is not.
    pub(crate) fn new(child: &impl IsA<gtk::Widget>, minimum: i32, natural: i32) -> Self {
        let flexible_width: Self = glib::Object::new();
        let imp = flexible_width.imp();
        imp.minimum.set(minimum);
        imp.natural.set(natural);
        child.set_parent(&flexible_width);
        flexible_width
    }

    /// Change the width taken when there is room.
    pub(crate) fn set_natural_width(&self, natural: i32) {
        if self.imp().natural.replace(natural) != natural {
            self.queue_resize();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn widths(widget: &impl IsA<gtk::Widget>) -> (i32, i32) {
        let (minimum, natural, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
        (minimum, natural)
    }

    #[test]
    fn it_gives_its_child_a_natural_width_apart_from_its_minimum() {
        let ran = crate::test_support::with_gtk(|| {
            let child = gtk::Box::new(gtk::Orientation::Horizontal, 0);
            child.set_size_request(20, 10);
            let flexible = FlexibleWidth::new(&child, 50, 100);
            assert_eq!(widths(&flexible), (50, 100));

            flexible.set_natural_width(80);
            assert_eq!(widths(&flexible), (50, 80));

            // A child that needs more than the bounds widens them.
            child.set_size_request(120, 10);
            assert_eq!(widths(&flexible), (120, 120));
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
