// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The play-queue popover (issue #80).
//!
//! Rather than carrying a dedicated button or growing the LIBRARY
//! section, the queue hangs off the transport Next control: a right-click
//! on Next opens an arrow popover listing explicit Up Next entries followed
//! by a bounded peek at the source continuation. The internal play order can
//! cover a 10,000-track library without ever becoming a 10,000-row model.
//!
//! Each row is a two-line cell (title over artist) with the track's
//! artwork on the left. Explicit Up Next rows expose an evict control that
//! fades in on hover and reorder by drag-and-drop; source-continuation rows
//! are a read-only preview. Double-clicking either kind starts that track
//! within the existing queue. Mutations flow through [`PlaybackCommand`] so
//! the runtime's `PlaybackQueue` stays the single source of truth. A
//! secondary (right) click on the Next button opens the popover; being
//! autohide, it dismisses on click-outside or Escape.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use sustain_app_runtime::{ApplicationCommand, PlaybackCommand, PlaybackQueueEntry, TrackId};

use crate::{
    PlaybackChangedCallback, SharedRuntime,
    artwork_loader::{ArtworkLoader, ArtworkSource, DecodedArtwork},
    command_controller::SharedCommandController,
    util::{display_artist, display_title},
};

/// Edge length of a row's artwork tile. A touch smaller than the
/// now-playing tile ([`crate::TITLEBAR_HEIGHT`] = 72) so a row stays
/// compact enough to fit ten-plus in the popover, while still reading as
/// the same kind of cover thumbnail.
const QUEUE_ROW_ARTWORK_SIZE: i32 = 56;

/// Vertical padding applied to each row in the stylesheet, mirrored here
/// only to estimate the popover's capped height.
const QUEUE_ROW_VERTICAL_PADDING: i32 = 6;

/// How many rows are visible before the list begins to scroll inside the
/// popover. The issue asks for "about 10/12 tracks".
const QUEUE_VISIBLE_ROWS: i32 = 11;

/// Source playthrough is informational context, not a 10,000-row browser.
/// Explicitly curated Up Next entries are always included in full.
const QUEUE_CONTINUATION_PREVIEW_ROWS: usize = 10;

/// Fixed popover content width. Titles and artists ellipsize within it so
/// a long title never stretches the popover.
const QUEUE_CONTENT_WIDTH: i32 = 304;

const QUEUE_PAGE_LIST: &str = "list";
const QUEUE_PAGE_EMPTY: &str = "empty";
const QUEUE_EMPTY_TEXT: &str = "Nothing up next";
const QUEUE_SMART_SHUFFLE_EMPTY_TITLE: &str = "No manually queued track";
const QUEUE_SMART_SHUFFLE_EMPTY_HELPER: &str = "Smart Shuffle will choose the next track.";
const QUEUE_EVICT_ICON: &str = "window-close-symbolic";
const QUEUE_ARTWORK_MISSING_ICON: &str = "image-missing-symbolic";
const QUEUE_ARTWORK_MISSING_ICON_SIZE: i32 = QUEUE_ROW_ARTWORK_SIZE / 2;

/// Per-row context shared with the list factory: everything a row's
/// handlers need to render, reorder, or evict without reaching back into
/// the [`QueueView`].
#[derive(Clone)]
struct QueueRowContext {
    runtime: SharedRuntime,
    artwork_loader: ArtworkLoader,
    command_controller: SharedCommandController,
    /// Rebuilds the model and refreshes now-playing/MPRIS after an evict
    /// or reorder changes the queue.
    on_mutated: Rc<dyn Fn()>,
}

#[derive(Clone)]
struct QueueEmptyState {
    widget: gtk::Box,
    title: gtk::Label,
    helper: gtk::Label,
}

impl QueueEmptyState {
    fn new() -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 2);
        widget.add_css_class("queue-empty");
        widget.set_halign(gtk::Align::Center);
        widget.set_valign(gtk::Align::Center);
        widget.set_width_request(QUEUE_CONTENT_WIDTH);

        let title = gtk::Label::new(Some(QUEUE_EMPTY_TEXT));
        title.add_css_class("queue-empty-title");
        title.set_halign(gtk::Align::Center);
        title.set_justify(gtk::Justification::Center);
        title.set_wrap(true);
        widget.append(&title);

        let helper = gtk::Label::new(None);
        helper.add_css_class("queue-empty-helper");
        helper.set_halign(gtk::Align::Center);
        helper.set_justify(gtk::Justification::Center);
        helper.set_wrap(true);
        helper.set_visible(false);
        widget.append(&helper);

        Self {
            widget,
            title,
            helper,
        }
    }

    fn sync(&self, uses_lazy_continuation: bool) {
        let (title, helper) = empty_queue_text(uses_lazy_continuation);
        self.title.set_text(title);
        self.helper.set_text(helper.unwrap_or_default());
        self.helper.set_visible(helper.is_some());
    }
}

#[derive(Clone)]
pub(crate) struct QueueView {
    runtime: SharedRuntime,
    store: gio::ListStore,
    stack: gtk::Stack,
    empty_state: QueueEmptyState,
    is_open: Rc<Cell<bool>>,
}

impl QueueView {
    pub(crate) fn new(
        runtime: SharedRuntime,
        command_controller: SharedCommandController,
        artwork_loader: ArtworkLoader,
        playback_changed: PlaybackChangedCallback,
        next_button: &gtk::Button,
    ) -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let stack = gtk::Stack::new();
        let is_open = Rc::new(Cell::new(false));
        let empty_state = QueueEmptyState::new();

        let popover = gtk::Popover::new();
        popover.add_css_class("queue-popover");
        // Click-driven: a secondary-click on Next opens it, and autohide
        // gives click-outside / Escape dismissal for free — far simpler and
        // more robust than a hover bridge, and what the maintainer asked
        // for. Drag-to-reorder is initiated by a press inside the popover,
        // which does not trip the autohide dismissal.
        popover.set_autohide(true);
        popover.set_position(gtk::PositionType::Bottom);
        popover.set_has_arrow(true);

        let on_mutated: Rc<dyn Fn()> = {
            let runtime = runtime.clone();
            let store = store.clone();
            let stack = stack.clone();
            let empty_state = empty_state.clone();
            Rc::new(move || {
                rebuild_queue_model(&runtime, &store, &stack, &empty_state);
                playback_changed();
            })
        };

        let factory = build_queue_row_factory(QueueRowContext {
            runtime: runtime.clone(),
            artwork_loader,
            command_controller: command_controller.clone(),
            on_mutated: on_mutated.clone(),
        });

        let selection = gtk::NoSelection::new(Some(store.clone()));
        let list = gtk::ListView::new(Some(selection), Some(factory));
        list.add_css_class("queue-list");
        list.set_show_separators(false);
        list.set_single_click_activate(false);
        {
            let store = store.clone();
            let command_controller = command_controller.clone();
            list.connect_activate(move |_, position| {
                let Some(entry) = queue_row_entry(store.item(position)) else {
                    return;
                };
                if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
                    PlaybackCommand::PlayQueueTrack(entry.track_id()),
                )) {
                    on_mutated();
                }
            });
        }

        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("queue-scroller");
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_propagate_natural_height(true);
        scroller.set_propagate_natural_width(false);
        scroller.set_min_content_width(QUEUE_CONTENT_WIDTH);
        scroller.set_max_content_width(QUEUE_CONTENT_WIDTH);
        scroller.set_max_content_height(
            QUEUE_VISIBLE_ROWS * (QUEUE_ROW_ARTWORK_SIZE + 2 * QUEUE_ROW_VERTICAL_PADDING),
        );
        scroller.set_child(Some(&list));

        stack.add_named(&scroller, Some(QUEUE_PAGE_LIST));
        stack.add_named(&empty_state.widget, Some(QUEUE_PAGE_EMPTY));
        stack.set_visible_child_name(QUEUE_PAGE_EMPTY);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("queue-popover-content");
        content.append(&stack);
        popover.set_child(Some(&content));
        popover.set_parent(next_button);

        install_right_click_open(
            next_button,
            &popover,
            &runtime,
            &store,
            &stack,
            &empty_state,
            &is_open,
        );

        {
            let store = store.clone();
            let is_open = is_open.clone();
            popover.connect_closed(move |_| {
                is_open.set(false);
                // Release the (potentially large) model while the popover is
                // down; the next open rebuilds it.
                store.remove_all();
            });
        }

        {
            // A popover attached with `set_parent` must be unparented before
            // its parent is finalized or GTK warns and leaks the surface.
            let popover = popover.clone();
            next_button.connect_destroy(move |_| {
                if popover.parent().is_some() {
                    popover.unparent();
                }
            });
        }

        Self {
            runtime,
            store,
            stack,
            empty_state,
            is_open,
        }
    }

    /// Rebuild the visible queue if the popover is currently open. Wired
    /// into the shared playback-changed callback so skipping or
    /// auto-advancing a track while the queue is on screen keeps it live.
    pub(crate) fn refresh_if_visible(&self) {
        if self.is_open.get() {
            rebuild_queue_model(&self.runtime, &self.store, &self.stack, &self.empty_state);
        }
    }
}

/// Replace the model with explicit Up Next plus a bounded source-continuation
/// peek and switch to the matching page. Releases the
/// runtime borrow before the splice so the synchronous re-bind it
/// triggers can look tracks back up freely.
fn rebuild_queue_model(
    runtime: &SharedRuntime,
    store: &gio::ListStore,
    stack: &gtk::Stack,
    empty_state: &QueueEmptyState,
) {
    let (upcoming, uses_lazy_continuation) = {
        let runtime = runtime.borrow();
        (
            runtime.playback_queue_upcoming_preview(QUEUE_CONTINUATION_PREVIEW_ROWS),
            runtime.playback_queue_uses_lazy_continuation(),
        )
    };
    if upcoming.is_empty() {
        empty_state.sync(uses_lazy_continuation);
        store.remove_all();
        stack.set_visible_child_name(QUEUE_PAGE_EMPTY);
        return;
    }
    let objects: Vec<glib::BoxedAnyObject> = upcoming
        .into_iter()
        .map(glib::BoxedAnyObject::new)
        .collect();
    store.splice(0, store.n_items(), &objects);
    stack.set_visible_child_name(QUEUE_PAGE_LIST);
}

/// Open the queue on a secondary (right) click of the Next button. The
/// popover is autohide, so click-outside and Escape dismiss it; we only
/// have to build the model and pop it up. The model is rebuilt on each
/// open so the queue is always current.
fn install_right_click_open(
    next_button: &gtk::Button,
    popover: &gtk::Popover,
    runtime: &SharedRuntime,
    store: &gio::ListStore,
    stack: &gtk::Stack,
    empty_state: &QueueEmptyState,
    is_open: &Rc<Cell<bool>>,
) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gdk::BUTTON_SECONDARY);

    let popover = popover.clone();
    let runtime = runtime.clone();
    let store = store.clone();
    let stack = stack.clone();
    let empty_state = empty_state.clone();
    let is_open = is_open.clone();
    gesture.connect_pressed(move |gesture, _n_press, _x, _y| {
        // Claim the press so it does not also reach the window-drag handler
        // on the titlebar handle behind the button.
        gesture.set_state(gtk::EventSequenceState::Claimed);
        rebuild_queue_model(&runtime, &store, &stack, &empty_state);
        is_open.set(true);
        popover.popup();
    });
    next_button.add_controller(gesture);
}

fn empty_queue_text(uses_lazy_continuation: bool) -> (&'static str, Option<&'static str>) {
    if uses_lazy_continuation {
        (
            QUEUE_SMART_SHUFFLE_EMPTY_TITLE,
            Some(QUEUE_SMART_SHUFFLE_EMPTY_HELPER),
        )
    } else {
        (QUEUE_EMPTY_TEXT, None)
    }
}

fn build_queue_row_factory(ctx: QueueRowContext) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();

    let setup_ctx = ctx.clone();
    factory.connect_setup(move |_, item| {
        let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = build_queue_row();
        install_queue_row_handlers(&row, list_item, &setup_ctx);
        list_item.set_child(Some(&row));
    });

    let runtime = ctx.runtime.clone();
    let artwork_loader = ctx.artwork_loader.clone();
    factory.connect_bind(move |_, item| {
        bind_queue_row(item, &runtime, &artwork_loader);
    });

    factory
}

fn build_queue_row() -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    row.add_css_class("queue-row");
    row.set_hexpand(true);

    let artwork = gtk::Box::new(gtk::Orientation::Vertical, 0);
    artwork.add_css_class("queue-row-artwork");
    artwork.set_size_request(QUEUE_ROW_ARTWORK_SIZE, QUEUE_ROW_ARTWORK_SIZE);
    artwork.set_overflow(gtk::Overflow::Hidden);
    artwork.set_valign(gtk::Align::Center);

    let image = gtk::Image::from_icon_name(QUEUE_ARTWORK_MISSING_ICON);
    image.add_css_class("queue-row-artwork-missing-icon");
    image.set_pixel_size(QUEUE_ARTWORK_MISSING_ICON_SIZE);
    image.set_hexpand(true);
    image.set_vexpand(true);
    image.set_halign(gtk::Align::Fill);
    image.set_valign(gtk::Align::Fill);
    artwork.append(&image);
    row.append(&artwork);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.add_css_class("queue-row-text");
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);

    let title = gtk::Label::new(None);
    title.add_css_class("queue-row-title");
    title.set_xalign(0.0);
    title.set_hexpand(true);
    title.set_width_chars(1);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let artist = gtk::Label::new(None);
    artist.add_css_class("queue-row-artist");
    artist.set_xalign(0.0);
    artist.set_hexpand(true);
    artist.set_width_chars(1);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);

    text.append(&title);
    text.append(&artist);
    row.append(&text);

    let evict = gtk::Button::from_icon_name(QUEUE_EVICT_ICON);
    evict.add_css_class("queue-row-evict");
    evict.add_css_class("flat");
    evict.set_valign(gtk::Align::Center);
    evict.set_tooltip_text(Some("Remove from queue"));
    // Hidden until the row is hovered; kept allocated (opacity, not
    // visibility) so revealing it never shifts the title's width, and
    // untargetable while invisible so it is never an unseen click target.
    evict.set_opacity(0.0);
    evict.set_can_target(false);
    row.append(&evict);

    row
}

fn install_queue_row_handlers(row: &gtk::Box, list_item: &gtk::ListItem, ctx: &QueueRowContext) {
    let Some((_, _, _, evict)) = queue_row_widgets(row) else {
        return;
    };

    {
        let list_item = list_item.downgrade();
        let command_controller = ctx.command_controller.clone();
        let on_mutated = ctx.on_mutated.clone();
        evict.connect_clicked(move |_| {
            let Some(list_item) = list_item.upgrade() else {
                return;
            };
            let Some(entry) = queue_row_entry(list_item.item()) else {
                return;
            };
            if !entry.is_curated() {
                return;
            }
            if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
                PlaybackCommand::RemoveFromQueue(entry.track_id()),
            )) {
                on_mutated();
            }
        });
    }

    {
        let motion = gtk::EventControllerMotion::new();
        let list_item = list_item.downgrade();
        let evict_enter = evict.clone();
        motion.connect_enter(move |_, _, _| {
            let Some(list_item) = list_item.upgrade() else {
                return;
            };
            if !queue_row_entry(list_item.item()).is_some_and(PlaybackQueueEntry::is_curated) {
                return;
            }
            evict_enter.set_opacity(1.0);
            evict_enter.set_can_target(true);
        });
        let evict_leave = evict.clone();
        motion.connect_leave(move |_| {
            evict_leave.set_opacity(0.0);
            evict_leave.set_can_target(false);
        });
        row.add_controller(motion);
    }

    install_queue_row_drag(row, list_item);
    install_queue_row_drop(row, list_item, ctx);
}

fn install_queue_row_drag(row: &gtk::Box, list_item: &gtk::ListItem) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE);

    let list_item = list_item.downgrade();
    drag_source.connect_prepare(move |_, _, _| {
        let list_item = list_item.upgrade()?;
        let entry = queue_row_entry(list_item.item())?;
        if !entry.is_curated() {
            return None;
        }
        Some(gdk::ContentProvider::for_value(
            &queue_drag_payload(entry.track_id()).to_value(),
        ))
    });

    let row_for_icon = row.clone();
    drag_source.connect_drag_begin(move |source, _| {
        let paintable = gtk::WidgetPaintable::new(Some(&row_for_icon));
        source.set_icon(Some(&paintable), 0, 0);
    });

    row.add_controller(drag_source);
}

fn install_queue_row_drop(row: &gtk::Box, list_item: &gtk::ListItem, ctx: &QueueRowContext) {
    let drop_target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    drop_target.set_preload(true);

    let row_for_motion = row.clone();
    let item_for_motion = list_item.downgrade();
    drop_target.connect_motion(move |target, _, y| {
        if peek_queue_payload(target).is_none() {
            return gdk::DragAction::empty();
        }
        let Some(list_item) = item_for_motion.upgrade() else {
            return gdk::DragAction::empty();
        };
        if !queue_row_entry(list_item.item()).is_some_and(PlaybackQueueEntry::is_curated) {
            return gdk::DragAction::empty();
        }
        set_queue_drop_indicator(&row_for_motion, drop_after(&row_for_motion, y));
        gdk::DragAction::MOVE
    });

    let row_for_leave = row.clone();
    drop_target.connect_leave(move |_| clear_queue_drop_indicator(&row_for_leave));

    let row_for_drop = row.clone();
    let list_item = list_item.downgrade();
    let command_controller = ctx.command_controller.clone();
    let on_mutated = ctx.on_mutated.clone();
    drop_target.connect_drop(move |_, value, _, y| {
        clear_queue_drop_indicator(&row_for_drop);
        let Ok(text) = value.get::<String>() else {
            return false;
        };
        let Some(dragged) = parse_queue_payload(&text) else {
            return false;
        };
        let Some(list_item) = list_item.upgrade() else {
            return false;
        };
        let Some(target) = queue_row_entry(list_item.item()) else {
            return false;
        };
        if !target.is_curated() {
            return false;
        }
        let target_id = target.track_id();
        if dragged == target_id {
            return false;
        }
        let place_after = drop_after(&row_for_drop, y);
        if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
            PlaybackCommand::ReorderQueue {
                track_id: dragged,
                target_track_id: target_id,
                place_after,
            },
        )) {
            on_mutated();
            true
        } else {
            false
        }
    });

    row.add_controller(drop_target);
}

fn bind_queue_row(item: &glib::Object, runtime: &SharedRuntime, artwork_loader: &ArtworkLoader) {
    let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
        return;
    };
    let Some(row) = list_item
        .child()
        .and_then(|child| child.downcast::<gtk::Box>().ok())
    else {
        return;
    };
    let Some((image, title, artist, _evict)) = queue_row_widgets(&row) else {
        return;
    };
    let Some(entry) = queue_row_entry(list_item.item()) else {
        return;
    };
    let track_id = entry.track_id();

    apply_queue_row_tint(&row, list_item.position());
    apply_queue_row_kind(&row, entry);

    // Resolve text and artwork source under a single short borrow, dropped
    // before any work (the splice-driven rebind, the async request) that
    // could re-enter the runtime.
    let resolved = {
        let runtime = runtime.borrow();
        runtime.library_track(track_id).map(|track| {
            let source = runtime.absolute_track_path(track).map(|absolute| {
                ArtworkSource::embedded_track(track.location.path().to_path_buf(), absolute)
            });
            (
                display_title(track),
                display_artist(&track.metadata),
                source,
            )
        })
    };

    let Some((title_text, artist_text, source)) = resolved else {
        title.set_text("");
        artist.set_text("");
        apply_queue_missing_artwork(&image);
        return;
    };
    title.set_text(&title_text);
    artist.set_text(&artist_text);

    // Clear first so a recycled row never shows the previous track's cover
    // in the gap before the new one resolves.
    apply_queue_missing_artwork(&image);
    let Some(source) = source else {
        return;
    };
    if let Some(decoded) = artwork_loader.cached(&source) {
        apply_queue_artwork(&image, &decoded);
        return;
    }

    // Async decode: only paint if this recycled row is still bound to the
    // same track when the texture lands.
    let list_item_weak = list_item.downgrade();
    let image_weak = image.downgrade();
    artwork_loader.request(
        source,
        Box::new(move |decoded| {
            let Some(list_item) = list_item_weak.upgrade() else {
                return;
            };
            if queue_row_track_id(list_item.item()) != Some(track_id) {
                return;
            }
            let Some(image) = image_weak.upgrade() else {
                return;
            };
            apply_queue_artwork(&image, &decoded);
        }),
    );
}

fn apply_queue_artwork(image: &gtk::Image, decoded: &DecodedArtwork) {
    match decoded
        .tile_texture
        .as_ref()
        .or(decoded.detail_texture.as_ref())
    {
        Some(texture) => {
            image.set_pixel_size(QUEUE_ROW_ARTWORK_SIZE);
            image.set_paintable(Some(texture));
        }
        None => apply_queue_missing_artwork(image),
    }
}

fn apply_queue_missing_artwork(image: &gtk::Image) {
    image.set_pixel_size(QUEUE_ARTWORK_MISSING_ICON_SIZE);
    image.set_icon_name(Some(QUEUE_ARTWORK_MISSING_ICON));
}

fn apply_queue_row_tint(row: &gtk::Box, position: u32) {
    row.remove_css_class("queue-row-even");
    row.remove_css_class("queue-row-odd");
    if position % 2 == 0 {
        row.add_css_class("queue-row-even");
    } else {
        row.add_css_class("queue-row-odd");
    }
}

fn apply_queue_row_kind(row: &gtk::Box, entry: PlaybackQueueEntry) {
    let Some((_, _, _, evict)) = queue_row_widgets(row) else {
        return;
    };
    evict.set_opacity(0.0);
    evict.set_can_target(false);
    if entry.is_curated() {
        row.remove_css_class("queue-row-continuation");
    } else {
        row.add_css_class("queue-row-continuation");
    }
}

fn queue_row_widgets(row: &gtk::Box) -> Option<(gtk::Image, gtk::Label, gtk::Label, gtk::Button)> {
    let artwork = row.first_child()?.downcast::<gtk::Box>().ok()?;
    let image = artwork.first_child()?.downcast::<gtk::Image>().ok()?;
    let text = artwork.next_sibling()?.downcast::<gtk::Box>().ok()?;
    let title = text.first_child()?.downcast::<gtk::Label>().ok()?;
    let artist = title.next_sibling()?.downcast::<gtk::Label>().ok()?;
    let evict = text.next_sibling()?.downcast::<gtk::Button>().ok()?;
    Some((image, title, artist, evict))
}

fn queue_row_track_id(item: Option<glib::Object>) -> Option<TrackId> {
    queue_row_entry(item).map(PlaybackQueueEntry::track_id)
}

fn queue_row_entry(item: Option<glib::Object>) -> Option<PlaybackQueueEntry> {
    let boxed = item?.downcast::<glib::BoxedAnyObject>().ok()?;
    let entry = boxed.try_borrow::<PlaybackQueueEntry>().ok()?;
    Some(*entry)
}

fn drop_after(row: &gtk::Box, y: f64) -> bool {
    y > f64::from(row.height()) / 2.0
}

fn set_queue_drop_indicator(row: &gtk::Box, place_after: bool) {
    row.remove_css_class("queue-row-drop-above");
    row.remove_css_class("queue-row-drop-below");
    if place_after {
        row.add_css_class("queue-row-drop-below");
    } else {
        row.add_css_class("queue-row-drop-above");
    }
}

fn clear_queue_drop_indicator(row: &gtk::Box) {
    row.remove_css_class("queue-row-drop-above");
    row.remove_css_class("queue-row-drop-below");
}

/// Reorder payload prefix, deliberately distinct from the `tracks:`
/// payload the library tables emit so a queue-row drag only ever reorders
/// within the queue and is rejected by playlist/sidebar drop targets.
fn queue_drag_payload(track_id: TrackId) -> String {
    format!("queue-track:{}", track_id.get())
}

fn parse_queue_payload(text: &str) -> Option<TrackId> {
    let (kind, id) = text.split_once(':')?;
    if kind != "queue-track" {
        return None;
    }
    id.trim().parse::<i64>().ok().and_then(TrackId::new)
}

fn peek_queue_payload(target: &gtk::DropTarget) -> Option<TrackId> {
    let value = target.value()?;
    let text = value.get::<String>().ok()?;
    parse_queue_payload(&text)
}

#[cfg(test)]
mod tests {
    use super::{
        QUEUE_EMPTY_TEXT, QUEUE_SMART_SHUFFLE_EMPTY_HELPER, QUEUE_SMART_SHUFFLE_EMPTY_TITLE,
        empty_queue_text, parse_queue_payload, queue_drag_payload,
    };
    use sustain_app_runtime::TrackId;

    fn track_id(value: i64) -> TrackId {
        TrackId::new(value).expect("valid test track id")
    }

    #[test]
    fn queue_payload_round_trips() {
        assert_eq!(
            parse_queue_payload(&queue_drag_payload(track_id(42))),
            Some(track_id(42))
        );
    }

    #[test]
    fn queue_payload_rejects_foreign_prefixes() {
        assert_eq!(parse_queue_payload("tracks:42"), None);
        assert_eq!(parse_queue_payload("playlist:7"), None);
        assert_eq!(parse_queue_payload("queue-track:not-a-number"), None);
    }

    #[test]
    fn smart_shuffle_empty_text_explains_on_demand_continuation() {
        assert_eq!(empty_queue_text(false), (QUEUE_EMPTY_TEXT, None));
        assert_eq!(
            empty_queue_text(true),
            (
                QUEUE_SMART_SHUFFLE_EMPTY_TITLE,
                Some(QUEUE_SMART_SHUFFLE_EMPTY_HELPER)
            )
        );
    }
}
