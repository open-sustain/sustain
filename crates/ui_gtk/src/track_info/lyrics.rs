// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use sustain_app_runtime::{FieldChange, TrackMetadata};

use crate::metadata_diff::text_diff_preserve_newlines;

#[derive(Clone)]
pub(super) struct LyricsPage {
    pub(super) widget: gtk::ScrolledWindow,
    view: gtk::TextView,
}

impl LyricsPage {
    pub(super) fn new(initial: &TrackMetadata) -> Self {
        let view = gtk::TextView::new();
        view.add_css_class("track-info-lyrics-view");
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_accepts_tab(false);
        view.set_top_margin(16);
        view.set_bottom_margin(16);
        view.set_left_margin(16);
        view.set_right_margin(16);
        if let Some(text) = initial.lyrics.as_deref() {
            view.buffer().set_text(text);
        }

        let widget = gtk::ScrolledWindow::new();
        widget.add_css_class("track-info-lyrics");
        widget.set_margin_top(10);
        widget.set_hexpand(true);
        widget.set_vexpand(true);
        widget.set_min_content_height(280);
        widget.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        widget.set_child(Some(&view));

        Self { widget, view }
    }

    pub(super) fn lyrics_diff(&self, initial: &TrackMetadata) -> FieldChange<String> {
        let buffer = self.view.buffer();
        let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
        text_diff_preserve_newlines(initial.lyrics.as_deref(), &text)
    }

    pub(super) fn reload(&self, metadata: &TrackMetadata) {
        self.view
            .buffer()
            .set_text(metadata.lyrics.as_deref().unwrap_or_default());
    }
}
