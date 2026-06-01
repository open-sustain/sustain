// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use sustain_app_runtime::{
    ApplicationCommand, DuplicateConsolidationRequest, DuplicateMetadataField,
    DuplicateMetadataFieldSelection, DuplicateMetadataSelection, Track, TrackMetadata,
    default_duplicate_metadata_selection, highest_quality_duplicate_audio_track_ids,
};

use crate::{
    SharedRuntime,
    artwork_loader::{ArtworkLoader, ArtworkSource, DecodedArtwork},
    command_controller::SharedCommandController,
    date_format::format_system_time_short,
    track_context::{TrackActionCallback, TrackActionInvocation},
    util::{display_artist, display_title},
};

const ARTWORK_PREVIEW_SIZE: i32 = 68;
/// Upper bound on the ellipsized width of a column's text, in characters, so a
/// long path or tag value cannot stretch a track column.
const COLUMN_TEXT_MAX_WIDTH_CHARS: i32 = 36;

#[derive(Clone)]
struct MetadataSelector {
    field: DuplicateMetadataField,
    /// One radio button per duplicate track, index-aligned to the dialog's
    /// `tracks` slice. Exactly one is active, naming the track this field is
    /// taken from.
    buttons: Vec<gtk::CheckButton>,
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
        .default_width(900)
        .default_height(760)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_end(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);

    let intro = gtk::Label::new(Some(
        "Each duplicate is a column. Pick the survivor's audio file (highest quality is preselected, but overridable), its artwork, and each metadata field from whichever track has the best value — populated tags are cherry-picked by default. Click a \"Track N\" heading to take every field from that track. The other files are removed only after the survivor has been written and verified.",
    ));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    content.append(&intro);

    let metadata = default_duplicate_metadata_selection(&tracks).expect("non-empty tracks");
    let metadata_selectors = build_metadata_selectors(&tracks, &metadata);
    let audio_buttons = build_selection_radios(tracks.len(), highest_quality_audio_index(&tracks));
    // Artwork starts on the first track and is repointed at the highest-
    // resolution image once the asynchronous previews finish loading.
    let artwork_buttons = build_selection_radios(tracks.len(), 0);

    let (table, artwork_frames) = build_consolidation_table(
        &tracks,
        &audio_buttons,
        &artwork_buttons,
        &metadata_selectors,
    );
    content.append(&table);

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
    let audio_buttons_for_confirm = audio_buttons.clone();
    let artwork_buttons_for_confirm = artwork_buttons.clone();
    consolidate.connect_clicked(move |_| {
        let Some(audio_track_id) =
            selected_track_id(&tracks_for_confirm, &audio_buttons_for_confirm)
        else {
            return;
        };
        let Some(artwork_track_id) =
            selected_track_id(&tracks_for_confirm, &artwork_buttons_for_confirm)
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

    install_artwork_previews(
        runtime,
        &tracks,
        artwork_frames,
        artwork_loader,
        &artwork_buttons,
        &consolidate,
    );

    window.set_child(Some(&content));
    window.set_default_widget(Some(&cancel));
    window.present();
}

/// Build the single comparison table: one column per duplicate track. Each
/// column header carries that track's artwork and descriptive details; the rows
/// below are the survivor decisions — audio file, artwork, then every editable
/// metadata field — each a grouped radio so exactly one track wins per row.
fn build_consolidation_table(
    tracks: &[Track],
    audio_buttons: &[gtk::CheckButton],
    artwork_buttons: &[gtk::CheckButton],
    metadata_selectors: &[MetadataSelector],
) -> (gtk::ScrolledWindow, Vec<gtk::Frame>) {
    let grid = gtk::Grid::new();
    grid.set_row_spacing(6);
    grid.set_column_spacing(18);

    let mut artwork_frames = Vec::with_capacity(tracks.len());
    for (index, track) in tracks.iter().enumerate() {
        let column = i32::try_from(index + 1).unwrap_or(i32::MAX);
        let (header, frame) = build_track_header(index, track, metadata_selectors);
        grid.attach(&header, column, 0, 1, 1);
        artwork_frames.push(frame);
    }

    let mut row = 1;
    attach_radio_row(&grid, row, "Audio file:", audio_buttons);
    row += 1;
    attach_radio_row(&grid, row, "Artwork:", artwork_buttons);
    row += 1;
    for selector in metadata_selectors {
        attach_radio_row(
            &grid,
            row,
            metadata_field_label(selector.field),
            &selector.buttons,
        );
        row += 1;
    }

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&grid));
    (scroller, artwork_frames)
}

/// Per-column header: the "Track N" bulk-select button, the artwork preview
/// frame, and the track's descriptive details (identity, path, audio quality,
/// size, rating, plays, date added). Returns the frame so the caller can fill
/// in the artwork once it decodes.
fn build_track_header(
    index: usize,
    track: &Track,
    metadata_selectors: &[MetadataSelector],
) -> (gtk::Box, gtk::Frame) {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.set_hexpand(true);
    column.set_valign(gtk::Align::Start);

    // "Track N" doubles as the bulk action: take every metadata field from this
    // column at once (issue #122's "all tags from one track" mode). It does not
    // touch the audio/artwork rows, which follow their own quality presets.
    let select_all = gtk::Button::with_label(&format!("Track {}", index + 1));
    select_all.set_tooltip_text(Some("Take every metadata field from this track"));
    {
        let selectors = metadata_selectors.to_vec();
        select_all.connect_clicked(move |_| {
            for selector in &selectors {
                if let Some(button) = selector.buttons.get(index) {
                    button.set_active(true);
                }
            }
        });
    }
    column.append(&select_all);

    let frame = artwork_frame();
    frame.set_halign(gtk::Align::Center);
    column.append(&frame);

    let identity = header_label(
        &format!(
            "{} — {}",
            display_artist(&track.metadata),
            display_title(track)
        ),
        false,
        gtk::pango::EllipsizeMode::End,
    );
    identity.add_css_class("heading");
    column.append(&identity);

    // The filename is the most distinguishing part of the path, so ellipsize
    // from the start to keep the tail visible.
    column.append(&header_label(
        &track.location.path().display().to_string(),
        true,
        gtk::pango::EllipsizeMode::Start,
    ));
    column.append(&header_label(
        &audio_details(track),
        true,
        gtk::pango::EllipsizeMode::End,
    ));
    column.append(&header_label(
        &size_and_stats(track),
        true,
        gtk::pango::EllipsizeMode::End,
    ));
    if let Some(added) = track
        .statistics
        .date_added_at
        .and_then(format_system_time_short)
    {
        column.append(&header_label(
            &format!("Added {added}"),
            true,
            gtk::pango::EllipsizeMode::End,
        ));
    }

    (column, frame)
}

fn header_label(text: &str, dim: bool, ellipsize: gtk::pango::EllipsizeMode) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(ellipsize);
    label.set_max_width_chars(COLUMN_TEXT_MAX_WIDTH_CHARS);
    if dim {
        label.add_css_class("dim-label");
    }
    label
}

/// Codec, bitrate, and duration on one compact line, e.g. "FLAC · 1411 kbps · 3:45".
fn audio_details(track: &Track) -> String {
    let mut parts = Vec::new();
    let format = file_format(track);
    if !format.is_empty() {
        parts.push(format);
    }
    if let Some(bitrate) = track.metadata.bitrate_kbps {
        parts.push(format!("{bitrate} kbps"));
    }
    parts.push(format_duration(track));
    parts.join(" · ")
}

/// File size, rating, and play count on one compact line.
fn size_and_stats(track: &Track) -> String {
    format!(
        "{} · {} · {} plays",
        format_file_size(track.file_size_bytes.unwrap_or_default()),
        rating_stars(track.rating.stars()),
        track.statistics.play_count,
    )
}

/// A five-glyph rating display: `stars` filled then the rest empty.
fn rating_stars(stars: u8) -> String {
    let filled = usize::from(stars.min(5));
    format!("{}{}", "★".repeat(filled), "☆".repeat(5 - filled))
}

/// Attach a decision row: a right-aligned label in column 0 and one radio per
/// track across the remaining columns, aligned under their headers.
fn attach_radio_row(grid: &gtk::Grid, row: i32, label_text: &str, buttons: &[gtk::CheckButton]) {
    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(1.0);
    grid.attach(&label, 0, row, 1, 1);
    for (index, button) in buttons.iter().enumerate() {
        let column = i32::try_from(index + 1).unwrap_or(i32::MAX);
        grid.attach(button, column, row, 1, 1);
    }
}

/// A grouped radio set with `count` members, the member at `default_index`
/// active. Used for the audio and artwork rows, where the column header already
/// names the track so the radios carry no label of their own.
fn build_selection_radios(count: usize, default_index: usize) -> Vec<gtk::CheckButton> {
    let mut buttons = Vec::with_capacity(count);
    let mut anchor: Option<gtk::CheckButton> = None;
    for index in 0..count {
        let button = gtk::CheckButton::new();
        button.set_halign(gtk::Align::Center);
        match &anchor {
            Some(first) => button.set_group(Some(first)),
            None => anchor = Some(button.clone()),
        }
        button.set_active(index == default_index);
        buttons.push(button);
    }
    buttons
}

/// Index of the highest-quality audio track to preselect, falling back to the
/// first track when bitrates are unknown.
fn highest_quality_audio_index(tracks: &[Track]) -> usize {
    highest_quality_duplicate_audio_track_ids(tracks)
        .first()
        .and_then(|id| tracks.iter().position(|track| track.id == *id))
        .unwrap_or(0)
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
    artwork_buttons: &[gtk::CheckButton],
    consolidate: &gtk::Button,
) {
    let qualities = Rc::new(RefCell::new(vec![None; tracks.len()]));
    let remaining = Rc::new(Cell::new(tracks.len()));
    let manually_selected = Rc::new(Cell::new(false));
    let syncing_selection = Rc::new(Cell::new(false));

    for button in artwork_buttons {
        let manually_selected = manually_selected.clone();
        let syncing_selection = syncing_selection.clone();
        button.connect_toggled(move |button| {
            // Ignore the off-toggle of the previously active radio and our own
            // programmatic preselect; only a user-driven activation counts.
            if button.is_active() && !syncing_selection.get() {
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
        let artwork_buttons = artwork_buttons.to_vec();
        let consolidate = consolidate.clone();
        Rc::new(move |index, decoded| {
            if let Some(frame) = artwork_frames.get(index) {
                set_artwork_preview(frame, Some(&decoded));
            }
            if let Some(slot) = qualities.borrow_mut().get_mut(index) {
                *slot = artwork_quality(&decoded);
            }
            let left = remaining.get().saturating_sub(1);
            remaining.set(left);
            if left != 0 {
                return;
            }
            if !manually_selected.get()
                && let Some(best) = highest_quality_artwork_index(&qualities.borrow())
                && let Some(button) = artwork_buttons.get(best)
            {
                syncing_selection.set(true);
                button.set_active(true);
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
            let default_index = default
                .fields
                .iter()
                .find(|selection| selection.field == field)
                .and_then(|selection| {
                    tracks
                        .iter()
                        .position(|track| track.id == selection.track_id)
                })
                .unwrap_or(0);
            let mut buttons = Vec::with_capacity(tracks.len());
            let mut anchor: Option<gtk::CheckButton> = None;
            for (index, track) in tracks.iter().enumerate() {
                let button = metadata_value_radio(&track.metadata, field);
                match &anchor {
                    Some(first) => button.set_group(Some(first)),
                    None => anchor = Some(button.clone()),
                }
                // Set the default before any toggled handler is wired so this
                // construction-time activation does not refresh the preview.
                button.set_active(index == default_index);
                buttons.push(button);
            }
            MetadataSelector { field, buttons }
        })
        .collect()
}

/// A single metadata cell: a radio button whose label is the field's value for
/// one track, ellipsized so a long tag cannot widen the column. Unpopulated
/// values render dimmed as an em dash so gaps are obvious at a glance.
fn metadata_value_radio(
    metadata: &TrackMetadata,
    field: DuplicateMetadataField,
) -> gtk::CheckButton {
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(COLUMN_TEXT_MAX_WIDTH_CHARS);
    match metadata_field_display(metadata, field) {
        Some(value) => label.set_text(&value),
        None => {
            label.set_text("—");
            label.add_css_class("dim-label");
        }
    }
    let button = gtk::CheckButton::new();
    button.set_child(Some(&label));
    button
}

/// Index, into the dialog's `tracks` slice, of the active radio in a group.
/// `None` only if the group is somehow empty.
fn active_index(buttons: &[gtk::CheckButton]) -> Option<usize> {
    buttons.iter().position(|button| button.is_active())
}

fn selected_track_id(
    tracks: &[Track],
    buttons: &[gtk::CheckButton],
) -> Option<sustain_app_runtime::TrackId> {
    let index = active_index(buttons)?;
    tracks.get(index).map(|track| track.id)
}

fn selected_metadata(
    tracks: &[Track],
    selectors: &[MetadataSelector],
) -> Option<DuplicateMetadataSelection> {
    selectors
        .iter()
        .map(|selector| {
            let index = active_index(&selector.buttons)?;
            let track = tracks.get(index)?;
            Some(DuplicateMetadataFieldSelection {
                field: selector.field,
                track_id: track.id,
            })
        })
        .collect::<Option<Vec<_>>>()
        .map(|fields| DuplicateMetadataSelection { fields })
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

/// Human-readable value of one metadata field, or `None` when the field is
/// unpopulated (empty/whitespace text counts as unpopulated, matching the
/// domain's cherry-pick default).
fn metadata_field_display(
    metadata: &TrackMetadata,
    field: DuplicateMetadataField,
) -> Option<String> {
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
            .filter(|lyrics| !lyrics.trim().is_empty())
            .map(|lyrics| format!("{} characters", lyrics.chars().count())),
    };
    value.filter(|value| !value.trim().is_empty())
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
    use super::{ArtworkQuality, highest_quality_artwork_index, rating_stars};

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
    fn rating_stars_fills_then_empties_to_five() {
        assert_eq!(rating_stars(0), "☆☆☆☆☆");
        assert_eq!(rating_stars(3), "★★★☆☆");
        assert_eq!(rating_stars(5), "★★★★★");
        assert_eq!(rating_stars(9), "★★★★★");
    }
}
