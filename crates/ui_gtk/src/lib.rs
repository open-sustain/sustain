// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::{cell::RefCell, path::PathBuf, rc::Rc};

use gtk::glib;
use gtk::prelude::*;
use main_window::build_main_window;

pub use sustain_app_runtime::{
    ApplicationCommand, ApplicationRuntime, ApplicationRuntimeError, BackgroundResourceUsage,
    BackgroundTaskStatus, CdEncodingProfile, CdImportProgress, CdImportRequest, CdImportResult,
    CdImportSummary, CdLookupEvent, CdReadMode, ConnectedDevice, DiscRelease, DiscTrack,
    LibraryConsolidationResult, LibraryConsolidationSummary, LibraryHydrationSnapshot,
    LibraryHydrationState, LibraryImportProgress, LibraryImportResult, LibraryImportSummary,
    LibraryImportTask, LibraryManagementMode, LibraryScanResult, LibraryScanSummary,
    OpticalDiscoveryResult, RawTocTrack, SmartPlaylistTrackStatus, TocSnapshot, TocTrack,
    UserSettings, run_cd_import_task, run_library_consolidation_task, run_library_import_task,
    run_library_import_task_with_progress, run_library_scan_task,
};

mod accent;
mod albums;
mod app_css;
mod artwork_color;
mod artwork_loader;
mod cd_import_panel;
mod cell_registry;
mod centered_bar;
mod chart;
mod command_controller;
mod confirmation;
mod content_stack;
mod date_format;
mod device_panel;
mod duplicate_consolidation;
mod duplicates;
mod flexible_width;
mod frame_profile;
mod import_failures_dialog;
mod leading_icon;
mod library_consolidation;
mod library_import;
mod library_scan;
mod library_view_update;
mod local_time_zone_monitor;
mod main_window;
mod metadata_diff;
mod missing_track;
mod now_playing;
mod playlists_header;
mod preferences;
mod queue_view;
mod rating_stars;
mod shortcuts;
mod shuffle_icon;
mod sidebar;
mod sidebar_context;
mod smart_playlist_editor;
mod statistics;
mod status_bar;
mod suggestion_entry;
mod table_cell;
#[cfg(test)]
mod test_support;
mod titlebar;
mod track_context;
mod track_context_ops;
mod track_info;
mod track_summary;
mod track_table;
mod util;
mod verified_duplicates;
mod wall_clock_alarm;
mod window_chrome;
mod youtube_audio_replacement;

const TITLEBAR_HEIGHT: i32 = 72;
const TITLEBAR_LEFT_PADDING: i32 = 48;
const TITLEBAR_RIGHT_PADDING: i32 = 0;
const TITLEBAR_CONTROL_HEIGHT: i32 = 42;
const MEDIA_ICON_SIZE: i32 = 32;
/// The blank space either side of the now-playing area and at the top
/// bar's start edge.
const TITLEBAR_GAP: i32 = TITLEBAR_HEIGHT / 2;
/// How narrow the top bar's blank spaces get in a narrow window.
const TITLEBAR_MIN_GAP: i32 = 12;
const NOW_PLAYING_ICON_SIZE: i32 = 16;
const NOW_PLAYING_SIDE_WIDTH: i32 = 58;
const NOW_PLAYING_WIDTH: i32 = 600;
/// How narrow the now-playing area gets in a narrow window.
const NOW_PLAYING_MIN_WIDTH: i32 = NOW_PLAYING_WIDTH / 2;
const PREFERENCES_WIDTH: i32 = 560;
const SMART_PLAYLIST_EDITOR_WIDTH: i32 = 620;
const SMART_PLAYLIST_EDITOR_HEIGHT: i32 = 360;
const RESIZE_CORNER_SIZE: i32 = 18;
const RESIZE_EDGE_THICKNESS: i32 = 6;
const SIDEBAR_DEFAULT_WIDTH: i32 = 220;
const SIDEBAR_MIN_WIDTH: i32 = 150;
const SIDEBAR_MAX_WIDTH: i32 = 300;
const STATUS_BAR_HEIGHT: i32 = 28;
const VOLUME_WIDTH: i32 = 192;
/// How narrow the volume slider gets in a narrow window.
const VOLUME_MIN_WIDTH: i32 = VOLUME_WIDTH / 2;
const VOLUME_MAGNET_THRESHOLD: f64 = 0.90;
const WINDOW_SHADOW_MARGIN: i32 = 24;
/// Fixed reverse-DNS id of the installed `.desktop` entry / icon theme name.
///
/// This is the value used when **looking up the application's icon** (window
/// icon, now-playing fallback image) — those lookups must match the file
/// name shipped by the Debian package, regardless of which database the
/// running instance is pointing at.
///
/// The GTK *application id* used for single-instance routing is a separate
/// value derived from the resolved database path (see
/// `sustain-app`'s `instance_lock` module). Do not reuse `APP_ID` for
/// `gtk::Application::application_id`.
const APP_ID: &str = "io.github.open_sustain.sustain";
const SONGS_VIEW: &str = "songs";
const ALBUMS_VIEW: &str = "albums";
const STATISTICS_VIEW: &str = "statistics";
const PLAYLISTS_VIEW: &str = "playlists";
const DEVICES_VIEW: &str = "devices";
const CD_IMPORT_VIEW: &str = "cd-import";
const DUPLICATES_VIEW: &str = "duplicates";

pub(crate) type SharedRuntime = Rc<RefCell<ApplicationRuntime>>;
/// Applies the view work a batch of committed library changes calls for.
pub(crate) type LibraryViewUpdateCallback = Rc<dyn Fn(&library_view_update::LibraryViewUpdate)>;
/// Fired when the artwork embedded in tracks' files changed. Embedded
/// artwork is not a library field, so the runtime's committed library
/// changes do not report it; whoever changed it reports it here, after
/// updating the artwork loader's cache.
pub(crate) type ArtworkChangedCallback = Rc<dyn Fn(&[sustain_app_runtime::TrackId])>;
pub(crate) type ArtworkChangedHolder = Rc<RefCell<Option<ArtworkChangedCallback>>>;
pub(crate) type PlaybackChangedCallback = Rc<dyn Fn()>;
pub(crate) type ShowAlbumAction = Rc<dyn Fn(sustain_app_runtime::TrackId)>;
pub(crate) type ShowAlbumHolder = Rc<RefCell<Option<ShowAlbumAction>>>;
pub(crate) type SharedMprisService = Rc<sustain_desktop::MprisService>;
pub(crate) type MprisCommandReceiver = async_channel::Receiver<sustain_desktop::MprisCommand>;
pub(crate) type MetadataWriterEventReceiver =
    async_channel::Receiver<sustain_app_runtime::MetadataWriterEvent>;
pub(crate) type ArtworkFetchResultReceiver =
    async_channel::Receiver<sustain_app_runtime::ArtworkFetchResult>;
pub(crate) type YoutubeAudioDownloadResultReceiver =
    async_channel::Receiver<sustain_app_runtime::YoutubeAudioDownloadResult>;
pub(crate) type AnalysisProgressReceiver =
    async_channel::Receiver<sustain_app_runtime::AnalysisProgress>;
pub(crate) type OnlineProgressReceiver =
    async_channel::Receiver<sustain_app_runtime::OnlineProgress>;
pub(crate) type TrackUpdatedReceiver = async_channel::Receiver<sustain_app_runtime::TrackUpdated>;
pub(crate) type SmartShuffleRebuildResultReceiver =
    async_channel::Receiver<sustain_app_runtime::SmartShuffleRebuildResult>;
pub(crate) type DeviceSyncEventReceiver =
    async_channel::Receiver<sustain_app_runtime::DeviceSyncEvent>;
pub(crate) type DevicePlanResultReceiver =
    async_channel::Receiver<sustain_app_runtime::DevicePlanResult>;
pub(crate) type MtpDiscoveryResultReceiver =
    async_channel::Receiver<sustain_app_runtime::MtpDiscoveryResult>;
pub(crate) type OpticalDiscoveryResultReceiver =
    async_channel::Receiver<sustain_app_runtime::OpticalDiscoveryResult>;
pub(crate) type CdLookupEventReceiver = async_channel::Receiver<sustain_app_runtime::CdLookupEvent>;
pub(crate) type LibraryHydrationResultReceiver = async_channel::Receiver<
    sustain_app_runtime::ApplicationRuntimeResult<LibraryHydrationSnapshot>,
>;

pub fn run(
    mut runtime: ApplicationRuntime,
    application_id: &str,
    artwork_cache_dir: PathBuf,
    database_path: PathBuf,
    gtk_arguments: Vec<String>,
) -> glib::ExitCode {
    let run_profile = sustain_profiler::ProfileScope::start();
    sustain_profiler::profile_mark!(run_profile, "ui_gtk::run entered");
    let app = gtk::Application::builder()
        .application_id(application_id)
        .build();
    sustain_profiler::profile_mark!(run_profile, "GTK application built");

    // Install the result sink before starting the async metadata writer so a
    // restored outbox row that fails on its first startup attempt is visible
    // to the UI. The worker is still running before any UI mutation can
    // enqueue new work.
    let (writer_event_tx, writer_event_rx) =
        async_channel::unbounded::<sustain_app_runtime::MetadataWriterEvent>();
    runtime.set_metadata_writer_event_sink(writer_event_tx);
    sustain_profiler::profile_mark!(run_profile, "metadata writer start requested");
    if let Err(error) = runtime.start_metadata_writer() {
        eprintln!(
            "Sustain: async metadata writer could not start ({error:?}); tag writes will run on the main thread."
        );
    }
    sustain_profiler::profile_mark!(run_profile, "metadata writer started");

    // Start the artwork fetcher and install its result sink. The
    // fetcher only runs when a remote metadata service was installed
    // by the app entry; otherwise this is a no-op and any
    // `FetchArtwork` command returns `ArtworkFetchingUnavailable` at
    // dispatch time. The matching receiver is wired into the main
    // window below so successful fetches drive a `SetArtwork`
    // follow-up on the GTK main thread.
    let (fetch_result_tx, fetch_result_rx) =
        async_channel::unbounded::<sustain_app_runtime::ArtworkFetchResult>();
    runtime.set_artwork_fetch_result_sink(fetch_result_tx);
    sustain_profiler::profile_mark!(run_profile, "artwork fetcher start requested");
    if let Err(error) = runtime.start_artwork_fetcher() {
        // The only legitimate failure here is "no remote metadata
        // service installed", which is a normal state for builds
        // without networking enabled. Log and continue; the click-
        // to-fetch affordance simply stays inert.
        eprintln!(
            "Sustain: remote artwork retrieval disabled ({error:?}); the missing-artwork tile will not be clickable."
        );
    }

    sustain_profiler::profile_mark!(run_profile, "artwork fetcher started");

    let (youtube_audio_result_tx, youtube_audio_result_rx) =
        async_channel::unbounded::<sustain_app_runtime::YoutubeAudioDownloadResult>();
    runtime.set_youtube_audio_download_result_sink(youtube_audio_result_tx);
    sustain_profiler::profile_mark!(run_profile, "YouTube audio downloader start requested");
    if let Err(error) = runtime.start_youtube_audio_downloader() {
        eprintln!("Sustain: YouTube audio replacement disabled ({error:?}).");
    }
    sustain_profiler::profile_mark!(run_profile, "YouTube audio downloader started");

    // Install the shared track-updated channel BEFORE either scheduler
    // is started so each captures a live sender. The UI shell drains
    // the receiver on the main loop and reloads the touched row from
    // the library store; without this, scheduler writes (analysis BPM,
    // online lyrics/tags) would not surface until the next launch.
    let (track_updated_tx, track_updated_rx) =
        async_channel::unbounded::<sustain_app_runtime::TrackUpdated>();
    runtime.set_track_updated_sink(track_updated_tx);

    // The Smart Shuffle scheduler owns its result channel internally
    // (it pre-allocates the (sender, receiver) pair on construction);
    // grab a receiver clone here before the runtime is wrapped in the
    // shared cell, mirroring how the other channels surface to the
    // main loop. Without a drain, completed rebuilds would queue
    // forever in the channel and the index would never be adopted.
    let smart_shuffle_rebuild_result_rx = runtime.smart_shuffle_rebuild_result_receiver();
    let device_plan_result_rx = runtime.device_plan_result_receiver();
    let mtp_discovery_rx = runtime.mtp_discovery_receiver();
    let optical_discovery_rx = runtime.optical_discovery_receiver();
    let cd_lookup_rx = runtime.cd_lookup_receiver();
    let device_sync_event_rx = runtime.device_sync_event_receiver();
    let library_hydration_result_rx = runtime.library_hydration_result_receiver();

    // Start the paced background analysis scheduler. The progress sink
    // is installed before `start_analysis_scheduler` so the worker's
    // first Idle/Tick has somewhere to land. As with the metadata
    // writer and artwork fetcher, the scheduler only runs when the
    // user has toggled at least one analysis capability on in
    // Preferences — at startup with all tickboxes off, the worker
    // emits one Idle and blocks until a settings change.
    let (analysis_progress_tx, analysis_progress_rx) =
        async_channel::unbounded::<sustain_app_runtime::AnalysisProgress>();
    runtime.set_analysis_progress_sink(analysis_progress_tx);

    // Start the paced background online scheduler. Same shape as the
    // analysis scheduler — progress sink installed first, then the
    // worker. Failure is non-fatal: a build without a remote metadata
    // service simply has no online retrieval surface.
    let (online_progress_tx, online_progress_rx) =
        async_channel::unbounded::<sustain_app_runtime::OnlineProgress>();
    runtime.set_online_progress_sink(online_progress_tx);
    if runtime.library_hydration_state() == LibraryHydrationState::Ready {
        start_background_schedulers(&mut runtime);
        sustain_profiler::profile_mark!(run_profile, "background schedulers started");
    } else {
        sustain_profiler::profile_mark!(
            run_profile,
            "background schedulers deferred until library hydration"
        );
    }

    let runtime = Rc::new(RefCell::new(runtime));

    // Spawn the MPRIS worker. `start` returns immediately: the session-bus
    // connection and name acquisition run on the worker thread, off the
    // cold-start critical path, so this never delays window presentation or
    // the 225 ms first-idle budget (#98). State published before the bus is
    // ready queues and is applied once it connects; a hung or unavailable
    // bus is bounded and surfaces as a logged "disabled". The inbound
    // channel carries method calls from the MPRIS worker thread to the GTK
    // main thread, where they can safely touch the runtime.
    let (mpris_command_tx, mpris_command_rx) =
        async_channel::unbounded::<sustain_desktop::MprisCommand>();
    sustain_profiler::profile_mark!(run_profile, "MPRIS start requested");
    let mpris_service = match start_mpris(mpris_command_tx) {
        Ok(service) => Some(Rc::new(service)),
        Err(error) => {
            eprintln!("Sustain: MPRIS (media key) integration disabled: {error}");
            None
        }
    };
    // The receivers feed the one main window, built by the first
    // activation. A second `sustain` launch hands its activation to this
    // instance (`forward_activate`): that, and any later activation,
    // raises the window instead of building another one.
    let pending_receivers = Rc::new(RefCell::new(Some(
        crate::main_window::MainWindowAsyncReceivers {
            mpris_command_rx,
            metadata_writer_event_rx: writer_event_rx,
            artwork_fetch_result_rx: fetch_result_rx,
            youtube_audio_download_result_rx: youtube_audio_result_rx,
            analysis_progress_rx,
            online_progress_rx,
            track_updated_rx,
            smart_shuffle_rebuild_result_rx,
            device_sync_event_rx,
            device_plan_result_rx,
            mtp_discovery_rx,
            optical_discovery_rx,
            cd_lookup_rx,
            library_hydration_result_rx,
        },
    )));

    sustain_profiler::profile_mark!(run_profile, "connect_activate installation starting");
    app.connect_activate({
        let runtime = runtime.clone();
        move |app| {
            let activate_profile = sustain_profiler::ProfileScope::start();
            sustain_profiler::profile_mark!(activate_profile, "activate: entered");
            let Some(receivers) = pending_receivers.borrow_mut().take() else {
                // The main window exists: raise the application's window
                // the user focused last.
                if let Some(window) = app.active_window() {
                    window.present();
                }
                return;
            };
            let main_window = build_main_window(
                app,
                runtime.clone(),
                mpris_service.clone(),
                artwork_cache_dir.clone(),
                database_path.clone(),
                receivers,
                activate_profile,
            );
            if let Some(profile) = activate_profile {
                sustain_profiler::profile!(
                    "activate: build_main_window returned at {:.1}ms",
                    profile.elapsed_ms()
                );
            }
            frame_profile::install(&main_window.window);
            main_window.window.present();
            if let Some(profile) = activate_profile {
                sustain_profiler::profile!(
                    "activate: window.present() returned at {:.1}ms",
                    profile.elapsed_ms()
                );
            }
            // Fires after the main loop has finished its current dispatch
            // batch — i.e. roughly when the window has had a chance to map.
            let profile_for_idle = activate_profile;
            gtk::glib::idle_add_local_once(move || {
                if let Some(profile) = profile_for_idle {
                    sustain_profiler::profile!(
                        "activate: first idle reached at {:.1}ms",
                        profile.elapsed_ms()
                    );
                }
                main_window.run_deferred_startup();
                if let Some(profile) = profile_for_idle {
                    sustain_profiler::profile!(
                        "activate: deferred startup dispatched (library hydration kicked off) at {:.1}ms",
                        profile.elapsed_ms()
                    );
                }
            });
        }
    });
    sustain_profiler::profile_mark!(run_profile, "connect_activate installed");

    sustain_profiler::profile_mark!(run_profile, "app.run() entered");
    let exit_code = app.run_with_args(&gtk_arguments);

    // Stop producers before joining the tag-mirror actor. Canonical edits and
    // pending file-tag intent are already durable in SQLite, so deferred
    // retries safely resume next launch instead of extending shutdown.
    let mut runtime_guard = runtime.borrow_mut();
    runtime_guard.shutdown_library_hydration();
    runtime_guard.shutdown_device_plan_scheduler();
    runtime_guard.shutdown_device_sync_scheduler();
    runtime_guard.shutdown_analysis_scheduler();
    runtime_guard.shutdown_online_scheduler();
    runtime_guard.shutdown_artwork_fetcher();
    runtime_guard.shutdown_youtube_audio_downloader();
    runtime_guard.shutdown_metadata_writer();
    exit_code
}

/// Quit by closing every window. Each window's close handler runs — the
/// main window's saves the UI session — and the application ends with
/// its last window. [`gtk::prelude::GtkApplicationExt`]'s `quit` would
/// end the main loop without running them.
pub(crate) fn quit_application(app: &gtk::Application) {
    for window in app.windows() {
        window.close();
    }
}

pub(crate) fn start_background_schedulers(runtime: &mut ApplicationRuntime) {
    if let Err(error) = runtime.start_analysis_scheduler() {
        // The only legitimate failure here is "no library store installed",
        // which would be an internal misconfiguration. Log and continue;
        // analysis simply does not run.
        eprintln!("Sustain: analysis scheduler could not start ({error:?}).");
    }
    if let Err(error) = runtime.start_online_scheduler() {
        eprintln!(
            "Sustain: online scheduler disabled ({error:?}); background lyrics/artwork retrieval will not run."
        );
    }
}

/// What became of an activation forwarded to the running Sustain.
#[derive(Debug)]
pub enum ForwardedActivation {
    /// The running Sustain's window was asked to come to the front.
    Forwarded,
    /// No Sustain window is running to take it: whatever holds the
    /// library is not the application (such as `--force-backfill`), or
    /// it has not registered on the session bus yet.
    NoWindowToRaise,
    /// The session bus could not be reached, or did not take the message.
    Failed(glib::Error),
}

/// Ask the already-running Sustain that owns `application_id` to raise
/// its window, then return.
///
/// Used by `sustain-app` when the per-database single-instance lock is
/// already held: instead of opening a second window, we send the primary
/// the `org.freedesktop.Application.Activate` call GApplication itself
/// sends, so it raises/focuses its existing window. This process only
/// calls the primary; it never registers `application_id`, so it can
/// neither stand in for a primary that is missing nor take the name from
/// one still starting up. With no primary on the bus it reports
/// [`ForwardedActivation::NoWindowToRaise`].
pub fn forward_activate(application_id: &str) -> ForwardedActivation {
    let connection =
        match gtk::gio::bus_get_sync(gtk::gio::BusType::Session, gtk::gio::Cancellable::NONE) {
            Ok(connection) => connection,
            Err(error) => return ForwardedActivation::Failed(error),
        };
    // The object path GApplication exports itself at.
    let object_path = format!("/{}", application_id.replace('.', "/").replace('-', "_"));
    match connection.call_sync(
        Some(application_id),
        &object_path,
        "org.freedesktop.Application",
        "Activate",
        Some(&activation_parameters(
            std::env::var("XDG_ACTIVATION_TOKEN").ok(),
            std::env::var("DESKTOP_STARTUP_ID").ok(),
        )),
        None,
        gtk::gio::DBusCallFlags::NO_AUTO_START,
        -1,
        gtk::gio::Cancellable::NONE,
    ) {
        Ok(_) => ForwardedActivation::Forwarded,
        // The name has no owner: `dbus-daemon` and `dbus-broker` say so
        // under different names.
        Err(error)
            if error.matches(gtk::gio::DBusError::ServiceUnknown)
                || error.matches(gtk::gio::DBusError::NameHasNoOwner) =>
        {
            ForwardedActivation::NoWindowToRaise
        }
        Err(error) => ForwardedActivation::Failed(error),
    }
}

/// The `(a{sv})` argument of `org.freedesktop.Application.Activate`: the
/// platform data GApplication sends, whose launcher activation token lets
/// the primary's window take the focus.
fn activation_parameters(
    activation_token: Option<String>,
    desktop_startup_id: Option<String>,
) -> glib::Variant {
    let platform_data = glib::VariantDict::new(None);
    if let Some(token) = activation_token {
        platform_data.insert("activation-token", token);
    }
    if let Some(id) = desktop_startup_id {
        platform_data.insert("desktop-startup-id", id);
    }
    glib::Variant::tuple_from_iter([platform_data.end()])
}

fn start_mpris(
    command_tx: async_channel::Sender<sustain_desktop::MprisCommand>,
) -> sustain_desktop::DesktopResult<sustain_desktop::MprisService> {
    sustain_desktop::MprisService::start(sustain_desktop::MprisStartConfig {
        command_sink: sustain_desktop::MprisPlaybackSink::new(move |command| {
            // Unbounded channel: try_send only fails if closed, i.e. the
            // GTK main loop has exited and the receiver was dropped.
            // Silent drop is the right behavior at shutdown.
            let _ = command_tx.try_send(command);
        }),
    })
}

#[cfg(test)]
mod activation_tests {
    use super::activation_parameters;
    use gtk::glib;

    #[test]
    fn an_activation_carries_the_platform_data_activate_takes() {
        let parameters =
            activation_parameters(Some("token".to_owned()), Some("startup".to_owned()));

        assert_eq!(parameters.type_().as_str(), "(a{sv})");
        let platform_data = glib::VariantDict::new(Some(&parameters.child_value(0)));
        assert_eq!(
            platform_data
                .lookup::<String>("activation-token")
                .ok()
                .flatten(),
            Some("token".to_owned())
        );
        assert_eq!(
            platform_data
                .lookup::<String>("desktop-startup-id")
                .ok()
                .flatten(),
            Some("startup".to_owned())
        );
    }
}
