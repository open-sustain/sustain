// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Notices the local time zone changing while Sustain runs.
//!
//! GLib reads the local zone from `/etc/localtime` each time it is asked
//! for it (unless `TZ` names one), so a zone chosen in the system settings
//! takes effect at once for every date read afterwards — but not for what
//! was read before. The change shows up as `/etc/localtime` changing:
//! `timedatectl` and the desktop settings point the symlink elsewhere.

use std::path::Path;

use gtk::gio::{self, prelude::*};

/// Where the system keeps the local time zone.
const LOCAL_TIME_ZONE_FILE: &str = "/etc/localtime";

/// Watches the local time zone. Dropping it stops the watch.
pub(crate) struct LocalTimeZoneMonitor {
    _monitor: gio::FileMonitor,
}

impl LocalTimeZoneMonitor {
    /// Calls `on_change` from the main loop each time the local time zone
    /// may have changed.
    pub(crate) fn new(on_change: impl Fn() + 'static) -> Result<Self, gtk::glib::Error> {
        Self::watching(Path::new(LOCAL_TIME_ZONE_FILE), on_change)
    }

    /// Calls `on_change` each time the file at `path` is changed,
    /// replaced, or removed.
    fn watching(path: &Path, on_change: impl Fn() + 'static) -> Result<Self, gtk::glib::Error> {
        let monitor = gio::File::for_path(path)
            .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)?;
        monitor.connect_changed(move |_, _, _, event| {
            if zone_may_have_changed(event) {
                on_change();
            }
        });
        Ok(Self { _monitor: monitor })
    }
}

/// Whether a file event can mean another zone: what the file holds or
/// points to changed. Its attributes, and the end of a burst of changes
/// already reported, cannot.
fn zone_may_have_changed(event: gio::FileMonitorEvent) -> bool {
    !matches!(
        event,
        gio::FileMonitorEvent::AttributeChanged
            | gio::FileMonitorEvent::ChangesDoneHint
            | gio::FileMonitorEvent::PreUnmount
            | gio::FileMonitorEvent::Unmounted
    )
}

#[cfg(test)]
#[path = "local_time_zone_monitor_tests.rs"]
mod tests;
