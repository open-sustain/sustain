// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::gdk;

use super::{bar_count, bar_levels, played_color, unplayed_color};

const WHITE: gdk::RGBA = gdk::RGBA::new(1.0, 1.0, 1.0, 1.0);

#[test]
fn each_bar_shows_the_loudest_amplitude_under_it() {
    assert_eq!(
        bar_levels(&[0, 255, 51, 102], 2),
        vec![1.0, 0.4],
        "a peak between two samples of a bar is kept"
    );
}

#[test]
fn more_bars_than_amplitudes_repeat_them() {
    assert_eq!(bar_levels(&[0, 255], 4), vec![0.0, 0.0, 1.0, 1.0]);
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
