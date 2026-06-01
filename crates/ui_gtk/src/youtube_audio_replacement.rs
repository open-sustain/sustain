// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::rc::Rc;

use gtk::prelude::*;
use sustain_app_runtime::{ApplicationCommand, TrackId};

use crate::{
    SharedRuntime,
    command_controller::SharedCommandController,
    track_context::{TrackActionVisibility, YoutubeAudioReplacementCallback},
};

pub(crate) fn youtube_audio_replacement_visibility(
    runtime: &SharedRuntime,
) -> TrackActionVisibility {
    let runtime = runtime.clone();
    Rc::new(move |track_ids| {
        let [track_id] = track_ids else {
            return false;
        };
        runtime
            .borrow()
            .youtube_audio_replacement_is_eligible(*track_id)
    })
}

pub(crate) fn youtube_audio_replacement_callback(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
) -> YoutubeAudioReplacementCallback {
    let parent = parent.clone();
    let command_controller = command_controller.clone();
    Rc::new(move |track_id| {
        open_dialog(&parent, &command_controller, track_id);
    })
}

fn open_dialog(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
    track_id: TrackId,
) {
    let window = gtk::Window::builder()
        .title("Replace Audio from YouTube")
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .resizable(false)
        .build();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);
    content.set_margin_end(18);

    let explanation = gtk::Label::new(Some(
        "Paste one YouTube video URL. Sustain uses your installed yt-dlp and FFmpeg to download the best available audio. The replacement is accepted only when its duration matches and its bitrate is not junk or worse than the current file.",
    ));
    explanation.set_wrap(true);
    explanation.set_xalign(0.0);
    content.append(&explanation);

    let entry = gtk::Entry::builder()
        .placeholder_text("https://www.youtube.com/watch?v=...")
        .hexpand(true)
        .build();
    content.append(&entry);

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let download = gtk::Button::with_label("Download and Replace");
    download.add_css_class("suggested-action");
    actions.append(&cancel);
    actions.append(&download);
    content.append(&actions);
    window.set_child(Some(&content));

    let submit: Rc<dyn Fn()> = {
        let window = window.clone();
        let entry = entry.clone();
        let command_controller = command_controller.clone();
        Rc::new(move || {
            if command_controller.dispatch_succeeded(
                ApplicationCommand::ReplaceTrackAudioFromYoutube {
                    track_id,
                    url: entry.text().to_string(),
                },
            ) {
                window.close();
            }
        })
    };
    cancel.connect_clicked({
        let window = window.clone();
        move |_| window.close()
    });
    download.connect_clicked({
        let submit = submit.clone();
        move |_| submit()
    });
    entry.connect_activate(move |_| submit());
    window.present();
}
