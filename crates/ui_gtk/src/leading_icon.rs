// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A symbolic icon followed by one child, such as a row's name, for the
//! rows of long lists.
//!
//! The obvious build, a box holding an image and the name, is the costly
//! one in a list. GTK restyles a window's every visible node each time the
//! pointer crosses from one widget to another, as it does on every wheel
//! step over a list, whose rows move under the pointer. A `box` or an
//! `image` cannot reuse its style through that: the theme names both under
//! a hovered ancestor, so GTK computes each afresh, in each of the couple
//! of hundred rows a list keeps realized, frame after frame. This widget
//! draws its icon itself, and only its child is a node of its own.
//!
//! The icon takes the widget's CSS `color` (node name `leading-icon`).

use std::cell::RefCell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// The icon's size, a theme's normal icon size.
const ICON_SIZE: i32 = 16;

/// Space between the icon and the child.
const SPACING: i32 = 6;

/// The width the icon takes before the child.
const LEAD: i32 = ICON_SIZE + SPACING;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct LeadingIcon {
        pub(super) icon_name: RefCell<Option<String>>,
        pub(super) child: RefCell<Option<gtk::Widget>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LeadingIcon {
        const NAME: &'static str = "SustainLeadingIcon";
        type Type = super::LeadingIcon;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("leading-icon");
        }
    }

    impl ObjectImpl for LeadingIcon {
        fn dispose(&self) {
            if let Some(child) = self.child.take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for LeadingIcon {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.child.borrow().clone() else {
                return (0, 0, -1, -1);
            };
            if orientation == gtk::Orientation::Horizontal {
                let (minimum, natural, _, _) = child.measure(orientation, for_size);
                return (minimum + LEAD, natural + LEAD, -1, -1);
            }
            let child_width = if for_size < 0 {
                -1
            } else {
                (for_size - LEAD).max(0)
            };
            let (minimum, natural, minimum_baseline, natural_baseline) =
                child.measure(orientation, child_width);
            // The icon is centred on the child; it only adds height when it
            // is the taller of the two, and then moves the child's baseline
            // down by half the difference.
            let baseline = |baseline: i32, height: i32| {
                if baseline < 0 {
                    -1
                } else {
                    baseline + (ICON_SIZE - height).max(0) / 2
                }
            };
            (
                minimum.max(ICON_SIZE),
                natural.max(ICON_SIZE),
                baseline(minimum_baseline, minimum),
                baseline(natural_baseline, natural),
            )
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(child) = self.child.borrow().clone() else {
                return;
            };
            let x = if self.obj().direction() == gtk::TextDirection::Rtl {
                0
            } else {
                LEAD
            };
            let transform = gsk::Transform::new().translate(&graphene::Point::new(x as f32, 0.0));
            child.allocate((width - LEAD).max(0), height, baseline, Some(transform));
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            if let Some(name) = self.icon_name.borrow().as_deref() {
                widget.snapshot_icon(snapshot, name);
            }
            if let Some(child) = self.child.borrow().as_ref() {
                widget.snapshot_child(child, snapshot);
            }
        }
    }
}

glib::wrapper! {
    pub struct LeadingIcon(ObjectSubclass<imp::LeadingIcon>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl LeadingIcon {
    /// An icon, none until one is set, followed by `child`.
    pub(crate) fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let leading_icon: Self = glib::Object::new();
        child.set_parent(&leading_icon);
        leading_icon
            .imp()
            .child
            .replace(Some(child.clone().upcast()));
        leading_icon
    }

    pub(crate) fn child(&self) -> Option<gtk::Widget> {
        self.imp().child.borrow().clone()
    }

    /// Show the symbolic icon `name` from the icon theme.
    pub(crate) fn set_icon_name(&self, name: &str) {
        let imp = self.imp();
        if imp.icon_name.borrow().as_deref() == Some(name) {
            return;
        }
        imp.icon_name.replace(Some(name.to_owned()));
        self.queue_draw();
    }

    #[cfg(test)]
    pub(crate) fn icon_name(&self) -> Option<String> {
        self.imp().icon_name.borrow().clone()
    }

    /// The icon, centred at the start of the widget. The theme looks it up
    /// from its own cache, so it follows a change of icon theme or scale
    /// at the next draw.
    fn snapshot_icon(&self, snapshot: &gtk::Snapshot, name: &str) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let paintable = gtk::IconTheme::for_display(&display).lookup_icon(
            name,
            &[],
            ICON_SIZE,
            self.scale_factor().max(1),
            self.direction(),
            gtk::IconLookupFlags::FORCE_SYMBOLIC,
        );
        let x = if self.direction() == gtk::TextDirection::Rtl {
            self.width() - ICON_SIZE
        } else {
            0
        };
        let y = (self.height() - ICON_SIZE) / 2;
        snapshot.save();
        snapshot.translate(&graphene::Point::new(x as f32, y as f32));
        paintable.snapshot_symbolic(
            snapshot,
            f64::from(ICON_SIZE),
            f64::from(ICON_SIZE),
            &[self.color()],
        );
        snapshot.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_makes_room_for_its_icon_before_its_child() {
        let ran = crate::test_support::with_gtk(|| {
            let label = gtk::Label::new(Some("Mixes"));
            let alone = gtk::Label::new(Some("Mixes"));
            let row = LeadingIcon::new(&label);
            row.set_icon_name("folder-symbolic");

            let (_, row_width, _, _) = row.measure(gtk::Orientation::Horizontal, -1);
            let (_, label_width, _, _) = alone.measure(gtk::Orientation::Horizontal, -1);
            assert_eq!(row_width, label_width + LEAD);

            let (row_height, _, _, _) = row.measure(gtk::Orientation::Vertical, -1);
            assert!(row_height >= ICON_SIZE);

            assert_eq!(row.icon_name().as_deref(), Some("folder-symbolic"));
            assert_eq!(row.child().as_ref(), Some(label.upcast_ref()));
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
