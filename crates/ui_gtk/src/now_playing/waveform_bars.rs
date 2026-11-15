// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The playing track's waveform, drawn as the Now Playing progress (#41).
//!
//! Small vertical bars, rounded on top like the seek tick, one per few
//! pixels: the bars already played take the color of the cover, the rest
//! stay a neutral gray. The bars come from the preview waveform the audio
//! analysis stored, so showing it costs no analysis of its own.

use gtk::{cairo, gdk};

/// Width of one bar, in pixels.
const BAR_WIDTH: f64 = 2.0;
/// Space between two bars, in pixels.
const BAR_GAP: f64 = 1.0;
const BAR_RADIUS: f64 = 1.0;
/// The height a silent stretch keeps, so the bars still read as one line.
const MIN_BAR_HEIGHT: f64 = 1.0;

/// The opacity of the theme's foreground for the bars already played
/// when the cover gives no color, and for the bars still to play: the
/// opacities of the progress bar's fill and trough.
const PLAYED_ALPHA: f32 = 0.48;
const UNPLAYED_ALPHA: f32 = 0.18;

/// The level, from 0 to 1, of each of `bar_count` bars spread over
/// `amplitudes`: the loudest amplitude under each bar, so a short peak
/// never disappears between two bars.
pub(super) fn bar_levels(amplitudes: &[u8], bar_count: usize) -> Vec<f64> {
    if amplitudes.is_empty() {
        return Vec::new();
    }
    (0..bar_count)
        .map(|bar| {
            let start = bar * amplitudes.len() / bar_count;
            let end = ((bar + 1) * amplitudes.len() / bar_count).max(start + 1);
            let loudest = amplitudes[start..end.min(amplitudes.len())]
                .iter()
                .copied()
                .max()
                .unwrap_or(0);
            f64::from(loudest) / f64::from(u8::MAX)
        })
        .collect()
}

/// How many bars fit in `width` pixels.
pub(super) fn bar_count(width: f64) -> usize {
    ((width + BAR_GAP) / (BAR_WIDTH + BAR_GAP)).floor().max(0.0) as usize
}

/// The color of the bars already played: the cover's, as the Now Playing
/// view brings it within reach of the chrome, or the theme's foreground as
/// the progress bar fills.
pub(super) fn played_color(cover: Option<gdk::RGBA>, foreground: gdk::RGBA) -> gdk::RGBA {
    cover.unwrap_or_else(|| foreground.with_alpha(PLAYED_ALPHA))
}

pub(super) fn unplayed_color(foreground: gdk::RGBA) -> gdk::RGBA {
    foreground.with_alpha(UNPLAYED_ALPHA)
}

/// Draw `levels` as bars across `width`, bottom-aligned in `height`: the
/// bars left of `played_fraction` in `played`, the others in `unplayed`.
pub(super) fn draw(
    context: &cairo::Context,
    levels: &[f64],
    played_fraction: f64,
    width: f64,
    height: f64,
    played: gdk::RGBA,
    unplayed: gdk::RGBA,
) {
    let played_until = played_fraction.clamp(0.0, 1.0) * width;
    for (index, level) in levels.iter().enumerate() {
        let x = index as f64 * (BAR_WIDTH + BAR_GAP);
        let bar_height = (level * height).clamp(MIN_BAR_HEIGHT, height);
        let color = if x + BAR_WIDTH / 2.0 < played_until {
            played
        } else {
            unplayed
        };
        rounded_top_rect(
            context,
            x,
            height - bar_height,
            BAR_WIDTH,
            bar_height,
            BAR_RADIUS,
        );
        context.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            f64::from(color.alpha()),
        );
        let _result = context.fill();
    }
}

/// Trace a `width` × `height` rectangle at (`x`, `y`) whose top corners
/// are rounded by `radius`: a waveform bar, or the seek tick.
pub(super) fn rounded_top_rect(
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

#[cfg(test)]
#[path = "waveform_bars_tests.rs"]
mod tests;
