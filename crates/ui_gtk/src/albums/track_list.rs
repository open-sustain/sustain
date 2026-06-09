// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use std::rc::Rc;
use sustain_app_runtime::{PlaybackQueueRequest, PlaybackQueueSource, TrackId};

use super::{
    ensure_shuffle_disabled,
    model::{AlbumTrackViewModel, duration_text, track_number_text},
};
use crate::{
    PlaybackChangedCallback,
    cell_registry::{BindingRegistry, CellBinding, list_item_key},
    command_controller::SharedCommandController,
    missing_track::{LocateMissingTrackCallback, play_track_or_offer_locate},
    track_context::TrackRowContextMenu,
};

const STATUS_ICON_SIZE: i32 = 14;
const NUMBER_OR_STATUS_CELL_WIDTH: i32 = 24;
const TRACK_NUMBER_MIN_CHARS: i32 = 2;
const TRACK_TITLE_MAX_CHARS: i32 = 56;
const STATUS_ICON_PLAYING: &str = "audio-volume-high-symbolic";
const STATUS_ICON_MISSING: &str = "dialog-warning-symbolic";

#[derive(Clone)]
pub(super) struct AlbumTrackListView {
    widget: gtk::ScrolledWindow,
}

impl AlbumTrackListView {
    pub(super) fn new(
        tracks: &[AlbumTrackViewModel],
        ordered_track_ids: Vec<TrackId>,
        context_menu: TrackRowContextMenu,
        command_controller: SharedCommandController,
        playback_changed: PlaybackChangedCallback,
        locate_missing_track: LocateMissingTrackCallback,
        playing_track_id: Option<TrackId>,
    ) -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        for track in tracks {
            store.append(&glib::BoxedAnyObject::new(track.clone()));
        }
        // Albums-view track rows are not selectable: nothing in the UI acts on
        // an album-view selection, so a persistent highlight would be visual
        // noise that lies about state. Activation (double-click / Enter) and
        // right-click context menus still work without a selection model.
        let selection = gtk::NoSelection::new(Some(store));

        let album_track_ids = ordered_track_ids.clone();
        let context_menu = AlbumTrackContextMenu::new(context_menu, ordered_track_ids);
        let factory = build_row_factory(context_menu.clone(), playing_track_id);

        let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
        list.add_css_class("album-track-table");
        list.set_show_separators(false);
        list.set_single_click_activate(false);
        list.set_hexpand(true);
        list.set_vexpand(false);
        context_menu.install_controller(&list);

        let command_controller_for_activate = command_controller;
        let playback_changed_for_activate = playback_changed;
        // The album's playable tracks form the queue context: activating
        // any row plays it and then auto-advances through the rest of the
        // album in display order, instead of leaking into the broader
        // library. Captured once at view construction; the album-detail
        // panel is rebuilt when the album changes, so this list stays in
        // sync with what the user sees.
        list.connect_activate(move |_list, position| {
            let Some(track_id) = row_track_id(selection.item(position)) else {
                return;
            };
            ensure_shuffle_disabled(&command_controller_for_activate);
            play_track_or_offer_locate(
                &command_controller_for_activate,
                track_id,
                PlaybackQueueRequest::Explicit {
                    source: PlaybackQueueSource::Album,
                    ordered_track_ids: album_track_ids.clone(),
                },
                &playback_changed_for_activate,
                &locate_missing_track,
            );
        });

        // `GtkListView` implements `GtkScrollable` and expects to live inside a
        // `GtkScrolledWindow`. Wrap it in a non-scrolling one with
        // `propagate-natural-height` so the list requests the full height of
        // its rows and the outer Albums-view scroller handles overflow.
        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        scroller.set_propagate_natural_height(true);
        scroller.set_propagate_natural_width(false);
        scroller.set_hexpand(true);
        scroller.set_vexpand(false);
        scroller.add_css_class("album-track-table-scroller");
        scroller.set_child(Some(&list));

        Self { widget: scroller }
    }

    pub(super) fn widget(&self) -> gtk::ScrolledWindow {
        self.widget.clone()
    }
}

fn build_row_factory(
    context_menu: AlbumTrackContextMenu,
    playing_track_id: Option<TrackId>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    // The album-detail palette classes are always added; they are inert
    // until the detail panel installs the artwork-derived palette
    // provider display-wide (which happens asynchronously once the cover
    // decodes, #107). Albums without artwork never install a provider, so
    // the classes simply resolve to the default theme.
    let context_for_setup = context_menu;
    factory.connect_setup(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };

        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("album-track-row");
        row.set_hexpand(true);

        // First cell: track number, replaced by speaker (playing) or
        // warning (missing) icon when one of those states applies. The
        // icon and the number live in the same Box; visibility is toggled
        // in `refresh_status_icon` so only one is shown at a time and the
        // cell width stays stable.
        let number_or_status = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        number_or_status.add_css_class("album-track-number-cell");
        number_or_status.set_halign(gtk::Align::End);
        number_or_status.set_valign(gtk::Align::Center);
        number_or_status.set_size_request(NUMBER_OR_STATUS_CELL_WIDTH, -1);

        let status_icon = gtk::Image::new();
        status_icon.set_pixel_size(STATUS_ICON_SIZE);
        status_icon.set_halign(gtk::Align::End);
        status_icon.set_valign(gtk::Align::Center);
        status_icon.add_css_class("track-table-status-icon");
        status_icon.set_visible(false);
        number_or_status.append(&status_icon);

        let number = gtk::Label::new(None);
        number.add_css_class("album-track-number");
        number.set_xalign(1.0);
        number.set_width_chars(TRACK_NUMBER_MIN_CHARS);
        number.add_css_class("album-detail-palette-muted");
        number_or_status.append(&number);

        row.append(&number_or_status);

        let title = gtk::Label::new(None);
        title.add_css_class("album-track-title");
        title.set_xalign(0.0);
        title.set_hexpand(true);
        title.set_width_chars(1);
        title.set_max_width_chars(TRACK_TITLE_MAX_CHARS);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.add_css_class("album-detail-palette-primary");
        row.append(&title);

        let duration = gtk::Label::new(None);
        duration.add_css_class("album-track-duration");
        duration.set_xalign(1.0);
        duration.add_css_class("album-detail-palette-muted");
        row.append(&duration);

        context_for_setup.register_row(list_item, &row);

        list_item.set_child(Some(&row));
    });

    factory.connect_bind(move |_factory, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(row) = list_item
            .child()
            .and_then(|child| child.downcast::<gtk::Box>().ok())
        else {
            return;
        };
        let Some((status_icon, number, title, duration)) = row_widgets(&row) else {
            return;
        };
        let Some(row_object) = list_item
            .item()
            .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
        else {
            return;
        };
        let Ok(track) = row_object.try_borrow::<AlbumTrackViewModel>() else {
            return;
        };

        number.set_text(&track_number_text(&track));
        title.set_text(&track.title);
        duration.set_text(&duration_text(track.duration_seconds));

        sync_missing_class(&title, track.is_missing);
        drop(track);

        refresh_status_icon(list_item, &status_icon, &number, playing_track_id);
    });

    factory
}

fn row_widgets(row: &gtk::Box) -> Option<(gtk::Image, gtk::Label, gtk::Label, gtk::Label)> {
    let number_or_status = row.first_child()?.downcast::<gtk::Box>().ok()?;
    let status = number_or_status
        .first_child()?
        .downcast::<gtk::Image>()
        .ok()?;
    let number = status.next_sibling()?.downcast::<gtk::Label>().ok()?;
    let title = number_or_status
        .next_sibling()?
        .downcast::<gtk::Label>()
        .ok()?;
    let duration = title.next_sibling()?.downcast::<gtk::Label>().ok()?;
    Some((status, number, title, duration))
}

fn sync_missing_class(title: &gtk::Label, is_missing: bool) {
    if is_missing {
        title.add_css_class("album-track-missing");
    } else {
        title.remove_css_class("album-track-missing");
    }
}

fn row_track_id(item: Option<glib::Object>) -> Option<TrackId> {
    let row_object = item?.downcast::<glib::BoxedAnyObject>().ok()?;
    let track = row_object.try_borrow::<AlbumTrackViewModel>().ok()?;
    Some(track.id)
}

#[derive(Clone)]
struct AlbumTrackContextMenu {
    menu: TrackRowContextMenu,
    ordered_track_ids: Rc<Vec<TrackId>>,
    rows: BindingRegistry<AlbumTrackContextRow>,
}

impl AlbumTrackContextMenu {
    fn new(menu: TrackRowContextMenu, ordered_track_ids: Vec<TrackId>) -> Self {
        Self {
            menu,
            ordered_track_ids: Rc::new(ordered_track_ids),
            rows: BindingRegistry::default(),
        }
    }

    fn install_controller(&self, widget: &impl IsA<gtk::Widget>) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_SECONDARY);
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);

        let context = self.clone();
        gesture.connect_released(move |gesture, _n_press, x, y| {
            let Some(widget) = gesture.widget() else {
                return;
            };
            let Some(hit) = context.row_at(&widget, x, y) else {
                return;
            };
            let Some(track_id) = row_track_id(hit.item()) else {
                return;
            };

            gesture.set_state(gtk::EventSequenceState::Claimed);
            context.menu.popup_at(
                vec![track_id],
                context.ordered_track_ids.as_ref().clone(),
                &widget,
                x,
                y,
            );
        });
        widget.add_controller(gesture);
    }

    fn register_row(&self, list_item: &gtk::ListItem, row: &gtk::Box) {
        self.rows.push(AlbumTrackContextRow {
            key: list_item_key(list_item),
            widget: row.clone().upcast::<gtk::Widget>().downgrade(),
            list_item: list_item.downgrade(),
        });
    }

    fn row_at(&self, event_widget: &gtk::Widget, x: f64, y: f64) -> Option<gtk::ListItem> {
        let mut current = event_widget.pick(x, y, gtk::PickFlags::DEFAULT);
        while let Some(widget) = current {
            if let Some(hit) = self.list_item_for_widget(&widget) {
                return Some(hit);
            }
            current = widget.parent();
        }
        None
    }

    fn list_item_for_widget(&self, widget: &gtk::Widget) -> Option<gtk::ListItem> {
        self.rows.find_map_live(|row| {
            let registered = row.widget.upgrade()?;
            if registered == *widget {
                row.list_item.upgrade()
            } else {
                None
            }
        })
    }
}

struct AlbumTrackContextRow {
    key: usize,
    widget: glib::WeakRef<gtk::Widget>,
    list_item: glib::WeakRef<gtk::ListItem>,
}

impl CellBinding for AlbumTrackContextRow {
    fn key(&self) -> usize {
        self.key
    }

    fn list_item(&self) -> Option<gtk::ListItem> {
        self.list_item.upgrade()
    }

    fn is_live(&self) -> bool {
        self.widget.upgrade().is_some() && self.list_item.upgrade().is_some()
    }
}

fn refresh_status_icon(
    list_item: &gtk::ListItem,
    icon: &gtk::Image,
    number: &gtk::Label,
    playing_track_id: Option<TrackId>,
) {
    let Some(row_object) = list_item
        .item()
        .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
    else {
        clear_status_icon(icon, number);
        return;
    };
    let Ok(track) = row_object.try_borrow::<AlbumTrackViewModel>() else {
        clear_status_icon(icon, number);
        return;
    };

    icon.remove_css_class("track-table-status-playing");
    icon.remove_css_class("track-table-status-missing");

    if track.is_missing {
        icon.set_icon_name(Some(STATUS_ICON_MISSING));
        icon.add_css_class("track-table-status-missing");
        icon.set_visible(true);
        number.set_visible(false);
        return;
    }

    if matches!(playing_track_id, Some(playing_id) if track.id == playing_id) {
        icon.set_icon_name(Some(STATUS_ICON_PLAYING));
        icon.add_css_class("track-table-status-playing");
        icon.set_visible(true);
        number.set_visible(false);
        return;
    }

    clear_status_icon(icon, number);
}

fn clear_status_icon(icon: &gtk::Image, number: &gtk::Label) {
    icon.set_icon_name(None);
    icon.set_visible(false);
    number.set_visible(true);
}
