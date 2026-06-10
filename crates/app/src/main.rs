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
    // Parse Sustain-owned flags before initialization so `--profile` can
    // govern every subsequent startup landmark and still be stripped from
    // the argv GTK receives.
    let parsed_args = match launch::parse_process_args(std::env::args()) {
        Ok(Some(parsed_args)) => parsed_args,
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
    let cli = parsed_args.cli;
    let mut startup_profile = sustain_profiler::StartupProfiler::start_if(cli.profile);
    sustain_profiler::profile_startup!(startup_profile, "main() entered");

    // Bind the process locale and gettext catalogs before anything else: this
    // is the one and only locale mutation, it must precede GTK initialization
    // and thread creation, and `setlocale(LC_ALL, "")` here also lets GTK
    // localize its own stock strings. Argument parsing above is pure Rust and
    // does not observe the process locale. The calls are O(1); the
    // (kilobyte-sized) catalog is mmapped lazily on first lookup during UI
    // construction.
    sustain_i18n::init();
    sustain_profiler::profile_startup!(startup_profile, "locale bound");

    // Resolve the config, database, and artwork-cache locations up front
    // (issue #7). The database path is resolved before anything else so the
    // single-instance lock is keyed off the exact same path the library store
    // will open. See `launch.rs` for the precedence rules and
    // `instance_lock.rs` for the integrity rationale.
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
            if cli.force_backfill {
                // The backfill rewrites audio files; a second writer on the
                // same library could clobber a concurrent tag write. There is
                // no window to raise for a headless command — refuse instead.
                eprintln!(
                    "Sustain: another instance is running for this library ({}). Close it before running --force-backfill.",
                    lock_path.display()
                );
                process::exit(1);
            }
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

    startup_profile.activate();
    sustain_profiler::profile_startup!(startup_profile, "instance lock acquired");
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

    sustain_profiler::profile_startup!(startup_profile, "settings store opened");
    match sustain_library_store::SqliteLibraryStore::open(&paths.database) {
        Ok(library_store) => {
            sustain_profiler::profile_startup!(startup_profile, "SQLite store opened");
            let was_freshly_created = library_store.was_freshly_created();
            // The force-backfill command iterates every track immediately, so
            // it needs the library hydrated synchronously; the UI prefers the
            // deferred path for a responsive cold start.
            let services_result = if cli.force_backfill {
                runtime.set_library_services(
                    Arc::new(library_store),
                    Arc::new(sustain_metadata::LoftyMetadataService),
                )
            } else {
                runtime.set_library_services_deferred_hydration(
                    Arc::new(library_store),
                    Arc::new(sustain_metadata::LoftyMetadataService),
                )
            };
            if let Err(error) = services_result {
                eprintln!("Sustain: library services failed to initialize ({error:?}).");
                process::exit(1);
            }
            if cli.force_backfill {
                sustain_profiler::profile_startup!(
                    startup_profile,
                    "library services installed (track hydration synchronous)"
                );
            } else {
                sustain_profiler::profile_startup!(
                    startup_profile,
                    "library services installed (track hydration deferred)"
                );
            }
            if was_freshly_created {
                if let Err(error) = runtime.seed_default_smart_playlists() {
                    eprintln!("Sustain: failed to seed default smart playlists ({error:?}).");
                }
            }
        }
        Err(sustain_library_store::StoreError::DatabaseAhead { current, supported }) => {
            eprintln!(
                "Sustain: library database schema version {current} is newer than this build supports ({supported})."
            );
            eprintln!("Install a newer Sustain build before opening this library.");
            process::exit(1);
        }
        Err(sustain_library_store::StoreError::UnversionedDatabaseNotEmpty) => {
            eprintln!(
                "Sustain: the library database predates schema versioning and cannot be upgraded safely."
            );
            eprintln!(
                "Delete {} and restart Sustain to rebuild it by scanning your library.",
                paths.database.display()
            );
            process::exit(1);
        }
        Err(error) => {
            eprintln!("Sustain: library database is unavailable ({error:?}).");
            process::exit(1);
        }
    }

    // Hidden maintenance command (#143): rewrite every track's file tags
    // from the authoritative SQLite values, then exit without ever building
    // the UI, playback, or networked-metadata services.
    if cli.force_backfill {
        run_force_backfill(&runtime);
    }

    if let Ok(playback_service) = sustain_playback::GStreamerPlaybackService::new() {
        runtime = runtime.with_playback_service(Box::new(playback_service));
    }
    sustain_profiler::profile_startup!(startup_profile, "playback service initialized");

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
    sustain_profiler::profile_startup!(startup_profile, "remote metadata service installed");

    // Install the CD-import backend: the libdiscid-backed optical probe and
    // the GStreamer encoder. Both are zero-sized handles; discovery itself is
    // deferred until after first idle by the UI, so this never touches a
    // drive at startup and cannot regress the cold-start budget.
    runtime.set_cd_backend(
        Arc::new(sustain_cd_import::SystemOpticalProbe::new()),
        Arc::new(sustain_cd_import::GStreamerCdEncoder::new()),
    );
    sustain_profiler::profile_startup!(
        startup_profile,
        "CD-import backend installed, handing off to ui_gtk::run"
    );

    // Known GTK/GDK runtime warning on some Wayland/Vulkan setups:
    // `vkAcquireNextImageKHR(): ... VK_SUBOPTIMAL_KHR`.
    // This is emitted below Sustain by GTK's Vulkan renderer when the swapchain
    // becomes suboptimal, commonly around resize/scale/surface changes. Rendering
    // can still present successfully, so we intentionally do not filter the log or
    // force `GSK_RENDERER` here. If it becomes visually broken, prefer documenting
    // `GSK_RENDERER=ngl` / `GSK_RENDERER=gl` as a user workaround before changing
    // the app default.
    sustain_ui_gtk::run(
        runtime,
        GTK_APPLICATION_ID,
        paths.cache_dir,
        paths.database,
        parsed_args.gtk_arguments,
    );
}

/// Drive the hidden `--force-backfill` command to completion and exit.
///
/// Streams one line per track to stderr (so progress is visible while a
/// large library is rewritten) and prints a final tally. Exits non-zero if
/// any track failed or the pass could not run at all, so the command is
/// usable from a script.
fn run_force_backfill(runtime: &sustain_app_runtime::ApplicationRuntime) -> ! {
    use sustain_app_runtime::ForceBackfillOutcome;

    eprintln!("Sustain: force-backfill — rewriting file tags from the library database.");
    let summary = runtime.force_backfill_tags(|progress| {
        let label = track_label(progress.track);
        match &progress.outcome {
            ForceBackfillOutcome::Written => {
                eprintln!("[{}/{}] {label}", progress.done, progress.total);
            }
            ForceBackfillOutcome::SkippedMissing => {
                eprintln!(
                    "[{}/{}] {label} — skipped (file missing)",
                    progress.done, progress.total
                );
            }
            ForceBackfillOutcome::Failed(reason) => {
                eprintln!(
                    "[{}/{}] {label} — FAILED: {reason}",
                    progress.done, progress.total
                );
            }
        }
    });
    match summary {
        Ok(summary) => {
            eprintln!(
                "Sustain: force-backfill complete — {} written, {} skipped (missing), {} failed, {} total.",
                summary.written, summary.skipped_missing, summary.failed, summary.total
            );
            process::exit(i32::from(summary.failed > 0));
        }
        Err(error) => {
            eprintln!("Sustain: force-backfill could not run ({error:?}).");
            process::exit(1);
        }
    }
}

/// A human-readable one-line identifier for a track in backfill progress
/// output: "Artist — Title", the title alone, or the library-relative path
/// when neither tag is populated.
fn track_label(track: &sustain_app_runtime::Track) -> String {
    let nonempty = |value: &str| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };
    let title = track.metadata.title.as_deref().and_then(nonempty);
    let artist = track.metadata.artist.as_deref().and_then(nonempty);
    match (artist, title) {
        (Some(artist), Some(title)) => format!("{artist} — {title}"),
        (None, Some(title)) => title,
        _ => track.location.relative_path.as_path().display().to_string(),
    }
}
