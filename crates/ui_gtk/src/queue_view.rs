// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The play-queue popover (issue #80).
//!
//! Rather than carrying a dedicated button or growing the LIBRARY
//! section, the queue hangs off the transport Next control: resting the
//! pointer on Next for a beat opens an arrow popover listing what plays
//! after the current track. The list is a virtualised `gtk::ListView` so
//! the common case — the entire library queued from the Songs view —
//! stays cheap; only the handful of visible rows are ever realised.
//!
//! Each row is a two-line cell (title over artist) with the track's
//! artwork on the left and an evict control that fades in on hover. Rows
//! reorder by drag-and-drop and evict on click; both flow through
//! [`PlaybackCommand`] so the runtime's `PlaybackQueue` stays the single
//! source of truth. The popover opens after a short hover (so a quick
//! fly-over does not trigger it) and closes once the pointer leaves both
//! the button and the popover, with a small grace window so crossing the
//! arrow gap between them does not flicker it shut.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use sustain_app_runtime::{ApplicationCommand, PlaybackCommand, TrackId};

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

/// Fixed popover content width. Titles and artists ellipsize within it so
/// a long title never stretches the popover.
const QUEUE_CONTENT_WIDTH: i32 = 380;

/// Hover dwell before the popover opens. Long enough to reject a quick
/// fly-over of the Next button, short enough to feel immediate.
const QUEUE_HOVER_OPEN_DELAY: Duration = Duration::from_millis(50);

/// Grace window after the pointer leaves the button or popover before the
/// popover closes. Covers the brief moment when the pointer is crossing
/// the arrow gap between the two and momentarily inside neither.
const QUEUE_HOVER_CLOSE_GRACE: Duration = Duration::from_millis(150);

const QUEUE_PAGE_LIST: &str = "list";
const QUEUE_PAGE_EMPTY: &str = "empty";
const QUEUE_EVICT_ICON: &str = "window-close-symbolic";
const NEXT_BUTTON_TOOLTIP: &str = "Next";

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
    /// Set while a row drag is in flight so the hover bridge never closes
    /// the popover mid-reorder.
    drag_active: Rc<Cell<bool>>,
    /// Re-arms the close check when a drag ends, in case it finished with
    /// the pointer already outside the popover.
    schedule_close: Rc<dyn Fn()>,
}

#[derive(Clone)]
pub(crate) struct QueueView {
    runtime: SharedRuntime,
    store: gio::ListStore,
    stack: gtk::Stack,
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

        let popover = gtk::Popover::new();
        popover.add_css_class("queue-popover");
        // Hover-driven, not click-driven: autohide would grab input and
        // close on the first outside click. We own show/hide via motion.
        popover.set_autohide(false);
        popover.set_position(gtk::PositionType::Bottom);
        popover.set_has_arrow(true);

        // Hover-bridge state shared between the button and popover motion
        // controllers and the open/close timers.
        let pointer_in_button = Rc::new(Cell::new(false));
        let pointer_in_popover = Rc::new(Cell::new(false));
        let drag_active = Rc::new(Cell::new(false));
        let open_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        let close_timer: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

        let schedule_close = build_schedule_close(
            &popover,
            &pointer_in_button,
            &pointer_in_popover,
            &drag_active,
            &close_timer,
        );

        let on_mutated: Rc<dyn Fn()> = {
            let runtime = runtime.clone();
            let store = store.clone();
            let stack = stack.clone();
            Rc::new(move || {
                rebuild_queue_model(&runtime, &store, &stack);
                playback_changed();
            })
        };

        let factory = build_queue_row_factory(QueueRowContext {
            runtime: runtime.clone(),
            artwork_loader,
            command_controller,
            on_mutated,
            drag_active: drag_active.clone(),
            schedule_close: schedule_close.clone(),
        });

        let selection = gtk::NoSelection::new(Some(store.clone()));
        let list = gtk::ListView::new(Some(selection), Some(factory));
        list.add_css_class("queue-list");
        list.set_show_separators(false);
        list.set_single_click_activate(false);

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

        let empty_label = gtk::Label::new(Some("Nothing up next"));
        empty_label.add_css_class("queue-empty");
        empty_label.set_halign(gtk::Align::Center);
        empty_label.set_valign(gtk::Align::Center);
        empty_label.set_width_request(QUEUE_CONTENT_WIDTH);

        stack.add_named(&scroller, Some(QUEUE_PAGE_LIST));
        stack.add_named(&empty_label, Some(QUEUE_PAGE_EMPTY));
        stack.set_visible_child_name(QUEUE_PAGE_EMPTY);

        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.add_css_class("queue-popover-content");
        content.append(&stack);
        popover.set_child(Some(&content));
        popover.set_parent(next_button);

        install_button_hover(
            next_button,
            &popover,
            &runtime,
            &store,
            &stack,
            &is_open,
            &pointer_in_button,
            &open_timer,
            &close_timer,
            &schedule_close,
        );
        install_popover_hover(&content, &pointer_in_popover, &close_timer, &schedule_close);

        {
            let store = store.clone();
            let is_open = is_open.clone();
            let next_button = next_button.clone();
            popover.connect_closed(move |_| {
                is_open.set(false);
                next_button.set_tooltip_text(Some(NEXT_BUTTON_TOOLTIP));
                // Release the (potentially library-sized) model while the
                // popover is down; the next open rebuilds it.
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
            is_open,
        }
    }

    /// Rebuild the visible queue if the popover is currently open. Wired
    /// into the shared playback-changed callback so skipping or
    /// auto-advancing a track while the queue is on screen keeps it live.
    pub(crate) fn refresh_if_visible(&self) {
        if self.is_open.get() {
            rebuild_queue_model(&self.runtime, &self.store, &self.stack);
        }
    }
}

/// Replace the model with the current upcoming tail and switch to the
/// matching page. Reads the upcoming ids (a cheap copy), releasing the
/// runtime borrow before the splice so the synchronous re-bind it
/// triggers can look tracks back up freely.
fn rebuild_queue_model(runtime: &SharedRuntime, store: &gio::ListStore, stack: &gtk::Stack) {
    let upcoming = runtime.borrow().playback_queue_upcoming_track_ids();
    if upcoming.is_empty() {
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

#[allow(clippy::too_many_arguments)]
fn install_button_hover(
    next_button: &gtk::Button,
    popover: &gtk::Popover,
    runtime: &SharedRuntime,
    store: &gio::ListStore,
    stack: &gtk::Stack,
    is_open: &Rc<Cell<bool>>,
    pointer_in_button: &Rc<Cell<bool>>,
    open_timer: &Rc<RefCell<Option<glib::SourceId>>>,
    close_timer: &Rc<RefCell<Option<glib::SourceId>>>,
    schedule_close: &Rc<dyn Fn()>,
) {
    let motion = gtk::EventControllerMotion::new();

    {
        let popover = popover.clone();
        let runtime = runtime.clone();
        let store = store.clone();
        let stack = stack.clone();
        let is_open = is_open.clone();
        let pointer_in_button = pointer_in_button.clone();
        let open_timer = open_timer.clone();
        let close_timer = close_timer.clone();
        let next_button = next_button.clone();
        motion.connect_enter(move |_, _, _| {
            pointer_in_button.set(true);
            cancel_timer(&close_timer);
            if is_open.get() || open_timer.borrow().is_some() {
                return;
            }
            let popover = popover.clone();
            let runtime = runtime.clone();
            let store = store.clone();
            let stack = stack.clone();
            let is_open = is_open.clone();
            let pointer_in_button = pointer_in_button.clone();
            let open_timer_inner = open_timer.clone();
            let next_button = next_button.clone();
            let id = glib::timeout_add_local_once(QUEUE_HOVER_OPEN_DELAY, move || {
                open_timer_inner.borrow_mut().take();
                // The dwell guard rejected a fly-over unless the pointer is
                // still resting on the button.
                if !pointer_in_button.get() {
                    return;
                }
                // Nothing playing → the queue has no anchor and Next is a
                // no-op, so an empty popover would be pure noise.
                if runtime.borrow().playback_queue_current_track_id().is_none() {
                    return;
                }
                rebuild_queue_model(&runtime, &store, &stack);
                is_open.set(true);
                // Suppress the redundant "Next" tooltip while the richer
                // queue surface is open; restored on close.
                next_button.set_tooltip_text(None);
                popover.popup();
            });
            *open_timer.borrow_mut() = Some(id);
        });
    }

    {
        // The leave handler defers to the shared close scheduler, which
        // re-checks both pointer cells and the drag flag when it fires —
        // so a pointer that left the button only to enter the popover
        // keeps it open without the button controller needing to know.
        let pointer_in_button = pointer_in_button.clone();
        let open_timer = open_timer.clone();
        let schedule_close = schedule_close.clone();
        motion.connect_leave(move |_| {
            pointer_in_button.set(false);
            cancel_timer(&open_timer);
            schedule_close();
        });
    }

    next_button.add_controller(motion);
}

fn install_popover_hover(
    content: &gtk::Box,
    pointer_in_popover: &Rc<Cell<bool>>,
    close_timer: &Rc<RefCell<Option<glib::SourceId>>>,
    schedule_close: &Rc<dyn Fn()>,
) {
    let motion = gtk::EventControllerMotion::new();
    {
        let pointer_in_popover = pointer_in_popover.clone();
        let close_timer = close_timer.clone();
        motion.connect_enter(move |_, _, _| {
            pointer_in_popover.set(true);
            cancel_timer(&close_timer);
        });
    }
    {
        let pointer_in_popover = pointer_in_popover.clone();
        let schedule_close = schedule_close.clone();
        motion.connect_leave(move |_| {
            pointer_in_popover.set(false);
            schedule_close();
        });
    }
    content.add_controller(motion);
}

/// The shared close scheduler captured by row drags and the popover's own
/// leave handler. Closes only when the pointer rests in neither the
/// button nor the popover and no drag is in flight.
fn build_schedule_close(
    popover: &gtk::Popover,
    pointer_in_button: &Rc<Cell<bool>>,
    pointer_in_popover: &Rc<Cell<bool>>,
    drag_active: &Rc<Cell<bool>>,
    close_timer: &Rc<RefCell<Option<glib::SourceId>>>,
) -> Rc<dyn Fn()> {
    let popover = popover.clone();
    let pointer_in_button = pointer_in_button.clone();
    let pointer_in_popover = pointer_in_popover.clone();
    let drag_active = drag_active.clone();
    let close_timer = close_timer.clone();
    Rc::new(move || {
        cancel_timer(&close_timer);
        let popover = popover.clone();
        let pointer_in_button = pointer_in_button.clone();
        let pointer_in_popover = pointer_in_popover.clone();
        let drag_active = drag_active.clone();
        let close_timer_inner = close_timer.clone();
        let id = glib::timeout_add_local_once(QUEUE_HOVER_CLOSE_GRACE, move || {
            close_timer_inner.borrow_mut().take();
            if drag_active.get() {
                return;
            }
            if pointer_in_button.get() || pointer_in_popover.get() {
                return;
            }
            popover.popdown();
        });
        *close_timer.borrow_mut() = Some(id);
    })
}

fn cancel_timer(timer: &Rc<RefCell<Option<glib::SourceId>>>) {
    if let Some(existing) = timer.borrow_mut().take() {
        existing.remove();
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

    let image = gtk::Image::new();
    image.set_pixel_size(QUEUE_ROW_ARTWORK_SIZE);
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
            let Some(track_id) = queue_row_track_id(list_item.item()) else {
                return;
            };
            if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
                PlaybackCommand::RemoveFromQueue(track_id),
            )) {
                on_mutated();
            }
        });
    }

    {
        let motion = gtk::EventControllerMotion::new();
        let evict_enter = evict.clone();
        motion.connect_enter(move |_, _, _| {
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

    install_queue_row_drag(row, list_item, ctx);
    install_queue_row_drop(row, list_item, ctx);
}

fn install_queue_row_drag(row: &gtk::Box, list_item: &gtk::ListItem, ctx: &QueueRowContext) {
    let drag_source = gtk::DragSource::new();
    drag_source.set_actions(gdk::DragAction::MOVE);

    let list_item = list_item.downgrade();
    drag_source.connect_prepare(move |_, _, _| {
        let list_item = list_item.upgrade()?;
        let track_id = queue_row_track_id(list_item.item())?;
        Some(gdk::ContentProvider::for_value(
            &queue_drag_payload(track_id).to_value(),
        ))
    });

    let row_for_icon = row.clone();
    let drag_active = ctx.drag_active.clone();
    drag_source.connect_drag_begin(move |source, _| {
        drag_active.set(true);
        let paintable = gtk::WidgetPaintable::new(Some(&row_for_icon));
        source.set_icon(Some(&paintable), 0, 0);
    });

    let drag_active = ctx.drag_active.clone();
    let schedule_close = ctx.schedule_close.clone();
    drag_source.connect_drag_end(move |_, _, _| {
        drag_active.set(false);
        schedule_close();
    });

    row.add_controller(drag_source);
}

fn install_queue_row_drop(row: &gtk::Box, list_item: &gtk::ListItem, ctx: &QueueRowContext) {
    let drop_target = gtk::DropTarget::new(glib::Type::STRING, gdk::DragAction::MOVE);
    drop_target.set_preload(true);

    let row_for_motion = row.clone();
    drop_target.connect_motion(move |target, _, y| {
        if peek_queue_payload(target).is_none() {
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
        let Some(target_id) = queue_row_track_id(list_item.item()) else {
            return false;
        };
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
    let Some(track_id) = queue_row_track_id(list_item.item()) else {
        return;
    };

    apply_queue_row_tint(&row, list_item.position());

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
        image.set_paintable(None::<&gdk::Paintable>);
        return;
    };
    title.set_text(&title_text);
    artist.set_text(&artist_text);

    // Clear first so a recycled row never shows the previous track's cover
    // in the gap before the new one resolves.
    image.set_paintable(None::<&gdk::Paintable>);
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
        Some(texture) => image.set_paintable(Some(texture)),
        None => image.set_paintable(None::<&gdk::Paintable>),
    }
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
    let boxed = item?.downcast::<glib::BoxedAnyObject>().ok()?;
    let track_id = boxed.try_borrow::<TrackId>().ok()?;
    Some(*track_id)
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
    use super::{parse_queue_payload, queue_drag_payload};
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
}
