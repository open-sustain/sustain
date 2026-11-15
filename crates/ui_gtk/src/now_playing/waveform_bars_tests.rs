// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::gdk;

use super::{bar_count, bar_levels, played_color, unplayed_color};

const WHITE: gdk::RGBA = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);

#[test]
fn each_bar_shows_the_loudest_amplitude_under_it() {
    let levels = bar_levels(&[0, 255, 51, 102], 2);
    assert_eq!(
        levels[0], 1.0,
        "a peak between two samples of a bar is kept"
    );
    assert!(levels[1] > 0.0 && levels[1] < 1.0);
}

#[test]
fn more_bars_than_amplitudes_repeat_them() {
    assert_eq!(bar_levels(&[0, 255], 4), vec![0.0, 0.0, 1.0, 1.0]);
}

#[test]
fn a_brickwalled_track_does_not_draw_a_flat_line() {
    // Every amplitude within 1 dB of the peak.
    let amplitudes = [255, 240, 250, 230, 255, 235];
    let levels = bar_levels(&amplitudes, amplitudes.len());
    let lowest = levels.iter().copied().fold(1.0, f64::min);
    assert_eq!(levels[0], 1.0);
    assert!(
        lowest < 0.9,
        "1 dB below the peak spreads over the 6 dB span: {lowest}"
    );
    assert!(lowest > 0.5);
}

#[test]
fn the_peak_reaches_full_height_whatever_the_level() {
    assert_eq!(bar_levels(&[10, 40, 20], 3)[1], 1.0);
}

#[test]
fn a_dynamic_track_spreads_over_a_wide_span() {
    // A tenth of the track 29 dB down widens the span to 29 dB, so a
    // passage 6 dB down stands well clear of the floor instead of
    // bottoming out as on the narrowest span.
    let mut amplitudes = vec![255_u8; 10];
    amplitudes.extend([128_u8; 80]);
    amplitudes.extend([9_u8; 10]);
    let levels = bar_levels(&amplitudes, 100);
    assert!(levels[50] > 0.7 && levels[50] < 0.85, "{}", levels[50]);
    assert_eq!(levels[99], 0.0, "the quietest passage sits on the floor");
}

#[test]
fn a_silent_track_draws_no_bar_above_the_line() {
    assert_eq!(bar_levels(&[0, 0, 0], 3), vec![0.0, 0.0, 0.0]);
}

#[test]
fn a_track_without_amplitudes_has_no_bars() {
    assert!(bar_levels(&[], 10).is_empty());
}

#[test]
fn bars_fill_the_width_with_their_gaps() {
    assert_eq!(bar_count(0.0), 0);
    assert_eq!(bar_count(2.0), 1);
    assert_eq!(bar_count(5.0), 2);
    assert_eq!(bar_count(299.0), 100);
}

#[test]
fn played_bars_take_the_cover_color() {
    let red = gdk::RGBA::new(0.8, 0.1, 0.1, 1.0);

    assert_eq!(played_color(Some(red), WHITE), red);
}

#[test]
fn without_a_cover_the_bars_are_shaded_like_the_progress_bar() {
    assert_eq!(played_color(None, WHITE), WHITE.with_alpha(0.48));
    assert_eq!(unplayed_color(WHITE), WHITE.with_alpha(0.18));
}

#[test]
fn a_few_quiet_outliers_do_not_widen_the_span() {
    // Three samples in a hundred, 48 dB down, sit under the 5th
    // percentile: the span stays the 6 dB floor, so a passage 3 dB down
    // draws half height.
    let mut amplitudes = vec![255_u8; 50];
    amplitudes.extend([180_u8; 47]);
    amplitudes.extend([1_u8; 3]);
    let levels = bar_levels(&amplitudes, 100);
    assert!(levels[60] > 0.45 && levels[60] < 0.55, "{}", levels[60]);
}

#[test]
fn the_span_stops_at_thirty_decibels() {
    // A tenth of the track 42 dB down widens the span only to 30 dB.
    let mut amplitudes = vec![255_u8; 50];
    amplitudes.extend([128_u8; 40]);
    amplitudes.extend([2_u8; 10]);
    let levels = bar_levels(&amplitudes, 100);
    assert!(levels[60] > 0.78 && levels[60] < 0.82, "{}", levels[60]);
}

#[test]
fn silence_does_not_change_the_sounding_levels() {
    let without_silence = bar_levels(&[255, 128], 2);
    let mut with_silence = vec![255_u8, 128];
    with_silence.extend([0_u8; 8]);
    let levels = bar_levels(&with_silence, 10);
    assert_eq!(levels[..2], without_silence[..]);
    assert!(levels[2..].iter().all(|level| *level == 0.0));
}
