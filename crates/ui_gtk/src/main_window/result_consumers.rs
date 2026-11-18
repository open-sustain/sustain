// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Glib main-loop consumers for the runtime's background-worker output:
//! metadata-write results, analysis/online scheduler progress, per-track
//! refresh events, Smart Shuffle rebuilds, and artwork fetches. Each drains an
//! `async_channel` receiver (or installs a runtime observer) and applies the
//! result on the GTK main thread.

use std::collections::HashMap;

use sustain_app_runtime::{DeviceSyncEvent, TrackUpdated};
use sustain_i18n::gettext;

use crate::library_view_update::LibraryViewUpdate;

use super::*;

/// Maximum number of Songs rows appended during one idle dispatch. This keeps
/// GTK list-model notifications bounded so a large library hydrates over
/// multiple frames instead of monopolizing the main loop.
const INITIAL_LIBRARY_ROWS_BATCH_SIZE: usize = 128;

pub(super) struct LibraryHydrationResultConsumerContext {
    pub(super) receiver: LibraryHydrationResultReceiver,
    pub(super) runtime: SharedRuntime,
    pub(super) songs_table: TrackTable,
    pub(super) sidebar: PlaylistSidebar,
    pub(super) current_search_text: Rc<RefCell<String>>,
    pub(super) library_views_refresh: LibraryViewsRefreshCallback,
    pub(super) post_hydration_startup: PostHydrationStartup,
    /// The clock of the `activate:` startup landmarks, first idle among
    /// them, when profiling.
    pub(super) launch_profile: Option<sustain_profiler::ProfileScope>,
}

struct InitialLibraryRowsPublication {
    runtime: SharedRuntime,
    songs_table: TrackTable,
    sidebar: PlaylistSidebar,
    current_search_text: Rc<RefCell<String>>,
    library_views_refresh: LibraryViewsRefreshCallback,
    post_hydration_startup: PostHydrationStartup,
    launch_profile: Option<sustain_profiler::ProfileScope>,
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
        sidebar,
        current_search_text,
        library_views_refresh,
        post_hydration_startup,
        launch_profile,
    } = context;
    glib::MainContext::default().spawn_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        if !runtime.borrow_mut().apply_library_hydration_result(result) {
            if let Some(profile) = launch_profile {
                sustain_profiler::profile!(
                    "activate: library hydration failed at {:.1}ms",
                    profile.elapsed_ms()
                );
            }
            run_once(&post_hydration_startup);
            return;
        }
        if let Some(profile) = launch_profile {
            sustain_profiler::profile!(
                "activate: library snapshot adopted at {:.1}ms; publishing Songs rows",
                profile.elapsed_ms()
            );
        }
        publish_initial_library_rows(InitialLibraryRowsPublication {
            runtime,
            songs_table,
            sidebar,
            current_search_text,
            library_views_refresh,
            post_hydration_startup,
            launch_profile,
        });
    });
}

fn publish_initial_library_rows(publication: InitialLibraryRowsPublication) {
    let InitialLibraryRowsPublication {
        runtime,
        songs_table,
        sidebar,
        current_search_text,
        library_views_refresh,
        post_hydration_startup,
        launch_profile,
    } = publication;
    let hydration_profile = sustain_profiler::ProfileScope::start();
    let normalized_search = normalize_query(&current_search_text.borrow());
    let (total_tracks, mut rows) = {
        let runtime = runtime.borrow();
        let honor_sort_tags = runtime.settings().library.honor_sort_tags;
        let rows: Vec<_> = runtime
            .library_tracks()
            .iter()
            .filter(|track| {
                normalized_search.is_empty() || runtime.search_matches(track.id, &normalized_search)
            })
            .map(|track| TrackTableRow::from_track(track, honor_sort_tags))
            .collect();
        (runtime.library_tracks().len(), rows)
    };
    let visible_tracks = rows.len();
    // Ordered up front, the rows stream in final order: the first batch is
    // already the top of the sorted list, and nothing reshuffles as it fills.
    songs_table.order_rows(&mut rows);
    let generation = songs_table.begin_progressive_replace();
    let mut pending_rows = rows.into_iter();

    glib::idle_add_local(move || {
        let batch: Vec<_> = pending_rows
            .by_ref()
            .take(INITIAL_LIBRARY_ROWS_BATCH_SIZE)
            .collect();
        let still_current = songs_table.append_progressive_rows(generation, batch);
        if still_current && pending_rows.len() > 0 {
            return glib::ControlFlow::Continue;
        }
        // A header click while the rows streamed in sorted only the rows
        // published by then; settle the full set.
        if still_current {
            songs_table.sort_current_rows();
        }

        {
            let mut runtime = runtime.borrow_mut();
            runtime.finish_library_hydration_publication();
            crate::start_background_schedulers(&mut runtime);
        }
        sidebar.refresh();
        // Songs holds its rows; a view opened while the library loaded
        // still shows the empty library it found.
        library_views_refresh();
        run_once(&post_hydration_startup);
        // The library is usable from here on: Songs holds its rows in
        // order, and searching, editing and playing are open.
        if let (Some(launch), Some(publication)) = (launch_profile, hydration_profile) {
            let published = if still_current {
                format!("{visible_tracks} visible / {total_tracks} total Songs rows published")
            } else {
                "Songs rows superseded by a newer view".to_owned()
            };
            sustain_profiler::profile!(
                "activate: library usable at {:.1}ms ({published} in {:.1}ms)",
                launch.elapsed_ms(),
                publication.elapsed_ms()
            );
        }
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
    receiver: MetadataWriterEventReceiver,
    runtime: SharedRuntime,
    artwork_changed_holder: ArtworkChangedHolder,
    relocation_completed: crate::missing_track::MissingTrackRelocationCompletedCallback,
    artwork_loader: ArtworkLoader,
) {
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
                | sustain_app_runtime::MetadataWriterEvent::VerifiedDuplicateProgress { .. }
                | sustain_app_runtime::MetadataWriterEvent::VerifiedDuplicateConsolidation(_)
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
                | sustain_app_runtime::MetadataWriterEvent::VerifiedDuplicateProgress { .. }
                | sustain_app_runtime::MetadataWriterEvent::VerifiedDuplicateConsolidation(_)
                | sustain_app_runtime::MetadataWriterEvent::YoutubeAudioReplacement(_) => None,
            };
            let merges = event.committed_duplicate_merges().to_vec();
            // Captured before the runtime reloads the library: the removed
            // tracks' sources cannot be looked up afterwards.
            let duplicate_artwork_sources = merges
                .iter()
                .flat_map(|merge| {
                    std::iter::once(merge.survivor_id)
                        .chain(merge.removed_track_ids.iter().copied())
                })
                .filter_map(|track_id| artwork_source_for_track(&runtime, track_id))
                .collect::<Vec<_>>();
            runtime.borrow_mut().apply_metadata_writer_event(event);
            // Events that change the library reload it from SQLite, which
            // records the change for the views.
            // A completed artwork write changes no SQLite row, but the
            // artwork the views show comes from the file it rewrote.
            if let Some(track_id) = mirror_track_id
                && let Some(callback) = artwork_changed_holder.borrow().as_ref()
            {
                callback(&[track_id]);
            }
            if let Some((track_id, succeeded)) = relocation {
                relocation_completed(track_id, succeeded);
            }
            for source in duplicate_artwork_sources {
                artwork_loader.invalidate(&source);
            }
        }
    });
}

pub(super) fn install_youtube_audio_download_result_consumer(
    receiver: YoutubeAudioDownloadResultReceiver,
    runtime: SharedRuntime,
) {
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
    receiver: AnalysisProgressReceiver,
    runtime: SharedRuntime,
) {
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
    receiver: OnlineProgressReceiver,
    runtime: SharedRuntime,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(progress) = receiver.recv().await {
            runtime.borrow_mut().apply_online_progress(progress);
        }
    });
}

/// Wires the runtime's committed library changes to the views. The
/// runtime wakes the observer when changes start waiting; the views take
/// every waiting change on the next idle, outside the runtime borrow the
/// wake fires under, and apply them as one batch. A change committed while
/// the batch applies wakes the observer again for the next one.
pub(super) fn install_library_changes_observer(
    runtime: &SharedRuntime,
    apply: crate::LibraryViewUpdateCallback,
) {
    let weak_runtime = Rc::downgrade(runtime);
    runtime
        .borrow_mut()
        .set_library_changes_observer(Box::new(move || {
            let weak_runtime = weak_runtime.clone();
            let apply = apply.clone();
            glib::idle_add_local_once(move || {
                let Some(runtime) = weak_runtime.upgrade() else {
                    return;
                };
                let changes = runtime.borrow_mut().take_library_changes();
                if !changes.is_empty() {
                    apply(&LibraryViewUpdate::for_changes(&changes));
                }
            });
        }));
}

/// Drains the tracks the analysis and online schedulers committed changes
/// to. Each is fed into
/// [`ApplicationRuntime::apply_track_updated`], which reloads the row
/// from the library store and records what changed for the views.
pub(super) fn install_track_updated_consumer(
    receiver: TrackUpdatedReceiver,
    runtime: SharedRuntime,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(first) = receiver.recv().await {
            // Coalesce a burst before touching the store: a full analysis
            // or online sweep posts tracks faster than the main loop drains
            // them, and one track commonly arrives several times in quick
            // succession (bpm, then key, then audio). Read each track back
            // at most once per drain, keeping whether any announcement of
            // it changed its analysis.
            let mut pending: HashMap<TrackId, bool> =
                HashMap::from([(first.track_id, first.analysis)]);
            while let Ok(next) = receiver.try_recv() {
                *pending.entry(next.track_id).or_default() |= next.analysis;
            }
            let mut runtime = runtime.borrow_mut();
            for (track_id, analysis) in pending {
                runtime.apply_track_updated(TrackUpdated { track_id, analysis });
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
    receiver: SmartShuffleRebuildResultReceiver,
    runtime: SharedRuntime,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = receiver.recv().await {
            runtime
                .borrow_mut()
                .apply_smart_shuffle_rebuild_result(result);
        }
    });
}

/// Drains [`DeviceSyncEvent`]s
/// posted by the background device-sync worker and feeds them into
/// [`ApplicationRuntime::apply_device_sync_event`], which updates the
/// progress notification and, on completion, persists the device
/// manifest and publishes the outcome. A run starting or ending turns the
/// synced device's sidebar badge on or off; a run ending has the device
/// panel plan the drive it changed afresh.
pub(super) fn install_device_sync_event_consumer(
    receiver: DeviceSyncEventReceiver,
    runtime: SharedRuntime,
    sidebar: PlaylistSidebar,
    device_panel: DeviceSyncPanel,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(event) = receiver.recv().await {
            let starts_or_ends = !matches!(event, DeviceSyncEvent::Progress { .. });
            let ends = matches!(event, DeviceSyncEvent::Finished { .. });
            runtime.borrow_mut().apply_device_sync_event(event);
            if starts_or_ends {
                sidebar.refresh_transfer_badges();
            }
            if ends {
                device_panel.replan();
            }
        }
    });
}

pub(super) fn install_device_plan_result_consumer(
    receiver: DevicePlanResultReceiver,
    runtime: SharedRuntime,
    device_panel: DeviceSyncPanel,
) {
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
    receiver: MtpDiscoveryResultReceiver,
    runtime: SharedRuntime,
    render_devices: Rc<dyn Fn()>,
) {
    glib::MainContext::default().spawn_local(async move {
        while let Ok(result) = receiver.recv().await {
            if runtime.borrow_mut().apply_mtp_discovery(result) {
                render_devices();
            }
        }
    });
}

/// Delay before the one-shot launch rebuild fires. A second is plenty
/// to clear the cold-start window (the 225 ms first-idle budget plus
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
    pub(super) receiver: ArtworkFetchResultReceiver,
    pub(super) runtime: SharedRuntime,
    pub(super) command_controller: SharedCommandController,
    pub(super) artwork_loader: ArtworkLoader,
    pub(super) now_playing: crate::now_playing::NowPlayingView,
    pub(super) playback_changed: PlaybackChangedCallback,
    pub(super) artwork_changed_holder: ArtworkChangedHolder,
}

pub(super) fn install_artwork_fetch_result_consumer(context: ArtworkFetchResultConsumerContext) {
    let ArtworkFetchResultConsumerContext {
        receiver,
        runtime,
        command_controller,
        artwork_loader,
        now_playing,
        playback_changed,
        artwork_changed_holder,
    } = context;
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
                        // Replace the in-memory entry with the freshly
                        // fetched bytes. Priming drops the disk-cache
                        // row: the next miss after the metadata writer
                        // lands the tag write rebuilds it from the
                        // file, with the correct post-write fingerprint.
                        artwork_loader.prime(source, bytes.clone());
                    }
                    if queued {
                        (
                            sustain_app_runtime::NotificationSeverity::Info,
                            gettext("Artwork fetched. Saving it to the audio file."),
                        )
                    } else {
                        (
                            sustain_app_runtime::NotificationSeverity::Error,
                            gettext("The fetched artwork could not be queued for saving."),
                        )
                    }
                }
                ArtworkFetchOutcome::NoMatch => (
                    sustain_app_runtime::NotificationSeverity::Info,
                    gettext("No cover art found for this track."),
                ),
                ArtworkFetchOutcome::Rejected => (
                    sustain_app_runtime::NotificationSeverity::Warning,
                    gettext("The fetched cover art was rejected because it is unsupported, corrupt, or too large."),
                ),
                ArtworkFetchOutcome::Failed => (
                    sustain_app_runtime::NotificationSeverity::Error,
                    gettext("Could not fetch cover art."),
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
            if let Some(callback) = artwork_changed_holder.borrow().as_ref() {
                callback(&[result.track_id]);
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
    let track = runtime.library_track(track_id)?;
    let absolute = runtime.absolute_track_path(track)?;
    let cache_path = track.location.path().to_path_buf();
    Some(crate::artwork_loader::ArtworkSource::embedded_track(
        cache_path, absolute,
    ))
}
