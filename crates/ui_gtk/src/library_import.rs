// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{path::PathBuf, rc::Rc, sync::mpsc, time::Duration};

use gtk::glib;
use gtk::prelude::*;
use gtk::{gdk, gio};
use sustain_app_runtime::{NotificationCategory, NotificationSeverity, runtime_error_text};

use super::{
    ApplicationRuntimeError, LibraryImportProgress, LibraryImportResult, LibraryImportTask,
    SharedRuntime, run_library_import_task_with_progress,
};

enum LibraryImportWorkerEvent {
    Progress(LibraryImportProgress),
    Finished(Result<LibraryImportResult, ApplicationRuntimeError>),
}

pub(crate) type LibraryImportRequestedCallback =
    Rc<dyn Fn(Vec<PathBuf>) -> Result<(), ApplicationRuntimeError>>;

/// Starts an import of the dropped paths. When it is done, the runtime
/// publishes the new tracks to the views, and a window over `parent`
/// lists any file the import left out.
pub(crate) fn library_import_requested_callback(
    runtime: &SharedRuntime,
    parent: &gtk::Window,
) -> LibraryImportRequestedCallback {
    let runtime = runtime.clone();
    let parent = parent.downgrade();

    Rc::new(move |paths| {
        let task = prepare_library_import(&runtime, paths)?;

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let outcome = run_library_import_task_with_progress(task, |progress| {
                let _sent = tx.send(LibraryImportWorkerEvent::Progress(progress));
            });
            let _sent = tx.send(LibraryImportWorkerEvent::Finished(outcome));
        });

        poll_library_import(rx, runtime.clone(), parent.clone());
        Ok(())
    })
}

/// An import that starts cleanly reports its running and outcome state
/// through the runtime's LibraryImport notifications. A *start* failure
/// (another task running, services unavailable) is reported here, the
/// single entry point for an import, so it reaches the user through the
/// same lane. An unsupported managed-library filesystem has already
/// raised its own persistent warning.
fn prepare_library_import(
    runtime: &SharedRuntime,
    paths: Vec<PathBuf>,
) -> Result<LibraryImportTask, ApplicationRuntimeError> {
    let mut runtime = runtime.borrow_mut();
    runtime.prepare_library_import(paths).inspect_err(|error| {
        if !matches!(
            error,
            ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(_)
        ) {
            runtime.push_ephemeral_notification(
                NotificationCategory::LibraryImport,
                NotificationSeverity::Error,
                runtime_error_text(error),
            );
        }
    })
}

pub(crate) fn install_file_drop_target(
    drop_zone: &impl IsA<gtk::Widget>,
    drop_indicator: &impl IsA<gtk::Widget>,
    import_requested: LibraryImportRequestedCallback,
) {
    let drop_target = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);

    let indicator_for_enter = drop_indicator.clone().upcast::<gtk::Widget>();
    drop_target.connect_enter(move |_target, _x, _y| {
        indicator_for_enter.add_css_class(LIBRARY_DROP_ACTIVE_CLASS);
        gdk::DragAction::COPY
    });
    let indicator_for_leave = drop_indicator.clone().upcast::<gtk::Widget>();
    drop_target.connect_leave(move |_target| {
        indicator_for_leave.remove_css_class(LIBRARY_DROP_ACTIVE_CLASS);
    });

    drop_target.connect_drop(move |_target, value, _x, _y| {
        let Ok(file_list) = value.get::<gdk::FileList>() else {
            return false;
        };
        let paths = local_paths_from_file_list(&file_list);
        if paths.is_empty() {
            return false;
        }
        import_requested(paths).is_ok()
    });
    drop_zone.add_controller(drop_target);
}

pub(crate) const LIBRARY_DROP_INDICATOR_CLASS: &str = "library-drop-indicator";
const LIBRARY_DROP_ACTIVE_CLASS: &str = "library-drop-active";

fn poll_library_import(
    rx: mpsc::Receiver<LibraryImportWorkerEvent>,
    runtime: SharedRuntime,
    parent: glib::WeakRef<gtk::Window>,
) {
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let mut latest_progress = None;
        let mut finished = None;
        loop {
            match rx.try_recv() {
                Ok(LibraryImportWorkerEvent::Progress(progress)) => {
                    latest_progress = Some(progress);
                }
                Ok(LibraryImportWorkerEvent::Finished(outcome)) => {
                    finished = Some(outcome);
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    finished = Some(Err(ApplicationRuntimeError::LibraryImportFailed));
                    break;
                }
            }
        }
        if let Some(progress) = latest_progress {
            runtime
                .borrow_mut()
                .update_library_import_progress(progress.processed_files, progress.total_files);
        }
        match finished {
            Some(Ok(mut result)) => {
                let failures = std::mem::take(&mut result.failures);
                let import_finished = !result.summary.cancelled && !result.summary.stopped_on_error;
                runtime.borrow_mut().apply_library_import_result(result);
                if !failures.is_empty()
                    && let Some(parent) = parent.upgrade()
                {
                    crate::import_failures_dialog::show_import_failures(
                        &parent,
                        failures,
                        import_finished,
                    );
                }
                glib::ControlFlow::Break
            }
            Some(Err(error)) => {
                runtime.borrow_mut().fail_library_import(error);
                glib::ControlFlow::Break
            }
            None => glib::ControlFlow::Continue,
        }
    });
}

fn local_paths_from_file_list(file_list: &gdk::FileList) -> Vec<PathBuf> {
    file_list
        .files()
        .into_iter()
        .filter_map(|file: gio::File| file.path())
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use sustain_app_runtime::{ApplicationRuntime, NotificationCategory, NotificationSeverity};

    use super::prepare_library_import;

    #[test]
    fn import_start_failure_reports_through_the_notification_center() {
        // A fresh runtime has no library services, so preparing an import
        // fails before any worker or GTK work starts.
        let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));

        assert!(prepare_library_import(&runtime, vec!["song.flac".into()]).is_err());

        let runtime = runtime.borrow();
        let notification = runtime
            .notifications()
            .current_ephemeral()
            .expect("an import start-failure notification");
        assert_eq!(notification.category, NotificationCategory::LibraryImport);
        assert_eq!(notification.severity, NotificationSeverity::Error);
    }
}
