// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Glib main-loop consumers for the runtime's background-worker output:
//! metadata-write results, analysis/online scheduler progress, per-track
//! refresh events, Smart Shuffle rebuilds, and artwork fetches. Each drains an
//! `async_channel` receiver (or installs a runtime observer) and applies the
//! result on the GTK main thread.

use std::collections::HashSet;

use super::*;

/// Maximum number of Songs rows appended during one idle dispatch. This keeps
/// GTK list-model notifications bounded so a large library hydrates over
/// multiple frames instead of monopolizing the main loop.
const INITIAL_LIBRARY_ROWS_BATCH_SIZE: usize = 128;

pub(super) struct LibraryHydrationResultConsumerContext {
    pub(super) receiver: Option<LibraryHydrationResultReceiver>,
    pub(super) runtime: SharedRuntime,
    pub(super) songs_table: TrackTable,
    pub(super) status_bar: StatusBar,
    pub(super) titlebar: Titlebar,
    pub(super) sidebar: PlaylistSidebar,
    pub(super) content_stack: gtk::Stack,
    pub(super) current_search_text: Rc<RefCell<String>>,
    pub(super) post_hydration_startup: PostHydrationStartup,
}

struct InitialLibraryRowsPublication {
    runtime: SharedRuntime,
    songs_table: TrackTable,
    status_bar: StatusBar,
    titlebar: Titlebar,
    sidebar: PlaylistSidebar,
    content_stack: gtk::Stack,
    current_search_text: Rc<RefCell<String>>,
    post_hydration_startup: PostHydrationStartup,
}

/// Adopt the runtime snapshot loaded after first idle, then publish its Songs
/// rows in bounded GTK idle batches. The runtime remains mutation-gated until
/// the last batch lands, so the borrowed slice cannot change under the cursor.
pub(super) fn install_library_hydration_result_consumer(
    context: LibraryHydrationResultConsumerContext,
) {
    let LibraryHydrationResultConsumerContext {
        receiver,
        runtime,
        songs_table,
        status_bar,
        titlebar,
        sidebar,
        content_stack,
        current_search_text,
        post_hydration_startup,
    } = context;
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        if !runtime.borrow_mut().apply_library_hydration_result(result) {
            run_once(&post_hydration_startup);
            return;
        }
        eprintln!("[TIMING]   hydrate: SQLite snapshot adopted; publishing Songs rows");
        publish_initial_library_rows(InitialLibraryRowsPublication {
            runtime,
            songs_table,
            status_bar,
            titlebar,
            sidebar,
            content_stack,
            current_search_text,
            post_hydration_startup,
        });
    });
}

fn publish_initial_library_rows(publication: InitialLibraryRowsPublication) {
    let InitialLibraryRowsPublication {
        runtime,
        songs_table,
        status_bar,
        titlebar,
        sidebar,
        content_stack,
        current_search_text,
        post_hydration_startup,
    } = publication;
    let started = std::time::Instant::now();
    let normalized_search = normalize_query(&current_search_text.borrow());
    let (total_tracks, honor_sort_tags) = {
        let runtime = runtime.borrow();
        (
            runtime.library_tracks().len(),
            runtime.settings().library.honor_sort_tags,
        )
    };
    let generation = songs_table.begin_progressive_replace();
    let cursor = Rc::new(Cell::new(0usize));
    let visible_count = Rc::new(Cell::new(0usize));
    let duration_seconds = Rc::new(Cell::new(0u64));
    let size_bytes = Rc::new(Cell::new(0u64));

    glib::idle_add_local(move || {
        let start = cursor.get();
        let end = (start + INITIAL_LIBRARY_ROWS_BATCH_SIZE).min(total_tracks);
        let rows: Vec<_> = {
            let runtime = runtime.borrow();
            runtime.library_tracks()[start..end]
                .iter()
                .filter(|track| {
                    normalized_search.is_empty()
                        || runtime.search_matches(track.id, &normalized_search)
                })
                .map(|track| TrackTableRow::from_track(track, honor_sort_tags))
                .collect()
        };
        visible_count.set(visible_count.get() + rows.len());
        duration_seconds.set(
            duration_seconds.get() + rows.iter().map(|track| track.duration_seconds).sum::<u64>(),
        );
        size_bytes
            .set(size_bytes.get() + rows.iter().map(|track| track.file_size_bytes).sum::<u64>());
        let still_current = songs_table.append_progressive_rows(generation, rows);
        cursor.set(end);
        if still_current && end < total_tracks {
            return glib::ControlFlow::Continue;
        }

        if still_current && content_stack.visible_child_name().as_deref() == Some(SONGS_VIEW) {
            status_bar.update_summary_values(
                visible_count.get(),
                duration_seconds.get(),
                size_bytes.get(),
            );
        }
        {
            let mut runtime = runtime.borrow_mut();
            runtime.finish_library_hydration_publication();
            crate::start_background_schedulers(&mut runtime);
        }
        sidebar.refresh_silently();
        update_play_pause_sensitivity(&titlebar, &runtime.borrow());
        run_once(&post_hydration_startup);
        eprintln!(
            "[TIMING]   hydrate: Songs rows published in {:.1}ms",
            started.elapsed().as_secs_f64() * 1000.0
        );
        glib::ControlFlow::Break
    });
}

fn run_once(callback: &PostHydrationStartup) {
    if let Some(callback) = callback.borrow_mut().take() {
        callback();
    }
}

/// Drains [`sustain_app_runtime::MetadataWriterEvent`]s posted by the async
/// metadata writer and applies authoritative row refreshes plus retry-state
/// notifications.
///
/// Mirror failures retain their durable outbox row for bounded-backoff retry.
/// Managed retarget results reload SQLite because a filesystem or durability
/// failure may have happened on either side of the atomic row commit.
pub(super) fn install_metadata_writer_event_consumer(
    receiver: Option<MetadataWriterEventReceiver>,
    runtime: SharedRuntime,
    track_row_changed_holder: TrackRowChangedHolder,
    library_changed_holder: LibraryChangedHolder,
    relocation_completed: crate::missing_track::MissingTrackRelocationCompletedCallback,
    artwork_loader: ArtworkLoader,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = receiver.recv().await {
            let relocation = match &event {
                sustain_app_runtime::MetadataWriterEvent::MissingTrackRelocation(result) => {
                    Some((result.track_id, result.outcome.is_ok()))
                }
                sustain_app_runtime::MetadataWriterEvent::Mirror(_)
                | sustain_app_runtime::MetadataWriterEvent::TrackAvailabilityChanged(_)
                | sustain_app_runtime::MetadataWriterEvent::ManagedRetarget(_)
                | sustain_app_runtime::MetadataWriterEvent::DuplicateConsolidation(_)
                | sustain_app_runtime::MetadataWriterEvent::YoutubeAudioReplacement(_) => None,
            };
            let mirror_track_id = match &event {
                sustain_app_runtime::MetadataWriterEvent::Mirror(result)
                    if result.kind == sustain_app_runtime::MetadataWriteKind::Artwork =>
                {
                    Some(result.track_id)
                }
                sustain_app_runtime::MetadataWriterEvent::Mirror(_) => None,
                sustain_app_runtime::MetadataWriterEvent::TrackAvailabilityChanged(_)
                | sustain_app_runtime::MetadataWriterEvent::ManagedRetarget(_)
                | sustain_app_runtime::MetadataWriterEvent::MissingTrackRelocation(_)
                | sustain_app_runtime::MetadataWriterEvent::DuplicateConsolidation(_)
                | sustain_app_runtime::MetadataWriterEvent::YoutubeAudioReplacement(_) => None,
            };
            let library_changed = matches!(
                &event,
                sustain_app_runtime::MetadataWriterEvent::DuplicateConsolidation(result)
                    if result.outcome.is_ok()
            );
            let duplicate_artwork_sources = match &event {
                sustain_app_runtime::MetadataWriterEvent::DuplicateConsolidation(result) => {
                    result.outcome.as_ref().ok().map(|result| {
                        std::iter::once(result.survivor_id)
                            .chain(result.removed_track_ids.iter().copied())
                            .filter_map(|track_id| artwork_source_for_track(&runtime, track_id))
                            .collect::<Vec<_>>()
                    })
                }
                _ => None,
            };
            runtime.borrow_mut().apply_metadata_writer_event(event);
            // Events that change track data reload SQLite through
            // `apply_track_updated`, which fires the standard observer. Plain
            // mirror completion changes no SQLite row, but artwork consumers
            // still need a row repaint.
            if let Some(track_id) = mirror_track_id
                && let Some(callback) = track_row_changed_holder.borrow().as_ref()
            {
                callback(track_id, TrackRowChangedKind::Artwork);
            }
            if let Some((track_id, succeeded)) = relocation {
                relocation_completed(track_id, succeeded);
            }
            if let Some(sources) = duplicate_artwork_sources {
                for source in sources {
                    artwork_loader.invalidate(&source);
                }
            }
            if library_changed && let Some(callback) = library_changed_holder.borrow().as_ref() {
                callback();
            }
        }
    });
}

pub(super) fn install_youtube_audio_download_result_consumer(
    receiver: Option<YoutubeAudioDownloadResultReceiver>,
    runtime: SharedRuntime,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = receiver.recv().await {
            runtime
                .borrow_mut()
                .apply_youtube_audio_download_result(result);
        }
    });
}

/// Drains [`AnalysisProgress`](sustain_app_runtime::AnalysisProgress)
/// events posted by the background analysis scheduler. Each event is
/// applied to the runtime's notification center on the GTK main thread
/// via [`ApplicationRuntime::apply_analysis_progress`] — that's where
/// the persistent "Analyzing N/total..." notification is created,
/// updated in place per tick, and dismissed on Idle (with an
/// ephemeral summary toast when work actually happened).
pub(super) fn install_analysis_progress_consumer(
    receiver: Option<AnalysisProgressReceiver>,
    runtime: SharedRuntime,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(progress) = receiver.recv().await {
            runtime.borrow_mut().apply_analysis_progress(progress);
        }
    });
}

/// Symmetric to [`install_analysis_progress_consumer`] but for the
/// online scheduler. Each event lands in
/// [`ApplicationRuntime::apply_online_progress`] which owns the
/// matching persistent/ephemeral notification surface.
pub(super) fn install_online_progress_consumer(
    receiver: Option<OnlineProgressReceiver>,
    runtime: SharedRuntime,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(progress) = receiver.recv().await {
            runtime.borrow_mut().apply_online_progress(progress);
        }
    });
}

/// Wires the runtime's `track_data_observer` to the shared
/// per-row refresh callback. The runtime fires this observer after
/// every `apply_track_updated` — i.e. after a background worker has
/// mutated a single track in the library store. The deferred
/// closure invokes the standard row-refresh path so Songs/Albums/
/// Playlists views all repaint the touched row without rebuilding
/// the table.
pub(super) fn install_track_data_observer(
    runtime: &SharedRuntime,
    track_row_changed_holder: TrackRowChangedHolder,
) {
    runtime
        .borrow_mut()
        .set_track_data_observer(Box::new(move |track_id| {
            // The runtime is mid-borrow when this fires — defer the
            // refresh onto the GLib main loop so the closure can
            // re-borrow read-only without panicking.
            let track_row_changed_holder = track_row_changed_holder.clone();
            glib::idle_add_local_once(move || {
                if let Some(callback) = track_row_changed_holder.borrow().clone() {
                    callback(track_id, TrackRowChangedKind::Data);
                }
            });
        }));
}

/// Drains track-id events emitted by the analysis and online
/// schedulers. Each id is fed into
/// [`ApplicationRuntime::apply_track_updated`], which reloads the
/// row from the library store and fires the
/// `track_data_observer` so the UI repaints.
pub(super) fn install_track_updated_consumer(
    receiver: Option<TrackUpdatedReceiver>,
    runtime: SharedRuntime,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(first) = receiver.recv().await {
            // Coalesce a burst before touching the store: a full analysis
            // or online sweep posts ids faster than the main loop drains
            // them, and one track commonly emits several in quick
            // succession (bpm, then key, then audio). Refresh each row at
            // most once per drain so repeated events for the same track do
            // not each pay for a keyed query and a repaint.
            let mut pending: HashSet<TrackId> = HashSet::from([first]);
            while let Ok(next) = receiver.try_recv() {
                pending.insert(next);
            }
            let mut runtime = runtime.borrow_mut();
            for track_id in pending {
                runtime.apply_track_updated(track_id);
            }
        }
    });
}

/// Drains
/// [`SmartShuffleRebuildResult`](sustain_app_runtime::SmartShuffleRebuildResult)s
/// posted by the background Smart Shuffle rebuild thread and feeds
/// them into
/// [`ApplicationRuntime::apply_smart_shuffle_rebuild_result`], which
/// adopts the new index in memory and persists its blob through the
/// library store. Without this drain, completed rebuilds would queue
/// forever in the `async_channel` and a freshly-rebuilt index would
/// never be picked up.
pub(super) fn install_smart_shuffle_rebuild_result_consumer(
    receiver: Option<SmartShuffleRebuildResultReceiver>,
    runtime: SharedRuntime,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = receiver.recv().await {
            runtime
                .borrow_mut()
                .apply_smart_shuffle_rebuild_result(result);
        }
    });
}

/// Drains [`DeviceSyncEvent`](sustain_app_runtime::DeviceSyncEvent)s
/// posted by the background device-sync worker and feeds them into
/// [`ApplicationRuntime::apply_device_sync_event`], which updates the
/// progress notification and, on completion, persists the device
/// manifest and publishes the outcome.
pub(super) fn install_device_sync_event_consumer(
    receiver: Option<DeviceSyncEventReceiver>,
    runtime: SharedRuntime,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = receiver.recv().await {
            runtime.borrow_mut().apply_device_sync_event(event);
        }
    });
}

pub(super) fn install_device_plan_result_consumer(
    receiver: Option<DevicePlanResultReceiver>,
    runtime: SharedRuntime,
    device_panel: DeviceSyncPanel,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = receiver.recv().await {
            if runtime.borrow_mut().apply_device_plan_result(result) {
                device_panel.refresh_plan();
            }
        }
    });
}

/// Drains the asynchronous Android/MTP discovery channel. The slow
/// per-phone storage + identity-marker probe runs on a worker thread; each
/// resolved set lands here. When it actually changes the cached device set,
/// `render_devices` repaints the sidebar and device panel.
pub(super) fn install_mtp_discovery_consumer(
    receiver: Option<MtpDiscoveryResultReceiver>,
    runtime: SharedRuntime,
    render_devices: Rc<dyn Fn()>,
) {
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = receiver.recv().await {
            if runtime.borrow_mut().apply_mtp_discovery(result) {
                render_devices();
            }
        }
    });
}

/// Delay before the one-shot launch rebuild fires. A second is plenty
/// to clear the cold-start window (the 150 ms first-idle budget plus
/// margin) so the rebuild's main-thread prep — cloning the track list
/// and loading cached acoustics before the build hands off to a
/// background worker — never counts against startup. The user never
/// perceives the delay: the index is only consulted once Smart Shuffle
/// actually picks a track.
pub(super) const SMART_SHUFFLE_LAUNCH_REBUILD_DELAY: std::time::Duration =
    std::time::Duration::from_secs(1);

/// Request a single Smart Shuffle index rebuild shortly after launch.
/// Launch is one of the events that legitimately changes the index
/// since it was last persisted — the library may have been edited while
/// the app was closed, or analysis may have completed in a prior
/// session — so the index is refreshed once on start. The rebuild runs
/// on the background worker and the scheduler coalesces re-entrant
/// requests, so an unchanged library simply rebuilds an identical index
/// in milliseconds. Deferred past [`SMART_SHUFFLE_LAUNCH_REBUILD_DELAY`]
/// so it cannot regress the cold-start budget.
pub(super) fn install_smart_shuffle_launch_rebuild(runtime: &SharedRuntime) {
    let runtime = runtime.clone();
    glib::timeout_add_local_once(SMART_SHUFFLE_LAUNCH_REBUILD_DELAY, move || {
        runtime.borrow_mut().request_smart_shuffle_rebuild();
    });
}

/// Drains [`ArtworkFetchResult`](sustain_app_runtime::ArtworkFetchResult)s
/// posted by the background artwork fetcher.
///
/// On a successful fetch, the cache is invalidated, the freshly-
/// decoded bytes are primed into the loader's in-memory cache (so
/// the imminent now-playing refresh paints the new cover without
/// waiting for the async tag write), and a follow-up `SetArtwork`
/// command persists the bytes through the standard tag-writing
/// path. Failure modes surface a non-modal status-bar message.
/// Every outcome clears the now-playing tile's pending-fetch state
/// and triggers a `playback_changed` refresh so the tile and every
/// downstream view (Albums grid, track-table cover columns) settles
/// on the new visual state.
pub(super) struct ArtworkFetchResultConsumerContext {
    pub(super) receiver: Option<ArtworkFetchResultReceiver>,
    pub(super) runtime: SharedRuntime,
    pub(super) command_controller: SharedCommandController,
    pub(super) artwork_loader: ArtworkLoader,
    pub(super) now_playing: crate::now_playing::NowPlayingView,
    pub(super) playback_changed: PlaybackChangedCallback,
    pub(super) track_row_changed_holder: TrackRowChangedHolder,
}

pub(super) fn install_artwork_fetch_result_consumer(context: ArtworkFetchResultConsumerContext) {
    let ArtworkFetchResultConsumerContext {
        receiver,
        runtime,
        command_controller,
        artwork_loader,
        now_playing,
        playback_changed,
        track_row_changed_holder,
    } = context;
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = receiver.recv().await {
            use sustain_app_runtime::ArtworkFetchOutcome;
            let (severity, body) = match &result.outcome {
                ArtworkFetchOutcome::Fetched(bytes) => {
                    let queued = command_controller
                        .dispatch(sustain_app_runtime::ApplicationCommand::SetArtwork {
                            track_id: result.track_id,
                            artwork: Some(bytes.clone()),
                        })
                        .is_ok();
                    if queued
                        && let Some(source) = artwork_source_for_track(&runtime, result.track_id)
                    {
                        // Drop the existing in-memory + disk-cache
                        // entry, then prime the in-memory entry with
                        // the freshly fetched bytes. The disk-cache
                        // row is left dropped: the next miss after
                        // the metadata writer lands the tag write
                        // will rebuild it from the file, with the
                        // correct post-write fingerprint.
                        artwork_loader.invalidate(&source);
                        artwork_loader.prime(source, bytes.clone());
                    }
                    if queued {
                        (
                            sustain_app_runtime::NotificationSeverity::Info,
                            "Artwork fetched. Saving it to the audio file.".to_owned(),
                        )
                    } else {
                        (
                            sustain_app_runtime::NotificationSeverity::Error,
                            "The fetched artwork could not be queued for saving.".to_owned(),
                        )
                    }
                }
                ArtworkFetchOutcome::NoMatch => (
                    sustain_app_runtime::NotificationSeverity::Info,
                    "No cover art found for this track.".to_owned(),
                ),
                ArtworkFetchOutcome::Rejected => (
                    sustain_app_runtime::NotificationSeverity::Warning,
                    "The fetched cover art was rejected because it is unsupported, corrupt, or too large."
                        .to_owned(),
                ),
                ArtworkFetchOutcome::Failed => (
                    sustain_app_runtime::NotificationSeverity::Error,
                    "Could not fetch cover art.".to_owned(),
                ),
            };
            // The corresponding "Fetching artwork…" persistent is
            // dismissed by the now-playing tile (it owns the
            // persistent id it pushed). Here we only publish the
            // outcome ephemeral.
            now_playing.notify_artwork_fetch_complete(result.track_id);
            runtime.borrow_mut().push_ephemeral_notification(
                sustain_app_runtime::NotificationCategory::ArtworkFetch,
                severity,
                body,
            );
            playback_changed();
            if let Some(callback) = track_row_changed_holder.borrow().as_ref() {
                callback(result.track_id, TrackRowChangedKind::Artwork);
            }
        }
    });
}

/// Resolve the [`ArtworkSource`](crate::artwork_loader::ArtworkSource)
/// for a track in the current library. Returns `None` when the track
/// no longer exists (removed mid-flight) or no library root is
/// configured — both safe states for the caller to treat as
/// "nothing to invalidate".
fn artwork_source_for_track(
    runtime: &SharedRuntime,
    track_id: TrackId,
) -> Option<crate::artwork_loader::ArtworkSource> {
    let runtime = runtime.borrow();
    let track = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id)?;
    let absolute = runtime.absolute_track_path(track)?;
    let cache_path = track.location.path().to_path_buf();
    Some(crate::artwork_loader::ArtworkSource::embedded_track(
        cache_path, absolute,
    ))
}
