// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A bar floating over content, drawn as frosted glass: the content behind
//! it shows through blurred, under the bar's own child (#316).
//!
//! GTK has no CSS backdrop filter, so the widget draws that content itself,
//! from a [`gtk::WidgetPaintable`] of the widget it floats over: clipped to
//! its own bounds, shifted so the part right behind it lines up, blurred.
//! The paintable renders the content's cached render node, so following a
//! scroll costs the blur of the bar's own strip, not a second redraw of the
//! content. It reports a change one frame after the content draws it, so
//! the blurred copy trails a scroll by a frame.

use std::cell::OnceCell;

use gtk::glib;
use gtk::graphene;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// Blur radius, in logical pixels.
const BLUR_RADIUS: f64 = 12.0;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct BackdropBlur {
        pub(super) backdrop: OnceCell<gtk::WidgetPaintable>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for BackdropBlur {
        const NAME: &'static str = "SustainBackdropBlur";
        type Type = super::BackdropBlur;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_layout_manager_type::<gtk::BinLayout>();
        }
    }

    impl ObjectImpl for BackdropBlur {
        fn dispose(&self) {
            if let Some(backdrop) = self.backdrop.get() {
                backdrop.set_widget(None::<&gtk::Widget>);
            }
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for BackdropBlur {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            if let Some(backdrop) = self.backdrop.get()
                && let Some(content) = backdrop.widget()
                && let Some(origin) = widget.compute_point(&content, &graphene::Point::zero())
            {
                let bounds =
                    graphene::Rect::new(0.0, 0.0, widget.width() as f32, widget.height() as f32);
                snapshot.push_clip(&bounds);
                snapshot.push_blur(BLUR_RADIUS);
                snapshot.translate(&graphene::Point::new(-origin.x(), -origin.y()));
                backdrop.snapshot(
                    snapshot,
                    f64::from(content.width()),
                    f64::from(content.height()),
                );
                snapshot.pop();
                snapshot.pop();
            }
            self.parent_snapshot(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct BackdropBlur(ObjectSubclass<imp::BackdropBlur>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl BackdropBlur {
    /// `child` drawn over the part of `content` behind this widget, blurred.
    /// The widget is meant to float over `content`, in a [`gtk::Overlay`].
    pub(crate) fn new(child: &impl IsA<gtk::Widget>, content: &impl IsA<gtk::Widget>) -> Self {
        let blur: Self = glib::Object::new();
        child.set_parent(&blur);
        let backdrop = gtk::WidgetPaintable::new(Some(content));
        backdrop.connect_invalidate_contents(glib::clone!(
            #[weak]
            blur,
            move |_| blur.queue_draw()
        ));
        blur.imp()
            .backdrop
            .set(backdrop)
            .expect("a new backdrop blur has no backdrop yet");
        blur
    }
}
