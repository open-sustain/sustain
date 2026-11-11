// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Playback wiring: the now-playing/titlebar/MPRIS refresh callback, queue
//! construction for the Songs/Albums/Playlists views, the playlists-header
//! play button, the toggle-or-start play action, play/pause sensitivity, and
//! the end-of-stream auto-advance hook.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn playback_changed_callback(
    runtime: &SharedRuntime,
    now_playing: &NowPlayingView,
    titlebar: &Titlebar,
    songs_table_holder: Rc<RefCell<Option<TrackTable>>>,
    albums_view_holder: Rc<RefCell<Option<AlbumsView>>>,
    playlists_table_holder: Rc<RefCell<Option<TrackTable>>>,
    queue_view_holder: Rc<RefCell<Option<QueueView>>>,
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
        // Advancing the current track shifts the queue tail; keep an open
        // queue popover in sync (a no-op when it is closed).
        if let Some(queue_view) = queue_view_holder.borrow().as_ref() {
            queue_view.refresh_if_visible();
        }
        if let Some(service) = mpris_service.as_deref() {
            service.publish_playback_state(now_playing_state.state.clone());
            service.publish_now_playing(now_playing_to_mpris_metadata(&now_playing_state));
        }
    })
}

/// The queue source of the Songs list: the library, or under a search the
/// results the user sees.
pub(super) fn songs_queue_source(search_text: &str) -> PlaybackQueueSource {
    searched_queue_source(PlaybackQueueSource::Library, search_text)
}

/// The queue source of the Playlists table for a sidebar selection: the
/// selected playlist or smart playlist, or under a search the results the
/// user sees. `None` for a selection that lists no track set — a folder.
pub(super) fn playlists_queue_source(
    selection: Option<SidebarSelection>,
    search_text: &str,
) -> Option<PlaybackQueueSource> {
    let listed = match selection? {
        SidebarSelection::Item(PlaylistItem::Playlist(playlist_id)) => {
            PlaybackQueueSource::Playlist(playlist_id)
        }
        SidebarSelection::Item(PlaylistItem::SmartPlaylist(smart_playlist_id)) => {
            PlaybackQueueSource::SmartPlaylist(smart_playlist_id)
        }
        // The table mirrors the Songs rows for the Music entry.
        SidebarSelection::Music => PlaybackQueueSource::Library,
        SidebarSelection::Item(PlaylistItem::Folder(_))
        | SidebarSelection::Albums
        | SidebarSelection::Statistics => return None,
    };
    Some(searched_queue_source(listed, search_text))
}

/// A search narrows a list to a slice of what it lists: the queue the
/// slice defines is those search results, not the listed source itself,
/// so library changes never widen it back (see `RepopulateQueue`).
fn searched_queue_source(listed: PlaybackQueueSource, search_text: &str) -> PlaybackQueueSource {
    let query = normalize_query(search_text);
    if query.is_empty() {
        listed
    } else {
        PlaybackQueueSource::SearchResults {
            within: Box::new(listed),
            query,
        }
    }
}

/// Activation handler for a track table: playback starts at the activated
/// row and continues through the table's rows in display order, so
/// without shuffle the next track is the next row. `queue_source` names
/// what the table lists at activation time; `None` (a list with no track
/// set) ignores the activation.
pub(super) fn track_activated_callback(
    command_controller: &SharedCommandController,
    playback_changed: PlaybackChangedCallback,
    locate_missing_track: &LocateMissingTrackCallback,
    queue_source: impl Fn() -> Option<PlaybackQueueSource> + 'static,
) -> TrackActivatedCallback {
    let command_controller = command_controller.clone();
    let locate_missing_track = locate_missing_track.clone();

    Rc::new(move |activation: TrackActivation| {
        let Some(source) = queue_source() else {
            return;
        };
        play_track_or_offer_locate(
            &command_controller,
            activation.track_id,
            PlaybackQueueRequest {
                source,
                ordered_track_ids: activation.ordered_track_ids,
            },
            &playback_changed,
            &locate_missing_track,
        );
    })
}

/// Re-sorting a list re-sorts the queue it defines: the runtime adopts
/// the new order when the queue was started from this list (see
/// [`PlaybackCommand::ResortQueue`]).
pub(super) fn install_queue_resort(
    table: &TrackTable,
    command_controller: &SharedCommandController,
    queue_source: impl Fn() -> Option<PlaybackQueueSource> + 'static,
) {
    let command_controller = command_controller.clone();
    table.set_rows_reordered_callback(Rc::new(move |ordered_track_ids| {
        let Some(source) = queue_source() else {
            return;
        };
        let _ = command_controller.dispatch(ApplicationCommand::Playback(
            PlaybackCommand::ResortQueue(PlaybackQueueRequest {
                source,
                ordered_track_ids,
            }),
        ));
    }));
}

/// Wires the Playlists header's play and shuffle buttons to start
/// playback from the Playlists table, matching the album detail header's
/// play/shuffle behaviour: the first non-missing track in display order
/// is the one PlayTrack anchors on; the shuffle toggle decides what comes
/// after.
pub(super) fn install_playlists_header_playback(
    playlists_header: &PlaylistsHeader,
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    view_queues: &VisibleViewQueues,
    playback_changed: &PlaybackChangedCallback,
) {
    playlists_header.connect_play(make_playlists_header_play_callback(
        false,
        command_controller,
        runtime,
        view_queues,
        playback_changed,
    ));
    playlists_header.connect_shuffle(make_playlists_header_play_callback(
        true,
        command_controller,
        runtime,
        view_queues,
        playback_changed,
    ));
}

fn make_playlists_header_play_callback(
    shuffle: bool,
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    view_queues: &VisibleViewQueues,
    playback_changed: &PlaybackChangedCallback,
) -> Rc<dyn Fn()> {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let view_queues = view_queues.clone();
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
        let Some(queue) = view_queues.playlists_queue_request() else {
            return;
        };
        let Some(track_id) = first_playable_track(&runtime.borrow(), &queue) else {
            return;
        };
        if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
            PlaybackCommand::PlayTrack { track_id, queue },
        )) {
            playback_changed();
        }
    })
}

fn first_playable_track(
    runtime: &ApplicationRuntime,
    queue: &PlaybackQueueRequest,
) -> Option<TrackId> {
    queue.ordered_track_ids.iter().copied().find(|track_id| {
        runtime
            .library_track(*track_id)
            .is_some_and(|track| !track.location.is_missing())
    })
}

/// The track lists the top-bar Play button and a cleared search reach
/// for: whichever view is visible defines the queue, in the order it
/// lists its tracks.
#[derive(Clone)]
pub(super) struct VisibleViewQueues {
    pub(super) content_stack: gtk::Stack,
    pub(super) songs_refresh: SongsViewRefresh,
    pub(super) albums_view: AlbumsView,
    pub(super) playlists_table: TrackTable,
    pub(super) duplicates_view: DuplicatesView,
    pub(super) sidebar: PlaylistSidebar,
    pub(super) current_search_text: Rc<RefCell<String>>,
}

impl VisibleViewQueues {
    /// "What would Play start right now?", resolved from the view the user
    /// is looking at when they press it: the first playable track of the
    /// visible list, or of the first album. `None` when that view has no
    /// playable track to anchor on. See issue #60.
    fn play_request(&self, runtime: &SharedRuntime) -> Option<(TrackId, PlaybackQueueRequest)> {
        if self.visible_view().as_deref() == Some(ALBUMS_VIEW) {
            return self.albums_view.first_album_play_request();
        }
        // Resolved before the runtime is borrowed: bringing a stale Songs
        // list up to date rebuilds its rows.
        let queue = self.listed_queue_request()?;
        let track_id = first_playable_track(&runtime.borrow(), &queue)?;
        Some((track_id, queue))
    }

    /// The queue a cleared search widens to: the visible list, now
    /// unfiltered. `None` for the views a search does not narrow into a
    /// queue — album playback always queues the whole album and is never
    /// search-narrowed (#78), the duplicate groups ignore the search, and
    /// the other views list no tracks. The runtime applies the request only
    /// while a track is playing and only when it still contains that track,
    /// so a stale view never clobbers the queue.
    pub(super) fn widened_queue_request(&self) -> Option<PlaybackQueueRequest> {
        match self.visible_view().as_deref() {
            Some(SONGS_VIEW) => Some(self.songs_refresh.queue_request()),
            Some(PLAYLISTS_VIEW) => self.playlists_queue_request(),
            _ => None,
        }
    }

    fn playlists_queue_request(&self) -> Option<PlaybackQueueRequest> {
        let source = playlists_queue_source(
            self.sidebar.current_selection(),
            &self.current_search_text.borrow(),
        )?;
        Some(PlaybackQueueRequest {
            source,
            ordered_track_ids: self.playlists_table.ordered_track_ids(),
        })
    }

    /// The queue the visible track list defines. Songs, and the views that
    /// list no tracks of their own (Statistics, devices, CD import), queue
    /// the library as the Songs view lists it.
    fn listed_queue_request(&self) -> Option<PlaybackQueueRequest> {
        match self.visible_view().as_deref() {
            Some(ALBUMS_VIEW) => None,
            Some(PLAYLISTS_VIEW) => self.playlists_queue_request(),
            Some(DUPLICATES_VIEW) => Some(PlaybackQueueRequest {
                source: PlaybackQueueSource::Duplicates,
                ordered_track_ids: self.duplicates_view.ordered_track_ids(),
            }),
            _ => Some(self.songs_refresh.queue_request()),
        }
    }

    fn visible_view(&self) -> Option<glib::GString> {
        self.content_stack.visible_child_name()
    }
}

/// Build the closure that backs both the top-bar Play button and the
/// Space shortcut. When a track is loaded it toggles play/pause; on a
/// cold start (controller stopped, nothing loaded) it begins playback
/// from the currently visible view (see [`VisibleViewQueues`]).
pub(super) fn make_toggle_or_start_playback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    view_queues: &VisibleViewQueues,
    playback_changed: &PlaybackChangedCallback,
) -> Rc<dyn Fn()> {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let view_queues = view_queues.clone();
    let playback_changed = playback_changed.clone();

    Rc::new(move || {
        // `Stopped` is the authoritative "no track is loaded in the
        // controller" signal: a paused/playing track resumes as usual,
        // and only a genuine cold start cold-starts the visible view.
        let is_stopped = matches!(runtime.borrow().playback_state(), PlaybackState::Stopped);
        let dispatched = if is_stopped {
            match view_queues.play_request(&runtime) {
                Some((track_id, queue)) => {
                    if queue.source == PlaybackQueueSource::Album {
                        crate::albums::ensure_shuffle_disabled(&command_controller);
                    }
                    command_controller.dispatch_succeeded(ApplicationCommand::Playback(
                        PlaybackCommand::PlayTrack { track_id, queue },
                    ))
                }
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

pub(super) fn install_playback_event_callback(
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
    playback_changed: &PlaybackChangedCallback,
) {
    let runtime_for_callback = runtime.clone();
    let command_controller = command_controller.clone();
    let playback_changed = playback_changed.clone();
    // The bus watch fires from glib's main context, the same thread that
    // services GTK events. Dispatching therefore happens at a quiescent
    // point, so no other borrow of the runtime can be in flight.
    runtime
        .borrow()
        .set_playback_event_callback(Box::new(move |event| match event {
            PlaybackEvent::TrackEnded => {
                if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
                    PlaybackCommand::PlayNextTrack,
                )) {
                    playback_changed();
                }
            }
            PlaybackEvent::FatalError(error) => {
                let _ = runtime_for_callback
                    .borrow_mut()
                    .handle_playback_backend_error(error);
                playback_changed();
            }
            PlaybackEvent::PauseRequested => {
                let _ = runtime_for_callback
                    .borrow_mut()
                    .handle_playback_pause_request();
                playback_changed();
            }
        }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_turns_a_listed_source_into_search_results() {
        let playlist_id = PlaylistId::new(1).expect("valid id");
        let selection = Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id)));

        assert_eq!(songs_queue_source("  "), PlaybackQueueSource::Library);
        assert_eq!(
            songs_queue_source(" ABBA "),
            PlaybackQueueSource::SearchResults {
                within: Box::new(PlaybackQueueSource::Library),
                query: "abba".to_owned(),
            }
        );
        assert_eq!(
            playlists_queue_source(selection, ""),
            Some(PlaybackQueueSource::Playlist(playlist_id))
        );
        assert_eq!(
            playlists_queue_source(selection, "abba"),
            Some(PlaybackQueueSource::SearchResults {
                within: Box::new(PlaybackQueueSource::Playlist(playlist_id)),
                query: "abba".to_owned(),
            })
        );
    }

    #[test]
    fn a_folder_lists_no_queue() {
        let folder_id = PlaylistFolderId::new(1).expect("valid id");

        assert_eq!(
            playlists_queue_source(
                Some(SidebarSelection::Item(PlaylistItem::Folder(folder_id))),
                ""
            ),
            None
        );
    }
}
