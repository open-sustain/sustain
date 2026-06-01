// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;

use super::{
    ALBUMS_VIEW, DEVICES_VIEW, DUPLICATES_VIEW, PLAYLISTS_VIEW, SONGS_VIEW, STATISTICS_VIEW,
};

pub(crate) fn build_content_stack(
    songs_view: &impl IsA<gtk::Widget>,
    albums_view: &impl IsA<gtk::Widget>,
    duplicates_view: &impl IsA<gtk::Widget>,
    statistics_view: &impl IsA<gtk::Widget>,
    playlists_view: &impl IsA<gtk::Widget>,
    devices_view: &impl IsA<gtk::Widget>,
) -> gtk::Stack {
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_vexpand(true);
    // Hidden pages must not contribute to the window's minimum size. The
    // Albums page can legitimately have a wider temporary natural width while
    // it is reflowing between column counts.
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);

    stack.add_named(songs_view, Some(SONGS_VIEW));
    stack.add_named(albums_view, Some(ALBUMS_VIEW));
    stack.add_named(duplicates_view, Some(DUPLICATES_VIEW));
    stack.add_named(statistics_view, Some(STATISTICS_VIEW));
    stack.add_named(playlists_view, Some(PLAYLISTS_VIEW));
    stack.add_named(devices_view, Some(DEVICES_VIEW));
    stack.set_visible_child_name(SONGS_VIEW);

    stack
}
