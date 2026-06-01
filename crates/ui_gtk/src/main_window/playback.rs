// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Playback wiring: the now-playing/titlebar/MPRIS refresh callback, queue
//! construction for the Songs/Albums/Playlists views, the playlists-header
//! play button, the toggle-or-start play action, play/pause sensitivity, and
//! the end-of-stream auto-advance hook.

use super::*;

pub(super) fn playback_changed_callback(
    runtime: &SharedRuntime,
    now_playing: &NowPlayingView,
    titlebar: &Titlebar,
    songs_table_holder: Rc<RefCell<Option<TrackTable>>>,
    albums_view_holder: Rc<RefCell<Option<AlbumsView>>>,
    playlists_table_holder: Rc<RefCell<Option<TrackTable>>>,
    mpris_service: Option<SharedMprisService>,
) -> PlaybackChangedCallback {
    let runtime = runtime.clone();
    let now_playing = now_playing.clone();
    let titlebar = titlebar.clone();

    Rc::new(move || {
        let now_playing_state = runtime.borrow().now_playing();
        sync_play_pause_icon(&titlebar.play_pause_icon, &now_playing_state.state);
        // A track loading/clearing changes whether Play resumes a current
        // track or must cold-start the visible view.
        update_play_pause_sensitivity(&titlebar, &runtime.borrow());
        let playing_track_id = now_playing_state.track.as_ref().map(|track| track.id);
        if let Some(songs_table) = songs_table_holder.borrow().as_ref() {
            songs_table.set_playing_track_id(playing_track_id);
        }
        if let Some(albums_view) = albums_view_holder.borrow().as_ref() {
            albums_view.set_playing_track_id(playing_track_id);
        }
        if let Some(playlists_table) = playlists_table_holder.borrow().as_ref() {
            playlists_table.set_playing_track_id(playing_track_id);
        }
        now_playing.refresh(&now_playing_state);
        if let Some(service) = mpris_service.as_deref() {
            service.publish_playback_state(now_playing_state.state.clone());
            service.publish_now_playing(now_playing_to_mpris_metadata(&now_playing_state));
        }
    })
}

/// Activation handler for the Songs view: the queue is the whole library,
/// so auto-advance and Next/Previous walk all playable tracks in library
/// order. Matches the iTunes 11 "Music" library default.
pub(super) fn library_track_activated_callback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    playback_changed: PlaybackChangedCallback,
    current_search_text: &Rc<RefCell<String>>,
    locate_missing_track: &LocateMissingTrackCallback,
) -> TrackActivatedCallback {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let current_search_text = current_search_text.clone();
    let locate_missing_track = locate_missing_track.clone();

    Rc::new(move |track_id: TrackId| {
        let queue = {
            let search_text = current_search_text.borrow().clone();
            queue_request_for_library(&runtime.borrow(), &search_text)
        };
        play_track_or_offer_locate(
            &command_controller,
            track_id,
            queue,
            &playback_changed,
            &locate_missing_track,
        );
    })
}

/// Activation handler for the Playlists view: the queue is whatever the
/// sidebar currently has selected.
///
/// - Regular playlist: queue is the playlist's entries in their
///   authoritative position order, so auto-advance stays inside the
///   playlist and replays it in the user-defined sequence.
/// - Smart playlist: queue is the smart playlist's current matching
///   tracks. The runtime's `PlaybackQueueSource::Selection` is used as
///   the source label (we don't yet model a smart-playlist source
///   variant) but the play order is the smart playlist's order.
/// - Library pseudo-entry: queue is the full (search-filtered)
///   library, matching the Songs view's behavior.
///
/// Any other selection (folders, no selection) falls back to the Library
/// queue — those targets don't activate tracks in normal use, but a
/// fallback keeps playback predictable if a future code path ever
/// double-clicks a row in that state.
pub(super) fn playlist_track_activated_callback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
    playback_changed: PlaybackChangedCallback,
    current_search_text: &Rc<RefCell<String>>,
    locate_missing_track: &LocateMissingTrackCallback,
) -> TrackActivatedCallback {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();
    let current_search_text = current_search_text.clone();
    let locate_missing_track = locate_missing_track.clone();

    Rc::new(move |track_id: TrackId| {
        let queue = {
            let search_text = current_search_text.borrow().clone();
            let runtime_borrow = runtime.borrow();
            queue_request_for_playlist_selection(
                &runtime_borrow,
                sidebar.current_selection(),
                &search_text,
            )
        };
        play_track_or_offer_locate(
            &command_controller,
            track_id,
            queue,
            &playback_changed,
            &locate_missing_track,
        );
    })
}

fn queue_request_for_playlist_selection(
    runtime: &ApplicationRuntime,
    selection: Option<SidebarSelection>,
    search_text: &str,
) -> PlaybackQueueRequest {
    if matches!(selection, Some(SidebarSelection::Music)) {
        return queue_request_for_library(runtime, search_text);
    }

    let candidates: Vec<(Track, PlaybackQueueSource)> = match selection {
        Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) => {
            let Some(playlist) = runtime
                .playlists()
                .iter()
                .find(|playlist| playlist.id == playlist_id)
            else {
                return PlaybackQueueRequest::Library;
            };
            let tracks_by_id: HashMap<TrackId, &Track> = runtime
                .library_tracks()
                .iter()
                .map(|track| (track.id, track))
                .collect();
            let mut entries: Vec<&PlaylistEntry> = playlist.entries.iter().collect();
            entries.sort_by_key(|entry| entry.position);
            let source = PlaybackQueueSource::Playlist(playlist_id);
            entries
                .into_iter()
                .filter_map(|entry| {
                    tracks_by_id
                        .get(&entry.track_id)
                        .copied()
                        .cloned()
                        .map(|track| (track, source.clone()))
                })
                .collect()
        }
        Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(smart_playlist_id))) => runtime
            .smart_playlist_matching_tracks(smart_playlist_id)
            .into_iter()
            .map(|track| (track.clone(), PlaybackQueueSource::Selection))
            .collect(),
        _ => return queue_request_for_library(runtime, search_text),
    };

    let source = match candidates.first() {
        Some((_, source)) => source.clone(),
        None => return PlaybackQueueRequest::Library,
    };
    let ordered_track_ids: Vec<TrackId> = candidates
        .into_iter()
        .filter(|(track, _)| {
            search_text.is_empty() || track_matches_search_text(track, search_text)
        })
        .map(|(track, _)| track.id)
        .collect();
    PlaybackQueueRequest::Explicit {
        source,
        ordered_track_ids,
    }
}

fn queue_request_for_library(
    runtime: &ApplicationRuntime,
    search_text: &str,
) -> PlaybackQueueRequest {
    if search_text.trim().is_empty() {
        return PlaybackQueueRequest::Library;
    }
    let ordered_track_ids = runtime
        .library_tracks()
        .iter()
        .filter(|track| track_matches_search_text(track, search_text))
        .map(|track| track.id)
        .collect();
    PlaybackQueueRequest::Explicit {
        source: PlaybackQueueSource::SearchResults,
        ordered_track_ids,
    }
}

/// Wires the Playlists header's play and shuffle buttons to start
/// playback from the sidebar's current selection, matching the album
/// detail header's play/shuffle behaviour: the first non-missing track
/// in the queue's display order is the one PlayTrack anchors on; the
/// shuffle toggle decides what comes after.
pub(super) fn install_playlists_header_playback(
    playlists_header: &PlaylistsHeader,
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
    current_search_text: &Rc<RefCell<String>>,
    playback_changed: &PlaybackChangedCallback,
) {
    playlists_header.connect_play(make_playlists_header_play_callback(
        false,
        command_controller,
        runtime,
        sidebar,
        current_search_text,
        playback_changed,
    ));
    playlists_header.connect_shuffle(make_playlists_header_play_callback(
        true,
        command_controller,
        runtime,
        sidebar,
        current_search_text,
        playback_changed,
    ));
}

fn make_playlists_header_play_callback(
    shuffle: bool,
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
    current_search_text: &Rc<RefCell<String>>,
    playback_changed: &PlaybackChangedCallback,
) -> Rc<dyn Fn()> {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();
    let current_search_text = current_search_text.clone();
    let playback_changed = playback_changed.clone();
    Rc::new(move || {
        // Set shuffle first so subsequent `PlayTrack` builds its queue
        // with the right ordering. Both dispatches are independent —
        // the runtime does not coalesce them. The playlist header's
        // Shuffle button pins the queue to Pure random regardless of
        // the transport setting — Smart's library-wide signals are
        // not the right fit for "shuffle this playlist's tracks".
        let shuffle_mode = if shuffle {
            ShuffleMode::Pure
        } else {
            ShuffleMode::Off
        };
        let _ = command_controller.dispatch(ApplicationCommand::Playback(
            PlaybackCommand::SetShuffleMode(shuffle_mode),
        ));
        let (queue, first_track) = {
            let runtime_borrow = runtime.borrow();
            let search_text = current_search_text.borrow().clone();
            let queue = queue_request_for_playlist_selection(
                &runtime_borrow,
                sidebar.current_selection(),
                &search_text,
            );
            let first_track = first_playable_track_for_queue(&runtime_borrow, &queue);
            (queue, first_track)
        };
        let Some(track_id) = first_track else {
            return;
        };
        if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
            PlaybackCommand::PlayTrack { track_id, queue },
        )) {
            playback_changed();
        }
    })
}

fn first_playable_track_for_queue(
    runtime: &ApplicationRuntime,
    queue: &PlaybackQueueRequest,
) -> Option<TrackId> {
    let library = runtime.library_tracks();
    match queue {
        PlaybackQueueRequest::Library => library
            .iter()
            .find(|track| !track.location.is_missing())
            .map(|track| track.id),
        PlaybackQueueRequest::Explicit {
            ordered_track_ids, ..
        } => {
            let missing: HashMap<TrackId, bool> = library
                .iter()
                .map(|track| (track.id, track.location.is_missing()))
                .collect();
            ordered_track_ids
                .iter()
                .copied()
                .find(|id| matches!(missing.get(id), Some(false)))
        }
    }
}

/// Build the closure that backs both the top-bar Play button and the
/// Space shortcut. When a track is loaded it toggles play/pause; on a
/// cold start (controller stopped, nothing loaded) it begins playback
/// from the currently visible view — Songs from the current
/// sort/filter, Albums from the first album, Playlists from the selected
/// playlist (falling back to the library). See issue #60.
pub(super) fn make_toggle_or_start_playback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    content_stack: &gtk::Stack,
    albums_view: &AlbumsView,
    sidebar: &PlaylistSidebar,
    current_search_text: &Rc<RefCell<String>>,
    playback_changed: &PlaybackChangedCallback,
) -> Rc<dyn Fn()> {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let content_stack = content_stack.clone();
    let albums_view = albums_view.clone();
    let sidebar = sidebar.clone();
    let current_search_text = current_search_text.clone();
    let playback_changed = playback_changed.clone();

    Rc::new(move || {
        // `Stopped` is the authoritative "no track is loaded in the
        // controller" signal: a paused/playing track resumes as usual,
        // and only a genuine cold start cold-starts the visible view.
        let is_stopped = matches!(runtime.borrow().playback_state(), PlaybackState::Stopped);
        let dispatched = if is_stopped {
            let request = {
                let runtime_borrow = runtime.borrow();
                let search_text = current_search_text.borrow().clone();
                play_request_for_visible_view(
                    &runtime_borrow,
                    &content_stack,
                    &albums_view,
                    sidebar.current_selection(),
                    &search_text,
                )
            };
            match request {
                Some((track_id, queue)) => command_controller.dispatch_succeeded(
                    ApplicationCommand::Playback(PlaybackCommand::PlayTrack { track_id, queue }),
                ),
                None => false,
            }
        } else {
            command_controller.dispatch_succeeded(ApplicationCommand::Playback(
                PlaybackCommand::TogglePlayPause,
            ))
        };
        if dispatched {
            playback_changed();
        }
    })
}

/// Resolve "what would Play start right now?" from the view the user is
/// currently looking at. Derived at click time — switching modes before
/// pressing Play changes which track starts. Returns `None` when the
/// visible view has no playable track to anchor on. See issue #60.
fn play_request_for_visible_view(
    runtime: &ApplicationRuntime,
    content_stack: &gtk::Stack,
    albums_view: &AlbumsView,
    sidebar_selection: Option<SidebarSelection>,
    search_text: &str,
) -> Option<(TrackId, PlaybackQueueRequest)> {
    match content_stack.visible_child_name().as_deref() {
        Some(ALBUMS_VIEW) => albums_view.first_album_play_request(),
        Some(PLAYLISTS_VIEW) => {
            let queue =
                queue_request_for_playlist_selection(runtime, sidebar_selection, search_text);
            first_playable_track_for_queue(runtime, &queue).map(|track_id| (track_id, queue))
        }
        // Songs view (and any unexpected state) plays the full
        // search-filtered library, matching double-click activation.
        _ => {
            let queue = queue_request_for_library(runtime, search_text);
            first_playable_track_for_queue(runtime, &queue).map(|track_id| (track_id, queue))
        }
    }
}

/// Resolve the queue request that should re-derive the play queue when the
/// search filter is cleared, based on the view the user is in: Songs widens
/// to the full library, Playlists widens to the full selected playlist /
/// smart playlist. Albums return `None` — album playback always queues the
/// whole album and is never search-narrowed, so there is nothing to widen
/// (#78). The runtime applies the request only while a track is playing and
/// only when it still contains that track, so a stale view never clobbers
/// the queue.
pub(super) fn repopulate_request_for_visible_view(
    runtime: &ApplicationRuntime,
    content_stack: &gtk::Stack,
    sidebar_selection: Option<SidebarSelection>,
) -> Option<PlaybackQueueRequest> {
    match content_stack.visible_child_name().as_deref() {
        Some(ALBUMS_VIEW) => None,
        Some(PLAYLISTS_VIEW) => Some(queue_request_for_playlist_selection(
            runtime,
            sidebar_selection,
            "",
        )),
        // Songs view (and any unexpected state) widens to the full library,
        // matching its play/activation behaviour.
        _ => Some(queue_request_for_library(runtime, "")),
    }
}

/// Enable the top-bar Play button when there is something it can act on
/// — a track already loaded in the controller (so it pauses/resumes) or
/// at least one track in the library (so it cold-starts the visible
/// view). Disabled only when the library is empty and nothing is loaded,
/// so a press is never a silent no-op. See issue #60.
pub(super) fn update_play_pause_sensitivity(titlebar: &Titlebar, runtime: &ApplicationRuntime) {
    let has_current_track = !matches!(runtime.playback_state(), PlaybackState::Stopped);
    let library_has_tracks = !runtime.library_tracks().is_empty();
    titlebar.set_play_pause_sensitive(has_current_track || library_has_tracks);
}

pub(super) fn install_track_ended_callback(
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
    playback_changed: &PlaybackChangedCallback,
) {
    let command_controller = command_controller.clone();
    let playback_changed = playback_changed.clone();
    // The bus watch fires from glib's main context, the same thread that
    // services GTK events. Dispatching PlayNextTrack therefore happens at a
    // quiescent point, so no other borrow of the runtime can be in flight.
    runtime.borrow().set_track_ended_callback(Box::new(move || {
        if command_controller
            .dispatch_succeeded(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack))
        {
            playback_changed();
        }
    }));
}
