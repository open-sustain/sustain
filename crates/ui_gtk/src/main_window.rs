// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use sustain_app_runtime::{
    MetadataChange, PlaybackCommand, PlaybackQueueRequest, PlaybackQueueSource, PlaybackState,
    Playlist, PlaylistEntry, PlaylistFolder, PlaylistFolderId, PlaylistItem, Rating, ShuffleMode,
    Track, TrackColumnLayout, TrackColumnLayoutScope, TrackId, UiSettings, UiSidebarSelection,
    normalize_query, track_matches_search_text,
};

use super::{
    ALBUMS_VIEW, APP_ID, AnalysisProgressReceiver, ApplicationCommand, ApplicationRuntime,
    ArtworkFetchResultReceiver, AvailabilityChangedCallback, ConnectedDevice, DEVICES_VIEW,
    DeviceSyncEventReceiver, LibraryChangedCallback, LibraryChangedHolder,
    MetadataWriterEventReceiver, MprisCommandReceiver, OnlineProgressReceiver, PLAYLISTS_VIEW,
    PlaybackChangedCallback, SIDEBAR_DEFAULT_WIDTH, SIDEBAR_MAX_WIDTH, SIDEBAR_MIN_WIDTH,
    SONGS_VIEW, STATISTICS_VIEW, SharedMprisService, SharedRuntime, ShowAlbumAction,
    ShowAlbumHolder, SmartPlaylistTrackStatus, SmartShuffleRebuildResultReceiver,
    TrackRowChangedCallback, TrackRowChangedHolder, TrackUpdatedReceiver,
    accent::install_accent_css,
    albums::AlbumsView,
    app_css::install_app_css,
    artwork_loader::ArtworkLoader,
    command_controller::{SharedCommandController, UiCommandController},
    content_stack::build_content_stack,
    device_panel::DeviceSyncPanel,
    library_consolidation::{
        library_consolidation_requested_callback, maybe_auto_resume_library_consolidation,
    },
    library_import::{
        LIBRARY_DROP_INDICATOR_CLASS, install_file_drop_target, library_import_requested_callback,
    },
    library_scan::library_scan_requested_callback,
    now_playing::NowPlayingView,
    playlists_header::{PlaylistsHeader, PlaylistsHeaderState},
    preferences::{install_preferences_action, settings_button},
    shortcuts::{
        GlobalShortcutContext, create_new_playlist, install_global_shortcuts,
        open_new_smart_playlist_editor,
    },
    sidebar::{PlaylistSidebar, SidebarSelection, build_content_area},
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
        TrackActionVisibility, TrackAnalyzeEnabledQuery, TrackAnalyzeRunCallback,
        TrackContextAction, TrackContextActionSet, TrackRetrieveBusyQuery,
        TrackRetrieveRunCallback, TrackRowContextMenu,
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
};

mod keyboard;
mod mpris_bridge;
mod playback;
mod playlists;
mod result_consumers;
mod search;
mod sidebar_callbacks;
mod sidebar_collapse;
mod track_callbacks;

pub(crate) use sidebar_collapse::SidebarCollapseController;

use keyboard::{KeyboardShortcutContext, install_keyboard_shortcuts};
use mpris_bridge::{install_mpris_command_consumer, now_playing_to_mpris_metadata};
use playback::{
    install_playlists_header_playback, install_track_ended_callback,
    library_track_activated_callback, make_toggle_or_start_playback, playback_changed_callback,
    playlist_track_activated_callback, update_play_pause_sensitivity,
};
use playlists::{
    add_to_playlist_callback, add_to_playlist_provider, install_playlists_view_activator,
    playlist_table_rows_for, refresh_playlists_view_if_visible,
};
use result_consumers::{
    ArtworkFetchResultConsumerContext, install_analysis_progress_consumer,
    install_artwork_fetch_result_consumer, install_device_sync_event_consumer,
    install_metadata_writer_event_consumer, install_online_progress_consumer,
    install_smart_shuffle_launch_rebuild, install_smart_shuffle_rebuild_result_consumer,
    install_track_data_observer, install_track_updated_consumer,
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
    pub analysis_progress_rx: Option<AnalysisProgressReceiver>,
    pub online_progress_rx: Option<OnlineProgressReceiver>,
    pub track_updated_rx: Option<TrackUpdatedReceiver>,
    pub smart_shuffle_rebuild_result_rx: Option<SmartShuffleRebuildResultReceiver>,
    pub device_sync_event_rx: Option<DeviceSyncEventReceiver>,
}

pub(crate) fn build_main_window(
    app: &gtk::Application,
    runtime: SharedRuntime,
    mpris_service: Option<SharedMprisService>,
    receivers: MainWindowAsyncReceivers,
) -> BuiltMainWindow {
    let MainWindowAsyncReceivers {
        mpris_command_rx,
        metadata_writer_event_rx,
        artwork_fetch_result_rx,
        analysis_progress_rx,
        online_progress_rx,
        track_updated_rx,
        smart_shuffle_rebuild_result_rx,
        device_sync_event_rx,
    } = receivers;
    let tbw = std::time::Instant::now();
    macro_rules! tlog {
        ($label:expr) => {
            eprintln!(
                "[TIMING]     build_main_window+{:>7.1}ms {}",
                tbw.elapsed().as_secs_f64() * 1000.0,
                $label
            );
        };
    }
    tlog!("entered");
    // Coarse timing landmarks live in this function (and in `main` /
    // `ui_gtk::run`) so a launch regression shows up the first time
    // anyone runs the app from a terminal. Keep them sparse: only
    // phases that can plausibly grow with library size or new
    // features warrant a print. Per-callback timings inside hot
    // paths are intentionally absent.
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

    let library_tracks = runtime_library_table_rows(&runtime.borrow(), &initial_search_text);
    tlog!("library rows materialised");
    let status_bar = {
        let runtime_for_cancel = runtime.clone();
        StatusBar::new(
            &library_tracks,
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
    let artwork_loader = ArtworkLoader::new(metadata_service);

    let now_playing = NowPlayingView::new(
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
        mpris_service.clone(),
    );
    connect_titlebar_playback_controls(
        &titlebar,
        &runtime,
        command_controller.clone(),
        playback_changed.clone(),
    );
    install_track_ended_callback(&runtime, &command_controller, &playback_changed);
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
        initial_ui_settings.library_section_collapsed,
        initial_ui_settings.playlists_section_collapsed,
    );
    let sidebar_widget = sidebar.widget();

    let library_track_activated = library_track_activated_callback(
        &command_controller,
        &runtime,
        playback_changed.clone(),
        &current_search_text,
    );
    let library_changed_holder: LibraryChangedHolder = Rc::new(RefCell::new(None));
    let track_row_changed_holder: TrackRowChangedHolder = Rc::new(RefCell::new(None));
    let parent_window = window.clone().upcast::<gtk::Window>();
    let show_album_holder: ShowAlbumHolder = Rc::new(RefCell::new(None));
    let context_actions = track_context_actions(
        &runtime,
        &parent_window,
        &show_album_holder,
        &command_controller,
        playback_changed.clone(),
        library_changed_holder.clone(),
        track_row_changed_holder.clone(),
    );
    let add_to_playlist_provider = add_to_playlist_provider(&runtime);
    let add_to_playlist_callback =
        add_to_playlist_callback(&command_controller, &runtime, &library_changed_holder);
    let context_menu = TrackRowContextMenu::new(context_actions, parent_window.clone())
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
        );
    let playlist_context_actions = playlist_track_context_actions(
        &runtime,
        &parent_window,
        &show_album_holder,
        &command_controller,
        playback_changed.clone(),
        library_changed_holder.clone(),
        track_row_changed_holder.clone(),
        &sidebar,
    );
    let playlist_context_menu = TrackRowContextMenu::new(playlist_context_actions, parent_window)
        .with_add_to_playlist(add_to_playlist_provider, add_to_playlist_callback)
        .with_analyze_menu(
            track_analyze_run_callback(&runtime),
            analysis_enabled_query(&runtime),
        )
        .with_retrieve_menu(
            track_retrieve_run_callback(&runtime),
            online_busy_query(&runtime),
        );
    let rating_changed =
        rating_changed_callback(&command_controller, track_row_changed_holder.clone());
    let songs_inline_edit = inline_edit_hooks(
        &runtime,
        &command_controller,
        track_row_changed_holder.clone(),
    );
    let songs_table = build_track_table(
        library_tracks.clone(),
        Some(library_track_activated.clone()),
        Some(context_menu.clone()),
        Some(rating_changed.clone()),
        None,
        Some(songs_inline_edit),
    );
    tlog!("songs table populated");
    songs_table_holder.replace(Some(songs_table.clone()));
    let albums_view = AlbumsView::new(
        runtime.clone(),
        command_controller.clone(),
        playback_changed.clone(),
        context_menu,
        artwork_loader.clone(),
    );
    albums_view_holder.replace(Some(albums_view.clone()));
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
    );
    let playlists_table = build_track_table(
        Vec::new(),
        Some(playlist_track_activated),
        Some(playlist_context_menu),
        Some(rating_changed),
        Some(playlist_row_reorder),
        None,
    );
    playlists_table_holder.replace(Some(playlists_table.clone()));
    install_track_column_layout_persistence(&runtime, &songs_table, &playlists_table, &sidebar);
    playback_changed();
    tlog!("tables + playback wired");
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
    let statistics_view = StatisticsView::new(runtime.clone());
    let content_stack = build_content_stack(
        &songs_drop_overlay,
        &albums_view.widget(),
        &statistics_view.widget(),
        &playlists_view,
        device_panel.widget(),
    );
    install_albums_view_activator(&content_stack, &albums_view);
    install_statistics_view_activator(&content_stack, &statistics_view);
    install_device_sync_view(&content_stack, &sidebar, &device_panel, &runtime);
    // The playlists table is built empty. It only needs to be populated
    // when the user actually opens the Playlists view; rebuilding it on
    // every library_changed / selection change while Songs is visible
    // is wasted work and dominates startup time on large libraries
    // (measured: ~672ms for 8890 rows in `replace_rows`).
    let playlists_dirty: Rc<Cell<bool>> = Rc::new(Cell::new(true));
    install_playlists_view_activator(
        &content_stack,
        &runtime,
        &playlists_table,
        &playlists_header,
        &sidebar,
        &current_search_text,
        &playlists_dirty,
    );
    tlog!("content stack + activators installed");
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
        &sidebar,
        &status_bar,
        &current_search_text,
        device_panel.current_device_cell(),
    );
    device_panel.set_summary_refresh(visible_summary_refresh.clone());
    let library_changed = library_changed_callback(
        &runtime,
        &songs_table,
        &albums_view,
        &statistics_view,
        &sidebar,
        &titlebar,
        visible_summary_refresh.clone(),
        &current_search_text,
    );
    install_track_availability_observer(&runtime, &songs_table, &playlists_table);
    let track_row_changed = track_row_changed_callback(TrackRowChangedContext {
        runtime: &runtime,
        songs_table: &songs_table,
        albums_view: &albums_view,
        playlists_table: &playlists_table,
        playlists_header: &playlists_header,
        sidebar: &sidebar,
        content_stack: &content_stack,
        playlists_dirty: &playlists_dirty,
        visible_summary_refresh: visible_summary_refresh.clone(),
        current_search_text: &current_search_text,
        device_panel: &device_panel,
    });
    track_row_changed_holder.replace(Some(track_row_changed));
    install_metadata_writer_event_consumer(
        metadata_writer_event_rx,
        runtime.clone(),
        track_row_changed_holder.clone(),
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
    install_analysis_progress_consumer(analysis_progress_rx, runtime.clone());
    install_online_progress_consumer(online_progress_rx, runtime.clone());
    install_track_data_observer(&runtime, track_row_changed_holder.clone());
    install_track_updated_consumer(track_updated_rx, runtime.clone());
    install_smart_shuffle_rebuild_result_consumer(smart_shuffle_rebuild_result_rx, runtime.clone());
    install_device_sync_event_consumer(device_sync_event_rx, runtime.clone());
    install_smart_shuffle_launch_rebuild(&runtime);
    // The sidebar is now the sole navigation surface: its selection
    // chooses which content-stack page is visible (Music → SONGS_VIEW,
    // Albums → ALBUMS_VIEW, an Item → PLAYLISTS_VIEW). The non-default
    // selections are applied AFTER first-frame by
    // [`DeferredStartup`] so the cold-start budget covers only the
    // cheap Music page.
    sidebar.set_selection_changed(sidebar_selection_changed_callback(
        &runtime,
        &playlists_table,
        &playlists_header,
        &content_stack,
        &playlists_dirty,
        visible_summary_refresh.clone(),
        &current_search_text,
    ));
    let device_populate: Box<dyn FnOnce()> = {
        let sidebar = sidebar.clone();
        let runtime = runtime.clone();
        Box::new(move || {
            let devices = runtime.borrow().connected_devices();
            sidebar.set_devices(&devices);
        })
    };
    let resume_pending_metadata_writes: Box<dyn FnOnce()> = {
        let runtime = runtime.clone();
        Box::new(move || runtime.borrow().resume_pending_metadata_writes())
    };
    let deferred_startup = DeferredStartup::new(
        initial_ui_settings.sidebar_selection,
        sidebar.clone(),
        device_populate,
        resume_pending_metadata_writes,
    );
    install_search_wiring(
        &titlebar,
        SearchWiringContext {
            current_search_text: current_search_text.clone(),
            runtime: runtime.clone(),
            songs_table: songs_table.clone(),
            albums_view: albums_view.clone(),
            playlists_table: playlists_table.clone(),
            playlists_header: playlists_header.clone(),
            sidebar: sidebar.clone(),
            content_stack: content_stack.clone(),
            playlists_dirty: playlists_dirty.clone(),
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
    let consolidation_requested = library_consolidation_requested_callback(&runtime);
    let import_requested = library_import_requested_callback(&runtime, library_changed.clone());
    install_file_drop_target(&songs_drop_overlay, &songs_drop_indicator, import_requested);
    install_preferences_action(
        app,
        &window,
        command_controller.clone(),
        scan_requested.clone(),
        consolidation_requested.clone(),
    );

    let main_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    main_content.set_hexpand(true);
    main_content.set_vexpand(true);
    let command_controller_for_global_shortcuts = command_controller.clone();

    // Sidebar footer: the [cog] Settings button is the visual
    // entry-point to Preferences (the Ctrl+, accelerator is the power-
    // user path, registered separately by `install_preferences_action`).
    sidebar.footer().append(&settings_button(
        &window,
        command_controller,
        scan_requested,
        consolidation_requested.clone(),
    ));

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

    let albums_view_for_reveal = albums_view.clone();
    let sidebar_for_show_album = sidebar.clone();
    let show_album_action: ShowAlbumAction = Rc::new(move |track_id| {
        sidebar_for_show_album.select_albums();
        albums_view_for_reveal.reveal_album_for_track(track_id);
    });
    show_album_holder.replace(Some(show_album_action));
    install_keyboard_shortcuts(
        &window,
        KeyboardShortcutContext {
            toggle_or_start_playback: toggle_or_start_playback.clone(),
            runtime: runtime.clone(),
            songs_table: songs_table.clone(),
            playlists_table: playlists_table.clone(),
            albums_view: albums_view.clone(),
            content_stack: content_stack.clone(),
            sidebar: sidebar.clone(),
        },
    );
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
        content_stack: content_stack.clone(),
        library_changed_holder: library_changed_holder.clone(),
        track_row_changed_holder: track_row_changed_holder.clone(),
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
        if let Err(error) =
            runtime_for_close
                .borrow_mut()
                .save_ui_settings(ui_settings_from_widgets(
                    &titlebar_for_close,
                    &sidebar_for_close,
                    collapse_controller_for_close.is_collapsed(),
                    collapse_controller_for_close.expanded_width(),
                ))
        {
            eprintln!("sustain: failed to persist UI state on close: {error:?}");
        }
        glib::Propagation::Proceed
    });

    tlog!("widgets assembled");
    // If "Keep my library organized" is on, schedule consolidation
    // immediately. Idempotent (empty plan when already organized) and
    // the natural resume point for a previous run interrupted by a
    // kill or crash.
    maybe_auto_resume_library_consolidation(&runtime, &consolidation_requested);
    tlog!("auto-resume kicked off");

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
    restore_selection: Option<Box<dyn FnOnce()>>,
    populate_devices: Box<dyn FnOnce()>,
    resume_pending_metadata_writes: Box<dyn FnOnce()>,
}

impl DeferredStartup {
    fn new(
        selection: UiSidebarSelection,
        sidebar: PlaylistSidebar,
        populate_devices: Box<dyn FnOnce()>,
        resume_pending_metadata_writes: Box<dyn FnOnce()>,
    ) -> Self {
        let restore_selection: Option<Box<dyn FnOnce()>> = match selection {
            UiSidebarSelection::Music => None,
            UiSidebarSelection::Albums => Some(Box::new(move || sidebar.select_albums())),
            UiSidebarSelection::Statistics => Some(Box::new(move || sidebar.select_statistics())),
            UiSidebarSelection::Playlist(item) => Some(Box::new(move || sidebar.select_item(item))),
        };
        Self {
            restore_selection,
            populate_devices,
            resume_pending_metadata_writes,
        }
    }

    fn run(self) {
        if let Some(restore) = self.restore_selection {
            restore();
        }
        // Device enumeration probes the filesystem, so it runs here on
        // the first idle rather than during the cold-start window.
        (self.populate_devices)();
        // Restored mirror retries can parse audio tags and garbage-collect
        // external artwork blobs. Start them after the first-idle budget gate.
        (self.resume_pending_metadata_writes)();
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
/// Wire the DEVICES sidebar section to the device-sync panel and keep
/// the device list live.
///
/// Selecting a device shows its panel and flips the content stack to it;
/// switching to any other page clears the device row highlight so only
/// one navigation surface looks active at a time.
///
/// Discovery is otherwise run once, on the first idle, which would miss a
/// stick plugged in (or auto-mounted by udisks moments) after launch.
/// GIO's [`gio::VolumeMonitor`] is the native mount/unmount source on the
/// session; each event re-runs the cheap `/proc/mounts` discovery and
/// rebuilds the section. The monitor is a process singleton that GIO
/// finalises once its last reference drops (its internal pointer is
/// weak), which would silence further events — so it is parked in the
/// content-stack notify closure below, owned for the whole session and
/// freed with the UI. Anchoring it there rather than on the sidebar
/// avoids a sidebar↔monitor reference cycle.
fn install_device_sync_view(
    content_stack: &gtk::Stack,
    sidebar: &PlaylistSidebar,
    device_panel: &DeviceSyncPanel,
    runtime: &SharedRuntime,
) {
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

    let refresh_devices: Rc<dyn Fn()> = {
        let sidebar = sidebar.clone();
        let runtime = runtime.clone();
        Rc::new(move || sidebar.set_devices(&runtime.borrow().connected_devices()))
    };
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
        // singleton lives as long as the content stack; see the doc
        // comment above.
        let _keep_monitor_alive = &volume_monitor;
        // Leaving a transient view's page (the device panel today) drops
        // its sidebar highlight so the persistent selection shows through.
        if stack.visible_child_name().as_deref() != Some(DEVICES_VIEW) {
            sidebar.clear_transient_highlight();
        }
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
    }
}

#[allow(clippy::too_many_arguments)]
fn library_changed_callback(
    runtime: &SharedRuntime,
    songs_table: &TrackTable,
    albums_view: &AlbumsView,
    statistics_view: &StatisticsView,
    sidebar: &PlaylistSidebar,
    titlebar: &Titlebar,
    visible_summary_refresh: VisibleSummaryRefreshCallback,
    current_search_text: &Rc<RefCell<String>>,
) -> LibraryChangedCallback {
    let runtime = runtime.clone();
    let songs_table = songs_table.clone();
    let albums_view = albums_view.clone();
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
    sidebar: &PlaylistSidebar,
    status_bar: &StatusBar,
    current_search_text: &Rc<RefCell<String>>,
    current_device: Rc<RefCell<Option<ConnectedDevice>>>,
) -> VisibleSummaryRefreshCallback {
    let runtime = runtime.clone();
    let content_stack = content_stack.clone();
    let sidebar = sidebar.clone();
    let status_bar = status_bar.clone();
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
        let search_text = current_search_text.borrow().clone();
        let rows = visible_view_rows(
            &runtime.borrow(),
            &content_stack,
            sidebar.current_selection(),
            &search_text,
        );
        status_bar.update_summary(&rows);
    })
}

fn visible_view_rows(
    runtime: &ApplicationRuntime,
    content_stack: &gtk::Stack,
    sidebar_selection: Option<SidebarSelection>,
    search_text: &str,
) -> Vec<TrackTableRow> {
    if content_stack.visible_child_name().as_deref() == Some(PLAYLISTS_VIEW) {
        playlist_table_rows_for(runtime, sidebar_selection, search_text)
    } else {
        runtime_library_table_rows(runtime, search_text)
    }
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
