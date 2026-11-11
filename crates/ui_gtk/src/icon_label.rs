// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A label led by a symbolic icon, for the rows of long lists.
//!
//! The obvious build, a box holding an image and a label, is the costly
//! one in a list. GTK restyles a window's every visible node each time the
//! pointer crosses from one widget to another, as it does on every wheel
//! step over a list, whose rows move under the pointer. A `box` or an
//! `image` cannot reuse its style through that: the theme names both under
//! a hovered ancestor, so GTK computes each afresh, in each of the couple
//! of hundred rows a list keeps realized, frame after frame. This widget
//! draws its icon itself and holds only the label, which keeps its style.
//!
//! The icon takes the widget's CSS `color` (node name `icon-label`).

use std::cell::RefCell;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// The icon's size, a theme's normal icon size.
const ICON_SIZE: i32 = 16;

/// Space between the icon and the label.
const SPACING: i32 = 6;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct IconLabel {
        pub(super) icon_name: RefCell<Option<String>>,
        pub(super) label: RefCell<Option<gtk::Label>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for IconLabel {
        const NAME: &'static str = "SustainIconLabel";
        type Type = super::IconLabel;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("icon-label");
        }
    }

    impl ObjectImpl for IconLabel {
        fn constructed(&self) {
            self.parent_constructed();
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_parent(&*self.obj());
            self.label.replace(Some(label));
        }

        fn dispose(&self) {
            if let Some(label) = self.label.take() {
                label.unparent();
            }
        }
    }

    impl WidgetImpl for IconLabel {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let Some(label) = self.label.borrow().clone() else {
                return (0, 0, -1, -1);
            };
            let lead = ICON_SIZE + SPACING;
            match orientation {
                gtk::Orientation::Horizontal => {
                    let (minimum, natural, _, _) = label.measure(orientation, for_size);
                    (minimum + lead, natural + lead, -1, -1)
                }
                _ => {
                    let label_width = if for_size < 0 {
                        -1
                    } else {
                        (for_size - lead).max(0)
                    };
                    let (minimum, natural, minimum_baseline, natural_baseline) =
                        label.measure(orientation, label_width);
                    // The icon is centred on the label; it only adds
                    // height when it is the taller of the two, and then
                    // pushes the baseline down by half the difference.
                    let pad = |height: i32| (ICON_SIZE - height).max(0) / 2;
                    let baseline = |baseline: i32, height: i32| {
                        if baseline < 0 {
                            -1
                        } else {
                            baseline + pad(height)
                        }
                    };
                    (
                        minimum.max(ICON_SIZE),
                        natural.max(ICON_SIZE),
                        baseline(minimum_baseline, minimum),
                        baseline(natural_baseline, natural),
                    )
                }
            }
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            let Some(label) = self.label.borrow().clone() else {
                return;
            };
            let lead = ICON_SIZE + SPACING;
            let x = if self.obj().direction() == gtk::TextDirection::Rtl {
                0
            } else {
                lead
            };
            let transform = gsk::Transform::new().translate(&graphene::Point::new(x as f32, 0.0));
            label.allocate((width - lead).max(0), height, baseline, Some(transform));
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            if let Some(name) = self.icon_name.borrow().as_deref() {
                widget.snapshot_icon(snapshot, name);
            }
            if let Some(label) = self.label.borrow().as_ref() {
                widget.snapshot_child(label, snapshot);
            }
        }
    }
}

glib::wrapper! {
    pub struct IconLabel(ObjectSubclass<imp::IconLabel>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl IconLabel {
    pub(crate) fn new() -> Self {
        glib::Object::new()
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

    pub(crate) fn set_text(&self, text: &str) {
        if let Some(label) = self.imp().label.borrow().as_ref() {
            label.set_label(text);
        }
    }

    #[cfg(test)]
    pub(crate) fn icon_name(&self) -> Option<String> {
        self.imp().icon_name.borrow().clone()
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> glib::GString {
        self.imp()
            .label
            .borrow()
            .as_ref()
            .map(gtk::Label::label)
            .unwrap_or_default()
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

impl Default for IconLabel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_makes_room_for_its_icon_before_the_label() {
        let ran = crate::test_support::with_gtk(|| {
            let row = IconLabel::new();
            row.set_icon_name("folder-symbolic");
            row.set_text("Mixes");
            let label = gtk::Label::new(Some("Mixes"));

            let (_, row_width, _, _) = row.measure(gtk::Orientation::Horizontal, -1);
            let (_, label_width, _, _) = label.measure(gtk::Orientation::Horizontal, -1);
            assert_eq!(row_width, label_width + ICON_SIZE + SPACING);

            let (row_height, _, _, _) = row.measure(gtk::Orientation::Vertical, -1);
            assert!(row_height >= ICON_SIZE);

            assert_eq!(row.icon_name().as_deref(), Some("folder-symbolic"));
            assert_eq!(row.text(), "Mixes");
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
