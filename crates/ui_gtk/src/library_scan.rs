// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{path::PathBuf, rc::Rc, sync::mpsc, time::Duration};

use gtk::glib;
use sustain_app_runtime::{NotificationCategory, NotificationSeverity, runtime_error_text};

use super::{
    ApplicationRuntimeError, LibraryChangedCallback, SharedRuntime, run_library_scan_task,
};

pub(crate) type LibraryScanRequestedCallback =
    Rc<dyn Fn(PathBuf) -> Result<(), ApplicationRuntimeError>>;

pub(crate) fn library_scan_requested_callback(
    runtime: &SharedRuntime,
    library_changed: LibraryChangedCallback,
) -> LibraryScanRequestedCallback {
    let runtime = runtime.clone();

    Rc::new(move |library_path| {
        // A scan that starts cleanly reports its running/outcome state
        // through the runtime's LibraryScan notifications. A *start*
        // failure (another task running, services unavailable) is reported
        // here, the single entry point for kicking off a scan, so it
        // reaches the user through the same lane instead of being lost.
        let task = {
            let mut runtime = runtime.borrow_mut();
            match runtime.prepare_library_scan(library_path) {
                Ok(task) => task,
                Err(error) => {
                    runtime.push_ephemeral_notification(
                        NotificationCategory::LibraryScan,
                        NotificationSeverity::Error,
                        runtime_error_text(&error),
                    );
                    return Err(error);
                }
            }
        };

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _sent = tx.send(run_library_scan_task(task));
        });

        poll_library_scan(rx, runtime.clone(), library_changed.clone());
        Ok(())
    })
}

fn poll_library_scan(
    rx: mpsc::Receiver<Result<super::LibraryScanResult, ApplicationRuntimeError>>,
    runtime: SharedRuntime,
    library_changed: LibraryChangedCallback,
) {
    glib::timeout_add_local(Duration::from_millis(100), move || match rx.try_recv() {
        Ok(Ok(result)) => {
            runtime.borrow_mut().apply_library_scan_result(result);
            library_changed();
            glib::ControlFlow::Break
        }
        Ok(Err(error)) => {
            runtime.borrow_mut().fail_library_scan(error);
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => {
            // The runtime's notification observer republishes the
            // "Cancelling..." state on every cancellation-flag flip;
            // we no longer need to poke the widget from here.
            glib::ControlFlow::Continue
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            runtime
                .borrow_mut()
                .fail_library_scan(ApplicationRuntimeError::LibraryScanFailed);
            glib::ControlFlow::Break
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, path::PathBuf, rc::Rc};

    use sustain_app_runtime::{ApplicationRuntime, NotificationCategory, NotificationSeverity};

    use super::library_scan_requested_callback;

    #[test]
    fn scan_start_failure_reports_through_the_notification_center() {
        // A runtime with no library services makes `prepare_library_scan`
        // fail with `LibraryServicesUnavailable` before it touches any GTK
        // main-loop work, so the start-failure path is exercised here
        // without a display.
        let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
        let callback = library_scan_requested_callback(&runtime, Rc::new(|| {}));

        assert!(callback(PathBuf::from("/music")).is_err());

        let runtime = runtime.borrow();
        let notification = runtime
            .notifications()
            .current_ephemeral()
            .expect("a scan start-failure notification");
        assert_eq!(notification.category, NotificationCategory::LibraryScan);
        assert_eq!(notification.severity, NotificationSeverity::Error);
    }
}
