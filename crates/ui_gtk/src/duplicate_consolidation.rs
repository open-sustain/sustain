// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::rc::Rc;

use gtk::prelude::*;
use sustain_app_runtime::{ApplicationCommand, DuplicateConsolidationRequest, Track};

use crate::{
    SharedRuntime,
    command_controller::SharedCommandController,
    date_format::format_system_time_short,
    track_context::{TrackActionCallback, TrackActionInvocation},
};

pub(crate) fn consolidate_duplicates_callback(
    parent: &gtk::Window,
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
) -> TrackActionCallback {
    let parent = parent.clone();
    let runtime = runtime.clone();
    let command_controller = command_controller.clone();
    Rc::new(move |invocation: TrackActionInvocation| {
        let tracks = invocation
            .selected_track_ids
            .iter()
            .filter_map(|track_id| runtime.borrow().library_track(*track_id).cloned())
            .collect::<Vec<_>>();
        if tracks.len() != invocation.selected_track_ids.len() || tracks.len() < 2 {
            return;
        }
        open_dialog(&parent, &command_controller, tracks);
    })
}

fn open_dialog(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
    tracks: Vec<Track>,
) {
    let window = gtk::Window::builder()
        .title("Consolidate Duplicate Tracks")
        .transient_for(parent)
        .modal(true)
        .default_width(880)
        .default_height(560)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_end(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);

    let intro = gtk::Label::new(Some(
        "Choose which audio file survives and which track supplies metadata and artwork. The other files will be removed after the survivor has been written and verified.",
    ));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    content.append(&intro);

    let rows = track_rows(&tracks);
    content.append(&rows);

    let choices = choice_labels(&tracks);
    let choice_refs = choices.iter().map(String::as_str).collect::<Vec<_>>();
    let audio = gtk::DropDown::from_strings(&choice_refs);
    let metadata = gtk::DropDown::from_strings(&choice_refs);
    let artwork = gtk::DropDown::from_strings(&choice_refs);
    let selectors = gtk::Grid::new();
    selectors.set_row_spacing(8);
    selectors.set_column_spacing(12);
    append_selector(&selectors, 0, "Audio file:", &audio);
    append_selector(&selectors, 1, "Metadata:", &metadata);
    append_selector(&selectors, 2, "Artwork:", &artwork);
    content.append(&selectors);

    let preview = gtk::Label::new(None);
    preview.add_css_class("dim-label");
    preview.set_xalign(0.0);
    preview.set_wrap(true);
    content.append(&preview);
    refresh_preview(&preview, &tracks, &audio, &metadata, &artwork);
    for selector in [&audio, &metadata, &artwork] {
        let preview = preview.clone();
        let tracks = tracks.clone();
        let audio = audio.clone();
        let metadata = metadata.clone();
        let artwork = artwork.clone();
        selector.connect_selected_notify(move |_| {
            refresh_preview(&preview, &tracks, &audio, &metadata, &artwork);
        });
    }

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let consolidate = gtk::Button::with_label("Consolidate");
    consolidate.add_css_class("destructive-action");
    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_confirm = window.clone();
    let command_controller = command_controller.clone();
    consolidate.connect_clicked(move |_| {
        let Some(audio_track_id) = selected_track_id(&tracks, &audio) else {
            return;
        };
        let Some(metadata_track_id) = selected_track_id(&tracks, &metadata) else {
            return;
        };
        let Some(artwork_track_id) = selected_track_id(&tracks, &artwork) else {
            return;
        };
        let request = DuplicateConsolidationRequest {
            track_ids: tracks.iter().map(|track| track.id).collect(),
            audio_track_id,
            metadata_track_id,
            artwork_track_id,
        };
        if command_controller
            .dispatch_succeeded(ApplicationCommand::ConsolidateDuplicateTracks(request))
        {
            window_for_confirm.close();
        }
    });
    buttons.append(&cancel);
    buttons.append(&consolidate);
    content.append(&buttons);

    window.set_child(Some(&content));
    window.set_default_widget(Some(&cancel));
    window.present();
}

fn track_rows(tracks: &[Track]) -> gtk::ScrolledWindow {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(4);
    grid.set_column_spacing(12);
    for (column, heading) in [
        "Track", "Location", "Format", "Bitrate", "Duration", "Size", "Rating", "Plays", "Added",
    ]
    .into_iter()
    .enumerate()
    {
        let label = gtk::Label::new(Some(heading));
        label.add_css_class("heading");
        label.set_xalign(0.0);
        grid.attach(&label, column as i32, 0, 1, 1);
    }
    for (index, track) in tracks.iter().enumerate() {
        let row = i32::try_from(index + 1).unwrap_or(i32::MAX);
        let values = [
            track.metadata.title.clone().unwrap_or_default(),
            track.location.path().display().to_string(),
            file_format(track),
            track
                .metadata
                .bitrate_kbps
                .map(|value| format!("{value} kbps"))
                .unwrap_or_default(),
            format_duration(track),
            format_file_size(track.file_size_bytes.unwrap_or_default()),
            track.rating.stars().to_string(),
            track.statistics.play_count.to_string(),
            track
                .statistics
                .date_added_at
                .and_then(format_system_time_short)
                .unwrap_or_default(),
        ];
        for (column, value) in values.into_iter().enumerate() {
            let label = gtk::Label::new(Some(&value));
            label.set_xalign(0.0);
            label.set_selectable(true);
            grid.attach(&label, column as i32, row, 1, 1);
        }
    }
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&grid));
    scroller
}

fn append_selector(grid: &gtk::Grid, row: i32, text: &str, selector: &gtk::DropDown) {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(1.0);
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(selector, 1, row, 1, 1);
}

fn refresh_preview(
    preview: &gtk::Label,
    tracks: &[Track],
    audio: &gtk::DropDown,
    metadata: &gtk::DropDown,
    artwork: &gtk::DropDown,
) {
    let audio = selected_track(tracks, audio);
    let metadata = selected_track(tracks, metadata);
    let artwork = selected_track(tracks, artwork);
    let Some((audio, metadata, artwork)) = audio
        .zip(metadata)
        .zip(artwork)
        .map(|((audio, metadata), artwork)| (audio, metadata, artwork))
    else {
        return;
    };
    preview.set_text(&format!(
        "Result: {} - {} | audio: {} | metadata: {} | artwork: {}",
        metadata.metadata.artist.as_deref().unwrap_or(""),
        metadata.metadata.title.as_deref().unwrap_or(""),
        audio.location.path().display(),
        metadata.location.path().display(),
        artwork.location.path().display(),
    ));
}

fn selected_track<'a>(tracks: &'a [Track], selector: &gtk::DropDown) -> Option<&'a Track> {
    tracks.get(selector.selected() as usize)
}

fn selected_track_id(
    tracks: &[Track],
    selector: &gtk::DropDown,
) -> Option<sustain_app_runtime::TrackId> {
    selected_track(tracks, selector).map(|track| track.id)
}

fn choice_labels(tracks: &[Track]) -> Vec<String> {
    tracks
        .iter()
        .map(|track| {
            format!(
                "{} - {} ({})",
                track.metadata.artist.as_deref().unwrap_or(""),
                track.metadata.title.as_deref().unwrap_or(""),
                track.location.path().display(),
            )
        })
        .collect()
}

fn file_format(track: &Track) -> String {
    track
        .location
        .path()
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_uppercase)
        .unwrap_or_default()
}

fn format_duration(track: &Track) -> String {
    let seconds = track
        .metadata
        .duration
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn format_file_size(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}
