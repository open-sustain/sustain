// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{rc::Rc, sync::mpsc, time::Duration};

use gtk::glib;
use sustain_app_runtime::{NotificationCategory, NotificationSeverity, runtime_error_text};

use super::{
    ApplicationRuntimeError, LibraryManagementMode, SharedRuntime, run_library_consolidation_task,
};

pub(crate) type LibraryConsolidationRequestedCallback =
    Rc<dyn Fn() -> Result<(), ApplicationRuntimeError>>;

/// Kicks off a consolidation pass if the user has opted into managed
/// library organization. Used at application startup so an interrupted
/// previous run (kill, crash, system power loss) resumes silently
/// instead of leaving the library half-organized forever.
///
/// Idempotent and cheap when there is nothing to do: the consolidation
/// planner returns an empty plan for an already-organized library and
/// completes immediately. The outcome notification auto-dismisses
/// after [`sustain_app_runtime::EPHEMERAL_NOTIFICATION_DURATION`] so a
/// boring "0 moved, 0 missing" launch fades away on its own.
pub(crate) fn maybe_auto_resume_library_consolidation(
    runtime: &SharedRuntime,
    consolidation_requested: &LibraryConsolidationRequestedCallback,
) {
    let should_resume = {
        let runtime = runtime.borrow();
        let settings = runtime.settings();
        settings.library.management_mode == LibraryManagementMode::CopyAddedFilesIntoLibrary
            && settings.library_path().is_some_and(|path| path.is_dir())
            && !runtime.background_task_status().is_running()
    };
    if should_resume {
        let _ = consolidation_requested();
    }
}

pub(crate) fn library_consolidation_requested_callback(
    runtime: &SharedRuntime,
) -> LibraryConsolidationRequestedCallback {
    let runtime = runtime.clone();

    Rc::new(move || {
        // A consolidation that starts cleanly reports its
        // running/outcome state through the runtime's LibraryConsolidation
        // notifications. A *start* failure is reported here, the single
        // entry point, so it reaches the user through the same lane.
        let task = {
            let mut runtime = runtime.borrow_mut();
            match runtime.prepare_library_consolidation() {
                Ok(task) => task,
                Err(error) => {
                    if !matches!(
                        error,
                        ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(_)
                    ) {
                        runtime.push_ephemeral_notification(
                            NotificationCategory::LibraryConsolidation,
                            NotificationSeverity::Error,
                            runtime_error_text(&error),
                        );
                    }
                    return Err(error);
                }
            }
        };

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _sent = tx.send(run_library_consolidation_task(task));
        });

        poll_library_consolidation(rx, runtime.clone());
        Ok(())
    })
}

// Consolidation only mutates each moved track's stored relative path
// (in SQLite and in the in-memory library_tracks vec). It does not
// add, remove, or otherwise change anything the user sees:
//   - track metadata, rating, statistics, availability flag: unchanged
//   - playlist membership (linked by TrackId, not path): unchanged
//   - sidebar entries, now-playing tile, table rows: all unchanged
// The notification lane is the entire UI contract. Triggering
// `library_changed()` here would force the songs table, albums view,
// sidebar tree, and playlists table to rebuild for nothing — measured
// at multiple seconds of `replace_rows` work on a 10k library.
fn poll_library_consolidation(
    rx: mpsc::Receiver<Result<super::LibraryConsolidationResult, ApplicationRuntimeError>>,
    runtime: SharedRuntime,
) {
    glib::timeout_add_local(Duration::from_millis(100), move || match rx.try_recv() {
        Ok(Ok(result)) => {
            runtime
                .borrow_mut()
                .apply_library_consolidation_result(result);
            glib::ControlFlow::Break
        }
        Ok(Err(error)) => {
            runtime.borrow_mut().fail_library_consolidation(error);
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => {
            runtime
                .borrow_mut()
                .fail_library_consolidation(ApplicationRuntimeError::LibraryConsolidationFailed);
            glib::ControlFlow::Break
        }
    });
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use sustain_app_runtime::{ApplicationRuntime, NotificationCategory, NotificationSeverity};

    use super::library_consolidation_requested_callback;

    #[test]
    fn consolidation_start_failure_reports_through_the_notification_center() {
        // A fresh runtime is in the default "reference files in place" mode,
        // so `prepare_library_consolidation` fails before any GTK work and
        // the start-failure path is exercised here without a display.
        let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
        let callback = library_consolidation_requested_callback(&runtime);

        assert!(callback().is_err());

        let runtime = runtime.borrow();
        let notification = runtime
            .notifications()
            .current_ephemeral()
            .expect("a consolidation start-failure notification");
        assert_eq!(
            notification.category,
            NotificationCategory::LibraryConsolidation
        );
        assert_eq!(notification.severity, NotificationSeverity::Error);
    }
}
