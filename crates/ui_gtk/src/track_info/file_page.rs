// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{fs, path::Path};

use gtk::prelude::*;
use sustain_app_runtime::Track;

use super::{
    form::attach_readonly_field,
    format::{
        format_channels, format_duration_label, format_kind, format_modified, format_optional_unit,
        format_sample_rate, format_size_label,
    },
};

pub(super) struct FilePage {
    pub(super) widget: gtk::Box,
}

impl FilePage {
    pub(super) fn new(track: &Track, absolute_path: Option<&Path>) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 6);
        widget.add_css_class("track-info-file");
        widget.set_margin_top(10);
        let page = Self { widget };
        page.reload(track, absolute_path);
        page
    }

    pub(super) fn reload(&self, track: &Track, absolute_path: Option<&Path>) {
        while let Some(child) = self.widget.first_child() {
            self.widget.remove(&child);
        }
        self.widget.append(&build_grid(track, absolute_path));
    }
}

fn build_grid(track: &Track, absolute_path: Option<&Path>) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(4);
    grid.set_column_spacing(12);
    grid.set_hexpand(true);

    let file_metadata = absolute_path.and_then(|path| fs::metadata(path).ok());

    let mut row: i32 = 0;
    attach_readonly_field(&grid, row, "Kind", &format_kind(absolute_path));
    row += 1;
    attach_readonly_field(
        &grid,
        row,
        "Duration",
        &format_duration_label(track.metadata.duration),
    );
    row += 1;
    attach_readonly_field(
        &grid,
        row,
        "Size",
        &format_size_label(file_metadata.as_ref().map(|metadata| metadata.len())),
    );
    row += 1;
    attach_readonly_field(
        &grid,
        row,
        "Bit rate",
        &format_optional_unit(track.metadata.bitrate_kbps, "kbps"),
    );
    row += 1;
    attach_readonly_field(
        &grid,
        row,
        "Sample rate",
        &format_sample_rate(track.metadata.sample_rate_hz),
    );
    row += 1;
    attach_readonly_field(
        &grid,
        row,
        "Channels",
        &format_channels(track.metadata.channels),
    );
    row += 1;
    attach_readonly_field(&grid, row, "Format", &format_kind(absolute_path));
    row += 1;
    attach_readonly_field(
        &grid,
        row,
        "Date modified",
        &format_modified(
            file_metadata
                .as_ref()
                .and_then(|metadata| metadata.modified().ok()),
        ),
    );
    row += 1;
    let location_text = absolute_path
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| String::from("\u{2014}"));
    attach_readonly_field(&grid, row, "Location", &location_text);

    grid
}
