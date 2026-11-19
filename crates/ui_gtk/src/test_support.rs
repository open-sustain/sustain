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
                // Artwork tests decode on their own harness threads, so
                // this thread may be the second to use GIO: the VFS goes
                // first, as in the app (#384).
                crate::initialize_gio_vfs();
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
/// A test that needs GTK fails without a display rather than passing
/// with its assertions unrun: run the widget tests under a display
/// server, as CI does with `xvfb-run`.
pub(crate) fn with_gtk<F>(work: F)
where
    F: FnOnce() + Send + 'static,
{
    let thread = gtk_thread();
    assert!(
        thread.available,
        "GTK could not initialize: widget tests need a display (run under xvfb-run)"
    );
    let (done_tx, done_rx) = mpsc::channel();
    thread
        .jobs
        .send(Job {
            work: Box::new(work),
            done: done_tx,
        })
        .expect("gtk test thread accepts jobs");
    if let Err(panic) = done_rx.recv().expect("gtk test thread reports completion") {
        std::panic::resume_unwind(panic);
    }
}

/// A metadata service with no files behind it: every read finds nothing
/// and every write fails. For widget tests that need an
/// [`ArtworkLoader`](crate::artwork_loader::ArtworkLoader) but load no
/// artwork.
struct NoFiles;

impl sustain_app_runtime::MetadataService for NoFiles {
    fn read_initial_tags(
        &self,
        _path: &std::path::Path,
    ) -> sustain_app_runtime::MetadataResult<sustain_app_runtime::InitialTags> {
        Err(sustain_app_runtime::MetadataError::ReadFailed)
    }

    fn write_metadata(
        &self,
        _path: &std::path::Path,
        _change: sustain_app_runtime::MetadataChange,
    ) -> sustain_app_runtime::MetadataResult<()> {
        Err(sustain_app_runtime::MetadataError::WriteFailed)
    }

    fn write_rating(
        &self,
        _path: &std::path::Path,
        _rating: sustain_app_runtime::Rating,
    ) -> sustain_app_runtime::MetadataResult<()> {
        Err(sustain_app_runtime::MetadataError::WriteFailed)
    }

    fn read_artwork(
        &self,
        _path: &std::path::Path,
    ) -> sustain_app_runtime::MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_artwork(
        &self,
        _path: &std::path::Path,
        _artwork: Option<Vec<u8>>,
    ) -> sustain_app_runtime::MetadataResult<()> {
        Err(sustain_app_runtime::MetadataError::WriteFailed)
    }
}

/// An artwork loader over [`NoFiles`], caching under a directory of its
/// own in the system temporary directory.
pub(crate) fn artwork_loader() -> crate::artwork_loader::ArtworkLoader {
    let cache_dir =
        std::env::temp_dir().join(format!("sustain-ui-test-artwork-{}", std::process::id()));
    crate::artwork_loader::ArtworkLoader::new(std::sync::Arc::new(NoFiles), cache_dir)
}

/// A runtime whose library holds `tracks`, kept in an in-memory store.
pub(crate) fn runtime_with_tracks(tracks: Vec<sustain_app_runtime::Track>) -> crate::SharedRuntime {
    runtime_with_tracks_and_clock(tracks, std::sync::Arc::new(sustain_app_runtime::LocalClock))
}

/// A clock that reads whatever it was last set to, in the local calendar.
pub(crate) struct SettableClock(std::sync::Mutex<std::time::SystemTime>);

impl SettableClock {
    pub(crate) fn new(now: std::time::SystemTime) -> Self {
        Self(std::sync::Mutex::new(now))
    }

    pub(crate) fn set(&self, now: std::time::SystemTime) {
        *self.0.lock().expect("the clock is not poisoned") = now;
    }
}

impl sustain_app_runtime::Clock for SettableClock {
    fn now(&self) -> std::time::SystemTime {
        *self.0.lock().expect("the clock is not poisoned")
    }

    fn calendar(&self) -> std::sync::Arc<dyn sustain_app_runtime::Calendar> {
        sustain_app_runtime::Clock::calendar(&sustain_app_runtime::LocalClock)
    }
}

/// A runtime whose library holds `tracks`, kept in an in-memory store,
/// reading the time from `clock`.
pub(crate) fn runtime_with_tracks_and_clock(
    tracks: Vec<sustain_app_runtime::Track>,
    clock: std::sync::Arc<dyn sustain_app_runtime::Clock>,
) -> crate::SharedRuntime {
    use sustain_library_store::LibraryStore;

    let store = sustain_library_store::SqliteLibraryStore::open_in_memory()
        .expect("an in-memory library store opens");
    for track in tracks {
        store.save_track(track).expect("the store saves the track");
    }
    let runtime = sustain_app_runtime::ApplicationRuntime::new()
        .with_library_services(std::sync::Arc::new(store), std::sync::Arc::new(NoFiles))
        .expect("the runtime takes the store")
        .with_clock(clock);
    std::rc::Rc::new(std::cell::RefCell::new(runtime))
}

/// A track `id` titled `title`, available at `<id>.flac`.
pub(crate) fn track(id: i64, title: &str) -> sustain_app_runtime::Track {
    sustain_app_runtime::Track {
        id: sustain_app_runtime::TrackId::new(id).expect("positive track id"),
        location: sustain_app_runtime::TrackLocation::available(
            sustain_app_runtime::TrackRelativePath::new(std::path::PathBuf::from(format!(
                "{id}.flac"
            )))
            .expect("valid relative path"),
        ),
        metadata: sustain_app_runtime::TrackMetadata {
            title: Some(title.to_owned()),
            ..sustain_app_runtime::TrackMetadata::default()
        },
        rating: sustain_app_runtime::Rating::new(0).expect("rating in range"),
        statistics: sustain_app_runtime::PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    }
}

/// Closes the open window titled `title` and asserts it is gone: once the
/// main loop has run, neither the window nor any widget it held is still
/// alive. A handler that strongly captures a widget of its own tree keeps
/// that widget, or the whole tree, alive after the window closes (#370);
/// this is the check every dialog's disposal test makes.
pub(crate) fn assert_closes_and_disposes(title: &str) {
    use gtk::prelude::*;

    let window = gtk::Window::list_toplevels()
        .into_iter()
        .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
        .find(|window| window.title().as_deref() == Some(title))
        .expect("a window with the dialog's title is open");
    let widgets: Vec<_> = descendants(window.upcast_ref())
        .iter()
        .map(ObjectExt::downgrade)
        .collect();
    let weak_window = window.downgrade();
    window.close();
    drop(window);
    let context = gtk::glib::MainContext::default();
    while context.iteration(false) {}

    assert!(
        weak_window.upgrade().is_none(),
        "the closed {title:?} window is still alive"
    );
    let alive: Vec<_> = widgets
        .iter()
        .filter_map(|widget| widget.upgrade())
        .map(|widget| widget.type_().name())
        .collect();
    assert!(
        alive.is_empty(),
        "widgets of the closed {title:?} window are still alive: {alive:?}"
    );
}

/// Every widget below `widget`, depth first.
pub(crate) fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    use gtk::prelude::*;

    let mut found = Vec::new();
    let mut child = widget.first_child();
    while let Some(current) = child {
        found.push(current.clone());
        found.extend(descendants(&current));
        child = current.next_sibling();
    }
    found
}

#[cfg(test)]
mod tests {
    use super::with_gtk;
    use gtk::prelude::*;

    /// Regression for #113: many harness threads racing to use GTK
    /// would, without a single owning thread, trip gtk4's "initialize
    /// GTK from two different threads" panic or a `g_static_resource_init`
    /// critical. Funnelled through [`with_gtk`] they must all complete
    /// cleanly.
    #[test]
    fn concurrent_with_gtk_calls_serialize_safely() {
        let handles: Vec<_> = (0..16)
            .map(|_| {
                std::thread::spawn(|| {
                    with_gtk(|| {
                        let column = gtk::Box::new(gtk::Orientation::Vertical, 0);
                        column.append(&gtk::Label::new(Some("probe")));
                    });
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("worker thread did not panic");
        }
    }
}
