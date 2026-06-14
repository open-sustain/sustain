// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gio, glib};
use sustain_app_runtime::{
    CdImportProgress, CdImportRequest, CdImportResult, DeviceTarget, MetadataChange,
    PlaybackCommand, PlaybackEvent, PlaybackQueueRequest, PlaybackQueueSource, PlaybackState,
    Playlist, PlaylistEntry, PlaylistFolder, PlaylistFolderId, PlaylistItem, Rating, ShuffleMode,
    TocSnapshot, Track, TrackColumnLayout, TrackColumnLayoutScope, TrackId, UiSettings,
    UiSidebarSelection, normalize_query,
};

use super::{
    ALBUMS_VIEW, APP_ID, AnalysisProgressReceiver, ApplicationCommand, ApplicationRuntime,
    ApplicationRuntimeError, ArtworkFetchResultReceiver, AvailabilityChangedCallback,
    CD_IMPORT_VIEW, CdLookupEventReceiver, ConnectedDevice, DEVICES_VIEW, DUPLICATES_VIEW,
    DevicePlanResultReceiver, DeviceSyncEventReceiver, LibraryChangedCallback,
    LibraryChangedHolder, LibraryHydrationResultReceiver, MetadataWriterEventReceiver,
    MprisCommandReceiver, MtpDiscoveryResultReceiver, OnlineProgressReceiver,
    OpticalDiscoveryResultReceiver, PLAYLISTS_VIEW, PlaybackChangedCallback, SIDEBAR_DEFAULT_WIDTH,
    SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH, SONGS_VIEW, STATISTICS_VIEW, SharedMprisService,
    SharedRuntime, ShowAlbumAction, ShowAlbumHolder, SmartPlaylistTrackStatus,
    SmartShuffleRebuildResultReceiver, TrackRowChangedCallback, TrackRowChangedHolder,
    TrackRowChangedKind, TrackUpdatedReceiver, YoutubeAudioDownloadResultReceiver,
    accent::install_accent_css,
    albums::AlbumsView,
    app_css::install_app_css,
    artwork_loader::ArtworkLoader,
    cd_import_panel::{CdImportPanel, CdImportRequestedCallback},
    command_controller::{SharedCommandController, UiCommandController},
    content_stack::build_content_stack,
    device_panel::DeviceSyncPanel,
    duplicate_consolidation::consolidate_duplicates_callback,
    duplicates::DuplicatesView,
    library_consolidation::{
        library_consolidation_requested_callback, maybe_auto_resume_library_consolidation,
    },
    library_import::{
        LIBRARY_DROP_INDICATOR_CLASS, LibraryImportCompletedCallback, install_file_drop_target,
        library_import_requested_callback,
    },
    library_scan::library_scan_requested_callback,
    missing_track::{
        LocateMissingTrackCallback, PendingLocatedPlaybacks, locate_missing_track_callback,
        missing_track_relocation_completed_callback, play_track_or_offer_locate,
    },
    now_playing::NowPlayingView,
    playlists_header::{PlaylistsHeader, PlaylistsHeaderState},
    preferences::{ViewSettingsChangedCallback, install_preferences_action, settings_button},
    queue_view::QueueView,
    shortcuts::{
        GlobalShortcutContext, create_new_playlist, install_global_shortcuts,
        open_new_smart_playlist_editor,
    },
    sidebar::{PlaylistSidebar, SidebarDeviceEntry, SidebarSelection, build_content_area},
    sidebar_context::{
        NEW_PLAYLIST_FOLDER_DEFAULT_NAME, SidebarActionCallback, SidebarContextAction,
        SidebarContextMenu, unique_default_name,
    },
    smart_playlist_editor::{SmartPlaylistEditorMode, open_smart_playlist_editor},
    statistics::StatisticsView,
    status_bar::StatusBar,
    titlebar::{
        Titlebar, build_titlebar, connect_titlebar_play_button, connect_titlebar_playback_controls,
        connect_titlebar_search, sync_play_pause_icon,
    },
    track_context::{
        AddToPlaylistCallback, AddToPlaylistEntry, AddToPlaylistProvider, TrackActionCallback,
        TrackActionInvocation, TrackActionVisibility, TrackAnalyzeEnabledQuery,
        TrackAnalyzeRunCallback, TrackContextAction, TrackContextActionSet,
        TrackContextInvocationState, TrackRetrieveBusyQuery, TrackRetrieveRunCallback,
        TrackRowContextMenu,
    },
    track_context_ops::{
        add_to_queue_callback, copy_files_callback, get_info_callback, play_next_callback,
        playback_has_current_track_visibility, show_album_callback, show_in_folder_callback,
        track_has_album_visibility,
    },
    track_table::{
        EditableField, InlineEditHooks, RatingChangedCallback, RowDropPosition, RowReorderCallback,
        RowReorderDrop, TrackActivatedCallback, TrackTable, TrackTableRow, build_track_table,
    },
    window_chrome::{install_resize_handles, install_window_state_chrome},
    youtube_audio_replacement::{
        youtube_audio_replacement_callback, youtube_audio_replacement_visibility,
    },
};

mod mpris_bridge;
mod playback;
mod playlists;
mod result_consumers;
mod search;
mod sidebar_callbacks;
mod sidebar_collapse;
mod track_callbacks;

pub(crate) use sidebar_collapse::SidebarCollapseController;

use mpris_bridge::{install_mpris_command_consumer, now_playing_to_mpris_metadata};
use playback::{
    install_playback_event_callback, install_playlists_header_playback,
    library_track_activated_callback, make_toggle_or_start_playback, playback_changed_callback,
    playlist_track_activated_callback, repopulate_request_for_visible_view,
    update_play_pause_sensitivity,
};
use playlists::{
    PlaylistsViewRefreshContext, add_to_playlist_callback, add_to_playlist_provider,
    install_playlists_view_activator, playlist_table_rows_for, refresh_playlists_view_if_visible,
};
use result_consumers::{
    ArtworkFetchResultConsumerContext, LibraryHydrationResultConsumerContext,
    install_analysis_progress_consumer, install_artwork_fetch_result_consumer,
    install_device_plan_result_consumer, install_device_sync_event_consumer,
    install_library_hydration_result_consumer, install_metadata_writer_event_consumer,
    install_mtp_discovery_consumer, install_online_progress_consumer,
    install_smart_shuffle_launch_rebuild, install_smart_shuffle_rebuild_result_consumer,
    install_track_data_observer, install_track_updated_consumer,
    install_youtube_audio_download_result_consumer,
};
use search::{SearchWiringContext, install_search_wiring};
use sidebar_callbacks::{
    sidebar_action_callback, sidebar_analysis_enabled_query, sidebar_analysis_run_callback,
    sidebar_delete_callback, sidebar_edit_smart_playlist_callback, sidebar_move_callback,
    sidebar_online_busy_query, sidebar_online_run_callback, sidebar_rename_callback,
    sidebar_selection_changed_callback, sidebar_tracks_drop_callback,
};
use track_callbacks::{
    TrackRowChangedContext, inline_edit_hooks, playlist_row_reorder_callback,
    playlist_track_context_actions, rating_changed_callback, track_context_actions,
    track_row_changed_callback,
};

/// Recompute the status-bar summary (track count, total duration) for
/// whichever view is currently visible. Fired after sidebar-driven
/// view switches, library mutations, and search keystrokes.
pub(crate) type VisibleSummaryRefreshCallback = Rc<dyn Fn()>;
type PostHydrationStartup = Rc<RefCell<Option<Box<dyn FnOnce()>>>>;

/// Channel receivers the main window installs as glib consumers on
/// the GTK main loop. Bundled into a struct rather than passed as
/// individual `build_main_window` parameters so the function signature
/// stays under clippy's argument-count threshold and so adding the
/// next background worker is a one-line struct extension instead of
/// touching every call site.
pub(crate) struct MainWindowAsyncReceivers {
    pub mpris_command_rx: Option<MprisCommandReceiver>,
    pub metadata_writer_event_rx: Option<MetadataWriterEventReceiver>,
    pub artwork_fetch_result_rx: Option<ArtworkFetchResultReceiver>,
    pub youtube_audio_download_result_rx: Option<YoutubeAudioDownloadResultReceiver>,
    pub analysis_progress_rx: Option<AnalysisProgressReceiver>,
    pub online_progress_rx: Option<OnlineProgressReceiver>,
    pub track_updated_rx: Option<TrackUpdatedReceiver>,
    pub smart_shuffle_rebuild_result_rx: Option<SmartShuffleRebuildResultReceiver>,
    pub device_sync_event_rx: Option<DeviceSyncEventReceiver>,
    pub device_plan_result_rx: Option<DevicePlanResultReceiver>,
    pub mtp_discovery_rx: Option<MtpDiscoveryResultReceiver>,
    pub optical_discovery_rx: Option<OpticalDiscoveryResultReceiver>,
    pub cd_lookup_rx: Option<CdLookupEventReceiver>,
    pub library_hydration_result_rx: Option<LibraryHydrationResultReceiver>,
}

pub(crate) fn build_main_window(
    app: &gtk::Application,
    runtime: SharedRuntime,
    mpris_service: Option<SharedMprisService>,
    artwork_cache_dir: PathBuf,
    database_path: PathBuf,
    receivers: MainWindowAsyncReceivers,
) -> BuiltMainWindow {
    let MainWindowAsyncReceivers {
        mpris_command_rx,
        metadata_writer_event_rx,
        artwork_fetch_result_rx,
        youtube_audio_download_result_rx,
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
    } = receivers;
    let window_profile = sustain_profiler::ProfileScope::start();
    sustain_profiler::profile_mark!(window_profile, "build_main_window entered");
    // Coarse timing landmarks live in this function (and in `main` /
    // `ui_gtk::run`) so a launch regression is visible when profiling
    // is enabled. Keep them sparse: only phases that can plausibly grow
    // with library size or new features warrant a print. Per-callback
    // timings inside hot paths are intentionally absent.
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .decorated(false)
        .title("Sustain")
        .default_width(1100)
        .default_height(720)
        .build();
    window.add_css_class("app-window");
    window.set_resizable(true);
    install_app_icon();
    window.set_icon_name(Some(APP_ID));
    install_app_css();
    install_accent_css();

    let initial_ui_settings = runtime.borrow().settings().ui.clone();
    let initial_search_text = initial_ui_settings.search_text.trim().to_owned();

    // Shared current-search-text state. Captured by all view-rebuild paths
    // and persisted on normal shutdown with the rest of the UI session.
    let current_search_text: Rc<RefCell<String>> =
        Rc::new(RefCell::new(initial_search_text.clone()));

    // Library hydration is always deferred in the app path (`main.rs`
    // hydrates synchronously only for the force-backfill CLI command,
    // which never reaches the window). The library is therefore empty
    // while this function runs: the songs table and the status bar start
    // from explicitly empty rows — like the playlists table below — and
    // the hydration-complete consumer populates them.
    sustain_profiler::profile_mark!(
        window_profile,
        "library rows materialized (0 rows; hydration deferred)"
    );
    let status_bar = {
        let runtime_for_cancel = runtime.clone();
        StatusBar::new(
            &[],
            Rc::new(move || {
                runtime_for_cancel
                    .borrow()
                    .request_background_task_cancellation();
            }),
        )
    };
    let command_controller: SharedCommandController =
        Rc::new(UiCommandController::new(runtime.clone()));
    // Wire the lane to observe runtime notifications before any
    // callback can push a notification — otherwise an early ephemeral
    // would land in the queue without a renderer attached.
    status_bar.attach_to_runtime(&runtime);

    let songs_table_holder: Rc<RefCell<Option<TrackTable>>> = Rc::new(RefCell::new(None));
    let albums_view_holder: Rc<RefCell<Option<AlbumsView>>> = Rc::new(RefCell::new(None));
    let playlists_table_holder: Rc<RefCell<Option<TrackTable>>> = Rc::new(RefCell::new(None));
    // The queue popover is built after the titlebar (it parents itself to
    // the Next button), but the playback-changed callback is built before
    // it; the holder lets that callback reach the queue once it exists so
    // skipping a track refreshes an open queue live.
    let queue_view_holder: Rc<RefCell<Option<QueueView>>> = Rc::new(RefCell::new(None));

    // One artwork loader for the whole window. Sharing it across views
    // means the on-disk cache, in-memory cache, and worker pool are all
    // single-instance — a track resolved by the Albums grid is
    // immediately available to the now-playing tile and vice versa.
    // Construction launches worker threads, so do it once after the
    // metadata service is installed and before any view subscribes.
    let metadata_service = runtime
        .borrow()
        .metadata_service()
        .expect("metadata service must be installed before building the main window");
    let artwork_loader = ArtworkLoader::new(metadata_service, artwork_cache_dir);

    let now_playing = NowPlayingView::new(
        &window,
        runtime.clone(),
        command_controller.clone(),
        artwork_loader.clone(),
    );
    let initial_volume = runtime.borrow().settings().playback.volume;
    let titlebar = build_titlebar(now_playing.widget(), initial_volume);
    titlebar.set_search_text(&initial_search_text);
    let playback_changed = playback_changed_callback(
        &runtime,
        &now_playing,
        &titlebar,
        songs_table_holder.clone(),
        albums_view_holder.clone(),
        playlists_table_holder.clone(),
        queue_view_holder.clone(),
        mpris_service.clone(),
    );
    connect_titlebar_playback_controls(
        &titlebar,
        &runtime,
        command_controller.clone(),
        playback_changed.clone(),
    );
    install_playback_event_callback(&runtime, &command_controller, &playback_changed);
    install_mpris_command_consumer(
        mpris_command_rx,
        command_controller.clone(),
        playback_changed.clone(),
        app.clone(),
        window.clone(),
    );

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("app-shell");
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.set_overflow(gtk::Overflow::Hidden);

    let sidebar = PlaylistSidebar::new(
        runtime.clone(),
        initial_ui_settings.playlists_section_collapsed,
    );
    let sidebar_widget = sidebar.widget();

    let library_changed_holder: LibraryChangedHolder = Rc::new(RefCell::new(None));
    let track_row_changed_holder: TrackRowChangedHolder = Rc::new(RefCell::new(None));
    let parent_window = window.clone().upcast::<gtk::Window>();
    let pending_located_playbacks: PendingLocatedPlaybacks = Rc::new(RefCell::new(HashMap::new()));
    let missing_track_relocation_completed = missing_track_relocation_completed_callback(
        &command_controller,
        &pending_located_playbacks,
        playback_changed.clone(),
        &artwork_loader,
    );
    let locate_missing_track = locate_missing_track_callback(
        &parent_window,
        &command_controller,
        &pending_located_playbacks,
        &missing_track_relocation_completed,
        &artwork_loader,
    );
    let library_track_activated = library_track_activated_callback(
        &command_controller,
        &runtime,
        playback_changed.clone(),
        &current_search_text,
        &locate_missing_track,
    );
    let show_album_holder: ShowAlbumHolder = Rc::new(RefCell::new(None));
    let track_context_invocation = TrackContextInvocationState::default();
    let context_actions = track_context_actions(
        &runtime,
        &parent_window,
        &show_album_holder,
        &command_controller,
        playback_changed.clone(),
        library_changed_holder.clone(),
        track_row_changed_holder.clone(),
        &artwork_loader,
    );
    let add_to_playlist_provider = add_to_playlist_provider(&runtime);
    let add_to_playlist_callback =
        add_to_playlist_callback(&command_controller, &runtime, &library_changed_holder);
    let context_menu = TrackRowContextMenu::new(
        context_actions,
        parent_window.clone(),
        track_context_invocation.clone(),
    )
    .with_add_to_playlist(
        add_to_playlist_provider.clone(),
        add_to_playlist_callback.clone(),
    )
    .with_analyze_menu(
        track_analyze_run_callback(&runtime),
        analysis_enabled_query(&runtime),
    )
    .with_retrieve_menu(
        track_retrieve_run_callback(&runtime),
        online_busy_query(&runtime),
        youtube_audio_replacement_callback(&parent_window, &command_controller),
        youtube_audio_replacement_visibility(&runtime),
    );
    let playlist_context_actions = playlist_track_context_actions(
        &runtime,
        &parent_window,
        &show_album_holder,
        &command_controller,
        playback_changed.clone(),
        library_changed_holder.clone(),
        track_row_changed_holder.clone(),
        &artwork_loader,
        &sidebar,
    );
    let playlist_context_menu = TrackRowContextMenu::new(
        playlist_context_actions,
        parent_window.clone(),
        track_context_invocation.clone(),
    )
    .with_add_to_playlist(add_to_playlist_provider, add_to_playlist_callback)
    .with_analyze_menu(
        track_analyze_run_callback(&runtime),
        analysis_enabled_query(&runtime),
    )
    .with_retrieve_menu(
        track_retrieve_run_callback(&runtime),
        online_busy_query(&runtime),
        youtube_audio_replacement_callback(&parent_window, &command_controller),
        youtube_audio_replacement_visibility(&runtime),
    );
    let rating_changed =
        rating_changed_callback(&command_controller, track_row_changed_holder.clone());
    let songs_inline_edit = inline_edit_hooks(
        &runtime,
        &command_controller,
        track_row_changed_holder.clone(),
    );
    let songs_table = build_track_table(
        Vec::new(),
        Some(library_track_activated.clone()),
        Some(context_menu.clone()),
        Some(rating_changed.clone()),
        None,
        Some(songs_inline_edit),
    );
    sustain_profiler::profile_mark!(
        window_profile,
        "Songs table populated (0 rows; hydration deferred)"
    );
    songs_table_holder.replace(Some(songs_table.clone()));
    let albums_view = AlbumsView::new(
        runtime.clone(),
        command_controller.clone(),
        playback_changed.clone(),
        context_menu.clone(),
        locate_missing_track.clone(),
        artwork_loader.clone(),
    );
    albums_view_holder.replace(Some(albums_view.clone()));
    let duplicates_view = DuplicatesView::new(
        runtime.clone(),
        context_menu,
        library_track_activated.clone(),
        rating_changed.clone(),
        inline_edit_hooks(
            &runtime,
            &command_controller,
            track_row_changed_holder.clone(),
        ),
    );
    let playlist_row_reorder = playlist_row_reorder_callback(
        &command_controller,
        &runtime,
        &sidebar,
        &playlists_table_holder,
        &current_search_text,
    );
    let playlist_track_activated = playlist_track_activated_callback(
        &command_controller,
        &runtime,
        &sidebar,
        playback_changed.clone(),
        &current_search_text,
        &locate_missing_track,
    );
    // Inline editing in the Playlists view (regular and smart playlists
    // share this table). A separate hooks instance from the Songs table's
    // because each `InlineEditController` owns its table's open-edit state.
    // Edits route through the same `UpdateMetadata` + `track_row_changed`
    // path: for a smart playlist whose membership the edit changes, the
    // callback reflows the view; otherwise it refreshes the row in place.
    // The drag-to-reorder handle lives on the status column, so it does not
    // contend with the text columns' edit gesture (#158).
    let playlists_inline_edit = inline_edit_hooks(
        &runtime,
        &command_controller,
        track_row_changed_holder.clone(),
    );
    let playlists_table = build_track_table(
        Vec::new(),
        Some(playlist_track_activated),
        Some(playlist_context_menu),
        Some(rating_changed),
        Some(playlist_row_reorder),
        Some(playlists_inline_edit),
    );
    playlists_table_holder.replace(Some(playlists_table.clone()));
    install_track_column_layout_persistence(&runtime, &songs_table, &playlists_table, &sidebar);
    playback_changed();
    sustain_profiler::profile_mark!(window_profile, "tables/playback wired");
    let playlists_header = PlaylistsHeader::new();
    let playlists_view = gtk::Box::new(gtk::Orientation::Vertical, 0);
    playlists_view.set_hexpand(true);
    playlists_view.set_vexpand(true);
    playlists_view.append(playlists_header.widget());
    playlists_view.append(&playlists_table.widget());
    install_playlists_header_playback(
        &playlists_header,
        &command_controller,
        &runtime,
        &sidebar,
        &current_search_text,
        &playback_changed,
    );
    let songs_drop_indicator = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    songs_drop_indicator.add_css_class(LIBRARY_DROP_INDICATOR_CLASS);
    songs_drop_indicator.set_can_target(false);
    songs_drop_indicator.set_hexpand(true);
    songs_drop_indicator.set_vexpand(true);

    let songs_drop_overlay = gtk::Overlay::new();
    songs_drop_overlay.set_hexpand(true);
    songs_drop_overlay.set_vexpand(true);
    songs_drop_overlay.set_child(Some(&songs_table.widget()));
    songs_drop_overlay.add_overlay(&songs_drop_indicator);

    let device_panel = DeviceSyncPanel::new(runtime.clone(), command_controller.clone());
    let cd_import_panel = CdImportPanel::new(runtime.clone());
    let statistics_view = StatisticsView::new(runtime.clone());
    let content_stack = build_content_stack(
        &songs_drop_overlay,
        &albums_view.widget(),
        &duplicates_view.widget(),
        &statistics_view.widget(),
        &playlists_view,
        device_panel.widget(),
        cd_import_panel.widget(),
    );
    install_albums_view_activator(&content_stack, &albums_view);
    install_duplicates_view(&content_stack, &sidebar, &duplicates_view);
    install_statistics_view_activator(&content_stack, &statistics_view);
    install_devices_section(DevicesSectionContext {
        content_stack: &content_stack,
        sidebar: &sidebar,
        device_panel: &device_panel,
        cd_import_panel: &cd_import_panel,
        runtime: &runtime,
        mtp_discovery_rx,
        optical_discovery_rx,
        cd_lookup_rx,
    });
    // The playlists table is built empty. It only needs to be populated
    // when the user actually opens the Playlists view; rebuilding it on
    // every library_changed / selection change while Songs is visible
    // is wasted work and dominates startup time on large libraries
    // (measured: ~672ms for 8890 rows in `replace_rows`).
    let playlists_dirty: Rc<Cell<bool>> = Rc::new(Cell::new(true));
    let playlists_refresh = PlaylistsViewRefreshContext::new(
        &content_stack,
        &playlists_table,
        &playlists_header,
        &status_bar,
        &playlists_dirty,
    );
    install_playlists_view_activator(&playlists_refresh, &runtime, &sidebar, &current_search_text);
    sustain_profiler::profile_mark!(window_profile, "content stack + activators installed");
    // The Play button's behaviour depends on the visible view, which now
    // exists. One shared closure drives both the button and the Space
    // shortcut so the two surfaces never diverge.
    let toggle_or_start_playback = make_toggle_or_start_playback(
        &command_controller,
        &runtime,
        &content_stack,
        &albums_view,
        &sidebar,
        &current_search_text,
        &playback_changed,
    );
    connect_titlebar_play_button(&titlebar, toggle_or_start_playback.clone());
    let visible_summary_refresh = visible_summary_refresh_callback(
        &runtime,
        &content_stack,
        &status_bar,
        &playlists_table,
        &current_search_text,
        device_panel.current_device_cell(),
    );
    device_panel.set_summary_refresh(visible_summary_refresh.clone());
    let library_changed = library_changed_callback(
        &runtime,
        &songs_table,
        &albums_view,
        &duplicates_view,
        &statistics_view,
        &sidebar,
        &titlebar,
        visible_summary_refresh.clone(),
        &current_search_text,
    );
    install_cd_import_requested(&cd_import_panel, &runtime, library_changed.clone());
    install_track_availability_observer(&runtime, &songs_table, &playlists_table);
    let track_row_changed = track_row_changed_callback(TrackRowChangedContext {
        runtime: &runtime,
        songs_table: &songs_table,
        albums_view: &albums_view,
        duplicates_view: &duplicates_view,
        playlists_table: &playlists_table,
        playlists_refresh: &playlists_refresh,
        sidebar: &sidebar,
        visible_summary_refresh: visible_summary_refresh.clone(),
        current_search_text: &current_search_text,
        device_panel: &device_panel,
    });
    track_row_changed_holder.replace(Some(track_row_changed));
    install_metadata_writer_event_consumer(
        metadata_writer_event_rx,
        runtime.clone(),
        track_row_changed_holder.clone(),
        library_changed_holder.clone(),
        missing_track_relocation_completed,
        artwork_loader.clone(),
    );
    install_artwork_fetch_result_consumer(ArtworkFetchResultConsumerContext {
        receiver: artwork_fetch_result_rx,
        runtime: runtime.clone(),
        command_controller: command_controller.clone(),
        artwork_loader: artwork_loader.clone(),
        now_playing: now_playing.clone(),
        playback_changed: playback_changed.clone(),
        track_row_changed_holder: track_row_changed_holder.clone(),
    });
    install_youtube_audio_download_result_consumer(
        youtube_audio_download_result_rx,
        runtime.clone(),
    );
    install_analysis_progress_consumer(analysis_progress_rx, runtime.clone());
    install_online_progress_consumer(online_progress_rx, runtime.clone());
    install_track_data_observer(&runtime, track_row_changed_holder.clone());
    install_track_updated_consumer(track_updated_rx, runtime.clone());
    install_smart_shuffle_rebuild_result_consumer(smart_shuffle_rebuild_result_rx, runtime.clone());
    install_device_plan_result_consumer(
        device_plan_result_rx,
        runtime.clone(),
        device_panel.clone(),
    );
    install_device_sync_event_consumer(device_sync_event_rx, runtime.clone());
    // The sidebar is now the sole navigation surface: its selection
    // chooses which content-stack page is visible (Music → SONGS_VIEW,
    // Albums → ALBUMS_VIEW, an Item → PLAYLISTS_VIEW). The non-default
    // selections are applied AFTER first-frame by
    // [`DeferredStartup`] so the cold-start budget covers only the
    // cheap Music page.
    sidebar.set_selection_changed(sidebar_selection_changed_callback(
        &runtime,
        &playlists_table,
        &playlists_refresh,
        &content_stack,
        visible_summary_refresh.clone(),
        &current_search_text,
    ));
    let device_populate: Box<dyn FnOnce()> = {
        let sidebar = sidebar.clone();
        let runtime = runtime.clone();
        Box::new(move || {
            // Kick the async Android/MTP and optical-disc probes so a phone
            // or audio CD already present at launch appears once the workers
            // resolve (their discovery consumers re-render); show block
            // devices immediately. Both probes run only after this
            // first-idle landmark, so they never count against cold start.
            runtime.borrow_mut().refresh_mtp_devices();
            runtime.borrow_mut().refresh_optical_discs();
            sidebar.set_devices(&device_entries(&runtime));
        })
    };
    let resume_pending_metadata_writes: Box<dyn FnOnce()> = {
        let runtime = runtime.clone();
        Box::new(move || runtime.borrow().resume_pending_metadata_writes())
    };
    let initialize_queue_view: Box<dyn FnOnce()> = {
        let runtime = runtime.clone();
        let command_controller = command_controller.clone();
        let artwork_loader = artwork_loader.clone();
        let playback_changed = playback_changed.clone();
        let next_button = titlebar.next_button();
        let queue_view_holder = queue_view_holder.clone();
        Box::new(move || {
            // The queue popover is hidden behind a secondary click on the Next
            // button. Construct it after the first-idle landmark so startup
            // does not realize its list factory before the first paint.
            let queue_view = QueueView::new(
                runtime,
                command_controller,
                artwork_loader,
                playback_changed,
                &next_button,
            );
            queue_view_holder.replace(Some(queue_view));
        })
    };
    let consolidation_requested = library_consolidation_requested_callback(&runtime);
    let deferred_startup = DeferredStartup::new(
        initial_ui_settings.sidebar_selection,
        sidebar.clone(),
        initialize_queue_view,
        device_populate,
        resume_pending_metadata_writes,
        runtime.clone(),
        consolidation_requested.clone(),
    );
    install_library_hydration_result_consumer(LibraryHydrationResultConsumerContext {
        receiver: library_hydration_result_rx,
        runtime: runtime.clone(),
        songs_table: songs_table.clone(),
        status_bar: status_bar.clone(),
        titlebar: titlebar.clone(),
        sidebar: sidebar.clone(),
        content_stack: content_stack.clone(),
        current_search_text: current_search_text.clone(),
        post_hydration_startup: deferred_startup.post_hydration_startup(),
    });
    install_search_wiring(
        &titlebar,
        SearchWiringContext {
            current_search_text: current_search_text.clone(),
            command_controller: command_controller.clone(),
            runtime: runtime.clone(),
            songs_table: songs_table.clone(),
            albums_view: albums_view.clone(),
            playlists_refresh: playlists_refresh.clone(),
            sidebar: sidebar.clone(),
            content_stack: content_stack.clone(),
            status_bar: status_bar.clone(),
            visible_summary_refresh: visible_summary_refresh.clone(),
        },
    );
    sidebar.install_context_menu(SidebarContextMenu::new(sidebar_action_callback(
        &window,
        &command_controller,
        &runtime,
        &sidebar,
    )));
    sidebar.set_move_callback(sidebar_move_callback(
        &command_controller,
        &runtime,
        &sidebar,
    ));
    sidebar.set_rename_callback(sidebar_rename_callback(
        &command_controller,
        &runtime,
        &sidebar,
    ));
    sidebar.set_delete_callback(sidebar_delete_callback(&command_controller, &sidebar));
    sidebar.set_tracks_drop_callback(sidebar_tracks_drop_callback(
        &command_controller,
        &library_changed_holder,
    ));
    sidebar.set_edit_smart_playlist_callback(sidebar_edit_smart_playlist_callback(
        &window,
        &command_controller,
        &runtime,
        &sidebar,
    ));
    sidebar.set_analysis_run_callback(sidebar_analysis_run_callback(&runtime));
    sidebar.set_online_run_callback(sidebar_online_run_callback(&runtime));
    sidebar.set_analysis_enabled_query(sidebar_analysis_enabled_query(&runtime));
    sidebar.set_online_busy_query(sidebar_online_busy_query(&runtime));
    library_changed_holder.replace(Some(library_changed.clone()));
    let scan_requested = library_scan_requested_callback(&runtime, library_changed.clone());
    let import_completed = library_import_completed_callback(
        &runtime,
        &songs_table,
        &albums_view,
        &duplicates_view,
        &statistics_view,
        &sidebar,
        &titlebar,
        visible_summary_refresh.clone(),
        &current_search_text,
    );
    let import_requested = library_import_requested_callback(&runtime, import_completed);
    install_file_drop_target(&songs_drop_overlay, &songs_drop_indicator, import_requested);
    // Re-applies the Views toggles to the live UI the instant they
    // change: the sidebar's Duplicates/Statistics rows.
    let view_settings_changed: ViewSettingsChangedCallback = {
        let sidebar = sidebar.clone();
        let runtime = runtime.clone();
        std::rc::Rc::new(move || {
            let runtime = runtime.borrow();
            let ui = &runtime.settings().ui;
            sidebar.set_view_row_visibility(ui.sidebar_show_duplicates, ui.sidebar_show_statistics);
        })
    };

    install_preferences_action(
        app,
        &window,
        command_controller.clone(),
        database_path.clone(),
        scan_requested.clone(),
        consolidation_requested.clone(),
        view_settings_changed.clone(),
    );

    let main_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_content.set_hexpand(true);
    main_content.set_vexpand(true);
    let command_controller_for_global_shortcuts = command_controller.clone();

    // The [cog] Settings button is the visual entry-point to Preferences
    // (the Ctrl+, accelerator is the power-user path, registered separately
    // by `install_preferences_action`). It is mounted in the status bar's
    // bottom-left cluster below, beside the sidebar collapse toggle (#164).
    let settings_button = settings_button(
        &window,
        command_controller,
        database_path,
        scan_requested,
        consolidation_requested.clone(),
        view_settings_changed,
    );

    main_content.append(&content_stack);

    // gtk::Paned keeps drag-resize between SIDEBAR_MIN_WIDTH and
    // SIDEBAR_MAX_WIDTH, with the user's manually-set width preserved
    // for the next launch via the sidebar collapse controller. The
    // collapse animation tweens the Paned position rather than
    // hiding the sidebar widget, so the existing min/max clamp and
    // drag handle survive untouched. Construct the controller before
    // wiring shortcuts so Ctrl+N / Ctrl+Alt+N can re-expand a
    // collapsed sidebar before arming a row rename.
    let content_area = build_content_area(&sidebar_widget, &main_content);
    let collapse_controller = SidebarCollapseController::new(
        content_area.clone(),
        initial_ui_settings.sidebar_collapsed,
        initial_ui_settings.sidebar_width,
    );
    status_bar.install_sidebar_collapse_toggle(collapse_controller.toggle_widget());
    status_bar.install_settings_button(settings_button);

    let albums_view_for_reveal = albums_view.clone();
    let sidebar_for_show_album = sidebar.clone();
    let show_album_action: ShowAlbumAction = Rc::new(move |track_id| {
        sidebar_for_show_album.select_albums();
        albums_view_for_reveal.reveal_album_for_track(track_id);
    });
    show_album_holder.replace(Some(show_album_action));
    install_global_shortcuts(GlobalShortcutContext {
        app: app.clone(),
        window: window.clone(),
        command_controller: command_controller_for_global_shortcuts,
        runtime: runtime.clone(),
        sidebar: sidebar.clone(),
        sidebar_collapse: collapse_controller.clone(),
        titlebar: titlebar.clone(),
        songs_table: songs_table.clone(),
        playlists_table: playlists_table.clone(),
        duplicates_view: duplicates_view.clone(),
        albums_view: albums_view.clone(),
        content_stack: content_stack.clone(),
        toggle_or_start_playback: toggle_or_start_playback.clone(),
        playback_changed: playback_changed.clone(),
        library_changed_holder: library_changed_holder.clone(),
        track_row_changed_holder: track_row_changed_holder.clone(),
        artwork_loader: artwork_loader.clone(),
        track_context_invocation,
    });

    root.append(&titlebar.widget);
    root.append(&content_area);
    root.append(&status_bar.widget());

    // `window_frame` is the visible window: it carries `.window-frame`
    // (shadow + rounded corners) and hosts the resize-handle overlays so the
    // handles snap to the actual visible edges. `shadow_gutter` is the outer
    // box whose only job is to provide the inset where the shadow renders.
    let window_frame = gtk::Overlay::new();
    window_frame.set_child(Some(&root));
    install_resize_handles(&window_frame, &window);
    install_window_state_chrome(&window, &window_frame);

    let shadow_gutter = gtk::Box::new(gtk::Orientation::Vertical, 0);
    shadow_gutter.add_css_class("csd");
    shadow_gutter.set_hexpand(true);
    shadow_gutter.set_vexpand(true);
    shadow_gutter.append(&window_frame);
    window.set_child(Some(&shadow_gutter));

    // Any debounced save scheduled within the debounce window of shutdown
    // would otherwise be lost: the timer's main loop never gets to fire.
    let songs_table_for_close = songs_table.clone();
    let playlists_table_for_close = playlists_table.clone();
    let titlebar_for_close = titlebar.clone();
    let runtime_for_close = runtime.clone();
    let sidebar_for_close = sidebar.clone();
    let collapse_controller_for_close = collapse_controller.clone();
    window.connect_close_request(move |_window| {
        songs_table_for_close.flush_pending_layout_save();
        playlists_table_for_close.flush_pending_layout_save();
        titlebar_for_close.flush_pending_volume_save();
        // Deliberate close-time policy: the window is tearing down, so the
        // NotificationCenter has no surface left to show a failure on.
        // Persisting UI state is best-effort here; on failure log a clear
        // line to stderr rather than silently dropping it, and never block
        // the close on a settings write.
        let current_ui = runtime_for_close.borrow().settings().ui.clone();
        let ui_to_save = ui_settings_from_widgets(
            &titlebar_for_close,
            &sidebar_for_close,
            collapse_controller_for_close.is_collapsed(),
            collapse_controller_for_close.expanded_width(),
            &current_ui,
        );
        if let Err(error) = runtime_for_close.borrow_mut().save_ui_settings(ui_to_save) {
            eprintln!("sustain: failed to persist UI state on close: {error:?}");
        }
        glib::Propagation::Proceed
    });

    sustain_profiler::profile_mark!(window_profile, "widgets assembled");
    BuiltMainWindow {
        window,
        deferred_startup,
    }
}

pub(crate) struct BuiltMainWindow {
    pub(crate) window: gtk::ApplicationWindow,
    deferred_startup: DeferredStartup,
}

impl BuiltMainWindow {
    pub(crate) fn run_deferred_startup(self) {
        self.deferred_startup.run();
    }
}

/// Post-first-frame work scheduled to keep the cold-start budget tight.
///
/// The Music view is the cheap default and is already built into the
/// content stack by the time `present()` returns. Restoring Albums or a
/// specific playlist as the persisted selection would otherwise drag
/// album-grouping or playlist-table population into the startup
/// critical path — both can run on the first idle instead, after the
/// window has had a chance to paint.
struct DeferredStartup {
    runtime: SharedRuntime,
    first_idle_startup: Box<dyn FnOnce()>,
    post_hydration_startup: PostHydrationStartup,
}

impl DeferredStartup {
    fn new(
        selection: UiSidebarSelection,
        sidebar: PlaylistSidebar,
        initialize_queue_view: Box<dyn FnOnce()>,
        populate_devices: Box<dyn FnOnce()>,
        resume_pending_metadata_writes: Box<dyn FnOnce()>,
        runtime: SharedRuntime,
        consolidation_requested: crate::library_consolidation::LibraryConsolidationRequestedCallback,
    ) -> Self {
        let restore_selection: Option<Box<dyn FnOnce()>> = match selection {
            UiSidebarSelection::Music => None,
            UiSidebarSelection::Albums => Some(Box::new(move || sidebar.select_albums())),
            UiSidebarSelection::Statistics => Some(Box::new(move || sidebar.select_statistics())),
            UiSidebarSelection::Playlist(item) => Some(Box::new(move || sidebar.select_item(item))),
        };
        let post_hydration_startup: PostHydrationStartup = Rc::new(RefCell::new(Some(Box::new({
            let runtime = runtime.clone();
            move || {
                if let Some(restore) = restore_selection {
                    restore();
                }
                // Device enumeration probes the filesystem, so it runs
                // after the first-idle gate and initial hydration.
                populate_devices();
                // Restored mirror retries can parse audio tags and
                // garbage-collect external artwork blobs.
                resume_pending_metadata_writes();
                if runtime.borrow().library_hydration_state() == crate::LibraryHydrationState::Ready
                {
                    maybe_auto_resume_library_consolidation(&runtime, &consolidation_requested);
                    install_smart_shuffle_launch_rebuild(&runtime);
                }
            }
        }))));
        Self {
            runtime,
            first_idle_startup: initialize_queue_view,
            post_hydration_startup,
        }
    }

    fn post_hydration_startup(&self) -> PostHydrationStartup {
        self.post_hydration_startup.clone()
    }

    fn run(self) {
        let Self {
            runtime,
            first_idle_startup,
            post_hydration_startup,
        } = self;
        first_idle_startup();
        if !runtime.borrow_mut().start_library_hydration()
            && let Some(callback) = post_hydration_startup.borrow_mut().take()
        {
            callback();
        }
    }
}

/// Defer the cost of populating the Albums view until the user
/// actually switches to it. Activation groups the current library into
/// album rows and lets the virtualized Albums view bind only visible
/// rows; doing that at startup provides no benefit while Music is the
/// initial visible page. Hooking into the content stack's
/// visible-child notification keeps the activation trigger in one
/// place — any caller that flips the stack to `ALBUMS_VIEW`
/// automatically picks it up. `activate()` is idempotent, so the
/// notification firing on every later switch is harmless.
/// The dynamic DEVICES sidebar section holds two kinds of transient entry —
/// removable sync targets (opening the device-sync panel) and inserted audio
/// CDs (opening the CD-import page). They share one rendering and one set of
/// discovery sources, so they are wired together here.
struct DevicesSectionContext<'a> {
    content_stack: &'a gtk::Stack,
    sidebar: &'a PlaylistSidebar,
    device_panel: &'a DeviceSyncPanel,
    cd_import_panel: &'a CdImportPanel,
    runtime: &'a SharedRuntime,
    mtp_discovery_rx: Option<MtpDiscoveryResultReceiver>,
    optical_discovery_rx: Option<OpticalDiscoveryResultReceiver>,
    cd_lookup_rx: Option<CdLookupEventReceiver>,
}

/// The current DEVICES entries: removable sync targets followed by inserted
/// audio CDs, both rebuilt from live runtime state.
fn device_entries(runtime: &SharedRuntime) -> Vec<SidebarDeviceEntry> {
    let runtime = runtime.borrow();
    let mut entries: Vec<SidebarDeviceEntry> = runtime
        .connected_devices()
        .into_iter()
        .map(SidebarDeviceEntry::SyncTarget)
        .collect();
    entries.extend(
        runtime
            .optical_discs()
            .iter()
            .cloned()
            .map(SidebarDeviceEntry::AudioCd),
    );
    entries
}

/// Wire the DEVICES sidebar section to the device-sync panel and the
/// CD-import page, and keep the entry list live.
///
/// Selecting an entry shows its page and flips the content stack to it;
/// switching to any other page clears the row highlight so only one
/// navigation surface looks active at a time. Discovery (Android/MTP and
/// optical disc) runs off the main thread; GIO's [`gio::VolumeMonitor`] is
/// the native mount/media-change source and re-runs it. The monitor is a
/// process singleton GIO finalises once its last reference drops, so it is
/// parked in the content-stack notify closure below, owned for the whole
/// session and freed with the UI — anchoring it there avoids a
/// sidebar↔monitor reference cycle.
fn install_devices_section(context: DevicesSectionContext<'_>) {
    let DevicesSectionContext {
        content_stack,
        sidebar,
        device_panel,
        cd_import_panel,
        runtime,
        mtp_discovery_rx,
        optical_discovery_rx,
        cd_lookup_rx,
    } = context;

    {
        let content_stack = content_stack.clone();
        let device_panel = device_panel.clone();
        sidebar.set_device_selected_callback(Rc::new(move |connected: ConnectedDevice| {
            // Switch the stack first so `show_device`'s summary refresh
            // resolves against the device page, not the outgoing view.
            content_stack.set_visible_child_name(DEVICES_VIEW);
            device_panel.show_device(connected);
        }));
    }
    {
        let content_stack = content_stack.clone();
        let cd_import_panel = cd_import_panel.clone();
        let runtime = runtime.clone();
        sidebar.set_cd_selected_callback(Rc::new(move |snapshot: TocSnapshot| {
            content_stack.set_visible_child_name(CD_IMPORT_VIEW);
            cd_import_panel.show_disc(snapshot.clone());
            // Read-only MusicBrainz lookup off the main thread; results land
            // on the CD-lookup consumer below. Only show the "searching"
            // artwork when a lookup actually started — without a remote
            // service nothing would ever resolve it.
            if runtime.borrow_mut().lookup_disc_releases(&snapshot) {
                cd_import_panel.mark_lookup_started();
            }
        }));
    }
    {
        let runtime = runtime.clone();
        sidebar.set_device_eject_callback(Rc::new(move |device: ConnectedDevice| {
            eject_usb_device(&device, &runtime);
        }));
    }
    {
        let runtime = runtime.clone();
        sidebar.set_cd_eject_callback(Rc::new(move |snapshot: TocSnapshot| {
            eject_optical_disc(&snapshot.device_path, &runtime);
        }));
    }
    {
        let runtime = runtime.clone();
        cd_import_panel.set_eject_requested_callback(Rc::new(move |snapshot: TocSnapshot| {
            eject_optical_disc(&snapshot.device_path, &runtime);
        }));
    }

    // Render both kinds of DEVICES entry from current runtime state, refresh
    // the device panel, and — when the hardware backing a transient view has
    // gone away — leave that view for the selection that preceded it.
    let render_devices: Rc<dyn Fn()> = {
        let sidebar = sidebar.clone();
        let device_panel = device_panel.clone();
        let cd_import_panel = cd_import_panel.clone();
        let content_stack = content_stack.clone();
        let runtime = runtime.clone();
        Rc::new(move || {
            let connected = runtime.borrow().connected_devices();

            // Resolve "is the thing this transient view shows still here?"
            // before `set_devices` drops the (now stale) transient highlight.
            let shown_disc_gone = match cd_import_panel.current_snapshot() {
                Some(shown) => !runtime
                    .borrow()
                    .optical_discs()
                    .iter()
                    .any(|disc| disc.identity() == shown.identity()),
                None => false,
            };
            let shown_device_gone = match device_panel.shown_device() {
                Some(device) => !connected.iter().any(|other| other.id == device.id),
                None => false,
            };
            let visible = content_stack.visible_child_name();
            let on_cd_page = visible.as_deref() == Some(CD_IMPORT_VIEW);
            let on_device_page = visible.as_deref() == Some(DEVICES_VIEW);

            sidebar.set_devices(&device_entries(&runtime));
            device_panel.connected_devices_changed(&connected);

            if shown_disc_gone {
                cd_import_panel.forget_disc();
                runtime.borrow_mut().invalidate_cd_metadata();
                if on_cd_page {
                    sidebar.restore_persistent_selection();
                }
            }
            if shown_device_gone && on_device_page {
                sidebar.restore_persistent_selection();
            }
        })
    };
    // A full refresh kicks the asynchronous Android/MTP and optical-disc
    // probes (cheap on the main thread; their results re-render through the
    // discovery consumers) and renders immediately with what is known. The
    // optical probe is skipped while a CD import owns the drive.
    let refresh_devices: Rc<dyn Fn()> = {
        let render_devices = render_devices.clone();
        let runtime = runtime.clone();
        Rc::new(move || {
            {
                let mut runtime = runtime.borrow_mut();
                runtime.refresh_mtp_devices();
                if !matches!(
                    runtime.background_task_status(),
                    sustain_app_runtime::BackgroundTaskStatus::CdImportRunning
                ) {
                    runtime.refresh_optical_discs();
                }
            }
            render_devices();
        })
    };
    install_mtp_discovery_consumer(mtp_discovery_rx, runtime.clone(), render_devices.clone());

    // Optical-disc discovery results re-render the section.
    if let Some(receiver) = optical_discovery_rx {
        let runtime = runtime.clone();
        let render_devices = render_devices.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(result) = receiver.recv().await {
                if runtime.borrow_mut().apply_optical_discovery(result) {
                    render_devices();
                }
            }
        });
    }

    // MusicBrainz disc lookup + cover results, dropped when their generation
    // no longer matches the disc currently on the page.
    if let Some(receiver) = cd_lookup_rx {
        let runtime = runtime.clone();
        let cd_import_panel = cd_import_panel.clone();
        glib::MainContext::default().spawn_local(async move {
            while let Ok(event) = receiver.recv().await {
                match event {
                    sustain_app_runtime::CdLookupEvent::Releases {
                        generation,
                        releases,
                        failed,
                        ..
                    } => {
                        if !runtime
                            .borrow()
                            .is_current_cd_metadata_generation(generation)
                        {
                            continue;
                        }
                        if failed {
                            runtime.borrow_mut().push_ephemeral_notification(
                                sustain_app_runtime::NotificationCategory::CdImport,
                                sustain_app_runtime::NotificationSeverity::Warning,
                                "Could not reach MusicBrainz; using fallback CD metadata."
                                    .to_owned(),
                            );
                        }
                        cd_import_panel.apply_releases(releases, failed);
                    }
                    sustain_app_runtime::CdLookupEvent::Cover { generation, cover } => {
                        if runtime
                            .borrow()
                            .is_current_cd_metadata_generation(generation)
                        {
                            cd_import_panel.apply_cover(cover);
                        }
                    }
                }
            }
        });
    }

    let volume_monitor = gio::VolumeMonitor::get();
    {
        let refresh_devices = refresh_devices.clone();
        volume_monitor.connect_mount_added(move |_monitor, _mount| refresh_devices());
    }
    {
        let refresh_devices = refresh_devices.clone();
        volume_monitor.connect_mount_removed(move |_monitor, _mount| refresh_devices());
    }

    let sidebar = sidebar.clone();
    content_stack.connect_visible_child_name_notify(move |stack| {
        // `volume_monitor` is captured (not otherwise used here) so the
        // singleton lives as long as the content stack; see the doc comment.
        let _keep_monitor_alive = &volume_monitor;
        // Leaving a transient view's page (device panel or CD-import page)
        // drops its sidebar highlight so the persistent selection shows
        // through.
        if !matches!(
            stack.visible_child_name().as_deref(),
            Some(DEVICES_VIEW) | Some(CD_IMPORT_VIEW) | Some(DUPLICATES_VIEW)
        ) {
            sidebar.clear_transient_highlight();
        }
    });
}

/// Eject the optical disc in `device_path` via GIO, so the desktop's normal
/// eject machinery (GVfs) runs rather than us touching the device directly.
/// The removal propagates back through the volume monitor, which refreshes
/// the DEVICES section and — if the CD page is showing this disc — leaves it.
/// A failed eject is reported through the notification lane.
fn eject_optical_disc(device_path: &std::path::Path, runtime: &SharedRuntime) {
    let Some(device) = device_path.to_str() else {
        return;
    };
    let monitor = gio::VolumeMonitor::get();
    let Some(drive) = monitor
        .connected_drives()
        .into_iter()
        .find(|drive| drive.identifier("unix-device").as_deref() == Some(device))
    else {
        return;
    };
    if !drive.can_eject() {
        return;
    }
    let runtime = runtime.clone();
    drive.eject_with_operation(
        gio::MountUnmountFlags::NONE,
        Some(&gio::MountOperation::new()),
        gio::Cancellable::NONE,
        move |result| {
            if result.is_err() {
                runtime.borrow_mut().push_ephemeral_notification(
                    sustain_app_runtime::NotificationCategory::CdImport,
                    sustain_app_runtime::NotificationSeverity::Warning,
                    "Could not eject the disc.".to_owned(),
                );
            }
        },
    );
}

/// Eject or unmount a mounted USB sync target through GIO. Eject is
/// preferred when the backend exposes it; otherwise unmount keeps the user
/// action useful for ordinary removable filesystems whose desktop stack only
/// advertises unmount. Failures go through the notification lane.
fn eject_usb_device(device: &ConnectedDevice, runtime: &SharedRuntime) {
    let DeviceTarget::Filesystem { mount_path } = &device.target else {
        return;
    };
    let monitor = gio::VolumeMonitor::get();
    let Some(mount) = monitor
        .mounts()
        .into_iter()
        .find(|mount| mount.root().path().as_deref() == Some(mount_path.as_path()))
    else {
        runtime.borrow_mut().push_ephemeral_notification(
            sustain_app_runtime::NotificationCategory::DeviceSync,
            sustain_app_runtime::NotificationSeverity::Warning,
            "Could not find the mounted device to eject.".to_owned(),
        );
        return;
    };

    let runtime = runtime.clone();
    if mount.can_eject() {
        mount.eject_with_operation(
            gio::MountUnmountFlags::NONE,
            Some(&gio::MountOperation::new()),
            gio::Cancellable::NONE,
            move |result| {
                if result.is_err() {
                    runtime.borrow_mut().push_ephemeral_notification(
                        sustain_app_runtime::NotificationCategory::DeviceSync,
                        sustain_app_runtime::NotificationSeverity::Warning,
                        "Could not eject the device.".to_owned(),
                    );
                }
            },
        );
    } else if mount.can_unmount() {
        mount.unmount_with_operation(
            gio::MountUnmountFlags::NONE,
            Some(&gio::MountOperation::new()),
            gio::Cancellable::NONE,
            move |result| {
                if result.is_err() {
                    runtime.borrow_mut().push_ephemeral_notification(
                        sustain_app_runtime::NotificationCategory::DeviceSync,
                        sustain_app_runtime::NotificationSeverity::Warning,
                        "Could not unmount the device.".to_owned(),
                    );
                }
            },
        );
    } else {
        runtime.borrow_mut().push_ephemeral_notification(
            sustain_app_runtime::NotificationCategory::DeviceSync,
            sustain_app_runtime::NotificationSeverity::Warning,
            "This device cannot be ejected by the desktop.".to_owned(),
        );
    }
}

/// One event from the CD-import worker thread.
enum CdImportWorkerEvent {
    Progress(CdImportProgress),
    Finished(Result<CdImportResult, ApplicationRuntimeError>),
}

/// Wire the CD page's `Import CD` button to the runtime's prepare /
/// background-worker / apply path, mirroring library import: prepare claims
/// the mutation slot on the main thread, the rip runs on a worker, and its
/// progress/outcome are applied back on the GTK loop.
fn install_cd_import_requested(
    cd_import_panel: &CdImportPanel,
    runtime: &SharedRuntime,
    library_changed: LibraryChangedCallback,
) {
    let runtime = runtime.clone();
    let panel = cd_import_panel.clone();
    let callback: CdImportRequestedCallback = Rc::new(move |request: CdImportRequest| {
        let task = {
            let mut runtime = runtime.borrow_mut();
            match runtime.prepare_cd_import(request) {
                Ok(task) => task,
                Err(error) => {
                    runtime.fail_cd_import(error);
                    return;
                }
            }
        };
        // The slot is claimed; reflect it on the button immediately.
        panel.refresh_import_sensitivity();

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = crate::run_cd_import_task(task, |progress| {
                let _sent = tx.send(CdImportWorkerEvent::Progress(progress));
            });
            let _sent = tx.send(CdImportWorkerEvent::Finished(outcome));
        });
        poll_cd_import(rx, runtime.clone(), panel.clone(), library_changed.clone());
    });
    cd_import_panel.set_import_requested_callback(callback);
}

fn poll_cd_import(
    rx: std::sync::mpsc::Receiver<CdImportWorkerEvent>,
    runtime: SharedRuntime,
    cd_import_panel: CdImportPanel,
    library_changed: LibraryChangedCallback,
) {
    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        let mut latest_progress = None;
        let mut finished = None;
        loop {
            match rx.try_recv() {
                Ok(CdImportWorkerEvent::Progress(progress)) => latest_progress = Some(progress),
                Ok(CdImportWorkerEvent::Finished(outcome)) => {
                    finished = Some(outcome);
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    finished = Some(Err(ApplicationRuntimeError::CdImportFailed));
                    break;
                }
            }
        }
        if let Some(progress) = latest_progress {
            runtime
                .borrow_mut()
                .update_cd_import_progress(progress.completed_tracks, progress.total_tracks);
            // Reflect the rip inline in the track table's status column
            // (issue #195): done ticks for the completed tracks, a spinner on
            // the one being ripped.
            cd_import_panel
                .apply_import_progress(progress.completed_tracks, progress.current_track_number);
        }
        match finished {
            Some(Ok(result)) => {
                let imported = result.summary.imported_tracks;
                runtime.borrow_mut().apply_cd_import_result(result);
                library_changed();
                cd_import_panel.finish_import_display(imported);
                cd_import_panel.refresh_import_sensitivity();
                glib::ControlFlow::Break
            }
            Some(Err(error)) => {
                runtime.borrow_mut().fail_cd_import(error);
                // The prepare/dispatch failed before any track was imported;
                // clear every spinner.
                cd_import_panel.finish_import_display(0);
                cd_import_panel.refresh_import_sensitivity();
                glib::ControlFlow::Break
            }
            None => glib::ControlFlow::Continue,
        }
    });
}

fn install_duplicates_view(
    content_stack: &gtk::Stack,
    sidebar: &PlaylistSidebar,
    duplicates_view: &DuplicatesView,
) {
    let content_stack_for_sidebar = content_stack.clone();
    sidebar.set_duplicates_selected_callback(Rc::new(move || {
        content_stack_for_sidebar.set_visible_child_name(DUPLICATES_VIEW);
    }));
    let duplicates_view = duplicates_view.clone();
    content_stack.connect_visible_child_name_notify(move |stack| {
        duplicates_view.set_active(stack.visible_child_name().as_deref() == Some(DUPLICATES_VIEW));
    });
}

fn install_albums_view_activator(content_stack: &gtk::Stack, albums_view: &AlbumsView) {
    let albums_view = albums_view.clone();
    content_stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() == Some(ALBUMS_VIEW) {
            albums_view.activate();
        }
    });
}

/// Rebuild the Statistics page each time it becomes the visible view, so
/// its figures are current without doing the work during cold start or
/// while it is hidden. `refresh` is cheap (one O(n) pass plus a few dozen
/// small widgets), so an unconditional rebuild on every visit is simpler
/// — and always-fresh — versus dirty-flag bookkeeping.
fn install_statistics_view_activator(content_stack: &gtk::Stack, statistics_view: &StatisticsView) {
    let statistics_view = statistics_view.clone();
    content_stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() == Some(STATISTICS_VIEW) {
            statistics_view.refresh();
        }
    });
}

fn ui_settings_from_widgets(
    titlebar: &Titlebar,
    sidebar: &PlaylistSidebar,
    sidebar_collapsed: bool,
    sidebar_width: u32,
    current: &UiSettings,
) -> UiSettings {
    UiSettings {
        search_text: titlebar.search_text(),
        // `persisted_selection` (not `current_selection`) so a transient
        // view that is showing right now — e.g. a connected device — is
        // never what gets saved/restored; the persistent view beneath it
        // is. Re-opening a transient view at launch could fail or be
        // costly.
        sidebar_selection: match sidebar.persisted_selection() {
            Some(SidebarSelection::Music) | None => UiSidebarSelection::Music,
            Some(SidebarSelection::Albums) => UiSidebarSelection::Albums,
            Some(SidebarSelection::Statistics) => UiSidebarSelection::Statistics,
            Some(SidebarSelection::Item(item)) => UiSidebarSelection::Playlist(item),
        },
        sidebar_collapsed,
        sidebar_width: Some(sidebar_width),
        library_section_collapsed: sidebar.library_section_collapsed(),
        playlists_section_collapsed: sidebar.playlists_section_collapsed(),
        // View-preference toggles are set in the Views tab, not derived
        // from any widget here, so carry the current values forward
        // unchanged — otherwise this close-time save would reset them.
        sidebar_show_duplicates: current.sidebar_show_duplicates,
        sidebar_show_statistics: current.sidebar_show_statistics,
    }
}

#[allow(clippy::too_many_arguments)]
fn library_changed_callback(
    runtime: &SharedRuntime,
    songs_table: &TrackTable,
    albums_view: &AlbumsView,
    duplicates_view: &DuplicatesView,
    statistics_view: &StatisticsView,
    sidebar: &PlaylistSidebar,
    titlebar: &Titlebar,
    visible_summary_refresh: VisibleSummaryRefreshCallback,
    current_search_text: &Rc<RefCell<String>>,
) -> LibraryChangedCallback {
    let runtime = runtime.clone();
    let songs_table = songs_table.clone();
    let albums_view = albums_view.clone();
    let duplicates_view = duplicates_view.clone();
    let statistics_view = statistics_view.clone();
    let sidebar = sidebar.clone();
    let titlebar = titlebar.clone();
    let current_search_text = current_search_text.clone();

    Rc::new(move || {
        let search_text = current_search_text.borrow().clone();
        let rows = runtime_library_table_rows(&runtime.borrow(), &search_text);
        songs_table.replace_rows(rows);
        // AlbumsView re-derives the visible album set from the runtime's
        // current library using the search text it already holds, so we
        // pass no track snapshot and need not call set_search_text here.
        albums_view.replace_tracks();
        duplicates_view.refresh_if_active();
        // The Statistics page is library-wide; rebuild it only when it is
        // the visible view (otherwise its activator refreshes it on the
        // next visit), so a scan/import does not pay for an off-screen
        // rebuild.
        statistics_view.refresh_if_visible();
        // sidebar.refresh() rebuilds the sidebar tree-model and fires the
        // selection callback exactly once. That callback owns the
        // playlists view — it runs `refresh_playlists_view_if_visible`,
        // so library_changed never needs to touch the playlists table
        // directly.
        sidebar.refresh();
        // A scan/import/removal can flip the library between empty and
        // non-empty, which decides whether the Play button can cold-start
        // anything.
        update_play_pause_sensitivity(&titlebar, &runtime.borrow());
        visible_summary_refresh();
    })
}

#[allow(clippy::too_many_arguments)]
fn library_import_completed_callback(
    runtime: &SharedRuntime,
    songs_table: &TrackTable,
    albums_view: &AlbumsView,
    duplicates_view: &DuplicatesView,
    statistics_view: &StatisticsView,
    sidebar: &PlaylistSidebar,
    titlebar: &Titlebar,
    visible_summary_refresh: VisibleSummaryRefreshCallback,
    current_search_text: &Rc<RefCell<String>>,
) -> LibraryImportCompletedCallback {
    let runtime = runtime.clone();
    let songs_table = songs_table.clone();
    let albums_view = albums_view.clone();
    let duplicates_view = duplicates_view.clone();
    let statistics_view = statistics_view.clone();
    let sidebar = sidebar.clone();
    let titlebar = titlebar.clone();
    let current_search_text = current_search_text.clone();

    Rc::new(move |imported_tracks| {
        if imported_tracks.is_empty() {
            return;
        }
        let search_text = current_search_text.borrow().clone();
        let rows = {
            let runtime = runtime.borrow();
            imported_library_table_rows(&runtime, imported_tracks, &search_text)
        };
        songs_table.append_rows(rows);
        albums_view.replace_tracks();
        duplicates_view.refresh_if_active();
        statistics_view.refresh_if_visible();
        sidebar.refresh();
        update_play_pause_sensitivity(&titlebar, &runtime.borrow());
        visible_summary_refresh();
    })
}

/// Wires the runtime's `track_availability_observer` to a narrow
/// per-row refresh on both track tables. The runtime fires this
/// observer after every lazy `is_missing` flip (failed-play
/// detection, library-path re-stat, consolidation source miss).
/// The deferred closure snapshots `(track_id, is_missing)` from the
/// runtime and asks each loaded table to patch matching rows in
/// place; never a `replace_rows`, so scroll/focus/selection survive
/// — see the design note on [`AvailabilityChangedCallback`].
fn install_track_availability_observer(
    runtime: &SharedRuntime,
    songs_table: &TrackTable,
    playlists_table: &TrackTable,
) {
    let runtime_for_observer = runtime.clone();
    let songs_table = songs_table.clone();
    let playlists_table = playlists_table.clone();
    let refresh: AvailabilityChangedCallback = Rc::new(move || {
        let availability: HashMap<TrackId, bool> = runtime_for_observer
            .borrow()
            .library_tracks()
            .iter()
            .map(|track| (track.id, track.location.is_missing()))
            .collect();
        let lookup = |id: TrackId| availability.get(&id).copied();
        songs_table.refresh_missing_flags(&lookup);
        playlists_table.refresh_missing_flags(&lookup);
    });
    runtime
        .borrow_mut()
        .set_track_availability_observer(Box::new(move || {
            // The runtime is mid-borrow when this fires — defer
            // the refresh onto the GLib main loop so the closure
            // can re-borrow the runtime read-only without panicking.
            let refresh = refresh.clone();
            glib::idle_add_local_once(move || refresh());
        }));
}

fn visible_summary_refresh_callback(
    runtime: &SharedRuntime,
    content_stack: &gtk::Stack,
    status_bar: &StatusBar,
    playlists_table: &TrackTable,
    current_search_text: &Rc<RefCell<String>>,
    current_device: Rc<RefCell<Option<ConnectedDevice>>>,
) -> VisibleSummaryRefreshCallback {
    let runtime = runtime.clone();
    let content_stack = content_stack.clone();
    let status_bar = status_bar.clone();
    let playlists_table = playlists_table.clone();
    let current_search_text = current_search_text.clone();

    Rc::new(move || {
        // On the device view the summary reflects the device's selected
        // (deduplicated) tracks, not whatever table was last shown.
        if content_stack.visible_child_name().as_deref() == Some(DEVICES_VIEW) {
            let runtime = runtime.borrow();
            let honor_sort_tags = runtime.settings().library.honor_sort_tags;
            let rows: Vec<TrackTableRow> = current_device
                .borrow()
                .as_ref()
                .map(|device| {
                    runtime
                        .device_selected_tracks(&device.id)
                        .iter()
                        .map(|track| TrackTableRow::from_track(track, honor_sort_tags))
                        .collect()
                })
                .unwrap_or_default();
            status_bar.update_summary(&rows);
            return;
        }
        if content_stack.visible_child_name().as_deref() == Some(PLAYLISTS_VIEW) {
            let (track_count, duration_seconds, size_bytes) = playlists_table.summary_values();
            status_bar.update_summary_values(track_count, duration_seconds, size_bytes);
            return;
        }
        let search_text = current_search_text.borrow().clone();
        let rows = runtime_library_table_rows(&runtime.borrow(), &search_text);
        status_bar.update_summary(&rows);
    })
}

fn track_analyze_run_callback(runtime: &SharedRuntime) -> TrackAnalyzeRunCallback {
    let runtime = runtime.clone();
    Rc::new(move |track_ids, request| {
        let _ = runtime
            .borrow_mut()
            .request_tracks_analysis_run(track_ids, request);
    })
}

fn track_retrieve_run_callback(runtime: &SharedRuntime) -> TrackRetrieveRunCallback {
    let runtime = runtime.clone();
    Rc::new(move |track_ids, request| {
        let _ = runtime
            .borrow_mut()
            .request_tracks_online_run(track_ids, request);
    })
}

fn analysis_enabled_query(runtime: &SharedRuntime) -> TrackAnalyzeEnabledQuery {
    let runtime = runtime.clone();
    Rc::new(move |capability| analysis_capability_enabled(&runtime, capability))
}

/// Whether the online retrieval process is running right now. Shared by
/// the sidebar's per-playlist Retrieve submenu and the track-table's
/// per-track Retrieve submenu so both grey out their entries together
/// while a run is in flight, and offer them otherwise — independent of
/// the background toggle (issue #61).
fn online_busy_query(runtime: &SharedRuntime) -> TrackRetrieveBusyQuery {
    let runtime = runtime.clone();
    Rc::new(move || runtime.borrow().is_online_retrieval_running())
}

/// Read the global analysis-capability toggle from the live settings.
/// Shared by the sidebar's per-playlist submenu and the track-table's
/// per-track submenu so both see the exact same "is the background
/// sweep covering this capability?" answer.
fn analysis_capability_enabled(
    runtime: &SharedRuntime,
    capability: sustain_app_runtime::AnalysisCapability,
) -> bool {
    let runtime = runtime.borrow();
    let analysis = runtime.settings().analysis;
    match capability {
        sustain_app_runtime::AnalysisCapability::Bpm => analysis.bpm,
        sustain_app_runtime::AnalysisCapability::Key => analysis.key,
        sustain_app_runtime::AnalysisCapability::Audio => analysis.audio,
    }
}

/// Wires the persisted-layout machinery for both track tables.
///
/// - The Songs view always writes to the [`Default`] scope — it *is* the
///   "general song list view" the user asked for.
/// - The Playlists view writes to a per-playlist override only when a real
///   playlist or smart playlist is selected. Library / Folder / empty
///   selections are transient and never produce override rows (matches the
///   "user owns their changes; we don't fabricate them" semantics).
/// - The Songs view's initial layout is applied here. The Playlists view's
///   initial layout is applied by the synthetic first call that
///   [`PlaylistSidebar::set_selection_changed`] makes on its handler.
fn install_track_column_layout_persistence(
    runtime: &SharedRuntime,
    songs_table: &TrackTable,
    playlists_table: &TrackTable,
    sidebar: &PlaylistSidebar,
) {
    let runtime_for_songs = runtime.clone();
    songs_table.set_layout_changed_callback(Rc::new(move |layout| {
        let _ = runtime_for_songs
            .borrow()
            .save_track_column_layout(TrackColumnLayoutScope::Default, &layout);
    }));

    let runtime_for_playlists = runtime.clone();
    let sidebar_for_playlists = sidebar.clone();
    playlists_table.set_layout_changed_callback(Rc::new(move |layout| {
        let scope = match sidebar_for_playlists.current_selection() {
            Some(SidebarSelection::Item(PlaylistItem::Playlist(id))) => {
                TrackColumnLayoutScope::Playlist(id)
            }
            Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(id))) => {
                TrackColumnLayoutScope::SmartPlaylist(id)
            }
            _ => return,
        };
        let _ = runtime_for_playlists
            .borrow()
            .save_track_column_layout(scope, &layout);
    }));

    if let Ok(Some(default)) = runtime
        .borrow()
        .load_track_column_layout(TrackColumnLayoutScope::Default)
    {
        songs_table.apply_layout(&default);
    }
}

fn layout_for_selection(
    runtime: &ApplicationRuntime,
    selection: Option<SidebarSelection>,
) -> Option<TrackColumnLayout> {
    let override_scope = match selection {
        Some(SidebarSelection::Item(PlaylistItem::Playlist(id))) => {
            Some(TrackColumnLayoutScope::Playlist(id))
        }
        Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(id))) => {
            Some(TrackColumnLayoutScope::SmartPlaylist(id))
        }
        _ => None,
    };
    if let Some(scope) = override_scope {
        if let Ok(Some(layout)) = runtime.load_track_column_layout(scope) {
            return Some(layout);
        }
    }
    runtime
        .load_track_column_layout(TrackColumnLayoutScope::Default)
        .ok()
        .flatten()
}

fn runtime_library_table_rows(
    runtime: &ApplicationRuntime,
    search_text: &str,
) -> Vec<TrackTableRow> {
    let honor_sort_tags = runtime.settings().library.honor_sort_tags;
    // Normalize the query once, then test each track against the runtime's
    // precomputed search index — no per-track metadata cloning or
    // re-lowercasing on a 10k-track keystroke.
    let normalized = normalize_query(search_text);
    runtime
        .library_tracks()
        .iter()
        .filter(|track| normalized.is_empty() || runtime.search_matches(track.id, &normalized))
        .map(|track| TrackTableRow::from_track(track, honor_sort_tags))
        .collect()
}

fn imported_library_table_rows(
    runtime: &ApplicationRuntime,
    imported_tracks: &[Track],
    search_text: &str,
) -> Vec<TrackTableRow> {
    let honor_sort_tags = runtime.settings().library.honor_sort_tags;
    let normalized = normalize_query(search_text);
    imported_tracks
        .iter()
        .filter(|track| normalized.is_empty() || runtime.search_matches(track.id, &normalized))
        .map(|track| TrackTableRow::from_track(track, honor_sort_tags))
        .collect()
}

fn install_app_icon() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let theme = gtk::IconTheme::for_display(&display);

    // During development (cargo run), icons live under data/icons in the project tree.
    // At compile time, CARGO_MANIFEST_DIR points to crates/ui_gtk/.
    let dev_icons = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/icons");
    if dev_icons.exists() {
        theme.add_search_path(dev_icons);
    }
}
