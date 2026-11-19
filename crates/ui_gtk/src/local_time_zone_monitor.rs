// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Notices the local time zone changing while Sustain runs.
//!
//! GLib reads the local zone from `/etc/localtime` each time it is asked
//! for it (unless `TZ` names one), so a zone chosen in the system settings
//! takes effect at once for every date read afterwards — but not for what
//! was read before: whatever shows or decides by a local date listens to
//! the monitor and looks again. The change shows up as `/etc/localtime`
//! changing: `timedatectl` and the desktop settings point the symlink
//! elsewhere.

use std::path::Path;

use gtk::gio::{self, prelude::*};

/// Where the system keeps the local time zone.
const LOCAL_TIME_ZONE_FILE: &str = "/etc/localtime";

/// Watches the local time zone. Clones share the watch, which lasts until
/// [`stop`](Self::stop) or until the last clone is dropped.
#[derive(Clone)]
pub(crate) struct LocalTimeZoneMonitor {
    monitor: gio::FileMonitor,
}

impl LocalTimeZoneMonitor {
    /// Watches the system's local time zone.
    pub(crate) fn new() -> Result<Self, gtk::glib::Error> {
        Self::watching(Path::new(LOCAL_TIME_ZONE_FILE))
    }

    /// Watches the file at `path` being changed, replaced, or removed.
    fn watching(path: &Path) -> Result<Self, gtk::glib::Error> {
        let monitor = gio::File::for_path(path)
            .monitor_file(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)?;
        Ok(Self { monitor })
    }

    /// Stops the watch: no listener hears of a change after this.
    pub(crate) fn stop(&self) {
        self.monitor.cancel();
    }

    /// Calls `on_change` from the main loop each time the local time zone
    /// may have changed, for as long as the watch lasts.
    pub(crate) fn connect_change(&self, on_change: impl Fn() + 'static) {
        self.monitor.connect_changed(move |_, _, _, event| {
            if zone_may_have_changed(event) {
                on_change();
            }
        });
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
