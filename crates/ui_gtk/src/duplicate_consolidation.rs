// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use sustain_app_runtime::{
    ApplicationCommand, DuplicateConsolidationRequest, DuplicateMetadataField,
    DuplicateMetadataFieldSelection, DuplicateMetadataSelection, Track, TrackId,
    default_duplicate_metadata_selection, highest_quality_duplicate_audio_track_ids,
};

use crate::{
    SharedRuntime,
    artwork_loader::{ArtworkLoader, ArtworkSource, DecodedArtwork},
    command_controller::SharedCommandController,
    date_format::format_system_time_short,
    track_context::{TrackActionCallback, TrackActionInvocation},
};

const ARTWORK_PREVIEW_SIZE: i32 = 68;
const METADATA_SCROLLER_HEIGHT: i32 = 230;
const METADATA_VALUE_LABEL_MAX_CHARS: usize = 72;

#[derive(Clone)]
struct MetadataSelector {
    field: DuplicateMetadataField,
    dropdown: gtk::DropDown,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ArtworkQuality {
    pixels: u64,
    encoded_bytes: usize,
}

pub(crate) fn consolidate_duplicates_callback(
    parent: &gtk::Window,
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
    artwork_loader: &ArtworkLoader,
) -> TrackActionCallback {
    let parent = parent.clone();
    let runtime = runtime.clone();
    let command_controller = command_controller.clone();
    let artwork_loader = artwork_loader.clone();
    Rc::new(move |invocation: TrackActionInvocation| {
        let tracks = invocation
            .selected_track_ids
            .iter()
            .filter_map(|track_id| runtime.borrow().library_track(*track_id).cloned())
            .collect::<Vec<_>>();
        if tracks.len() != invocation.selected_track_ids.len() || tracks.len() < 2 {
            return;
        }
        open_dialog(
            &parent,
            &runtime,
            &command_controller,
            &artwork_loader,
            tracks,
        );
    })
}

fn open_dialog(
    parent: &gtk::Window,
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
    artwork_loader: &ArtworkLoader,
    tracks: Vec<Track>,
) {
    let window = gtk::Window::builder()
        .title("Consolidate Duplicate Tracks")
        .transient_for(parent)
        .modal(true)
        .default_width(980)
        .default_height(760)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_end(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);

    let intro = gtk::Label::new(Some(
        "Sustain keeps the highest-quality audio file. Review the artwork preset and choose metadata field-by-field; populated tags are cherry-picked by default. The other files are removed only after the survivor has been written and verified.",
    ));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    content.append(&intro);

    let (rows, artwork_frames) = track_rows(&tracks);
    content.append(&rows);

    let choices = choice_labels(&tracks);
    let choice_refs = choices.iter().map(String::as_str).collect::<Vec<_>>();
    let audio_track_ids = highest_quality_duplicate_audio_track_ids(&tracks);
    let audio_choices = choice_labels_for_track_ids(&tracks, &audio_track_ids);
    let audio_choice_refs = audio_choices.iter().map(String::as_str).collect::<Vec<_>>();
    let audio = gtk::DropDown::from_strings(&audio_choice_refs);
    audio.set_sensitive(audio_track_ids.len() > 1);
    let artwork = gtk::DropDown::from_strings(&choice_refs);

    let selectors = gtk::Grid::new();
    selectors.set_row_spacing(8);
    selectors.set_column_spacing(12);
    append_selector(&selectors, 0, "Audio file:", &audio);
    append_selector(&selectors, 1, "Artwork:", &artwork);
    content.append(&selectors);

    let metadata = default_duplicate_metadata_selection(&tracks).expect("non-empty tracks");
    let metadata_selectors = build_metadata_selectors(&tracks, &metadata);
    let metadata_grid = gtk::Grid::new();
    metadata_grid.set_row_spacing(6);
    metadata_grid.set_column_spacing(12);

    let mut metadata_presets = vec!["Cherry-pick populated tags".to_owned()];
    metadata_presets.extend(choices.iter().cloned());
    let metadata_preset_refs = metadata_presets
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let metadata_preset = gtk::DropDown::from_strings(&metadata_preset_refs);
    append_selector(
        &metadata_grid,
        0,
        "Set all metadata from:",
        &metadata_preset,
    );
    for (index, selector) in metadata_selectors.iter().enumerate() {
        let row = i32::try_from(index + 1).unwrap_or(i32::MAX);
        append_selector(
            &metadata_grid,
            row,
            metadata_field_label(selector.field),
            &selector.dropdown,
        );
    }
    let metadata_scroller = gtk::ScrolledWindow::new();
    metadata_scroller.set_min_content_height(METADATA_SCROLLER_HEIGHT);
    metadata_scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    metadata_scroller.set_child(Some(&metadata_grid));
    content.append(&metadata_scroller);

    let preview = gtk::Label::new(None);
    preview.add_css_class("dim-label");
    preview.set_xalign(0.0);
    preview.set_wrap(true);
    content.append(&preview);
    refresh_preview(
        &preview,
        &tracks,
        &audio_track_ids,
        &audio,
        &metadata_selectors,
        &artwork,
    );

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let consolidate = gtk::Button::with_label("Consolidate");
    consolidate.add_css_class("destructive-action");
    consolidate.set_sensitive(false);
    consolidate.set_tooltip_text(Some("Waiting for artwork previews"));
    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_confirm = window.clone();
    let command_controller = command_controller.clone();
    let tracks_for_confirm = tracks.clone();
    let metadata_selectors_for_confirm = metadata_selectors.clone();
    let audio_track_ids_for_confirm = audio_track_ids.clone();
    let audio_for_confirm = audio.clone();
    let artwork_for_confirm = artwork.clone();
    consolidate.connect_clicked(move |_| {
        let Some(audio_track_id) =
            selected_track_id(&audio_track_ids_for_confirm, &audio_for_confirm)
        else {
            return;
        };
        let Some(artwork_track_id) =
            selected_track(&tracks_for_confirm, &artwork_for_confirm).map(|track| track.id)
        else {
            return;
        };
        let Some(metadata) =
            selected_metadata(&tracks_for_confirm, &metadata_selectors_for_confirm)
        else {
            return;
        };
        let request = DuplicateConsolidationRequest {
            track_ids: tracks_for_confirm.iter().map(|track| track.id).collect(),
            audio_track_id,
            metadata,
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

    {
        let selectors = metadata_selectors.clone();
        metadata_preset.connect_selected_notify(move |preset| {
            let selected = preset.selected();
            let Some(track_index) = selected.checked_sub(1) else {
                return;
            };
            for selector in &selectors {
                selector.dropdown.set_selected(track_index);
            }
        });
    }

    for selector in metadata_selectors.iter().map(|selector| &selector.dropdown) {
        connect_preview_refresh(
            selector,
            &preview,
            &tracks,
            &audio_track_ids,
            &audio,
            &metadata_selectors,
            &artwork,
        );
    }
    connect_preview_refresh(
        &audio,
        &preview,
        &tracks,
        &audio_track_ids,
        &audio,
        &metadata_selectors,
        &artwork,
    );
    connect_preview_refresh(
        &artwork,
        &preview,
        &tracks,
        &audio_track_ids,
        &audio,
        &metadata_selectors,
        &artwork,
    );

    install_artwork_previews(
        runtime,
        &tracks,
        artwork_frames,
        artwork_loader,
        &artwork,
        &consolidate,
    );

    window.set_child(Some(&content));
    window.set_default_widget(Some(&cancel));
    window.present();
}

fn track_rows(tracks: &[Track]) -> (gtk::ScrolledWindow, Vec<gtk::Frame>) {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(4);
    grid.set_column_spacing(12);
    for (column, heading) in [
        "Artwork", "Track", "Location", "Format", "Bitrate", "Duration", "Size", "Rating", "Plays",
        "Added",
    ]
    .into_iter()
    .enumerate()
    {
        let label = gtk::Label::new(Some(heading));
        label.add_css_class("heading");
        label.set_xalign(0.0);
        grid.attach(&label, column as i32, 0, 1, 1);
    }
    let mut artwork_frames = Vec::with_capacity(tracks.len());
    for (index, track) in tracks.iter().enumerate() {
        let row = i32::try_from(index + 1).unwrap_or(i32::MAX);
        let artwork = artwork_frame();
        grid.attach(&artwork, 0, row, 1, 1);
        artwork_frames.push(artwork);

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
            grid.attach(
                &label,
                i32::try_from(column + 1).unwrap_or(i32::MAX),
                row,
                1,
                1,
            );
        }
    }
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&grid));
    (scroller, artwork_frames)
}

fn artwork_frame() -> gtk::Frame {
    let frame = gtk::Frame::new(None);
    frame.set_size_request(ARTWORK_PREVIEW_SIZE, ARTWORK_PREVIEW_SIZE);
    set_artwork_preview(&frame, None);
    frame
}

fn set_artwork_preview(frame: &gtk::Frame, decoded: Option<&DecodedArtwork>) {
    let image = match decoded.and_then(|decoded| {
        decoded
            .tile_texture
            .as_ref()
            .or(decoded.detail_texture.as_ref())
    }) {
        Some(texture) => {
            let image = gtk::Image::from_paintable(Some(texture));
            image.set_pixel_size(ARTWORK_PREVIEW_SIZE);
            image
        }
        None => {
            let image = gtk::Image::from_icon_name("image-missing-symbolic");
            image.set_pixel_size(ARTWORK_PREVIEW_SIZE / 2);
            image
        }
    };
    image.set_size_request(ARTWORK_PREVIEW_SIZE, ARTWORK_PREVIEW_SIZE);
    frame.set_child(Some(&image));
}

fn install_artwork_previews(
    runtime: &SharedRuntime,
    tracks: &[Track],
    artwork_frames: Vec<gtk::Frame>,
    artwork_loader: &ArtworkLoader,
    artwork_selector: &gtk::DropDown,
    consolidate: &gtk::Button,
) {
    let qualities = Rc::new(RefCell::new(vec![None; tracks.len()]));
    let remaining = Rc::new(Cell::new(tracks.len()));
    let manually_selected = Rc::new(Cell::new(false));
    let syncing_selection = Rc::new(Cell::new(false));

    {
        let manually_selected = manually_selected.clone();
        let syncing_selection = syncing_selection.clone();
        artwork_selector.connect_selected_notify(move |_| {
            if !syncing_selection.get() {
                manually_selected.set(true);
            }
        });
    }

    let apply_result: Rc<dyn Fn(usize, DecodedArtwork)> = {
        let artwork_frames = artwork_frames.clone();
        let qualities = qualities.clone();
        let remaining = remaining.clone();
        let manually_selected = manually_selected.clone();
        let syncing_selection = syncing_selection.clone();
        let artwork_selector = artwork_selector.clone();
        let consolidate = consolidate.clone();
        Rc::new(move |index, decoded| {
            let Some(frame) = artwork_frames.get(index) else {
                return;
            };
            set_artwork_preview(frame, Some(&decoded));
            qualities.borrow_mut()[index] = artwork_quality(&decoded);
            let left = remaining.get().saturating_sub(1);
            remaining.set(left);
            if left != 0 {
                return;
            }
            if !manually_selected.get()
                && let Some(index) = highest_quality_artwork_index(&qualities.borrow())
            {
                syncing_selection.set(true);
                artwork_selector.set_selected(u32::try_from(index).unwrap_or_default());
                syncing_selection.set(false);
            }
            consolidate.set_sensitive(true);
            consolidate.set_tooltip_text(None);
        })
    };

    for (index, track) in tracks.iter().enumerate() {
        let source = {
            let runtime = runtime.borrow();
            runtime.absolute_track_path(track).map(|absolute| {
                ArtworkSource::embedded_track(track.location.path().to_path_buf(), absolute)
            })
        };
        let Some(source) = source else {
            apply_result(index, DecodedArtwork::default());
            continue;
        };
        let apply_result = apply_result.clone();
        artwork_loader.request(
            source,
            Box::new(move |decoded| apply_result(index, decoded)),
        );
    }
}

fn artwork_quality(decoded: &DecodedArtwork) -> Option<ArtworkQuality> {
    Some(ArtworkQuality {
        pixels: decoded.dimensions?.pixels,
        encoded_bytes: decoded.encoded_bytes_len.unwrap_or_default(),
    })
}

fn highest_quality_artwork_index(qualities: &[Option<ArtworkQuality>]) -> Option<usize> {
    let mut best = None;
    for (index, quality) in qualities.iter().copied().enumerate() {
        let Some(quality) = quality else {
            continue;
        };
        if best.is_none_or(|(_, best_quality)| quality > best_quality) {
            best = Some((index, quality));
        }
    }
    best.map(|(index, _)| index)
}

fn build_metadata_selectors(
    tracks: &[Track],
    default: &DuplicateMetadataSelection,
) -> Vec<MetadataSelector> {
    DuplicateMetadataField::ALL
        .into_iter()
        .map(|field| {
            let values = tracks
                .iter()
                .map(|track| metadata_choice_label(track, field))
                .collect::<Vec<_>>();
            let value_refs = values.iter().map(String::as_str).collect::<Vec<_>>();
            let dropdown = gtk::DropDown::from_strings(&value_refs);
            if let Some(track_id) = default
                .fields
                .iter()
                .find(|selection| selection.field == field)
                .map(|selection| selection.track_id)
                && let Some(index) = tracks.iter().position(|track| track.id == track_id)
            {
                dropdown.set_selected(u32::try_from(index).unwrap_or_default());
            }
            MetadataSelector { field, dropdown }
        })
        .collect()
}

fn append_selector(grid: &gtk::Grid, row: i32, text: &str, selector: &gtk::DropDown) {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(1.0);
    grid.attach(&label, 0, row, 1, 1);
    grid.attach(selector, 1, row, 1, 1);
}

fn connect_preview_refresh(
    selector: &gtk::DropDown,
    preview: &gtk::Label,
    tracks: &[Track],
    audio_track_ids: &[TrackId],
    audio: &gtk::DropDown,
    metadata: &[MetadataSelector],
    artwork: &gtk::DropDown,
) {
    let preview = preview.clone();
    let tracks = tracks.to_vec();
    let audio_track_ids = audio_track_ids.to_vec();
    let audio = audio.clone();
    let metadata = metadata.to_vec();
    let artwork = artwork.clone();
    selector.connect_selected_notify(move |_| {
        refresh_preview(
            &preview,
            &tracks,
            &audio_track_ids,
            &audio,
            &metadata,
            &artwork,
        );
    });
}

fn refresh_preview(
    preview: &gtk::Label,
    tracks: &[Track],
    audio_track_ids: &[TrackId],
    audio: &gtk::DropDown,
    metadata: &[MetadataSelector],
    artwork: &gtk::DropDown,
) {
    let audio = selected_track_id(audio_track_ids, audio)
        .and_then(|track_id| tracks.iter().find(|track| track.id == track_id));
    let artwork = selected_track(tracks, artwork);
    let Some((audio, artwork)) = audio.zip(artwork) else {
        return;
    };
    preview.set_text(&format!(
        "Result: audio {} | metadata {} | artwork {}",
        audio.location.path().display(),
        metadata_summary(tracks, metadata),
        artwork.location.path().display(),
    ));
}

fn metadata_summary(tracks: &[Track], selectors: &[MetadataSelector]) -> String {
    let mut selected = selectors
        .iter()
        .filter_map(|selector| selected_track(tracks, &selector.dropdown).map(|track| track.id));
    let Some(first) = selected.next() else {
        return "unavailable".to_owned();
    };
    if selected.all(|track_id| track_id == first) {
        return tracks
            .iter()
            .find(|track| track.id == first)
            .map(|track| track.location.path().display().to_string())
            .unwrap_or_else(|| "unavailable".to_owned());
    }
    "mixed field selection".to_owned()
}

fn selected_metadata(
    tracks: &[Track],
    selectors: &[MetadataSelector],
) -> Option<DuplicateMetadataSelection> {
    selectors
        .iter()
        .map(|selector| {
            selected_track(tracks, &selector.dropdown).map(|track| {
                DuplicateMetadataFieldSelection {
                    field: selector.field,
                    track_id: track.id,
                }
            })
        })
        .collect::<Option<Vec<_>>>()
        .map(|fields| DuplicateMetadataSelection { fields })
}

fn selected_track<'a>(tracks: &'a [Track], selector: &gtk::DropDown) -> Option<&'a Track> {
    tracks.get(selector.selected() as usize)
}

fn selected_track_id(track_ids: &[TrackId], selector: &gtk::DropDown) -> Option<TrackId> {
    track_ids.get(selector.selected() as usize).copied()
}

fn choice_labels(tracks: &[Track]) -> Vec<String> {
    tracks.iter().map(choice_label).collect()
}

fn choice_labels_for_track_ids(tracks: &[Track], track_ids: &[TrackId]) -> Vec<String> {
    track_ids
        .iter()
        .filter_map(|track_id| tracks.iter().find(|track| track.id == *track_id))
        .map(choice_label)
        .collect()
}

fn choice_label(track: &Track) -> String {
    format!(
        "{} - {} ({})",
        track.metadata.artist.as_deref().unwrap_or(""),
        track.metadata.title.as_deref().unwrap_or(""),
        track.location.path().display(),
    )
}

fn metadata_choice_label(track: &Track, field: DuplicateMetadataField) -> String {
    format!(
        "{} ({})",
        truncate_label(metadata_field_value(&track.metadata, field)),
        track.location.path().display()
    )
}

fn truncate_label(value: String) -> String {
    let mut characters = value.chars();
    let prefix = characters
        .by_ref()
        .take(METADATA_VALUE_LABEL_MAX_CHARS)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}...")
    } else {
        prefix
    }
}

fn metadata_field_label(field: DuplicateMetadataField) -> &'static str {
    match field {
        DuplicateMetadataField::Title => "Title:",
        DuplicateMetadataField::Artist => "Artist:",
        DuplicateMetadataField::Album => "Album:",
        DuplicateMetadataField::AlbumArtist => "Album Artist:",
        DuplicateMetadataField::Composer => "Composer:",
        DuplicateMetadataField::Grouping => "Grouping:",
        DuplicateMetadataField::Genre => "Genre:",
        DuplicateMetadataField::TrackNumber => "Track Number:",
        DuplicateMetadataField::TrackTotal => "Track Total:",
        DuplicateMetadataField::DiscNumber => "Disc Number:",
        DuplicateMetadataField::DiscTotal => "Disc Total:",
        DuplicateMetadataField::Year => "Year:",
        DuplicateMetadataField::Compilation => "Compilation:",
        DuplicateMetadataField::Bpm => "BPM:",
        DuplicateMetadataField::Key => "Key:",
        DuplicateMetadataField::Comments => "Comments:",
        DuplicateMetadataField::Lyrics => "Lyrics:",
    }
}

fn metadata_field_value(
    metadata: &sustain_app_runtime::TrackMetadata,
    field: DuplicateMetadataField,
) -> String {
    let value = match field {
        DuplicateMetadataField::Title => metadata.title.clone(),
        DuplicateMetadataField::Artist => metadata.artist.clone(),
        DuplicateMetadataField::Album => metadata.album.clone(),
        DuplicateMetadataField::AlbumArtist => metadata.album_artist.clone(),
        DuplicateMetadataField::Composer => metadata.composer.clone(),
        DuplicateMetadataField::Grouping => metadata.grouping.clone(),
        DuplicateMetadataField::Genre => metadata.genre.clone(),
        DuplicateMetadataField::TrackNumber => metadata.track_number.map(|value| value.to_string()),
        DuplicateMetadataField::TrackTotal => metadata.track_total.map(|value| value.to_string()),
        DuplicateMetadataField::DiscNumber => metadata.disc_number.map(|value| value.to_string()),
        DuplicateMetadataField::DiscTotal => metadata.disc_total.map(|value| value.to_string()),
        DuplicateMetadataField::Year => metadata.year.map(|value| value.to_string()),
        DuplicateMetadataField::Compilation => metadata.compilation.map(|value| {
            if value {
                "Yes".to_owned()
            } else {
                "No".to_owned()
            }
        }),
        DuplicateMetadataField::Bpm => metadata.bpm.map(|value| value.to_string()),
        DuplicateMetadataField::Key => metadata.key.clone(),
        DuplicateMetadataField::Comments => metadata.comments.clone(),
        DuplicateMetadataField::Lyrics => metadata
            .lyrics
            .as_ref()
            .map(|lyrics| format!("{} characters", lyrics.chars().count())),
    };
    value
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "<empty>".to_owned())
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

#[cfg(test)]
mod tests {
    use super::{ArtworkQuality, highest_quality_artwork_index, truncate_label};

    #[test]
    fn artwork_preset_prefers_more_pixels_then_more_encoded_bytes() {
        assert_eq!(
            highest_quality_artwork_index(&[
                Some(ArtworkQuality {
                    pixels: 1_000,
                    encoded_bytes: 5_000,
                }),
                Some(ArtworkQuality {
                    pixels: 2_000,
                    encoded_bytes: 1_000,
                }),
                Some(ArtworkQuality {
                    pixels: 2_000,
                    encoded_bytes: 2_000,
                }),
            ]),
            Some(2)
        );
    }

    #[test]
    fn artwork_preset_ignores_tracks_without_artwork() {
        assert_eq!(
            highest_quality_artwork_index(&[
                None,
                Some(ArtworkQuality {
                    pixels: 1_000,
                    encoded_bytes: 5_000,
                }),
            ]),
            Some(1)
        );
        assert_eq!(highest_quality_artwork_index(&[None, None]), None);
    }

    #[test]
    fn metadata_dropdown_labels_truncate_long_display_values() {
        let long = "x".repeat(73);

        assert_eq!(truncate_label(long), format!("{}...", "x".repeat(72)));
        assert_eq!(truncate_label("short".to_owned()), "short");
    }
}
