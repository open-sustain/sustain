// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

use gtk::prelude::*;
use gtk::{cairo, glib};

const MARQUEE_EDGE_FADE_WIDTH: f64 = 28.0;
const MARQUEE_FRAME_MS: u64 = 33;
const MARQUEE_HEIGHT: i32 = 19;
const MARQUEE_LOOP_GAP: f64 = 48.0;

#[derive(Clone)]
pub(super) struct MarqueeLabel {
    root: gtk::Overlay,
    canvas: gtk::DrawingArea,
    draw_model: MarqueeDrawModel,
    x_position: Rc<Cell<f64>>,
    paused: Rc<Cell<bool>>,
    /// Pixels advanced per animation frame; set per line so the title and
    /// artist marquees scroll at different rates (issue #116).
    speed: f64,
    /// Upper bound on the viewport width. The marquee sizes itself to hug
    /// its text up to this width; longer text keeps this width and
    /// scrolls. Shared (`Rc`) so the title line can shrink the cap while
    /// the Lyrics chip shares its row and restore it when it doesn't.
    max_width: Rc<Cell<i32>>,
    /// How overflowing text behaves. Shared (`Rc`) with the draw model so
    /// a flip takes effect on the next frame. The title line switches to
    /// [`MarqueeOverflow::Truncate`] while the inline Lyrics chip shares
    /// it, so the chip is pinned to a title that holds still.
    overflow: Rc<Cell<MarqueeOverflow>>,
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

#[derive(Clone)]
struct MarqueeDrawModel {
    text: Rc<RefCell<String>>,
    text_width: Rc<Cell<f64>>,
    x_position: Rc<Cell<f64>>,
    fade_active: Rc<Cell<bool>>,
    overflow: Rc<Cell<MarqueeOverflow>>,
    style: MarqueeTextStyle,
}

impl MarqueeLabel {
    pub(super) fn new(css_class: &str, paused: Rc<Cell<bool>>, speed: f64, max_width: i32) -> Self {
        // Width starts at zero and is sized to the text on every
        // `set_text`; the marquee hugs its content so an inline neighbour
        // (the LCD chip strip) can sit right at the title's trailing edge.
        let root = gtk::Overlay::new();
        root.add_css_class("marquee-label");
        root.set_size_request(0, MARQUEE_HEIGHT);
        root.set_hexpand(false);
        root.set_halign(gtk::Align::Center);
        root.set_valign(gtk::Align::Center);
        root.set_overflow(gtk::Overflow::Hidden);

        let canvas = gtk::DrawingArea::new();
        canvas.add_css_class(css_class);
        canvas.set_content_width(0);
        canvas.set_content_height(MARQUEE_HEIGHT);
        canvas.set_size_request(0, MARQUEE_HEIGHT);
        canvas.set_hexpand(false);
        canvas.set_halign(gtk::Align::Center);
        canvas.set_overflow(gtk::Overflow::Hidden);

        let text = Rc::new(RefCell::new(String::new()));
        let text_width = Rc::new(Cell::new(0.0));
        let x_position = Rc::new(Cell::new(0.0));
        let fade_active = Rc::new(Cell::new(false));
        let overflow = Rc::new(Cell::new(MarqueeOverflow::Scroll));
        let draw_model = MarqueeDrawModel {
            text,
            text_width,
            x_position: x_position.clone(),
            fade_active,
            overflow: overflow.clone(),
            style: MarqueeTextStyle::from_css_class(css_class),
        };
        install_marquee_draw_func(&canvas, &draw_model);

        root.set_child(Some(&canvas));

        let marquee = Self {
            root,
            canvas,
            draw_model,
            x_position,
            paused,
            speed,
            max_width: Rc::new(Cell::new(max_width)),
            overflow,
        };
        marquee.install_animation();
        marquee
    }

    pub(super) fn widget(&self) -> gtk::Overlay {
        self.root.clone()
    }

    pub(super) fn set_text(&self, text: &str) {
        if self.draw_model.text.borrow().as_str() == text {
            return;
        }

        self.draw_model.text.replace(text.to_owned());
        self.resize_to_text(text);
        self.reset_to_start();
        self.canvas.queue_draw();
    }

    /// Adjust the viewport cap (e.g. shrink the title to make room for the
    /// inline chip, or restore full width when there is none) and re-size
    /// to the current text under the new cap.
    pub(super) fn set_max_width(&self, max_width: i32) {
        if self.max_width.get() == max_width {
            return;
        }
        self.max_width.set(max_width);
        let text = self.draw_model.text.borrow().clone();
        self.resize_to_text(&text);
    }

    /// Switch how overflowing text behaves. The title line truncates while
    /// the Lyrics chip shares its row and scrolls otherwise; resetting to
    /// the start makes the change visible on the next frame.
    pub(super) fn set_overflow_behavior(&self, overflow: MarqueeOverflow) {
        if self.overflow.get() == overflow {
            return;
        }
        self.overflow.set(overflow);
        self.reset_to_start();
    }

    /// Size the viewport to hug `text` up to the current `max_width`.
    /// Text wider than the cap keeps the capped width and scrolls; the
    /// stored `text_width` drives the overflow decision in [`Self::advance`].
    fn resize_to_text(&self, text: &str) {
        let measured = measure_marquee_text_width(text, self.draw_model.style);
        self.draw_model.text_width.set(measured);
        let viewport = (measured.ceil() as i32).clamp(0, self.max_width.get());
        self.canvas.set_content_width(viewport);
        self.canvas.set_size_request(viewport, MARQUEE_HEIGHT);
        self.root.set_size_request(viewport, MARQUEE_HEIGHT);
    }

    pub(super) fn reset_to_start(&self) {
        self.x_position.set(0.0);
        self.canvas.queue_draw();
    }

    fn install_animation(&self) {
        let marquee = self.clone();
        glib::timeout_add_local(Duration::from_millis(MARQUEE_FRAME_MS), move || {
            marquee.advance();
            glib::ControlFlow::Continue
        });
    }

    fn advance(&self) {
        let viewport_width = self.canvas.width();
        let text_width = self.draw_model.text_width.get();
        let overflows = viewport_width > 0 && text_width > f64::from(viewport_width) + 1.0;

        if self.overflow.get() == MarqueeOverflow::Truncate {
            // Hold the text still; fade its right edge when it overflows so
            // the cut-off against the inline chip is soft rather than hard.
            self.draw_model.fade_active.set(overflows);
            self.reset_to_start();
            return;
        }

        let should_scroll = overflows && !self.paused.get();

        self.draw_model.fade_active.set(should_scroll);

        if !should_scroll {
            self.reset_to_start();
            return;
        }

        let mut x_position = self.x_position.get() - self.speed;
        if x_position <= -text_width - MARQUEE_LOOP_GAP {
            x_position = 0.0;
        }

        self.x_position.set(x_position);
        self.canvas.queue_draw();
    }
}

fn install_marquee_draw_func(canvas: &gtk::DrawingArea, draw_model: &MarqueeDrawModel) {
    let draw_model = draw_model.clone();

    canvas.set_draw_func(move |canvas, context, width, height| {
        draw_marquee_text(canvas, context, width, height, &draw_model);
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
    draw_model: &MarqueeDrawModel,
) {
    let text = draw_model.text.borrow();
    if text.is_empty() {
        draw_model.text_width.set(0.0);
        return;
    }

    let _result = context.save();
    context.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
    context.clip();
    context.select_font_face(
        "Sans",
        cairo::FontSlant::Normal,
        draw_model.style.font_weight(),
    );
    context.set_font_size(draw_model.style.font_size());
    let overflow = draw_model.overflow.get();
    set_text_source(
        context,
        &canvas.color(),
        draw_model.style.alpha(),
        f64::from(width),
        draw_model.fade_active.get(),
        overflow,
    );

    let Ok(extents) = context.text_extents(&text) else {
        let _result = context.restore();
        return;
    };
    let measured_width = extents.x_advance().max(0.0);
    draw_model.text_width.set(measured_width);

    let overflows = measured_width > f64::from(width) + 1.0;
    // An overflowing line anchors to its scroll position (0 when
    // truncating, since `advance` holds it there); a fitting line centres.
    let x = if overflows {
        draw_model.x_position.get()
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
