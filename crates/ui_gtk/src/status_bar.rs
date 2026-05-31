// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::glib;
use gtk::prelude::*;
use sustain_app_runtime::{
    ApplicationRuntime, EPHEMERAL_NOTIFICATION_DURATION, NOTIFICATION_TRANSITION, Notification,
    NotificationId, NotificationKind,
};

use super::{STATUS_BAR_HEIGHT, SharedRuntime};
use crate::track_table::TrackTableRow;

pub(crate) type CancelBackgroundTaskCallback = Rc<dyn Fn()>;

/// Read-only model the notification lane renders from. Snapshotted by
/// `StatusBar` while holding a short borrow on the runtime, so the
/// widget code never reaches back into the runtime mid-refresh.
struct NotificationLaneInput {
    notification: Option<Notification>,
    cancelling: bool,
}

#[derive(Clone)]
pub(crate) struct StatusBar {
    root: gtk::CenterBox,
    summary: gtk::Label,
    lane: NotificationLane,
}

impl StatusBar {
    pub(crate) fn new(
        library_tracks: &[TrackTableRow],
        on_cancel_background_task: CancelBackgroundTaskCallback,
    ) -> Self {
        let root = gtk::CenterBox::new();
        root.add_css_class("status-bar");
        root.set_height_request(STATUS_BAR_HEIGHT);
        root.set_hexpand(true);

        let summary = gtk::Label::new(None);
        summary.set_xalign(0.5);

        let lane = NotificationLane::new(on_cancel_background_task);

        root.set_center_widget(Some(&summary));
        root.set_end_widget(Some(&lane.widget()));

        let status_bar = Self {
            root,
            summary,
            lane,
        };
        status_bar.update_summary(library_tracks);
        status_bar
    }

    pub(crate) fn widget(&self) -> gtk::CenterBox {
        self.root.clone()
    }

    /// Mount the sidebar collapse / expand toggle on the status bar's
    /// left side. The toggle is the only control that brings the
    /// sidebar back once collapsed, so it lives in always-visible
    /// chrome instead of inside the sidebar itself.
    pub(crate) fn install_sidebar_collapse_toggle(&self, button: gtk::Button) {
        button.set_valign(gtk::Align::Center);
        self.root.set_start_widget(Some(&button));
    }

    pub(crate) fn update_summary(&self, library_tracks: &[TrackTableRow]) {
        let duration_seconds = library_tracks
            .iter()
            .map(|track| track.duration_seconds)
            .sum();
        let size_bytes = library_tracks
            .iter()
            .map(|track| track.file_size_bytes)
            .sum();

        self.update_summary_values(library_tracks.len(), duration_seconds, size_bytes);
    }

    pub(crate) fn update_summary_values(
        &self,
        track_count: usize,
        duration_seconds: u64,
        size_bytes: u64,
    ) {
        self.summary.set_text(&library_status_text(
            track_count,
            duration_seconds,
            size_bytes,
        ));
    }

    /// Subscribe to notification mutations on the runtime so the lane
    /// re-renders whenever a notification is pushed, dismissed, or
    /// expired. Also installs the per-displayed-ephemeral auto-dismiss
    /// timer. Call exactly once per status-bar instance, before the
    /// runtime starts emitting notifications.
    pub(crate) fn attach_to_runtime(&self, runtime: &SharedRuntime) {
        self.refresh_from(&runtime.borrow());
        let lane = self.lane.clone();
        let runtime_for_observer = runtime.clone();
        runtime
            .borrow_mut()
            .set_notification_observer(Box::new(move || {
                // The runtime is mid-borrow when this fires, so defer the
                // render onto the GLib main loop. Multiple notifications
                // in a single tick collapse into multiple idles; the
                // render is idempotent so the redundant calls are cheap.
                let lane = lane.clone();
                let runtime = runtime_for_observer.clone();
                glib::idle_add_local_once(move || {
                    lane.refresh(read_lane_input(&runtime.borrow()));
                    lane.install_auto_dismiss(&runtime);
                });
            }));
    }

    fn refresh_from(&self, runtime: &ApplicationRuntime) {
        self.lane.refresh(read_lane_input(runtime));
    }
}

fn read_lane_input(runtime: &ApplicationRuntime) -> NotificationLaneInput {
    let notifications = runtime.notifications();
    let notification = notifications
        .current_ephemeral()
        .or_else(|| notifications.current_persistent())
        .cloned();
    NotificationLaneInput {
        notification,
        cancelling: runtime.background_task_cancellation_requested(),
    }
}

/// Pair of alternating slots driven by `gtk::Stack` for the slide
/// transition. Each notification is rendered into the inactive slot,
/// then we switch the visible child — GTK4 animates the swap via its
/// frame clock.
struct NotificationSlot {
    container: gtk::Box,
    spinner: gtk::Spinner,
    label: gtk::Label,
    cancel: gtk::Button,
}

impl NotificationSlot {
    fn new(on_cancel: CancelBackgroundTaskCallback) -> Self {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        container.add_css_class("task-status");
        container.set_valign(gtk::Align::Center);
        container.set_halign(gtk::Align::End);

        let spinner = gtk::Spinner::new();
        spinner.add_css_class("task-status-spinner");
        spinner.set_size_request(14, 14);
        spinner.set_visible(false);

        let label = gtk::Label::new(None);
        label.add_css_class("task-status-label");
        label.set_xalign(1.0);

        let cancel = gtk::Button::with_label("Cancel");
        cancel.add_css_class("task-status-cancel");
        cancel.set_valign(gtk::Align::Center);
        cancel.set_visible(false);
        cancel.set_tooltip_text(Some("Cancel"));
        let cancel_for_click = cancel.clone();
        cancel.connect_clicked(move |_| {
            // Cooperative cancellation: disable the button while the
            // worker winds down so a second click is not misleading.
            cancel_for_click.set_sensitive(false);
            on_cancel();
        });

        container.append(&spinner);
        container.append(&label);
        container.append(&cancel);

        Self {
            container,
            spinner,
            label,
            cancel,
        }
    }

    fn apply(&self, notification: &Notification, cancelling: bool) {
        let is_persistent = matches!(notification.kind, NotificationKind::Persistent { .. });
        let cancellable = matches!(
            notification.kind,
            NotificationKind::Persistent { cancellable: true }
        );

        let label_text = if is_persistent && cancelling {
            "Cancelling..."
        } else {
            notification.body.as_str()
        };
        self.label.set_text(label_text);

        self.spinner.set_visible(is_persistent);
        self.spinner.set_spinning(is_persistent);

        self.cancel.set_visible(cancellable);
        self.cancel.set_sensitive(cancellable && !cancelling);
    }
}

/// Notification renderer. Holds onto two GTK slots wired into a
/// `gtk::Stack` with a slide transition, plus the currently-displayed
/// id and the live auto-dismiss timer.
#[derive(Clone)]
struct NotificationLane {
    inner: Rc<NotificationLaneInner>,
}

struct NotificationLaneInner {
    stack: gtk::Stack,
    slot_a: NotificationSlot,
    slot_b: NotificationSlot,
    showing_slot_a: Cell<bool>,
    displayed_id: Cell<Option<NotificationId>>,
    displayed_is_ephemeral: Cell<bool>,
    auto_dismiss_token: RefCell<Option<glib::SourceId>>,
}

impl NotificationLane {
    fn new(on_cancel: CancelBackgroundTaskCallback) -> Self {
        let stack = gtk::Stack::new();
        stack.add_css_class("notification-lane");
        // GTK4's SlideLeft drives the new child in from the right and
        // pushes the previous child out to the left. The frame clock
        // runs the animation, so the main loop is never busy-waiting
        // on a timer. Crossfade-while-sliding is not a GTK4 primitive;
        // a precise slide-plus-fade overlap would need a custom
        // tick_callback animator. The bare slide carries the intent.
        stack.set_transition_type(gtk::StackTransitionType::SlideLeft);
        stack.set_transition_duration(NOTIFICATION_TRANSITION.as_millis() as u32);
        stack.set_visible(false);
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(true);
        // Status role carries an implicit polite live-region semantic
        // in the GTK4 accessibility model, so screen readers announce
        // each new body text that lands in the visible slot without
        // any extra wiring.
        stack.set_accessible_role(gtk::AccessibleRole::Status);

        let slot_a = NotificationSlot::new(on_cancel.clone());
        let slot_b = NotificationSlot::new(on_cancel);
        stack.add_named(&slot_a.container, Some("a"));
        stack.add_named(&slot_b.container, Some("b"));

        Self {
            inner: Rc::new(NotificationLaneInner {
                stack,
                slot_a,
                slot_b,
                showing_slot_a: Cell::new(true),
                displayed_id: Cell::new(None),
                displayed_is_ephemeral: Cell::new(false),
                auto_dismiss_token: RefCell::new(None),
            }),
        }
    }

    fn widget(&self) -> gtk::Stack {
        self.inner.stack.clone()
    }

    fn refresh(&self, input: NotificationLaneInput) {
        let Some(notification) = input.notification else {
            self.cancel_auto_dismiss();
            self.inner.stack.set_visible(false);
            self.inner.displayed_id.set(None);
            self.inner.displayed_is_ephemeral.set(false);
            return;
        };

        let is_ephemeral = matches!(notification.kind, NotificationKind::Ephemeral);
        let already_displayed = self.inner.displayed_id.get() == Some(notification.id);

        let inactive_slot = if self.inner.showing_slot_a.get() {
            &self.inner.slot_b
        } else {
            &self.inner.slot_a
        };
        inactive_slot.apply(&notification, input.cancelling);

        if already_displayed {
            // Same notification, possibly with an updated cancelling
            // flag. Re-skin the active slot in place — no transition.
            let active_slot = if self.inner.showing_slot_a.get() {
                &self.inner.slot_a
            } else {
                &self.inner.slot_b
            };
            active_slot.apply(&notification, input.cancelling);
            self.inner.stack.set_visible(true);
            return;
        }

        let target_name = if self.inner.showing_slot_a.get() {
            "b"
        } else {
            "a"
        };
        self.inner.stack.set_visible(true);
        // First reveal of the lane: skip the slide. The animation is
        // for "previous slides out, new comes in", and there is no
        // previous to slide. Running it anyway also costs ~250ms of
        // frame-clock activity at startup, which would push the
        // cold-start first-idle past the budget when the auto-resume
        // consolidation pushes its persistent.
        let was_idle = self.inner.displayed_id.get().is_none();
        if was_idle {
            self.inner
                .stack
                .set_visible_child_full(target_name, gtk::StackTransitionType::None);
        } else {
            self.inner.stack.set_visible_child_name(target_name);
        }
        self.inner
            .showing_slot_a
            .set(!self.inner.showing_slot_a.get());
        self.inner.displayed_id.set(Some(notification.id));
        self.inner.displayed_is_ephemeral.set(is_ephemeral);

        self.cancel_auto_dismiss();
    }

    /// Arm a one-shot timer that expires the current ephemeral after
    /// [`EPHEMERAL_NOTIFICATION_DURATION`]. No-op for persistent
    /// notifications or when no notification is displayed. Called
    /// after every refresh so a new ephemeral picks up its timer; the
    /// `displayed_id` check below makes redundant calls safe.
    fn install_auto_dismiss(&self, runtime: &SharedRuntime) {
        if !self.inner.displayed_is_ephemeral.get() {
            return;
        }
        if self.inner.auto_dismiss_token.borrow().is_some() {
            return;
        }
        let Some(target_id) = self.inner.displayed_id.get() else {
            return;
        };
        let runtime = runtime.clone();
        let lane = self.clone();
        let token = glib::timeout_add_local_once(EPHEMERAL_NOTIFICATION_DURATION, move || {
            lane.inner.auto_dismiss_token.replace(None);
            // The currently-displayed head may have changed between
            // scheduling and firing (e.g. the runtime dismissed it
            // some other way). Only expire if the id we armed for is
            // still on display.
            if lane.inner.displayed_id.get() != Some(target_id) {
                return;
            }
            runtime.borrow_mut().expire_current_ephemeral_notification();
        });
        self.inner.auto_dismiss_token.replace(Some(token));
    }

    fn cancel_auto_dismiss(&self) {
        if let Some(token) = self.inner.auto_dismiss_token.replace(None) {
            token.remove();
        }
    }
}

pub(crate) fn library_status_text(
    track_count: usize,
    duration_seconds: u64,
    size_bytes: u64,
) -> String {
    format!(
        "{} {}, {}, {}",
        track_count,
        pluralize(track_count, "song", "songs"),
        duration_text(duration_seconds),
        file_size_text(size_bytes),
    )
}

pub(crate) fn duration_text(duration_seconds: u64) -> String {
    let hours = duration_seconds / 3_600;
    if hours >= 24 {
        let days = hours / 24;
        format!("{} {}", days, pluralize(days as usize, "day", "days"))
    } else if hours >= 1 {
        format!("{} {}", hours, pluralize(hours as usize, "hour", "hours"))
    } else {
        let minutes = duration_seconds / 60;
        format!(
            "{} {}",
            minutes,
            pluralize(minutes as usize, "minute", "minutes")
        )
    }
}

fn file_size_text(size_bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;

    if size_bytes >= GB {
        format!("{} GB", size_bytes / GB)
    } else {
        format!("{} MB", size_bytes / MB)
    }
}

pub(crate) fn pluralize(
    count: usize,
    singular: &'static str,
    plural: &'static str,
) -> &'static str {
    if count == 1 { singular } else { plural }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_status_uses_hours_and_megabytes_for_small_libraries() {
        assert_eq!(
            library_status_text(2, 7_200, 250_000_000),
            "2 songs, 2 hours, 250 MB"
        );
    }

    #[test]
    fn library_status_uses_minutes_when_under_an_hour() {
        assert_eq!(
            library_status_text(3, 1_800, 12_000_000),
            "3 songs, 30 minutes, 12 MB"
        );
    }

    #[test]
    fn library_status_uses_singular_minute_for_a_single_minute() {
        assert_eq!(
            library_status_text(1, 60, 1_000_000),
            "1 song, 1 minute, 1 MB"
        );
    }

    #[test]
    fn library_status_uses_days_and_gigabytes_for_large_libraries() {
        assert_eq!(
            library_status_text(1, 172_800, 3_000_000_000),
            "1 song, 2 days, 3 GB"
        );
    }
}
