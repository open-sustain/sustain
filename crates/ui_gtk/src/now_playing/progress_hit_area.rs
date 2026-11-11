// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::prelude::*;
use gtk::{cairo, gdk};

use super::super::command_controller::SharedCommandController;
use super::model::progress_fraction_from_x;
use sustain_app_runtime::{ApplicationCommand, PlaybackCommand};

const INDICATOR_TICK_WIDTH: f64 = 6.0;
const INDICATOR_TICK_RADIUS: f64 = 2.0;
const SCRUB_SEEK_THROTTLE: Duration = Duration::from_millis(125);
const SCRUB_SEEK_MIN_STEP: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub(super) struct ProgressHitArea {
    overlay: gtk::Overlay,
    progress: gtk::ProgressBar,
    indicator: gtk::DrawingArea,
    position_fraction: Rc<Cell<f64>>,
    has_track: Rc<Cell<bool>>,
    dragging: Rc<Cell<bool>>,
    hovered: Rc<Cell<bool>>,
}

impl ProgressHitArea {
    pub(super) fn new(
        command_controller: SharedCommandController,
        duration: Rc<Cell<Duration>>,
    ) -> Self {
        let progress = gtk::ProgressBar::new();
        progress.add_css_class("song-progress");
        progress.set_fraction(0.0);
        progress.set_hexpand(true);
        progress.set_halign(gtk::Align::Fill);
        progress.set_valign(gtk::Align::End);

        let overlay = gtk::Overlay::new();
        overlay.add_css_class("song-progress-hit-area");
        overlay.set_hexpand(true);
        overlay.set_halign(gtk::Align::Fill);
        overlay.set_valign(gtk::Align::End);
        overlay.set_cursor_from_name(Some("pointer"));
        overlay.set_child(Some(&progress));

        let indicator = gtk::DrawingArea::new();
        indicator.add_css_class("song-progress-indicator");
        indicator.set_can_target(false);
        indicator.set_hexpand(true);
        indicator.set_vexpand(true);
        overlay.add_overlay(&indicator);

        let position_fraction: Rc<Cell<f64>> = Rc::new(Cell::new(0.0));
        let has_track: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let dragging: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let hovered: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        install_indicator_draw(IndicatorDrawState {
            indicator: indicator.clone(),
            position_fraction: position_fraction.clone(),
            has_track: has_track.clone(),
            dragging: dragging.clone(),
            hovered: hovered.clone(),
        });
        install_drag_seek(SeekContext {
            overlay: overlay.clone(),
            progress: progress.clone(),
            indicator: indicator.clone(),
            position_fraction: position_fraction.clone(),
            has_track: has_track.clone(),
            dragging: dragging.clone(),
            last_seek_at: Rc::new(Cell::new(None)),
            last_seek_position: Rc::new(Cell::new(None)),
            command_controller,
            duration,
        });

        Self {
            overlay,
            progress,
            indicator,
            position_fraction,
            has_track,
            dragging,
            hovered,
        }
    }

    pub(super) fn widget(&self) -> &gtk::Overlay {
        &self.overlay
    }

    pub(super) fn set_position(&self, fraction: f64, has_track: bool) {
        if self.dragging.get() {
            return;
        }

        let clamped = fraction.clamp(0.0, 1.0);
        // The now-playing refresh sets the position every second, paused
        // or stopped too: an unchanged one must not draw a frame.
        if self.has_track.get() == has_track && self.position_fraction.get() == clamped {
            return;
        }
        self.has_track.set(has_track);
        self.position_fraction.set(clamped);
        self.progress.set_fraction(clamped);
        self.indicator.queue_draw();
    }

    pub(super) fn install_hover_visibility_on(&self, container: &impl IsA<gtk::Widget>) {
        let motion = gtk::EventControllerMotion::new();
        let indicator_for_enter = self.indicator.clone();
        let hovered_for_enter = self.hovered.clone();
        motion.connect_enter(move |_motion, _x, _y| {
            hovered_for_enter.set(true);
            indicator_for_enter.queue_draw();
        });

        let indicator_for_leave = self.indicator.clone();
        let hovered_for_leave = self.hovered.clone();
        let dragging_for_leave = self.dragging.clone();
        motion.connect_leave(move |_motion| {
            if dragging_for_leave.get() {
                return;
            }
            hovered_for_leave.set(false);
            indicator_for_leave.queue_draw();
        });

        container.add_controller(motion);
    }
}

struct IndicatorDrawState {
    indicator: gtk::DrawingArea,
    position_fraction: Rc<Cell<f64>>,
    has_track: Rc<Cell<bool>>,
    dragging: Rc<Cell<bool>>,
    hovered: Rc<Cell<bool>>,
}

fn install_indicator_draw(state: IndicatorDrawState) {
    let IndicatorDrawState {
        indicator,
        position_fraction,
        has_track,
        dragging,
        hovered,
    } = state;

    indicator
        .clone()
        .set_draw_func(move |area, context, width, height| {
            if !has_track.get() || (!hovered.get() && !dragging.get()) {
                return;
            }

            let width_f = f64::from(width);
            let height_f = f64::from(height);
            if width_f <= 0.0 || height_f <= 0.0 {
                return;
            }

            let max_left = (width_f - INDICATOR_TICK_WIDTH).max(0.0);
            let tick_left = (position_fraction.get().clamp(0.0, 1.0) * width_f)
                .round()
                .clamp(0.0, max_left);
            draw_indicator_tick(context, &area.color(), tick_left, height_f);
        });
}

fn draw_indicator_tick(context: &cairo::Context, body: &gdk::RGBA, x_left: f64, height: f64) {
    build_rounded_top_rect(
        context,
        x_left,
        0.0,
        INDICATOR_TICK_WIDTH,
        height,
        INDICATOR_TICK_RADIUS,
    );
    context.set_source_rgba(
        f64::from(body.red()),
        f64::from(body.green()),
        f64::from(body.blue()),
        1.0,
    );
    let _result = context.fill();
}

fn build_rounded_top_rect(
    context: &cairo::Context,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    radius: f64,
) {
    let radius = radius.max(0.0).min(width / 2.0).min(height);
    context.new_path();
    context.move_to(x, y + height);
    context.line_to(x, y + radius);
    context.arc(
        x + radius,
        y + radius,
        radius,
        std::f64::consts::PI,
        1.5 * std::f64::consts::PI,
    );
    context.line_to(x + width - radius, y);
    context.arc(
        x + width - radius,
        y + radius,
        radius,
        1.5 * std::f64::consts::PI,
        2.0 * std::f64::consts::PI,
    );
    context.line_to(x + width, y + height);
    context.close_path();
}

#[derive(Clone)]
struct SeekContext {
    overlay: gtk::Overlay,
    progress: gtk::ProgressBar,
    indicator: gtk::DrawingArea,
    position_fraction: Rc<Cell<f64>>,
    has_track: Rc<Cell<bool>>,
    dragging: Rc<Cell<bool>>,
    last_seek_at: Rc<Cell<Option<Instant>>>,
    last_seek_position: Rc<Cell<Option<Duration>>>,
    command_controller: SharedCommandController,
    duration: Rc<Cell<Duration>>,
}

impl SeekContext {
    fn preview(&self, x: f64) -> Option<f64> {
        preview_at_x(
            &self.overlay,
            &self.progress,
            &self.indicator,
            &self.position_fraction,
            &self.duration,
            x,
        )
    }

    fn dispatch_at_current_fraction(&self, force: bool) {
        let duration = self.duration.get();
        let position = position_from_fraction(duration, self.position_fraction.get());
        if position.is_zero() && duration.is_zero() {
            return;
        }
        if !force
            && self
                .last_seek_position
                .get()
                .is_some_and(|last| positions_are_too_close(last, position))
        {
            return;
        }
        dispatch_seek_position(&self.command_controller, position);
        self.last_seek_position.set(Some(position));
    }
}

fn install_drag_seek(context: SeekContext) {
    let drag = gtk::GestureDrag::new();
    drag.set_button(gdk::BUTTON_PRIMARY);
    drag.set_exclusive(true);

    let overlay = context.overlay.clone();
    install_drag_begin(&drag, context.clone());
    install_drag_update(&drag, context.clone());
    install_drag_end(&drag, context);

    overlay.add_controller(drag);
}

fn install_drag_begin(drag: &gtk::GestureDrag, context: SeekContext) {
    drag.connect_drag_begin(move |drag, x, _y| {
        claim_gesture_sequence(drag);
        if !context.has_track.get() {
            return;
        }
        context.dragging.set(true);
        context.last_seek_at.set(None);
        context.last_seek_position.set(None);
        context.preview(x);
    });
}

fn install_drag_update(drag: &gtk::GestureDrag, context: SeekContext) {
    drag.connect_drag_update(move |drag, offset_x, _offset_y| {
        claim_gesture_sequence(drag);
        if !context.has_track.get() {
            return;
        }
        let Some((start_x, _start_y)) = drag.start_point() else {
            return;
        };
        if context.preview(start_x + offset_x).is_none() {
            return;
        }

        let now = Instant::now();
        let should_dispatch = match context.last_seek_at.get() {
            None => true,
            Some(at) => now.saturating_duration_since(at) >= SCRUB_SEEK_THROTTLE,
        };
        if should_dispatch {
            context.dispatch_at_current_fraction(false);
            context.last_seek_at.set(Some(now));
        }
    });
}

fn install_drag_end(drag: &gtk::GestureDrag, context: SeekContext) {
    drag.connect_drag_end(move |drag, offset_x, _offset_y| {
        claim_gesture_sequence(drag);
        context.dragging.set(false);
        if !context.has_track.get() {
            return;
        }
        let Some((start_x, _start_y)) = drag.start_point() else {
            return;
        };
        if context.preview(start_x + offset_x).is_none() {
            return;
        }
        context.dispatch_at_current_fraction(true);
    });
}

fn claim_gesture_sequence(gesture: &impl IsA<gtk::Gesture>) {
    let _claimed = gesture.set_state(gtk::EventSequenceState::Claimed);
}

fn preview_at_x(
    overlay: &gtk::Overlay,
    progress: &gtk::ProgressBar,
    indicator: &gtk::DrawingArea,
    position_fraction: &Cell<f64>,
    duration: &Cell<Duration>,
    x: f64,
) -> Option<f64> {
    if duration.get().is_zero() {
        return None;
    }

    let fraction = progress_fraction_from_x(x, overlay.width())?;
    progress.set_fraction(fraction);
    position_fraction.set(fraction);
    indicator.queue_draw();
    Some(fraction)
}

fn position_from_fraction(duration: Duration, fraction: f64) -> Duration {
    duration.mul_f64(fraction.clamp(0.0, 1.0))
}

fn positions_are_too_close(left: Duration, right: Duration) -> bool {
    left.abs_diff(right) < SCRUB_SEEK_MIN_STEP
}

fn dispatch_seek_position(command_controller: &SharedCommandController, position: Duration) {
    let _result = command_controller.dispatch(ApplicationCommand::Playback(PlaybackCommand::Seek(
        position,
    )));
}
