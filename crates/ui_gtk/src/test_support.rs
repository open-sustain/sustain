// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Test-only helpers for exercising GTK widgets safely.
//!
//! GTK must be initialised exactly once and used only from the thread
//! that owns it. `gtk4`'s runtime enforces the first half outright: the
//! second thread to call [`gtk::init`] panics with `Attempted to
//! initialize GTK from two different threads.`, and a concurrent init
//! can also trip a `g_static_resource_init` critical. The Rust test
//! harness runs unit tests on many threads in parallel, so any two
//! widget tests that each call `gtk::init` race — an intermittent abort
//! whose timing depends on how the harness happens to schedule them.
//!
//! [`with_gtk`] removes that race by owning a single, lazily-spawned
//! GTK thread for the whole test process. Every widget test submits its
//! work to that one thread, which initialises GTK once and runs jobs
//! serially. Tests never touch GTK on their own harness thread, and the
//! remaining (non-GTK) tests keep running fully in parallel — no
//! process-wide `--test-threads=1` is needed.

use std::sync::OnceLock;
use std::sync::mpsc::{self, Sender};

/// A unit of widget work to run on the GTK thread, paired with the
/// channel used to report its panic outcome back to the caller.
struct Job {
    work: Box<dyn FnOnce() + Send>,
    done: Sender<std::thread::Result<()>>,
}

/// Handle to the process-wide GTK test thread.
struct GtkThread {
    jobs: Sender<Job>,
    /// Whether [`gtk::init`] succeeded. `false` under a headless
    /// environment with no display, in which case callers skip their
    /// widget assertions.
    available: bool,
}

fn gtk_thread() -> &'static GtkThread {
    static THREAD: OnceLock<GtkThread> = OnceLock::new();
    THREAD.get_or_init(|| {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("sustain-gtk-test".to_owned())
            .spawn(move || {
                let available = gtk::init().is_ok();
                if available {
                    // Process-global style setup belongs to the one
                    // thread that owns GTK, done once for all jobs.
                    crate::app_css::install_app_css();
                }
                ready_tx
                    .send(available)
                    .expect("gtk test thread readiness receiver is alive");
                if !available {
                    return;
                }
                for Job { work, done } in job_rx {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work));
                    let _ = done.send(outcome);
                }
            })
            .expect("spawn gtk test thread");
        let available = ready_rx.recv().expect("gtk test thread reports readiness");
        GtkThread {
            jobs: job_tx,
            available,
        }
    })
}

/// Runs `work` on the process's single GTK-owning thread, serialised
/// with every other GTK test, and propagates any panic back to the
/// caller so assertions inside `work` still fail the calling test.
///
/// Returns `true` when GTK was available and `work` ran; `false` under a
/// headless environment, where the caller should skip its assertions.
pub(crate) fn with_gtk<F>(work: F) -> bool
where
    F: FnOnce() + Send + 'static,
{
    let thread = gtk_thread();
    if !thread.available {
        return false;
    }
    let (done_tx, done_rx) = mpsc::channel();
    thread
        .jobs
        .send(Job {
            work: Box::new(work),
            done: done_tx,
        })
        .expect("gtk test thread accepts jobs");
    match done_rx.recv().expect("gtk test thread reports completion") {
        Ok(()) => true,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(test)]
mod tests {
    use super::with_gtk;
    use gtk::prelude::*;

    /// Regression for #113: many harness threads racing to use GTK
    /// would, without a single owning thread, trip gtk4's "initialize
    /// GTK from two different threads" panic or a `g_static_resource_init`
    /// critical. Funnelled through [`with_gtk`] they must all complete
    /// cleanly, and report one coherent availability.
    #[test]
    fn concurrent_with_gtk_calls_serialize_safely() {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                std::thread::spawn(|| {
                    with_gtk(|| {
                        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
                        column.append(&gtk::Label::new(Some("probe")));
                    })
                })
            })
            .collect();

        let ran: Vec<bool> = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker thread did not panic"))
            .collect();

        let ran_count = ran.iter().filter(|&&did| did).count();
        assert!(
            ran_count == 0 || ran_count == ran.len(),
            "with_gtk reported inconsistent availability across threads: {ran:?}"
        );
    }
}
