// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::prelude::*;
use gtk::{cairo, glib};

use crate::flexible_width::FlexibleWidth;

const MARQUEE_EDGE_FADE_WIDTH: f64 = 28.0;
const MARQUEE_FRAME_MS: u64 = 33;
const MARQUEE_HEIGHT: i32 = 19;
const MARQUEE_LOOP_GAP: f64 = 48.0;

#[derive(Clone)]
pub(super) struct MarqueeLabel {
    root: FlexibleWidth,
    canvas: gtk::DrawingArea,
    state: Rc<MarqueeState>,
}

/// How a marquee reacts when its text is wider than the viewport.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MarqueeOverflow {
    /// Loop the text leftward, redrawing a seamless second copy and
    /// fading both edges — the default for the title and artist lines.
    Scroll,
    /// Hold the text still, anchored to the left, fading only the right
    /// edge where it meets the inline Lyrics chip. No seamless second
    /// copy. Used for the title line while it shares its row with the
    /// chip, so the chip never has to chase a moving title.
    Truncate,
}

/// The line and how it moves, shared with the canvas's draw function and
/// resize handler, which hold no handle on the canvas itself.
struct MarqueeState {
    text: RefCell<String>,
    /// The width of `text`, measured when it was set.
    text_width: Cell<f64>,
    style: MarqueeTextStyle,
    /// Pixels advanced per animation frame; set per line so the title and
    /// artist marquees scroll at different rates (issue #116).
    speed: f64,
    /// Upper bound on the viewport's natural width. The marquee asks for
    /// the width of its text up to this; longer text scrolls. The title
    /// line lowers it while the Lyrics chip shares its row.
    max_width: Cell<i32>,
    /// The width the viewport was given, which a narrow window can make
    /// less than it asked for; text wider than this overflows.
    viewport_width: Cell<i32>,
    x_position: Cell<f64>,
    fade_active: Cell<bool>,
    /// How overflowing text behaves. The title line switches to
    /// [`MarqueeOverflow::Truncate`] while the inline Lyrics chip shares
    /// it, so the chip is pinned to a title that holds still.
    overflow: Cell<MarqueeOverflow>,
    /// Set while the pointer hovers the now-playing area: the line holds
    /// at its start so it can be read.
    paused: Cell<bool>,
    /// The scroll timer, running only while the line scrolls. A line that
    /// fits, truncates, or is paused costs nothing between changes.
    animation: RefCell<Option<glib::SourceId>>,
}

impl MarqueeState {
    fn overflows(&self) -> bool {
        self.text_width.get() > f64::from(self.viewport_width.get()) + 1.0
    }
}

impl MarqueeLabel {
    pub(super) fn new(css_class: &str, speed: f64, max_width: i32) -> Self {
        let canvas = gtk::DrawingArea::new();
        canvas.add_css_class(css_class);
        canvas.set_content_height(MARQUEE_HEIGHT);
        canvas.set_overflow(gtk::Overflow::Hidden);

        // The marquee asks for the width of its text, set on every
        // `set_text`, so an inline neighbour (the LCD chip strip) can sit
        // right at the title's trailing edge; it can get less.
        let root = FlexibleWidth::new(&canvas, 0, 0);
        root.add_css_class("marquee-label");
        root.set_halign(gtk::Align::Center);
        root.set_valign(gtk::Align::Center);

        let state = Rc::new(MarqueeState {
            text: RefCell::new(String::new()),
            text_width: Cell::new(0.0),
            style: MarqueeTextStyle::from_css_class(css_class),
            speed,
            max_width: Cell::new(max_width),
            viewport_width: Cell::new(0),
            x_position: Cell::new(0.0),
            fade_active: Cell::new(false),
            overflow: Cell::new(MarqueeOverflow::Scroll),
            paused: Cell::new(false),
            animation: RefCell::new(None),
        });
        install_marquee_draw_func(&canvas, &state);
        {
            let state = state.clone();
            canvas.connect_resize(move |canvas, width, _| {
                state.viewport_width.set(width);
                sync_animation(&state, canvas);
            });
        }

        Self {
            root,
            canvas,
            state,
        }
    }

    pub(super) fn widget(&self) -> &FlexibleWidth {
        &self.root
    }

    pub(super) fn set_text(&self, text: &str) {
        if self.state.text.borrow().as_str() == text {
            return;
        }

        self.state.text.replace(text.to_owned());
        self.resize_to_text();
        self.state.x_position.set(0.0);
        sync_animation(&self.state, &self.canvas);
    }

    /// Adjust the viewport cap (e.g. shrink the title to make room for the
    /// inline chip, or restore full width when there is none) and re-size
    /// to the current text under the new cap.
    pub(super) fn set_max_width(&self, max_width: i32) {
        if self.state.max_width.replace(max_width) != max_width {
            self.resize_to_text();
        }
    }

    /// Switch how overflowing text behaves. The title line truncates while
    /// the Lyrics chip shares its row and scrolls otherwise; resetting to
    /// the start makes the change visible on the next frame.
    pub(super) fn set_overflow_behavior(&self, overflow: MarqueeOverflow) {
        if self.state.overflow.replace(overflow) == overflow {
            return;
        }
        self.state.x_position.set(0.0);
        sync_animation(&self.state, &self.canvas);
    }

    /// Hold the line at its start (paused) or let it scroll again.
    pub(super) fn set_paused(&self, paused: bool) {
        if self.state.paused.replace(paused) == paused {
            return;
        }
        self.state.x_position.set(0.0);
        sync_animation(&self.state, &self.canvas);
    }

    /// Ask for the width of the text up to the current `max_width`. The
    /// viewport's resize brings the overflow decision in line with the
    /// width it actually gets.
    fn resize_to_text(&self) {
        let measured = measure_marquee_text_width(&self.state.text.borrow(), self.state.style);
        self.state.text_width.set(measured);
        let natural = (measured.ceil() as i32).clamp(0, self.state.max_width.get());
        self.root.set_natural_width(natural);
    }
}

/// Bring the edge fade and the scroll timer in line with the text,
/// viewport, overflow behaviour and pause state, and redraw. Called on
/// each of their changes, so nothing polls while the line holds still.
fn sync_animation(state: &Rc<MarqueeState>, canvas: &gtk::DrawingArea) {
    let overflows = state.overflows();
    let scrolls = match state.overflow.get() {
        MarqueeOverflow::Scroll => overflows && !state.paused.get(),
        // Hold the text still; fade its right edge when it overflows so
        // the cut-off against the inline chip is soft rather than hard.
        MarqueeOverflow::Truncate => false,
    };
    state.fade_active.set(match state.overflow.get() {
        MarqueeOverflow::Scroll => scrolls,
        MarqueeOverflow::Truncate => overflows,
    });

    let mut animation = state.animation.borrow_mut();
    if scrolls {
        if animation.is_none() {
            let state = state.clone();
            let canvas = canvas.clone();
            *animation = Some(glib::timeout_add_local(
                Duration::from_millis(MARQUEE_FRAME_MS),
                move || {
                    advance(&state, &canvas);
                    glib::ControlFlow::Continue
                },
            ));
        }
    } else {
        if let Some(source) = animation.take() {
            source.remove();
        }
        state.x_position.set(0.0);
    }
    canvas.queue_draw();
}

fn advance(state: &MarqueeState, canvas: &gtk::DrawingArea) {
    let text_width = state.text_width.get();
    let mut x_position = state.x_position.get() - state.speed;
    if x_position <= -text_width - MARQUEE_LOOP_GAP {
        x_position = 0.0;
    }

    state.x_position.set(x_position);
    canvas.queue_draw();
}

fn install_marquee_draw_func(canvas: &gtk::DrawingArea, state: &Rc<MarqueeState>) {
    let state = state.clone();
    canvas.set_draw_func(move |canvas, context, width, height| {
        draw_marquee_text(canvas, context, width, height, &state);
    });
}

/// Measure the rendered advance width of `text` using the same cairo
/// toy-font settings the draw function applies, so the marquee can size
/// its viewport to hug the text. Returns 0 for empty text or when a
/// measuring surface cannot be created.
fn measure_marquee_text_width(text: &str, style: MarqueeTextStyle) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let Ok(surface) = cairo::ImageSurface::create(cairo::Format::ARgb32, 1, 1) else {
        return 0.0;
    };
    let Ok(context) = cairo::Context::new(&surface) else {
        return 0.0;
    };
    context.select_font_face("Sans", cairo::FontSlant::Normal, style.font_weight());
    context.set_font_size(style.font_size());
    context
        .text_extents(text)
        .map(|extents| extents.x_advance().max(0.0))
        .unwrap_or(0.0)
}

#[derive(Clone, Copy)]
enum MarqueeTextStyle {
    Title,
    Secondary,
}

impl MarqueeTextStyle {
    fn from_css_class(css_class: &str) -> Self {
        if css_class == "now-playing-title" {
            Self::Title
        } else {
            Self::Secondary
        }
    }

    fn font_size(self) -> f64 {
        match self {
            Self::Title => 14.0,
            Self::Secondary => 12.0,
        }
    }

    fn font_weight(self) -> cairo::FontWeight {
        match self {
            Self::Title => cairo::FontWeight::Bold,
            Self::Secondary => cairo::FontWeight::Normal,
        }
    }

    fn alpha(self) -> f64 {
        match self {
            Self::Title => 1.0,
            Self::Secondary => 0.58,
        }
    }
}

fn draw_marquee_text(
    canvas: &gtk::DrawingArea,
    context: &cairo::Context,
    width: i32,
    height: i32,
    state: &MarqueeState,
) {
    let text = state.text.borrow();
    if text.is_empty() {
        return;
    }

    let _result = context.save();
    context.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
    context.clip();
    context.select_font_face("Sans", cairo::FontSlant::Normal, state.style.font_weight());
    context.set_font_size(state.style.font_size());
    let overflow = state.overflow.get();
    set_text_source(
        context,
        &canvas.color(),
        state.style.alpha(),
        f64::from(width),
        state.fade_active.get(),
        overflow,
    );

    let Ok(extents) = context.text_extents(&text) else {
        let _result = context.restore();
        return;
    };
    // The width measured when the text was set, so the drawing agrees with
    // the overflow decision that starts and stops the scroll timer.
    let measured_width = state.text_width.get();
    let overflows = measured_width > f64::from(width) + 1.0;
    // An overflowing line anchors to its scroll position (0 when
    // truncating, since `sync_animation` holds it there); a fitting line
    // centres.
    let x = if overflows {
        state.x_position.get()
    } else {
        (f64::from(width) - measured_width) / 2.0
    };
    let y = (f64::from(height) - extents.height()) / 2.0 - extents.y_bearing();
    draw_text_at(context, &text, x, y);

    // The seamless second copy only exists for the scrolling marquee; the
    // truncating one shows a single static copy that fades at the edge.
    if overflows && overflow == MarqueeOverflow::Scroll {
        draw_text_at(context, &text, x + measured_width + MARQUEE_LOOP_GAP, y);
    }

    let _result = context.restore();
}

fn set_context_color(context: &cairo::Context, color: &gtk::gdk::RGBA, alpha: f64) {
    context.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()) * alpha,
    );
}

fn set_text_source(
    context: &cairo::Context,
    color: &gtk::gdk::RGBA,
    alpha: f64,
    width: f64,
    fade_active: bool,
    overflow: MarqueeOverflow,
) {
    if !fade_active || width <= 0.0 {
        set_context_color(context, color, alpha);
        return;
    }

    let gradient = cairo::LinearGradient::new(0.0, 0.0, width, 0.0);
    let red = f64::from(color.red());
    let green = f64::from(color.green());
    let blue = f64::from(color.blue());
    let alpha = f64::from(color.alpha()) * alpha;
    let fade_stop = (MARQUEE_EDGE_FADE_WIDTH / width).clamp(0.0, 0.5);

    // The scrolling marquee fades both edges, as text enters and leaves on
    // either side. The truncating one is anchored at the left, so its
    // first character must stay fully opaque — only the right edge fades,
    // softening the cut-off into the inline chip.
    match overflow {
        MarqueeOverflow::Scroll => {
            gradient.add_color_stop_rgba(0.0, red, green, blue, 0.0);
            gradient.add_color_stop_rgba(fade_stop, red, green, blue, alpha);
        }
        MarqueeOverflow::Truncate => {
            gradient.add_color_stop_rgba(0.0, red, green, blue, alpha);
        }
    }
    gradient.add_color_stop_rgba(1.0 - fade_stop, red, green, blue, alpha);
    gradient.add_color_stop_rgba(1.0, red, green, blue, 0.0);
    let _result = context.set_source(&gradient);
}

fn draw_text_at(context: &cairo::Context, text: &str, x: f64, y: f64) {
    context.move_to(x, y);
    let _result = context.show_text(text);
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG_TEXT: &str = "A title far too long to fit in a twenty-pixel viewport";

    impl MarqueeLabel {
        fn is_animating(&self) -> bool {
            self.state.animation.borrow().is_some()
        }

        fn natural_width(&self) -> i32 {
            let (_, natural, _, _) = self.root.measure(gtk::Orientation::Horizontal, -1);
            natural
        }

        /// Lay the line out in `width`, as its row would, after a change.
        fn lay_out_in(&self, width: i32) {
            self.root.allocate(width, MARQUEE_HEIGHT, -1, None);
        }
    }

    #[test]
    fn only_an_overflowing_unpaused_scrolling_line_runs_its_timer() {
        crate::test_support::with_gtk(|| {
            let marquee = MarqueeLabel::new("now-playing-title", 1.0, 10_000);
            marquee.lay_out_in(20);
            assert!(!marquee.is_animating(), "an empty line holds still");

            marquee.set_text("Hi");
            marquee.lay_out_in(20);
            assert!(!marquee.is_animating(), "a fitting line holds still");

            marquee.set_text(LONG_TEXT);
            marquee.lay_out_in(20);
            assert!(marquee.is_animating(), "an overflowing line scrolls");

            marquee.set_paused(true);
            assert!(!marquee.is_animating(), "a paused line holds still");
            marquee.set_paused(false);
            assert!(marquee.is_animating(), "an unpaused line scrolls again");

            marquee.set_overflow_behavior(MarqueeOverflow::Truncate);
            assert!(!marquee.is_animating(), "a truncating line holds still");
            marquee.set_overflow_behavior(MarqueeOverflow::Scroll);
            assert!(marquee.is_animating());

            marquee.lay_out_in(10_000);
            assert!(!marquee.is_animating(), "a line given room holds still");
            assert_eq!(marquee.state.x_position.get(), 0.0, "and sits at its start");
        });
    }

    #[test]
    fn a_line_asks_for_the_width_of_its_text_up_to_its_cap() {
        crate::test_support::with_gtk(|| {
            let marquee = MarqueeLabel::new("now-playing-title", 1.0, 10_000);
            marquee.set_text("Hi");
            let hi = marquee.natural_width();
            assert!(hi > 0);

            marquee.set_text(LONG_TEXT);
            assert!(marquee.natural_width() > hi);

            marquee.set_max_width(hi);
            assert_eq!(marquee.natural_width(), hi);
        });
    }
}
