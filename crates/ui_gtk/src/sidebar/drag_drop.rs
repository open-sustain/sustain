// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{PlaylistFolderId, PlaylistId, PlaylistItem, SmartPlaylistId, TrackId};

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

pub(super) fn attach_drag_and_drop(
    row: &gtk::Widget,
    item: PlaylistItem,
    on_move: MoveCallbackHolder,
    on_tracks_drop: TracksDropCallbackHolder,
    current_indicator: SharedDropIndicator,
) {
    remove_drag_and_drop_controllers(row);

    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE);
    drag_source.connect_prepare(move |_source, _x, _y| {
        Some(gdk::ContentProvider::for_value(
            &drag_payload(item).to_value(),
        ))
    });
    row.add_controller(drag_source);

    let target_is_folder = matches!(item, PlaylistItem::Folder(_));
    let target_accepts_tracks = matches!(item, PlaylistItem::Playlist(_));
    let current_position: Rc<Cell<DropPosition>> = Rc::new(Cell::new(if target_is_folder {
        DropPosition::Into
    } else {
        DropPosition::Above
    }));

    let drop_target = gtk::DropTarget::new(
        glib::Type::STRING,
        gdk::DragAction::MOVE | gdk::DragAction::COPY,
    );
    drop_target.set_preload(true);

    let row_for_motion = row.clone();
    let current_position_for_motion = current_position.clone();
    let current_indicator_for_motion = current_indicator.clone();
    drop_target.connect_motion(move |target, _x, y| {
        let kind = peek_drag_kind(target);
        match kind {
            Some(DragKind::Tracks) if target_accepts_tracks => {
                if current_position_for_motion.get() != DropPosition::Into {
                    current_position_for_motion.set(DropPosition::Into);
                }
                set_drop_indicator(
                    &row_for_motion,
                    DropPosition::Into,
                    &current_indicator_for_motion,
                );
                gdk::DragAction::COPY
            }
            Some(DragKind::Tracks) => {
                clear_drop_indicator(&row_for_motion, &current_indicator_for_motion);
                gdk::DragAction::empty()
            }
            Some(DragKind::PlaylistItem) => {
                let row_height = row_for_motion.height() as f64;
                let position = drop_position_from_motion(y, row_height, target_is_folder);
                if current_position_for_motion.get() != position {
                    current_position_for_motion.set(position);
                }
                set_drop_indicator(&row_for_motion, position, &current_indicator_for_motion);
                gdk::DragAction::MOVE
            }
            None => {
                clear_drop_indicator(&row_for_motion, &current_indicator_for_motion);
                gdk::DragAction::empty()
            }
        }
    });

    let row_for_leave = row.clone();
    let current_indicator_for_leave = current_indicator.clone();
    drop_target.connect_leave(move |_target| {
        clear_drop_indicator(&row_for_leave, &current_indicator_for_leave);
    });

    let row_for_drop = row.clone();
    let current_position_for_drop = current_position.clone();
    let current_indicator_for_drop = current_indicator;
    drop_target.connect_drop(move |_target, value, _x, _y| {
        clear_drop_indicator(&row_for_drop, &current_indicator_for_drop);
        let position = current_position_for_drop.get();
        let Ok(text) = value.get::<String>() else {
            return false;
        };
        if let Some(track_ids) = parse_tracks_payload(&text) {
            if target_accepts_tracks && let Some(callback) = on_tracks_drop.borrow().as_ref() {
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
        if let Some(callback) = on_move.borrow().as_ref() {
            callback(source_item, item, position);
        }
        true
    });
    row.add_controller(drop_target);
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

fn remove_drag_and_drop_controllers(widget: &gtk::Widget) {
    let controllers = widget.observe_controllers();
    let mut to_remove: Vec<gtk::EventController> = Vec::new();
    for index in 0..controllers.n_items() {
        let Some(object) = controllers.item(index) else {
            continue;
        };
        let is_drag_or_drop = object.downcast_ref::<gtk::DragSource>().is_some()
            || object.downcast_ref::<gtk::DropTarget>().is_some();
        if !is_drag_or_drop {
            continue;
        }
        if let Ok(controller) = object.downcast::<gtk::EventController>() {
            to_remove.push(controller);
        }
    }
    for controller in to_remove {
        widget.remove_controller(&controller);
    }
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
