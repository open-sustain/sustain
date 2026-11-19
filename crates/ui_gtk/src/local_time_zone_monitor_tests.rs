// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};

use gtk::glib;

use super::LocalTimeZoneMonitor;
use crate::test_support::with_gtk;

/// A directory of its own under the system temporary directory, holding a
/// `localtime` symlink to `zone`.
fn zone_directory(name: &str, zone: &str) -> PathBuf {
    let directory =
        std::env::temp_dir().join(format!("sustain-time-zone-{name}-{}", std::process::id()));
    let _absent = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("create the test directory");
    std::os::unix::fs::symlink(zone, directory.join("localtime")).expect("link the zone");
    directory
}

/// Points `directory`'s `localtime` at `zone` the way `timedatectl` does:
/// a new symlink renamed over the old one.
fn point_zone_at(directory: &Path, zone: &str) {
    let staged = directory.join(".localtime.new");
    std::os::unix::fs::symlink(zone, &staged).expect("stage the new link");
    std::fs::rename(&staged, directory.join("localtime")).expect("replace the link");
}

/// A monitor of `directory`'s `localtime`, its changes counted.
fn counted_monitor(directory: &Path) -> (LocalTimeZoneMonitor, Rc<Cell<u32>>) {
    let changes = Rc::new(Cell::new(0));
    let counter = changes.clone();
    let monitor = LocalTimeZoneMonitor::watching(&directory.join("localtime"), move || {
        counter.set(counter.get() + 1);
    })
    .expect("watch the zone file");
    (monitor, changes)
}

/// Runs the main loop until `done`, for at most five seconds.
fn run_main_loop_until(done: impl Fn() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() && Instant::now() < deadline {
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn pointing_the_zone_elsewhere_is_a_change() {
    with_gtk(|| {
        let directory = zone_directory("relinked", "/usr/share/zoneinfo/UTC");
        let (_monitor, changes) = counted_monitor(&directory);

        point_zone_at(&directory, "/usr/share/zoneinfo/Europe/Paris");
        run_main_loop_until(|| changes.get() > 0);

        assert!(changes.get() > 0, "the relinked zone went unnoticed");
        std::fs::remove_dir_all(&directory).expect("remove the test directory");
    });
}

#[test]
fn an_untouched_zone_is_no_change() {
    with_gtk(|| {
        let directory = zone_directory("untouched", "/usr/share/zoneinfo/UTC");
        let (_monitor, changes) = counted_monitor(&directory);
        // Something else in the same directory changes.
        std::fs::write(directory.join("hostname"), "sustain").expect("write a neighbour");

        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(changes.get(), 0);
        std::fs::remove_dir_all(&directory).expect("remove the test directory");
    });
}
