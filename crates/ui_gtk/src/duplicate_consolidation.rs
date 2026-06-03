// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{
    ApplicationCommand, DuplicateConsolidationRequest, DuplicateMetadataField,
    DuplicateMetadataFieldSelection, DuplicateMetadataSelection, NotificationCategory,
    NotificationSeverity, Track, TrackMetadata, default_duplicate_metadata_selection,
    highest_quality_duplicate_audio_track_ids,
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
        // Refuse to open the dialog when a selected file is gone from disk:
        // consolidation rewrites and removes every selected file, so one
        // missing file would strand the merge in a stuck state (#126).
        let missing = runtime
            .borrow()
            .duplicate_consolidation_missing_files(&tracks);
        if !missing.is_empty() {
            let body = missing_files_message(&tracks, &missing);
            runtime.borrow_mut().push_ephemeral_notification(
                NotificationCategory::DuplicateConsolidation,
                NotificationSeverity::Error,
                body,
            );
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

/// User-facing reason the dialog refused to open: one or more selected files
/// are gone from disk and can't be consolidated (#126).
fn missing_files_message(tracks: &[Track], missing: &[sustain_app_runtime::TrackId]) -> String {
    let mut names = missing
        .iter()
        .filter_map(|id| tracks.iter().find(|track| track.id == *id))
        .map(display_title)
        .collect::<Vec<_>>();
    if names.len() > 1 {
        return format!(
            "{} of the selected files are missing from disk, so these duplicates can't be consolidated. Restore or remove them, then try again.",
            names.len()
        );
    }
    let name = names.pop().unwrap_or_default();
    format!(
        "\u{201c}{name}\u{201d} is missing from disk, so these duplicates can't be consolidated. Restore or remove it, then try again."
    )
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
        "Each duplicate is a column. Pick the survivor's audio file (highest quality is preselected, but overridable), its artwork, its rating, and each metadata field — the highlighted box in every row marks the value that will be kept. Populated tags are cherry-picked by default, with the oldest year preferred. Click a \"Track N\" button to take every tag and the rating from that track. Play counts and skips are summed and the oldest date added is kept, so those rows are shown for reference only. The other files are removed only after the survivor has been written and verified.",
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
    // Rating preselects the highest available, matching the prior auto-merge,
    // but is now a per-track choice like every metadata field.
    let rating_buttons = build_selection_radios(tracks.len(), highest_rated_index(&tracks));

    let (table, artwork_frames) = build_consolidation_table(
        &tracks,
        &audio_buttons,
        &artwork_buttons,
        &rating_buttons,
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
    let rating_buttons_for_confirm = rating_buttons.clone();
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
        let Some(rating_track_id) =
            selected_track_id(&tracks_for_confirm, &rating_buttons_for_confirm)
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
            rating_track_id,
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

    let key_controller = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    window.present();
}

/// Build the comparison table: one self-describing column per duplicate. The
/// top cell identifies the track (its "Track N" bulk button, identity, path);
/// each cell below is one survivor decision — artwork, audio file, rating, then
/// every editable metadata field — rendered as a labelled box that draws an
/// accent border when it holds the value that will be kept. The three trailing
/// rows (play count, skips, date added) are not choices: they are summed or
/// reduced automatically, so they render as plain non-selectable cells.
fn build_consolidation_table(
    tracks: &[Track],
    audio_buttons: &[gtk::CheckButton],
    artwork_buttons: &[gtk::CheckButton],
    rating_buttons: &[gtk::CheckButton],
    metadata_selectors: &[MetadataSelector],
) -> (gtk::ScrolledWindow, Vec<gtk::Frame>) {
    let grid = gtk::Grid::new();
    // Each cell carries its own 4px margin, so the grid adds no spacing itself.
    grid.set_row_spacing(0);
    grid.set_column_spacing(0);

    let mut row = 0;
    for (index, track) in tracks.iter().enumerate() {
        grid.attach(
            &build_track_header(index, track, rating_buttons, metadata_selectors),
            column_index(index),
            row,
            1,
            1,
        );
    }
    row += 1;

    let mut artwork_frames = Vec::with_capacity(tracks.len());
    for (index, button) in artwork_buttons.iter().enumerate() {
        let frame = artwork_frame();
        attach_artwork_cell(&grid, column_index(index), row, button, &frame);
        artwork_frames.push(frame);
    }
    row += 1;

    for (index, (track, button)) in tracks.iter().zip(audio_buttons).enumerate() {
        attach_selectable_cell(
            &grid,
            column_index(index),
            row,
            button,
            "Audio file",
            &cell_value_label(&audio_summary(track), false),
        );
    }
    row += 1;

    for (index, (track, button)) in tracks.iter().zip(rating_buttons).enumerate() {
        attach_selectable_cell(
            &grid,
            column_index(index),
            row,
            button,
            "Rating",
            &cell_value_label(&rating_stars(track.rating.stars()), false),
        );
    }
    row += 1;

    for selector in metadata_selectors {
        for (index, (track, button)) in tracks.iter().zip(&selector.buttons).enumerate() {
            attach_selectable_cell(
                &grid,
                column_index(index),
                row,
                button,
                metadata_field_name(selector.field),
                &metadata_cell_value(&track.metadata, selector.field),
            );
        }
        row += 1;
    }

    // Non-selectable, automatically-merged statistics. Shown per column so the
    // contributing values stay visible even though the merge is not a choice.
    for (index, track) in tracks.iter().enumerate() {
        grid.attach(
            &info_cell("Play count", &track.statistics.play_count.to_string()),
            column_index(index),
            row,
            1,
            1,
        );
    }
    row += 1;
    for (index, track) in tracks.iter().enumerate() {
        grid.attach(
            &info_cell("Skips", &track.statistics.skip_count.to_string()),
            column_index(index),
            row,
            1,
            1,
        );
    }
    row += 1;
    for (index, track) in tracks.iter().enumerate() {
        let added = track
            .statistics
            .date_added_at
            .and_then(format_system_time_short)
            .unwrap_or_else(|| "—".to_owned());
        grid.attach(
            &info_cell("Date added", &added),
            column_index(index),
            row,
            1,
            1,
        );
    }

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&grid));
    (scroller, artwork_frames)
}

/// Grid column for a track at `index`: one column per track, no legend column.
fn column_index(index: usize) -> i32 {
    i32::try_from(index).unwrap_or(i32::MAX)
}

/// Per-column header: the "Track N" bulk-select button plus the track's
/// identity and path. "Track N" takes every metadata field *and* the rating
/// from this column at once (issue #122's "all tags from one track" mode); the
/// audio and artwork rows keep their own quality presets.
fn build_track_header(
    index: usize,
    track: &Track,
    rating_buttons: &[gtk::CheckButton],
    metadata_selectors: &[MetadataSelector],
) -> gtk::Box {
    let column = gtk::Box::new(gtk::Orientation::Vertical, 4);
    column.set_hexpand(true);
    column.set_valign(gtk::Align::Start);
    column.set_margin_start(4);
    column.set_margin_end(4);
    column.set_margin_bottom(2);

    let select_all = gtk::Button::with_label(&format!("Track {}", index + 1));
    select_all.set_tooltip_text(Some("Take every tag and the rating from this track"));
    {
        let selectors = metadata_selectors.to_vec();
        let rating_buttons = rating_buttons.to_vec();
        select_all.connect_clicked(move |_| {
            for selector in &selectors {
                if let Some(button) = selector.buttons.get(index) {
                    button.set_active(true);
                }
            }
            if let Some(button) = rating_buttons.get(index) {
                button.set_active(true);
            }
        });
    }
    column.append(&select_all);

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

    column
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

/// Codec, bitrate, duration, and file size on one compact line, e.g.
/// "FLAC · 1411 kbps · 3:45 · 28.4 MiB".
fn audio_summary(track: &Track) -> String {
    let mut parts = Vec::new();
    let format = file_format(track);
    if !format.is_empty() {
        parts.push(format);
    }
    if let Some(bitrate) = track.metadata.bitrate_kbps {
        parts.push(format!("{bitrate} kbps"));
    }
    parts.push(format_duration(track));
    parts.push(format_file_size(track.file_size_bytes.unwrap_or_default()));
    parts.join(" · ")
}

/// A five-glyph rating display: `stars` filled then the rest empty.
fn rating_stars(stars: u8) -> String {
    let filled = usize::from(stars.min(5));
    format!("{}{}", "★".repeat(filled), "☆".repeat(5 - filled))
}

/// Attach one self-describing decision cell: a grouped radio whose child stacks
/// the field name (bold) over its value (normal weight). The `.consolidation-cell`
/// class draws the accent border when the radio is active, so the box itself
/// shows which value is kept. `button` already carries the group and default.
fn attach_selectable_cell(
    grid: &gtk::Grid,
    column: i32,
    row: i32,
    button: &gtk::CheckButton,
    name: &str,
    value: &impl IsA<gtk::Widget>,
) {
    button.set_child(Some(&cell_body(name, value)));
    button.set_hexpand(true);
    button.set_halign(gtk::Align::Fill);
    button.set_valign(gtk::Align::Start);
    button.add_css_class("consolidation-cell");
    grid.attach(button, column, row, 1, 1);
}

/// The artwork decision cell: like [`attach_selectable_cell`], but its value is
/// the preview frame, filled asynchronously once the image decodes.
fn attach_artwork_cell(
    grid: &gtk::Grid,
    column: i32,
    row: i32,
    button: &gtk::CheckButton,
    frame: &gtk::Frame,
) {
    frame.set_halign(gtk::Align::Start);
    attach_selectable_cell(grid, column, row, button, "Artwork", frame);
}

/// A non-selectable info cell: field name (bold) over an automatically-merged
/// value (dimmed). It mirrors the selectable cells' footprint without a radio,
/// signalling that play count, skips, and date added are not a choice.
fn info_cell(name: &str, value: &str) -> gtk::Box {
    let cell = cell_body(name, &cell_value_label(value, true));
    cell.set_hexpand(true);
    cell.set_valign(gtk::Align::Start);
    cell.add_css_class("consolidation-info-cell");
    cell
}

/// The stacked field-name / value body shared by every cell, tightly spaced.
fn cell_body(name: &str, value: &impl IsA<gtk::Widget>) -> gtk::Box {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let name_label = gtk::Label::new(Some(name));
    name_label.set_xalign(0.0);
    name_label.add_css_class("consolidation-field-name");
    body.append(&name_label);
    body.append(value);
    body
}

fn cell_value_label(text: &str, dim: bool) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(COLUMN_TEXT_MAX_WIDTH_CHARS);
    label.add_css_class("consolidation-field-value");
    if dim {
        label.add_css_class("dim-label");
    }
    label
}

/// A metadata field's value, or a dimmed em dash when it is unpopulated.
fn metadata_cell_value(metadata: &TrackMetadata, field: DuplicateMetadataField) -> gtk::Label {
    match metadata_field_display(metadata, field) {
        Some(value) => cell_value_label(&value, false),
        None => cell_value_label("—", true),
    }
}

/// A grouped radio set with `count` members, the member at `default_index`
/// active. Each radio's label (the field name and value) is set later when its
/// cell is built, so the bare buttons here only carry group and default state.
fn build_selection_radios(count: usize, default_index: usize) -> Vec<gtk::CheckButton> {
    let mut buttons = Vec::with_capacity(count);
    let mut anchor: Option<gtk::CheckButton> = None;
    for index in 0..count {
        let button = gtk::CheckButton::new();
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

/// Index of the highest-rated track to preselect for the rating, preserving the
/// previous auto-merge default of keeping the best rating. Ties pick the first.
fn highest_rated_index(tracks: &[Track]) -> usize {
    tracks
        .iter()
        .enumerate()
        .rev()
        .max_by_key(|(_, track)| track.rating)
        .map(|(index, _)| index)
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
            // Bare grouped buttons; each cell's name/value label is attached
            // later by `attach_selectable_cell`. Defaults are set before any
            // toggled handler is wired, so this construction-time activation
            // does not refresh the preview.
            let buttons = build_selection_radios(tracks.len(), default_index);
            MetadataSelector { field, buttons }
        })
        .collect()
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

fn metadata_field_name(field: DuplicateMetadataField) -> &'static str {
    match field {
        DuplicateMetadataField::Title => "Title",
        DuplicateMetadataField::Artist => "Artist",
        DuplicateMetadataField::Album => "Album",
        DuplicateMetadataField::AlbumArtist => "Album Artist",
        DuplicateMetadataField::Composer => "Composer",
        DuplicateMetadataField::Grouping => "Grouping",
        DuplicateMetadataField::Genre => "Genre",
        DuplicateMetadataField::TrackNumber => "Track Number",
        DuplicateMetadataField::TrackTotal => "Track Total",
        DuplicateMetadataField::DiscNumber => "Disc Number",
        DuplicateMetadataField::DiscTotal => "Disc Total",
        DuplicateMetadataField::Year => "Year",
        DuplicateMetadataField::Compilation => "Compilation",
        DuplicateMetadataField::Bpm => "BPM",
        DuplicateMetadataField::Key => "Key",
        DuplicateMetadataField::Comments => "Comments",
        DuplicateMetadataField::Lyrics => "Lyrics",
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
