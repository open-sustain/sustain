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
    NotificationAction, NotificationId, NotificationKind,
};
use sustain_i18n::{gettext, ngettext, npgettext, pgettext, phrase_list, plural_count, tr_format};

use super::{STATUS_BAR_HEIGHT, SharedRuntime};
use crate::track_summary::TrackSummary;

pub(crate) type CancelBackgroundTaskCallback = Rc<dyn Fn()>;
/// Carries out the action a notification offers, when its button is
/// clicked.
pub(crate) type NotificationActionCallback = Rc<dyn Fn(&NotificationAction)>;

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
    /// Bottom-left control cluster. Holds the sidebar collapse toggle and,
    /// to its right, the Settings entry — both always-visible chrome.
    start_cluster: gtk::Box,
    summary: gtk::Label,
    lane: NotificationLane,
}

impl StatusBar {
    pub(crate) fn new(
        on_cancel_background_task: CancelBackgroundTaskCallback,
        on_notification_action: NotificationActionCallback,
    ) -> Self {
        let root = gtk::CenterBox::new();
        root.add_css_class("status-bar");
        root.set_height_request(STATUS_BAR_HEIGHT);
        root.set_hexpand(true);

        let summary = gtk::Label::new(None);
        summary.set_xalign(0.5);

        let lane = NotificationLane::new(on_cancel_background_task, on_notification_action);

        let start_cluster = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        start_cluster.set_valign(gtk::Align::Center);

        root.set_start_widget(Some(&start_cluster));
        root.set_center_widget(Some(&summary));
        root.set_end_widget(Some(&lane.widget()));

        let status_bar = Self {
            root,
            start_cluster,
            summary,
            lane,
        };
        status_bar.update_summary(TrackSummary::default());
        status_bar
    }

    pub(crate) fn widget(&self) -> gtk::CenterBox {
        self.root.clone()
    }

    /// Mount the sidebar collapse / expand toggle as the leftmost item of
    /// the status bar's bottom-left cluster. The toggle is the only
    /// control that brings the sidebar back once collapsed, so it lives
    /// in always-visible chrome instead of inside the sidebar itself.
    /// Call before [`Self::install_settings_button`] so Settings sits to
    /// its right.
    pub(crate) fn install_sidebar_collapse_toggle(&self, button: gtk::Button) {
        button.set_valign(gtk::Align::Center);
        self.start_cluster.append(&button);
    }

    /// Mount the Settings entry to the right of the sidebar collapse
    /// toggle in the bottom-left cluster (#164).
    pub(crate) fn install_settings_button(&self, button: gtk::Button) {
        button.set_valign(gtk::Align::Center);
        self.start_cluster.append(&button);
    }

    pub(crate) fn update_summary(&self, summary: TrackSummary) {
        self.summary.set_text(&library_status_text(
            summary.track_count,
            summary.duration_seconds,
            summary.size_bytes,
        ));
    }

    /// Subscribe to notification mutations on the runtime so the lane
    /// re-renders whenever a notification is pushed, dismissed, or
    /// expired. Also installs the per-displayed-ephemeral auto-dismiss
    /// timer. Call exactly once per status-bar instance, before the
    /// runtime starts emitting notifications.
    pub(crate) fn attach_to_runtime(&self, runtime: &SharedRuntime) {
        self.refresh_from(&runtime.borrow());
        self.lane.hold_while_pointed_at(runtime);
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
    action_button: gtk::Button,
    /// The action of the notification the slot shows, for its button.
    action: Rc<RefCell<Option<NotificationAction>>>,
    cancel: gtk::Button,
}

impl NotificationSlot {
    fn new(on_cancel: CancelBackgroundTaskCallback, on_action: NotificationActionCallback) -> Self {
        let container = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        container.add_css_class("task-status");
        container.set_valign(gtk::Align::Center);
        container.set_halign(gtk::Align::End);

        let spinner = gtk::Spinner::new();
        spinner.add_css_class("task-status-spinner");
        spinner.set_size_request(14, 14);
        spinner.set_visible(false);

        // A body can carry a path or a diagnostic of any length: it is
        // cut to one line of the room the status bar has, and read whole
        // on hover.
        let label = gtk::Label::new(None);
        label.add_css_class("task-status-label");
        label.set_xalign(1.0);
        label.set_single_line_mode(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);

        let cancel = gtk::Button::with_label(&gettext("Cancel"));
        cancel.add_css_class("task-status-button");
        cancel.set_valign(gtk::Align::Center);
        cancel.set_visible(false);
        cancel.set_tooltip_text(Some(&gettext("Cancel")));
        let cancel_for_click = cancel.clone();
        cancel.connect_clicked(move |_| {
            // Cooperative cancellation: disable the button while the
            // worker winds down so a second click is not misleading.
            cancel_for_click.set_sensitive(false);
            on_cancel();
        });

        let action_button = gtk::Button::new();
        action_button.add_css_class("task-status-button");
        action_button.set_valign(gtk::Align::Center);
        action_button.set_visible(false);
        let action = Rc::new(RefCell::new(None::<NotificationAction>));
        let action_for_click = action.clone();
        action_button.connect_clicked(move |_| {
            let action = action_for_click.borrow().clone();
            if let Some(action) = action {
                on_action(&action);
            }
        });

        container.append(&spinner);
        container.append(&label);
        container.append(&action_button);
        container.append(&cancel);

        Self {
            container,
            spinner,
            label,
            action_button,
            action,
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
            gettext("Cancelling...")
        } else {
            notification.body.clone()
        };
        self.label.set_text(&label_text);
        self.label.set_tooltip_text(Some(&label_text));

        self.spinner.set_visible(is_persistent);
        self.spinner.set_spinning(is_persistent);

        match &notification.action {
            Some(action) => self.action_button.set_label(&action.label()),
            None => self.action_button.set_label(""),
        }
        self.action_button
            .set_visible(notification.action.is_some());
        self.action.replace(notification.action.clone());

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
    /// Whether the pointer is over the lane: an ephemeral is held while
    /// it is, so it can be read to the end and its button reached.
    pointer: gtk::EventControllerMotion,
}

impl NotificationLane {
    fn new(on_cancel: CancelBackgroundTaskCallback, on_action: NotificationActionCallback) -> Self {
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

        let slot_a = NotificationSlot::new(on_cancel.clone(), on_action.clone());
        let slot_b = NotificationSlot::new(on_cancel, on_action);
        stack.add_named(&slot_a.container, Some("a"));
        stack.add_named(&slot_b.container, Some("b"));
        let pointer = gtk::EventControllerMotion::new();
        stack.add_controller(pointer.clone());

        Self {
            inner: Rc::new(NotificationLaneInner {
                stack,
                slot_a,
                slot_b,
                showing_slot_a: Cell::new(true),
                displayed_id: Cell::new(None),
                displayed_is_ephemeral: Cell::new(false),
                auto_dismiss_token: RefCell::new(None),
                pointer,
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
            // Nothing can reach the slots' actions any more; what they
            // carry, such as a long import report, is let go.
            self.inner.slot_a.action.replace(None);
            self.inner.slot_b.action.replace(None);
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

    /// Hold the displayed ephemeral while the pointer is over the lane:
    /// its timer stops on entry and starts again, whole, on exit.
    fn hold_while_pointed_at(&self, runtime: &SharedRuntime) {
        let lane = self.clone();
        self.inner.pointer.connect_enter(move |_, _, _| {
            lane.cancel_auto_dismiss();
        });
        let lane = self.clone();
        let runtime = runtime.clone();
        self.inner.pointer.connect_leave(move |_| {
            lane.install_auto_dismiss(&runtime);
        });
    }

    /// Arm a one-shot timer that expires the current ephemeral after
    /// [`EPHEMERAL_NOTIFICATION_DURATION`]. No-op for persistent
    /// notifications, when no notification is displayed, or while the
    /// pointer holds the lane. Called after every refresh so a new
    /// ephemeral picks up its timer; the `displayed_id` check below
    /// makes redundant calls safe.
    fn install_auto_dismiss(&self, runtime: &SharedRuntime) {
        if !self.inner.displayed_is_ephemeral.get() || self.inner.pointer.contains_pointer() {
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
            // The runtime may have dismissed it since, ahead of the
            // refresh that tells the lane: it expires `target_id` only.
            runtime
                .borrow_mut()
                .expire_ephemeral_notification(target_id);
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
    phrase_list(&[
        songs_text(track_count),
        duration_text(duration_seconds),
        file_size_text(size_bytes),
    ])
}

/// "{count} songs", the library's track count in its grouped form.
pub(crate) fn songs_text(track_count: usize) -> String {
    tr_format!(
        npgettext(
            "count of songs",
            "{count} song",
            "{count} songs",
            plural_count(track_count)
        ),
        count = grouped(track_count as u64)
    )
}

pub(crate) fn duration_text(duration_seconds: u64) -> String {
    let hours = duration_seconds / 3_600;
    if hours >= 24 {
        let days = hours / 24;
        tr_format!(
            ngettext("{count} day", "{count} days", plural_count_u64(days)),
            count = grouped(days)
        )
    } else if hours >= 1 {
        tr_format!(
            ngettext("{count} hour", "{count} hours", plural_count_u64(hours)),
            count = hours
        )
    } else {
        let minutes = duration_seconds / 60;
        tr_format!(
            ngettext(
                "{count} minute",
                "{count} minutes",
                plural_count_u64(minutes)
            ),
            count = minutes
        )
    }
}

pub(crate) fn file_size_text(size_bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    const GB: u64 = 1_000_000_000;

    if size_bytes >= GB {
        // Translators: a size in gigabytes; use your language's unit symbol.
        tr_format!(gettext("{size} GB"), size = grouped(size_bytes / GB))
    } else {
        // Translators: a size in megabytes; use your language's unit symbol.
        tr_format!(gettext("{size} MB"), size = grouped(size_bytes / MB))
    }
}

/// `value` with its thousands grouped by the language's separator
/// (`49295` → `"49,295"` in English).
pub(crate) fn grouped(value: u64) -> String {
    // Translators: groups the thousands of a large number, as in "49,295".
    // Use your language's separator, such as "." or a narrow no-break space.
    let separator = pgettext("thousands separator", ",");
    let digits = value.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3 * separator.len());
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push_str(&separator);
        }
        text.push(digit);
    }
    text
}

/// [`plural_count`] for a `u64` count.
pub(crate) fn plural_count_u64(count: u64) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use sustain_app_runtime::{
        ImportFailureReport, NotificationAction, NotificationCategory, NotificationSeverity,
    };

    use super::*;
    use crate::test_support::{descendants, runtime_with_tracks, with_gtk};

    #[test]
    fn a_notification_offering_an_action_shows_a_button_that_carries_it_out() {
        with_gtk(|| {
            let carried_out = Rc::new(RefCell::new(Vec::<NotificationAction>::new()));
            let carried_out_by_click = carried_out.clone();
            let status_bar = StatusBar::new(
                Rc::new(|| {}),
                Rc::new(move |action| carried_out_by_click.borrow_mut().push(action.clone())),
            );
            let runtime = runtime_with_tracks(Vec::new());
            status_bar.attach_to_runtime(&runtime);
            let action = NotificationAction::ShowImportFailures(ImportFailureReport {
                failures: Vec::new().into(),
                import_finished: true,
            });

            runtime.borrow_mut().push_actionable_ephemeral_notification(
                NotificationCategory::LibraryImport,
                NotificationSeverity::Info,
                "Imported 1 track; 1 failed".to_owned(),
                action.clone(),
            );
            let shown_button = || {
                descendants(status_bar.widget().upcast_ref())
                    .into_iter()
                    .filter_map(|widget| widget.downcast::<gtk::Button>().ok())
                    .find(|button| {
                        button.label().as_deref() == Some("Details…")
                            && button.is_child_visible()
                            && button.is_visible()
                            && button.parent().is_some_and(|slot| slot.is_child_visible())
                    })
            };
            let context = glib::MainContext::default();
            let deadline = Instant::now() + Duration::from_secs(5);
            while shown_button().is_none() && Instant::now() < deadline {
                context.iteration(false);
            }
            shown_button()
                .expect("the lane shows the action's button")
                .emit_clicked();

            assert_eq!(*carried_out.borrow(), [action]);
        });
    }

    /// A notification naming a long path does not make the status bar,
    /// and the window holding it, wider than a short one does; its whole
    /// text is a hover away (#348).
    #[test]
    fn a_long_notification_does_not_widen_the_status_bar() {
        with_gtk(|| {
            let status_bar = StatusBar::new(Rc::new(|| {}), Rc::new(|_| {}));
            let runtime = runtime_with_tracks(Vec::new());
            status_bar.attach_to_runtime(&runtime);
            let minimum_width = || {
                status_bar
                    .widget()
                    .measure(gtk::Orientation::Horizontal, -1)
                    .0
            };
            let body = format!(
                "Could not plan the device sync: /media/stick/{}: File name too long",
                "Music/".repeat(60)
            );
            let shown_label = || {
                descendants(status_bar.widget().upcast_ref())
                    .into_iter()
                    .filter_map(|widget| widget.downcast::<gtk::Label>().ok())
                    .find(|label| label.text() == body.as_str())
            };

            let short = runtime.borrow_mut().push_ephemeral_notification(
                NotificationCategory::DeviceSyncPlanning,
                NotificationSeverity::Warning,
                "Short".to_owned(),
            );
            let context = glib::MainContext::default();
            while context.iteration(false) {}
            let short_width = minimum_width();
            runtime.borrow_mut().dismiss_notification(short);
            runtime.borrow_mut().push_ephemeral_notification(
                NotificationCategory::DeviceSyncPlanning,
                NotificationSeverity::Warning,
                body.clone(),
            );
            let deadline = Instant::now() + Duration::from_secs(5);
            while shown_label().is_none() && Instant::now() < deadline {
                context.iteration(false);
            }

            let label = shown_label().expect("the lane shows the long notification");
            assert_eq!(label.tooltip_text().as_deref(), Some(body.as_str()));
            assert!(
                minimum_width() <= short_width,
                "{} > {short_width}",
                minimum_width()
            );
        });
    }

    #[test]
    fn large_figures_group_their_thousands() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(1_000), "1,000");
        assert_eq!(grouped(49_295), "49,295");
        assert_eq!(grouped(1_234_567), "1,234,567");
        assert_eq!(
            library_status_text(10_265, 2_592_000_000, 1_234_000_000_000),
            "10,265 songs, 30,000 days, 1,234 GB"
        );
    }

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
