// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

mod instance_lock;
mod launch;

use std::{process, sync::Arc};

use crate::instance_lock::{AcquireOutcome, InstanceLock};

/// Canonical reverse-DNS GApplication id. Must match the basename of
/// the installed `.desktop` entry and the file name shipped under
/// `usr/share/icons/hicolor/*/apps/`, because GNOME Shell and other
/// Wayland compositors look up the alt-tab / dock / "running app" icon
/// by exact-matching the surface's `app_id` against either a desktop
/// file with the same id or an icon-theme entry with the same name.
/// Using a fixed string here (rather than deriving one from the
/// database path) is the only reliable way to surface Sustain's icon
/// in the window list. Sustain is a single-instance application; the
/// GApplication uniqueness check handles routing a second activation
/// to the running window, and [`instance_lock`] is the
/// filesystem-level backstop documented there.
const GTK_APPLICATION_ID: &str = "io.github.open_sustain.sustain";

fn main() {
    let t0 = std::time::Instant::now();
    macro_rules! tlog {
        ($label:expr) => {
            eprintln!(
                "[TIMING] {:>8.1}ms {}",
                t0.elapsed().as_secs_f64() * 1000.0,
                $label
            );
        };
    }
    tlog!("main() entered");
    // Parse developer-isolation flags and resolve the config, database,
    // and artwork-cache locations up front (issue #7). The database path
    // is resolved before anything else so the single-instance lock is
    // keyed off the exact same path the library store will open. See
    // `launch.rs` for the precedence rules and `instance_lock.rs` for the
    // integrity rationale.
    let cli = match launch::parse_args(std::env::args().skip(1)) {
        Ok(Some(cli)) => cli,
        Ok(None) => {
            println!("{}", launch::USAGE);
            return;
        }
        Err(error) => {
            eprintln!("Sustain: {error}.\n");
            eprintln!("{}", launch::USAGE);
            process::exit(2);
        }
    };
    let defaults = launch::XdgDefaults {
        config: sustain_settings::default_settings_path(),
        database: sustain_library_store::default_database_path(),
        cache_dir: launch::default_cache_dir(),
    };
    let working_dir = std::env::current_dir().ok();
    let paths = match launch::resolve_paths(&cli, working_dir.as_deref(), &defaults) {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("Sustain: cannot resolve on-disk locations: {error}.");
            process::exit(1);
        }
    };
    // Log the resolved locations so a developer sees immediately which
    // database, settings file, and cache the running instance is touching.
    eprintln!("Sustain: settings {}", paths.config.display());
    eprintln!("Sustain: database {}", paths.database.display());
    eprintln!("Sustain: artwork  {}", paths.cache_dir.display());

    let _instance_lock: InstanceLock = match instance_lock::acquire(&paths.database) {
        AcquireOutcome::Acquired(lock) => lock,
        AcquireOutcome::Held { lock_path } => {
            eprintln!(
                "Sustain: another instance is already running for this library ({}). Raising its window.",
                lock_path.display()
            );
            // Hand the activate off to the primary instance so its window
            // is raised/focused, then exit with whatever GTK reported for
            // the brief remote registration.
            let exit_code = sustain_ui_gtk::forward_activate(GTK_APPLICATION_ID);
            process::exit(i32::from(exit_code));
        }
        AcquireOutcome::Failed { lock_path, error } => {
            eprintln!(
                "Sustain: cannot acquire the single-instance lock at {} ({error}).",
                lock_path.display()
            );
            eprintln!(
                "Refusing to start without a lock — two Sustain processes writing the same library can corrupt it."
            );
            process::exit(1);
        }
    };

    tlog!("instance lock acquired");
    let settings_store = sustain_settings::TomlSettingsStore::new(&paths.config);
    let settings_path = settings_store.path().to_path_buf();
    let mut runtime = match sustain_app_runtime::ApplicationRuntime::with_settings_store(Box::new(
        settings_store,
    )) {
        Ok(runtime) => runtime,
        Err(_) => {
            // Sustain is pre-release and ships no migration code. Any load
            // failure on an existing file means the on-disk format is from a
            // previous development iteration. The fix is to delete it.
            eprintln!(
                "Sustain: settings file at {} could not be loaded.",
                settings_path.display()
            );
            eprintln!(
                "The file is in an incompatible/outdated format. Delete it and restart Sustain."
            );
            process::exit(1);
        }
    };

    tlog!("settings store opened");
    match sustain_library_store::SqliteLibraryStore::open(&paths.database) {
        Ok(library_store) => {
            tlog!("sqlite library store opened");
            let was_freshly_created = library_store.was_freshly_created();
            if let Err(error) = runtime.set_library_services_deferred_hydration(
                Arc::new(library_store),
                Arc::new(sustain_metadata::LoftyMetadataService),
            ) {
                eprintln!("Sustain: library services failed to initialize ({error:?}).");
                process::exit(1);
            }
            if was_freshly_created {
                if let Err(error) = runtime.seed_default_smart_playlists() {
                    eprintln!("Sustain: failed to seed default smart playlists ({error:?}).");
                }
            }
        }
        Err(error) => {
            eprintln!("Sustain: library database is unavailable ({error:?}).");
            process::exit(1);
        }
    }

    tlog!("library services installed (track hydration deferred)");
    if let Ok(playback_service) = sustain_playback::GStreamerPlaybackService::new() {
        runtime = runtime.with_playback_service(Box::new(playback_service));
    }
    tlog!("playback service initialized");

    // Install the networked metadata service. The User-Agent is
    // mandatory for MusicBrainz; the contact URL points back at the
    // project repository so the maintainer reaches a human if abuse
    // reports come in. The AcoustID key is a compile-time secret —
    // builds without `SUSTAIN_ACOUSTID_API_KEY` set are still
    // functional for tag-based identification and graceful no-ops for
    // fingerprint-based identification.
    let user_agent = format!(
        "Sustain/{version} ( {homepage} )",
        version = env!("CARGO_PKG_VERSION"),
        homepage = "https://github.com/open-sustain/sustain",
    );
    let http_client = std::sync::Arc::new(sustain_metadata_remote::HttpClient::new(
        sustain_metadata_remote::HttpClientConfig { user_agent },
    ));
    let remote_service = sustain_metadata_remote::ComposedRemoteMetadataService::from_http_client(
        http_client,
        sustain_metadata_remote::acoustid_api_key(),
    );
    runtime.set_remote_metadata_service(Arc::new(remote_service));
    tlog!("remote metadata service installed, handing off to ui_gtk::run");

    // Known GTK/GDK runtime warning on some Wayland/Vulkan setups:
    // `vkAcquireNextImageKHR(): ... VK_SUBOPTIMAL_KHR`.
    // This is emitted below Sustain by GTK's Vulkan renderer when the swapchain
    // becomes suboptimal, commonly around resize/scale/surface changes. Rendering
    // can still present successfully, so we intentionally do not filter the log or
    // force `GSK_RENDERER` here. If it becomes visually broken, prefer documenting
    // `GSK_RENDERER=ngl` / `GSK_RENDERER=gl` as a user workaround before changing
    // the app default.
    sustain_ui_gtk::run(runtime, GTK_APPLICATION_ID, paths.cache_dir);
}
