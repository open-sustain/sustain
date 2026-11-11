// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Frame timing under `--profile`. While the window draws frame after
//! frame — scrolling, animating — one summary line per second; nothing
//! while it sits idle, but for a lone frame slow enough to be seen, such
//! as the first frame of a view the user just switched to.
//!
//! Each frame clock cycle runs its phases in order: update, layout, paint.
//! The recorder's handlers run after GTK's own for each phase, so the time
//! from one phase's handler to the next is what GTK spent in the later
//! phase: measuring layout (style, size allocation, list rows bound) apart
//! from paint (snapshot and render) tells a frame that does too much
//! layout work from one that draws too much.

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::prelude::*;

/// A pause this long between two frames ends a run of drawing.
const RUN_PAUSE: Duration = Duration::from_millis(250);

/// How much of a run one summary line covers.
const SUMMARY_SPAN: Duration = Duration::from_secs(1);

/// A lone frame whose layout and paint take this long gets a line of its
/// own: half the frame interval of a 60 Hz display.
const SLOW_LONE_FRAME: Duration = Duration::from_millis(8);

/// The frames of a run of drawing since its last summary line.
struct FrameSpan {
    started_at: Instant,
    frames: u32,
    max_interval: Duration,
    layout_total: Duration,
    layout_max: Duration,
    paint_total: Duration,
    paint_max: Duration,
}

impl FrameSpan {
    fn new(started_at: Instant) -> Self {
        Self {
            started_at,
            frames: 0,
            max_interval: Duration::ZERO,
            layout_total: Duration::ZERO,
            layout_max: Duration::ZERO,
            paint_total: Duration::ZERO,
            paint_max: Duration::ZERO,
        }
    }

    fn add(&mut self, interval: Option<Duration>, layout: Duration, paint: Duration) {
        self.frames += 1;
        if let Some(interval) = interval {
            self.max_interval = self.max_interval.max(interval);
        }
        self.layout_total += layout;
        self.layout_max = self.layout_max.max(layout);
        self.paint_total += paint;
        self.paint_max = self.paint_max.max(paint);
    }

    /// A run of frames, or a lone frame that was slow.
    fn is_worth_reporting(&self) -> bool {
        self.frames > 1 || self.layout_max + self.paint_max >= SLOW_LONE_FRAME
    }

    fn report(&self, ended_at: Instant) {
        let frames = self.frames.max(1);
        sustain_profiler::profile!(
            "frames: {} in {:.0}ms, interval max {:.1}ms, layout avg {:.2} max {:.2}ms, paint avg {:.2} max {:.2}ms",
            self.frames,
            ms(ended_at - self.started_at),
            ms(self.max_interval),
            ms(self.layout_total / frames),
            ms(self.layout_max),
            ms(self.paint_total / frames),
            ms(self.paint_max),
        );
    }
}

#[derive(Default)]
struct FrameRecorder {
    update_done: Option<Instant>,
    layout_done: Option<Instant>,
    last_paint_done: Option<Instant>,
    span: Option<FrameSpan>,
}

impl FrameRecorder {
    fn paint_done(&mut self, now: Instant) {
        let (Some(update_done), Some(layout_done)) =
            (self.update_done.take(), self.layout_done.take())
        else {
            return;
        };
        let previous_paint_done = self.last_paint_done.replace(now);
        let interval = previous_paint_done.map(|previous| now.duration_since(previous));
        let continues_run = interval.is_some_and(|interval| interval < RUN_PAUSE);
        if !continues_run {
            // The run before the pause ends with the frame that preceded it.
            if let (Some(span), Some(ended_at)) = (
                self.span.take().filter(FrameSpan::is_worth_reporting),
                previous_paint_done,
            ) {
                span.report(ended_at);
            }
        }

        let span = self.span.get_or_insert_with(|| FrameSpan::new(now));
        span.add(
            interval.filter(|_| continues_run),
            layout_done - update_done,
            now - layout_done,
        );
        if now - span.started_at >= SUMMARY_SPAN {
            span.report(now);
            self.span = Some(FrameSpan::new(now));
        }
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// Record the frame timing of `window` once it is realized. A no-op unless
/// profiling is enabled.
pub(crate) fn install(window: &gtk::ApplicationWindow) {
    if !sustain_profiler::enabled() {
        return;
    }
    window.connect_realize(|window| {
        let Some(frame_clock) = window.frame_clock() else {
            return;
        };
        let recorder = Rc::new(RefCell::new(FrameRecorder::default()));
        let on_update = recorder.clone();
        frame_clock.connect_update(move |_| {
            on_update.borrow_mut().update_done = Some(Instant::now());
        });
        let on_layout = recorder.clone();
        frame_clock.connect_layout(move |_| {
            on_layout.borrow_mut().layout_done = Some(Instant::now());
        });
        frame_clock.connect_paint(move |_| {
            recorder.borrow_mut().paint_done(Instant::now());
        });
    });
}
