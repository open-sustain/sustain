// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{cell::RefCell, rc::Rc};

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{PlaylistFolderId, PlaylistId, PlaylistItem, SmartPlaylistId, TrackId};

use super::model::bound_playlist_item;
use super::{MoveCallbackHolder, TracksDropCallbackHolder};

pub(super) type SharedDropIndicator = Rc<RefCell<Option<gtk::Widget>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DropPosition {
    Above,
    Below,
    Into,
}

pub(crate) fn drop_position_from_motion(
    y: f64,
    row_height: f64,
    target_is_folder: bool,
) -> DropPosition {
    if row_height <= 0.0 {
        return if target_is_folder {
            DropPosition::Into
        } else {
            DropPosition::Above
        };
    }
    let ratio = (y / row_height).clamp(0.0, 1.0);
    if target_is_folder {
        if ratio < 0.25 {
            DropPosition::Above
        } else if ratio > 0.75 {
            DropPosition::Below
        } else {
            DropPosition::Into
        }
    } else if ratio < 0.5 {
        DropPosition::Above
    } else {
        DropPosition::Below
    }
}

/// Wire a row's drag source and drop target once, when the row is set
/// up. Both act on the item the row shows when the drag happens.
pub(super) fn attach_drag_and_drop(
    row: &gtk::Widget,
    list_item: &gtk::ListItem,
    on_move: MoveCallbackHolder,
    on_tracks_drop: TracksDropCallbackHolder,
    current_indicator: SharedDropIndicator,
) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE);
    let list_item_for_prepare = list_item.downgrade();
    drag_source.connect_prepare(move |_source, _x, _y| {
        let item = bound_playlist_item(&list_item_for_prepare)?;
        Some(gdk::ContentProvider::for_value(
            &drag_payload(item).to_value(),
        ))
    });
    row.add_controller(drag_source);

    let drop_target = gtk::DropTarget::new(
        glib::Type::STRING,
        gdk::DragAction::MOVE | gdk::DragAction::COPY,
    );
    drop_target.set_preload(true);

    let list_item_for_motion = list_item.downgrade();
    let current_indicator_for_motion = current_indicator.clone();
    drop_target.connect_motion(move |target, _x, y| {
        let (Some(row), Some(item)) = (target.widget(), bound_playlist_item(&list_item_for_motion))
        else {
            return gdk::DragAction::empty();
        };
        match peek_drag_kind(target) {
            Some(DragKind::Tracks) if accepts_tracks(item) => {
                set_drop_indicator(&row, DropPosition::Into, &current_indicator_for_motion);
                gdk::DragAction::COPY
            }
            Some(DragKind::PlaylistItem) => {
                let position = drop_position_from_motion(y, row.height() as f64, is_folder(item));
                set_drop_indicator(&row, position, &current_indicator_for_motion);
                gdk::DragAction::MOVE
            }
            Some(DragKind::Tracks) | None => {
                clear_drop_indicator(&row, &current_indicator_for_motion);
                gdk::DragAction::empty()
            }
        }
    });

    let current_indicator_for_leave = current_indicator.clone();
    drop_target.connect_leave(move |target| {
        if let Some(row) = target.widget() {
            clear_drop_indicator(&row, &current_indicator_for_leave);
        }
    });

    let list_item_for_drop = list_item.downgrade();
    let current_indicator_for_drop = current_indicator;
    drop_target.connect_drop(move |target, value, _x, y| {
        let Some(row) = target.widget() else {
            return false;
        };
        clear_drop_indicator(&row, &current_indicator_for_drop);
        let Some(item) = bound_playlist_item(&list_item_for_drop) else {
            return false;
        };
        let Ok(text) = value.get::<String>() else {
            return false;
        };
        if let Some(track_ids) = parse_tracks_payload(&text) {
            if accepts_tracks(item)
                && let Some(callback) = on_tracks_drop.borrow().as_ref()
            {
                callback(item, track_ids);
                return true;
            }
            return false;
        }
        let Some(source_item) = parse_drag_payload(&text) else {
            return false;
        };
        if source_item == item {
            return false;
        }
        let position = drop_position_from_motion(y, row.height() as f64, is_folder(item));
        if let Some(callback) = on_move.borrow().as_ref() {
            callback(source_item, item, position);
        }
        true
    });
    row.add_controller(drop_target);
}

fn is_folder(item: PlaylistItem) -> bool {
    matches!(item, PlaylistItem::Folder(_))
}

/// Only regular playlists take dropped tracks; a smart playlist's tracks
/// follow from its rules, and a folder holds no tracks.
fn accepts_tracks(item: PlaylistItem) -> bool {
    matches!(item, PlaylistItem::Playlist(_))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragKind {
    Tracks,
    PlaylistItem,
}

fn peek_drag_kind(target: &gtk::DropTarget) -> Option<DragKind> {
    let value = target.value()?;
    let text = value.get::<String>().ok()?;
    classify_drag_payload(&text)
}

fn classify_drag_payload(text: &str) -> Option<DragKind> {
    let prefix = text.split_once(':')?.0;
    match prefix {
        "tracks" => Some(DragKind::Tracks),
        "folder" | "playlist" | "smart" => Some(DragKind::PlaylistItem),
        _ => None,
    }
}

fn set_drop_indicator(
    row: &gtk::Widget,
    position: DropPosition,
    current_indicator: &SharedDropIndicator,
) {
    let mut current = current_indicator.borrow_mut();
    if let Some(previous) = current.as_ref()
        && previous != row
    {
        clear_indicator_classes(previous);
    }
    clear_indicator_classes(row);
    match position {
        DropPosition::Above => row.add_css_class("sidebar-drop-above"),
        DropPosition::Below => row.add_css_class("sidebar-drop-below"),
        DropPosition::Into => row.add_css_class("sidebar-drop-into"),
    }
    *current = Some(row.clone());
}

fn clear_drop_indicator(row: &gtk::Widget, current_indicator: &SharedDropIndicator) {
    clear_indicator_classes(row);
    let mut current = current_indicator.borrow_mut();
    if current.as_ref() == Some(row) {
        *current = None;
    }
}

fn clear_indicator_classes(row: &gtk::Widget) {
    row.remove_css_class("sidebar-drop-above");
    row.remove_css_class("sidebar-drop-below");
    row.remove_css_class("sidebar-drop-into");
}

fn drag_payload(item: PlaylistItem) -> String {
    match item {
        PlaylistItem::Folder(id) => format!("folder:{}", id.get()),
        PlaylistItem::Playlist(id) => format!("playlist:{}", id.get()),
        PlaylistItem::SmartPlaylist(id) => format!("smart:{}", id.get()),
    }
}

fn parse_drag_payload(text: &str) -> Option<PlaylistItem> {
    let (kind, id_str) = text.split_once(':')?;
    let id = id_str.parse::<i64>().ok()?;
    match kind {
        "folder" => PlaylistFolderId::new(id).map(PlaylistItem::Folder),
        "playlist" => PlaylistId::new(id).map(PlaylistItem::Playlist),
        "smart" => SmartPlaylistId::new(id).map(PlaylistItem::SmartPlaylist),
        _ => None,
    }
}

pub(crate) fn tracks_drag_payload(track_ids: &[TrackId]) -> String {
    let joined = track_ids
        .iter()
        .map(|id| id.get().to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!("tracks:{joined}")
}

pub(crate) fn parse_tracks_payload(text: &str) -> Option<Vec<TrackId>> {
    let (kind, ids_str) = text.split_once(':')?;
    if kind != "tracks" {
        return None;
    }
    let ids: Option<Vec<TrackId>> = ids_str
        .split(',')
        .map(|part| part.trim().parse::<i64>().ok().and_then(TrackId::new))
        .collect();
    let ids = ids?;
    if ids.is_empty() { None } else { Some(ids) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_payload_round_trips_for_each_kind() {
        let folder_id = PlaylistFolderId::new(42).expect("positive id");
        let playlist_id = PlaylistId::new(7).expect("positive id");
        let smart_id = SmartPlaylistId::new(3).expect("positive id");

        let cases = [
            PlaylistItem::Folder(folder_id),
            PlaylistItem::Playlist(playlist_id),
            PlaylistItem::SmartPlaylist(smart_id),
        ];
        for item in cases {
            let payload = drag_payload(item);
            assert_eq!(parse_drag_payload(&payload), Some(item));
        }
    }

    #[test]
    fn drag_payload_rejects_unknown_kind_and_invalid_id() {
        assert_eq!(parse_drag_payload("bogus:1"), None);
        assert_eq!(parse_drag_payload("folder:not-a-number"), None);
        assert_eq!(parse_drag_payload("folder:-3"), None);
        assert_eq!(parse_drag_payload("no-colon"), None);
    }

    #[test]
    fn drop_position_splits_non_folder_in_half() {
        assert_eq!(
            drop_position_from_motion(4.0, 20.0, false),
            DropPosition::Above
        );
        assert_eq!(
            drop_position_from_motion(15.0, 20.0, false),
            DropPosition::Below
        );
    }

    #[test]
    fn drop_position_uses_three_zones_for_folders() {
        assert_eq!(
            drop_position_from_motion(2.0, 20.0, true),
            DropPosition::Above
        );
        assert_eq!(
            drop_position_from_motion(10.0, 20.0, true),
            DropPosition::Into
        );
        assert_eq!(
            drop_position_from_motion(18.0, 20.0, true),
            DropPosition::Below
        );
    }

    #[test]
    fn tracks_payload_round_trips_for_multiple_ids() {
        let ids = vec![
            TrackId::new(1).expect("positive"),
            TrackId::new(7).expect("positive"),
            TrackId::new(42).expect("positive"),
        ];
        let payload = tracks_drag_payload(&ids);
        assert_eq!(payload, "tracks:1,7,42");
        assert_eq!(parse_tracks_payload(&payload), Some(ids));
    }

    #[test]
    fn tracks_payload_rejects_malformed_input() {
        assert_eq!(parse_tracks_payload("tracks:"), None);
        assert_eq!(parse_tracks_payload("tracks:abc"), None);
        assert_eq!(parse_tracks_payload("tracks:-1"), None);
        assert_eq!(parse_tracks_payload("playlist:1"), None);
    }

    #[test]
    fn classify_drag_payload_distinguishes_kinds() {
        assert_eq!(
            classify_drag_payload("tracks:1,2,3"),
            Some(DragKind::Tracks)
        );
        assert_eq!(
            classify_drag_payload("playlist:7"),
            Some(DragKind::PlaylistItem)
        );
        assert_eq!(
            classify_drag_payload("folder:42"),
            Some(DragKind::PlaylistItem)
        );
        assert_eq!(
            classify_drag_payload("smart:3"),
            Some(DragKind::PlaylistItem)
        );
        assert_eq!(classify_drag_payload("garbage"), None);
        assert_eq!(classify_drag_payload("unknown:42"), None);
    }

    #[test]
    fn drop_position_handles_zero_height_gracefully() {
        assert_eq!(
            drop_position_from_motion(0.0, 0.0, false),
            DropPosition::Above
        );
        assert_eq!(
            drop_position_from_motion(0.0, 0.0, true),
            DropPosition::Into
        );
    }
}
