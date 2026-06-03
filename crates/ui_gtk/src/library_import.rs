// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{path::PathBuf, rc::Rc, sync::mpsc, time::Duration};

use gtk::glib;
use gtk::prelude::*;
use gtk::{gdk, gio};

use super::{
    ApplicationRuntimeError, LibraryImportProgress, LibraryImportResult, SharedRuntime,
    run_library_import_task_with_progress,
};

enum LibraryImportWorkerEvent {
    Progress(LibraryImportProgress),
    Finished(Result<LibraryImportResult, ApplicationRuntimeError>),
}

pub(crate) type LibraryImportRequestedCallback =
    Rc<dyn Fn(Vec<PathBuf>) -> Result<(), ApplicationRuntimeError>>;
pub(crate) type LibraryImportCompletedCallback = Rc<dyn Fn(&[sustain_app_runtime::Track])>;

pub(crate) fn library_import_requested_callback(
    runtime: &SharedRuntime,
    import_completed: LibraryImportCompletedCallback,
) -> LibraryImportRequestedCallback {
    let runtime = runtime.clone();

    Rc::new(move |paths| {
        let task = {
            let mut runtime = runtime.borrow_mut();
            runtime.prepare_library_import(paths)?
        };

        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let outcome = run_library_import_task_with_progress(task, |progress| {
                let _sent = tx.send(LibraryImportWorkerEvent::Progress(progress));
            });
            let _sent = tx.send(LibraryImportWorkerEvent::Finished(outcome));
        });

        poll_library_import(rx, runtime.clone(), import_completed.clone());
        Ok(())
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
    import_completed: LibraryImportCompletedCallback,
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
            Some(Ok(result)) => {
                let imported_tracks = result.tracks.clone();
                runtime.borrow_mut().apply_library_import_result(result);
                import_completed(&imported_tracks);
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
