// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The now-playing transport's Smart Shuffle icon.
//!
//! A symbolic shuffle glyph painted in the resolved foreground colour
//! (the system accent when active, via the shared
//! `.now-playing-side-icon-active` CSS class) with an optional periodic
//! "reflection" — a narrow white gradient that sweeps diagonally from
//! the bottom-left corner to the top-right corner of the glyph every
//! few seconds while Smart Shuffle is engaged (issue #70).
//!
//! Pure CSS can only animate a symbolic icon's single foreground
//! `color`, which is why the previous Smart indicator could only roll
//! the accent colour up and down — it could not move a highlight across
//! the glyph. This custom widget snapshots the glyph twice: once in the
//! base colour, then a white copy masked to a moving gradient band, so
//! the shine is clipped to the glyph shape and reads as a specular
//! reflection.
//!
//! Off / Pure / Smart visuals reuse the same `.now-playing-side-icon`
//! and `.now-playing-side-icon-active` classes the repeat icon uses, so
//! opacity and accent colour stay identical to the rest of the
//! transport; only the Smart sweep is owned here.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};

/// Duration of a single diagonal shine sweep. Inside the
/// 400–600 ms feel the maintainer asked for in issue #70.
const SWEEP_MS: i64 = 520;

/// Wall-clock gap between the start of consecutive sweeps. The glyph
/// sits still (no frame-clock ticks, no redraws) for `PERIOD_MS -
/// SWEEP_MS` between reflections, so the animation costs nothing while
/// idle.
const PERIOD_MS: u64 = 2500;

/// Half-width of the bright band, as a fraction of the diagonal sweep
/// line. The band travels from fully off the bottom-left corner to
/// fully off the top-right corner over one sweep.
const HALF_BAND: f32 = 0.42;

/// Peak alpha of the white reflection at the band centre.
const SHINE_PEAK_ALPHA: f32 = 0.85;

/// Number of gradient samples used to approximate the triangular band.
/// Sampling at fixed, strictly-increasing offsets keeps the
/// `GskColorStop` list well-formed regardless of where the band centre
/// currently sits (including off either edge).
const SHINE_SAMPLES: usize = 14;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ShuffleShineIcon {
        pub(super) icon_name: RefCell<String>,
        pub(super) pixel_size: Cell<i32>,
        /// Symbolic paintable looked up for the current icon name, size,
        /// and display scale. Cached so the per-frame snapshot during a
        /// sweep does not re-hit the icon theme; refreshed on map and on
        /// scale-factor changes.
        pub(super) paintable: RefCell<Option<gtk::IconPaintable>>,
        /// Smart Shuffle engaged: the cadence timer is running and the
        /// glyph reflects periodically.
        pub(super) shine_active: Cell<bool>,
        /// A sweep is currently animating (a frame-clock tick is
        /// installed). Guards against overlapping sweeps.
        pub(super) sweep_running: Cell<bool>,
        /// Frame-clock time (µs) the active sweep started at, captured on
        /// its first tick.
        pub(super) sweep_start_us: Cell<Option<i64>>,
        /// Position of the bright band within the current sweep, `0.0`
        /// (entering from the bottom-left) to `1.0` (leaving past the
        /// top-right). `None` between sweeps — the glyph is drawn flat.
        pub(super) shine_phase: Cell<Option<f64>>,
        /// Active cadence timer, removed when Smart Shuffle disengages or
        /// the widget is disposed.
        pub(super) cadence_id: RefCell<Option<glib::SourceId>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ShuffleShineIcon {
        const NAME: &'static str = "SustainShuffleShineIcon";
        type Type = super::ShuffleShineIcon;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for ShuffleShineIcon {
        fn dispose(&self) {
            self.obj().stop_shine();
        }
    }

    impl WidgetImpl for ShuffleShineIcon {
        fn measure(&self, _orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // Square, fixed at the requested pixel size in both
            // orientations, exactly like the `gtk::Image` it replaces —
            // so the surrounding transport layout is unchanged.
            let size = self.pixel_size.get().max(0);
            (size, size, -1, -1)
        }

        fn map(&self) {
            self.parent_map();
            // Scale may have resolved (or changed) since construction;
            // make sure the cached paintable matches the surface we are
            // now mapped onto.
            self.obj().reload_paintable();
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().snapshot_icon(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct ShuffleShineIcon(ObjectSubclass<imp::ShuffleShineIcon>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl ShuffleShineIcon {
    pub(crate) fn new(icon_name: &str, pixel_size: i32) -> Self {
        let icon: Self = glib::Object::new();
        icon.imp().icon_name.replace(icon_name.to_owned());
        icon.imp().pixel_size.set(pixel_size);
        icon.add_css_class("now-playing-side-icon");
        icon.set_halign(gtk::Align::Center);
        icon.set_valign(gtk::Align::Center);
        icon.connect_scale_factor_notify(|icon| icon.reload_paintable());
        icon.reload_paintable();
        icon
    }

    /// Apply a shuffle mode to the icon. `active` toggles the shared
    /// accent-colour class (full opacity, accent foreground); `shine`
    /// engages the periodic reflection. Off → `(false, false)`, Pure →
    /// `(true, false)`, Smart → `(true, true)`.
    pub(crate) fn set_mode(&self, active: bool, shine: bool) {
        if active {
            self.add_css_class("now-playing-side-icon-active");
        } else {
            self.remove_css_class("now-playing-side-icon-active");
        }
        // A change of class restyles and so redraws the icon, and the
        // shine draws its own sweeps: a mode applied again, as the
        // now-playing refresh does every second, draws nothing.
        if shine {
            self.start_shine();
        } else {
            self.stop_shine();
        }
    }

    fn reload_paintable(&self) {
        let Some(display) = gdk::Display::default() else {
            return;
        };
        let theme = gtk::IconTheme::for_display(&display);
        let name = self.imp().icon_name.borrow().clone();
        if name.is_empty() {
            return;
        }
        let paintable = theme.lookup_icon(
            &name,
            &[],
            self.imp().pixel_size.get(),
            self.scale_factor().max(1),
            gtk::TextDirection::None,
            gtk::IconLookupFlags::FORCE_SYMBOLIC,
        );
        self.imp().paintable.replace(Some(paintable));
        self.queue_draw();
    }

    fn start_shine(&self) {
        let imp = self.imp();
        if imp.shine_active.get() {
            return;
        }
        imp.shine_active.set(true);

        // Kick off a fresh sweep every PERIOD_MS. The weak ref lets the
        // timer stop itself if the widget is gone; `stop_shine` removes
        // it on the normal disengage path.
        let weak = self.downgrade();
        let id = glib::timeout_add_local(Duration::from_millis(PERIOD_MS), move || {
            match weak.upgrade() {
                Some(icon) => {
                    icon.begin_sweep();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        imp.cadence_id.replace(Some(id));

        // First reflection immediately, so toggling Smart Shuffle shows
        // feedback at once rather than after a full period.
        self.begin_sweep();
    }

    fn stop_shine(&self) {
        let imp = self.imp();
        if let Some(id) = imp.cadence_id.take() {
            id.remove();
        }
        imp.shine_active.set(false);
        // Any in-flight sweep tick self-cancels on its next frame (it
        // observes `shine_active == false`); drop the highlight now so
        // the glyph returns to a flat fill without waiting a frame.
        if imp.shine_phase.take().is_some() {
            self.queue_draw();
        }
    }

    fn begin_sweep(&self) {
        let imp = self.imp();
        if imp.sweep_running.get() || !imp.shine_active.get() {
            return;
        }
        imp.sweep_running.set(true);
        imp.sweep_start_us.set(None);
        // GTK hands the widget back to the callback, so no captured ref
        // is needed; dropping the returned id does not cancel the
        // callback (it self-cancels by returning `Break`).
        self.add_tick_callback(|icon, clock| icon.on_shine_tick(clock));
    }

    fn on_shine_tick(&self, clock: &gdk::FrameClock) -> glib::ControlFlow {
        let imp = self.imp();
        if !imp.shine_active.get() {
            imp.sweep_running.set(false);
            if imp.shine_phase.take().is_some() {
                self.queue_draw();
            }
            return glib::ControlFlow::Break;
        }

        let now = clock.frame_time();
        let start = match imp.sweep_start_us.get() {
            Some(start) => start,
            None => {
                imp.sweep_start_us.set(Some(now));
                now
            }
        };
        let elapsed_ms = (now - start) / 1000;
        if elapsed_ms >= SWEEP_MS {
            imp.sweep_running.set(false);
            imp.sweep_start_us.set(None);
            imp.shine_phase.set(None);
            self.queue_draw();
            return glib::ControlFlow::Break;
        }

        let phase = (elapsed_ms as f64 / SWEEP_MS as f64).clamp(0.0, 1.0);
        imp.shine_phase.set(Some(phase));
        self.queue_draw();
        glib::ControlFlow::Continue
    }

    fn snapshot_icon(&self, snapshot: &gtk::Snapshot) {
        let imp = self.imp();
        let size = imp.pixel_size.get();
        if size <= 0 {
            return;
        }
        let Some(paintable) = imp.paintable.borrow().clone() else {
            return;
        };

        let size_f = size as f32;
        let x = ((self.width() as f32 - size_f) / 2.0).max(0.0);
        let y = ((self.height() as f32 - size_f) / 2.0).max(0.0);
        let base_color = self.color();

        snapshot.save();
        snapshot.translate(&graphene::Point::new(x, y));

        // Base glyph in the resolved foreground colour (accent when the
        // active class is set). CSS `opacity` is applied by GTK around
        // the whole widget snapshot, matching the repeat icon.
        paintable.snapshot_symbolic(snapshot, size as f64, size as f64, &[base_color]);

        // Smart Shuffle reflection: a white copy of the glyph, clipped to
        // the moving gradient band so the highlight tracks across the
        // glyph shape.
        if let Some(phase) = imp.shine_phase.get() {
            let bounds = graphene::Rect::new(0.0, 0.0, size_f, size_f);
            snapshot.push_mask(gsk::MaskMode::Alpha);
            append_shine_band(snapshot, &bounds, phase as f32);
            snapshot.pop();
            let white = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);
            paintable.snapshot_symbolic(snapshot, size as f64, size as f64, &[white]);
            snapshot.pop();
        }

        snapshot.restore();
    }
}

/// Append the transparent → white → transparent band used as the shine
/// mask. `phase` runs `0.0..=1.0`; the band centre travels along the
/// bottom-left → top-right diagonal from just off the bottom-left corner
/// to just off the top-right corner so the reflection enters and exits
/// cleanly.
fn append_shine_band(snapshot: &gtk::Snapshot, bounds: &graphene::Rect, phase: f32) {
    let center = -HALF_BAND + phase * (1.0 + 2.0 * HALF_BAND);
    let stops: Vec<gsk::ColorStop> = (0..=SHINE_SAMPLES)
        .map(|sample| {
            let offset = sample as f32 / SHINE_SAMPLES as f32;
            let alpha =
                (1.0 - (offset - center).abs() / HALF_BAND).clamp(0.0, 1.0) * SHINE_PEAK_ALPHA;
            gsk::ColorStop::new(offset, gdk::RGBA::new(1.0, 1.0, 1.0, alpha))
        })
        .collect();
    // Bottom-left corner → top-right corner. GTK's y axis points down,
    // so the start (offset 0.0) sits at the bottom-left and the end
    // (offset 1.0) at the top-right; the band sweeps up the diagonal.
    snapshot.append_linear_gradient(
        bounds,
        &graphene::Point::new(bounds.x(), bounds.y() + bounds.height()),
        &graphene::Point::new(bounds.x() + bounds.width(), bounds.y()),
        &stops,
    );
}
