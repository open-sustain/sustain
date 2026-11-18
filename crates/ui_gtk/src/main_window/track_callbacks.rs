// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Track-table row interactions: inline cell editing, rating clicks, the
//! per-row refresh callback, the Songs and Playlist context-menu action sets,
//! drag-to-reorder within a playlist, and the remove-from-playlist /
//! generic track-mutation helpers.

use std::collections::{HashMap, HashSet};

use crate::{
    ArtworkChangedCallback, LibraryViewUpdateCallback,
    library_view_update::{AlbumsUpdate, DuplicatesUpdate, LibraryViewUpdate},
};

use super::*;

/// Opens inline edit sessions for the library track tables. A session is
/// bound to the row's `TrackId` when it opens. It is seeded with the
/// field's authoritative value from the runtime (so Title is seeded with
/// the real tag, not the file-stem fallback the row displays). Its commit
/// funnels through the exact same `UpdateMetadata` write path the File Info
/// dialog uses — SQLite stays authoritative and the file tags are mirrored —
/// and the views follow the change the runtime commits.
pub(super) fn begin_inline_edit_callback(
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
) -> BeginEditCallback {
    let runtime = runtime.clone();
    let command_controller = command_controller.clone();
    Rc::new(move |object: &glib::Object, field: EditableField| {
        let track_id = crate::track_table::row_track_id(Some(object.clone()))?;
        let seed =
            track_metadata(&runtime, track_id).map(|metadata| field.seed_value(&metadata))?;
        let suggestions = match field {
            EditableField::Genre => runtime.borrow().distinct_genres(),
            _ => Vec::new(),
        };
        let runtime = runtime.clone();
        let command_controller = command_controller.clone();
        let commit = move |new_text: String| {
            // The track may have left the library while the editor was open.
            let Some(initial) = track_metadata(&runtime, track_id) else {
                return;
            };
            let change = match field.metadata_change(&initial, &new_text) {
                Ok(change) => change,
                Err(message) => {
                    // The cell shows the stored value again; say why.
                    command_controller.report_command_warning(message);
                    return;
                }
            };
            if change == MetadataChange::default() {
                // Re-typed the same value: no write.
                return;
            }
            // A refused edit is reported by the dispatch; a committed one
            // reaches the views as a runtime change.
            let _result = command_controller.dispatch(ApplicationCommand::UpdateMetadata {
                track_id,
                change: Box::new(change),
            });
        };
        Some(EditSession {
            seed,
            suggestions,
            commit: Box::new(commit),
        })
    })
}

fn track_metadata(runtime: &SharedRuntime, track_id: TrackId) -> Option<TrackMetadata> {
    runtime
        .borrow()
        .library_track(track_id)
        .map(|track| track.metadata.clone())
}

pub(super) fn rating_changed_callback(
    command_controller: &SharedCommandController,
) -> RatingChangedCallback {
    let command_controller = command_controller.clone();

    Rc::new(move |track_id: TrackId, rating: Rating| {
        command_controller.dispatch_succeeded(ApplicationCommand::SetRating { track_id, rating })
    })
}

/// Applies the view work for committed library changes.
///
/// Each touched track's rows are brought up to date with the runtime in
/// both track tables: in place while the track is in the library, gone
/// once it is not, and in at their sorted place when Songs lists the track
/// (it matches the search) but does not show it. A table rebuilt from the
/// runtime since the change was recorded already shows its outcome, which
/// is why this compares with what each table shows rather than trusting
/// what the change was. An edited row stays where it is even when the edit
/// no longer matches the search, so a row the user just edited does not
/// vanish under them.
///
/// Rows never interleave with Songs' streamed startup publication: the
/// runtime commits no change before that publication finishes.
///
/// Albums and Duplicates take the cheapest correct refresh the update calls
/// for; Statistics refreshes when it aggregates anything that changed. A
/// shown regular playlist whose entries changed reconciles with the
/// runtime, and a shown smart playlist does whenever its membership may
/// have changed — an in-place row update would lie.
pub(super) struct LibraryViewUpdateContext<'a> {
    pub(super) runtime: &'a SharedRuntime,
    pub(super) songs_table: &'a TrackTable,
    pub(super) albums_view: &'a AlbumsView,
    pub(super) duplicates_view: &'a DuplicatesView,
    pub(super) statistics_view: &'a StatisticsView,
    pub(super) playlists_table: &'a TrackTable,
    pub(super) playlists_refresh: &'a PlaylistsViewRefreshContext,
    pub(super) sidebar: &'a PlaylistSidebar,
    pub(super) titlebar: &'a Titlebar,
    pub(super) visible_summary_refresh: VisibleSummaryRefreshCallback,
    pub(super) current_search_text: &'a Rc<RefCell<String>>,
    pub(super) device_panel: &'a DeviceSyncPanel,
    pub(super) now_playing: &'a NowPlayingView,
}

pub(super) fn library_view_update_callback(
    ctx: LibraryViewUpdateContext<'_>,
) -> LibraryViewUpdateCallback {
    let runtime = ctx.runtime.clone();
    let songs_table = ctx.songs_table.clone();
    let albums_view = ctx.albums_view.clone();
    let duplicates_view = ctx.duplicates_view.clone();
    let statistics_view = ctx.statistics_view.clone();
    let playlists_table = ctx.playlists_table.clone();
    let playlists_refresh = ctx.playlists_refresh.clone();
    let sidebar = ctx.sidebar.clone();
    let titlebar = ctx.titlebar.clone();
    let current_search_text = ctx.current_search_text.clone();
    let visible_summary_refresh = ctx.visible_summary_refresh;
    let device_panel = ctx.device_panel.clone();
    let now_playing = ctx.now_playing.clone();

    Rc::new(move |update: &LibraryViewUpdate| {
        let touched = TouchedTracks::of(&runtime.borrow(), &update.track_ids);

        songs_table.remove_rows(&touched.departed);
        songs_table.update_rows(&touched.rows);
        let joined = {
            let runtime = runtime.borrow();
            let normalized = normalize_query(&current_search_text.borrow());
            let shown = songs_table.contained_track_ids(&touched.rows.keys().copied().collect());
            // In track-id order, the order the library lists them in, which
            // an unsorted table shows.
            update
                .track_ids
                .iter()
                .filter(|track_id| {
                    !shown.contains(track_id)
                        && (normalized.is_empty()
                            || runtime.search_matches(**track_id, &normalized))
                })
                .filter_map(|track_id| touched.rows.get(track_id).cloned())
                .collect::<Vec<_>>()
        };
        songs_table.append_rows(joined);

        match &update.albums {
            AlbumsUpdate::Unaffected => {}
            // In place, never `replace_tracks`: a background completion
            // (lyrics, tags, BPM, key) must not collapse the expanded
            // album or scroll the grid back to the top.
            AlbumsUpdate::Patch(track_ids) => albums_view.update_tracks(track_ids),
            AlbumsUpdate::Artwork(track_ids) => {
                for track_id in track_ids {
                    albums_view.refresh_track_artwork(*track_id);
                }
            }
            AlbumsUpdate::Regroup => albums_view.replace_tracks(),
        }
        match update.duplicates {
            DuplicatesUpdate::Rows => duplicates_view.update_rows(&touched.rows),
            DuplicatesUpdate::Regroup => duplicates_view.refresh_if_active(),
        }
        if update.statistics {
            statistics_view.refresh_if_visible();
        }

        playlists_table.remove_rows(&touched.departed);
        let playlist_rows_changed = match sidebar.current_selection() {
            Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) => {
                update.playlist_entries.contains(&playlist_id)
            }
            Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(smart_id))) => {
                // Smart-playlist *membership* may change with the edit —
                // but in the overwhelmingly common case (BPM/key/waveform
                // scan updating a track that either already matches or
                // already doesn't) the set is unchanged and only the row's
                // data needs to repaint. The runtime's per-track status
                // check tells the two apart; a track that left the library
                // counts too, as it may have held another out of a limited
                // playlist.
                let visible_track_ids = playlists_table
                    .contained_track_ids(&update.track_ids.iter().copied().collect());
                let runtime = runtime.borrow();
                let statuses = runtime.smart_playlist_track_statuses(smart_id, &update.track_ids);
                update
                    .track_ids
                    .iter()
                    .zip(statuses)
                    .any(|(track_id, status)| {
                        let was_in_table = visible_track_ids.contains(track_id);
                        matches!(
                            (status, was_in_table),
                            (SmartPlaylistTrackStatus::Included, false)
                                | (SmartPlaylistTrackStatus::Excluded, true)
                                | (SmartPlaylistTrackStatus::RequiresFullRebuild, _)
                        )
                    })
            }
            _ => false,
        };
        if playlist_rows_changed {
            schedule_playlists_structural_refresh(
                runtime.clone(),
                playlists_refresh.clone(),
                sidebar.clone(),
                current_search_text.clone(),
            );
        } else {
            // In-place row updates are idempotent for a hidden table; no
            // visibility gating needed.
            playlists_table.update_rows(&touched.rows);
        }

        // A track joining or leaving can flip the library between empty
        // and not, which decides whether the Play button can cold-start
        // anything.
        update_play_pause_sensitivity(&titlebar, &runtime.borrow());
        // The summary counts the rows shown, and a device syncing a
        // playlist selects other tracks once its entries change.
        visible_summary_refresh();
        // A background BPM/key/waveform completion changes the Pioneer
        // export readiness; refresh it when that view is on screen.
        device_panel.refresh_readiness();
        now_playing.analysis_changed(&update.analysed);
    })
}

/// The touched tracks of one update, split by whether the library still
/// holds them.
struct TouchedTracks {
    /// The row each touched track the library holds shows.
    rows: HashMap<TrackId, TrackTableRow>,
    /// The touched tracks the library no longer holds.
    departed: HashSet<TrackId>,
}

impl TouchedTracks {
    fn of(runtime: &ApplicationRuntime, track_ids: &[TrackId]) -> Self {
        let honor_sort_tags = runtime.settings().library.honor_sort_tags;
        let mut rows = HashMap::new();
        let mut departed = HashSet::new();
        for track_id in track_ids {
            match runtime.library_track(*track_id) {
                Some(track) => {
                    rows.insert(*track_id, TrackTableRow::from_track(track, honor_sort_tags));
                }
                None => {
                    departed.insert(*track_id);
                }
            }
        }
        Self { rows, departed }
    }
}

/// Reports changed embedded artwork through the same applier as committed
/// library changes.
pub(super) fn artwork_changed_callback(
    library_view_update: LibraryViewUpdateCallback,
) -> ArtworkChangedCallback {
    Rc::new(move |track_ids: &[TrackId]| {
        library_view_update(&LibraryViewUpdate::for_artwork(track_ids));
    })
}

/// Re-derive the shown playlist's rows on the next idle, once for a burst
/// of track changes, keeping the user's place in the table; a hidden view
/// is rebuilt on its next visit.
pub(super) fn schedule_playlists_structural_refresh(
    runtime: SharedRuntime,
    playlists_refresh: PlaylistsViewRefreshContext,
    sidebar: PlaylistSidebar,
    current_search_text: Rc<RefCell<String>>,
) {
    if !playlists_refresh.is_visible() {
        playlists_refresh.mark_dirty();
        return;
    }
    if playlists_refresh.mark_dirty_and_was_dirty() {
        return;
    }
    glib::idle_add_local_once(move || {
        let search_text = current_search_text.borrow().clone();
        refresh_playlists_view_if_visible(
            &runtime,
            &playlists_refresh,
            sidebar.current_selection(),
            &search_text,
            RowSetUpdate::Reconcile,
        );
    });
}

#[allow(clippy::too_many_arguments)]
pub(super) fn track_context_actions(
    runtime: &SharedRuntime,
    window: &gtk::Window,
    show_album_holder: &ShowAlbumHolder,
    command_controller: &SharedCommandController,
    playback_changed: PlaybackChangedCallback,
    artwork_changed_holder: ArtworkChangedHolder,
    artwork_loader: &ArtworkLoader,
) -> TrackContextActionSet {
    TrackContextActionSet::new(vec![
        TrackContextAction::play_next(
            play_next_callback(command_controller),
            playback_has_current_track_visibility(runtime),
        ),
        TrackContextAction::add_to_queue(
            add_to_queue_callback(command_controller),
            playback_has_current_track_visibility(runtime),
        ),
        TrackContextAction::get_info(get_info_callback(GetInfoCallbackContext {
            parent_window: window,
            runtime,
            command_controller,
            artwork_changed_holder: &artwork_changed_holder,
            playback_changed: playback_changed.clone(),
            artwork_loader,
        })),
        TrackContextAction::show_album(
            show_album_callback(show_album_holder),
            track_has_album_visibility(runtime),
        ),
        TrackContextAction::copy_files(copy_files_callback(runtime, window)),
        TrackContextAction::show_in_folder(show_in_folder_callback(runtime, window)),
        TrackContextAction::consolidate_duplicates(consolidate_duplicates_callback(
            window,
            runtime,
            command_controller,
            artwork_loader,
        )),
        TrackContextAction::remove_from_library(track_removal_callback(
            command_controller,
            playback_changed.clone(),
            remove_from_library_commands,
        )),
        TrackContextAction::move_to_trash(track_removal_callback(
            command_controller,
            playback_changed,
            move_to_trash_commands,
        )),
    ])
}

#[allow(clippy::too_many_arguments)]
pub(super) fn playlist_track_context_actions(
    runtime: &SharedRuntime,
    window: &gtk::Window,
    show_album_holder: &ShowAlbumHolder,
    command_controller: &SharedCommandController,
    playback_changed: PlaybackChangedCallback,
    artwork_changed_holder: ArtworkChangedHolder,
    artwork_loader: &ArtworkLoader,
    sidebar: &PlaylistSidebar,
) -> TrackContextActionSet {
    TrackContextActionSet::new(vec![
        TrackContextAction::play_next(
            play_next_callback(command_controller),
            playback_has_current_track_visibility(runtime),
        ),
        TrackContextAction::add_to_queue(
            add_to_queue_callback(command_controller),
            playback_has_current_track_visibility(runtime),
        ),
        TrackContextAction::get_info(get_info_callback(GetInfoCallbackContext {
            parent_window: window,
            runtime,
            command_controller,
            artwork_changed_holder: &artwork_changed_holder,
            playback_changed: playback_changed.clone(),
            artwork_loader,
        })),
        TrackContextAction::show_album(
            show_album_callback(show_album_holder),
            track_has_album_visibility(runtime),
        ),
        TrackContextAction::copy_files(copy_files_callback(runtime, window)),
        TrackContextAction::show_in_folder(show_in_folder_callback(runtime, window)),
        TrackContextAction::remove_from_playlist(
            remove_from_playlist_callback(command_controller, sidebar),
            current_selection_is_regular_playlist(sidebar),
        ),
        TrackContextAction::remove_from_library(track_removal_callback(
            command_controller,
            playback_changed.clone(),
            remove_from_library_commands,
        )),
        TrackContextAction::move_to_trash(track_removal_callback(
            command_controller,
            playback_changed,
            move_to_trash_commands,
        )),
    ])
}

/// Build the drag-reorder callback for the playlist track table. The callback
/// only acts when a *regular* playlist is selected in the sidebar — smart
/// playlists and the Library pseudo-entry are derived/dynamic and have no
/// authoritative entry order to mutate. No GTK-only row reorder path:
/// this dispatches `MovePlaylistEntries` so the runtime/SQLite are the
/// source of truth.
///
/// The committed reorder reaches the table as a change to the playlist's
/// entries: its rows take the playlist's new numbering and move in place,
/// carrying the selection with them, so the dropped rows stay selected
/// where the user put them and the view does not jump.
///
/// `new_position` is the insertion index in the playlist's *post-removal*
/// entries list (see `ApplicationCommand::MovePlaylistEntries`), so the
/// caller pre-shifts by the count of dragged tracks that currently sit
/// before the target row.
pub(super) fn playlist_row_reorder_callback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
) -> RowReorderCallback {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |drop: RowReorderDrop| -> bool {
        let Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) =
            sidebar.current_selection()
        else {
            // Drops on smart-playlist / library views are silently
            // ignored; the indicator was already cleared when GTK fired
            // the drop signal, so there is no visual residue.
            return false;
        };

        let new_position = {
            let runtime_borrow = runtime.borrow();
            let Some(playlist) = runtime_borrow
                .playlists()
                .iter()
                .find(|playlist| playlist.id == playlist_id)
            else {
                return false;
            };
            let Some(new_position) = compute_playlist_reorder_position(&playlist.entries, &drop)
            else {
                return false;
            };
            new_position
        };

        command_controller.dispatch_succeeded(ApplicationCommand::MovePlaylistEntries {
            playlist_id,
            track_ids: drop.dragged_track_ids,
            new_position,
        })
    })
}

/// Resolve the (`Above`/`Below`, target-row-track-id) pair from a drop into a
/// post-removal insertion index for `MovePlaylistEntries`.
///
/// Returns `None` when the target row is not in the playlist (shouldn't
/// happen for in-table drops, but the row id is opaque to the table's
/// drop target and is worth validating before dispatching), or when every
/// dragged track is the target row itself.
fn compute_playlist_reorder_position(
    entries: &[sustain_app_runtime::PlaylistEntry],
    drop: &RowReorderDrop,
) -> Option<u32> {
    let target_index = entries
        .iter()
        .position(|entry| entry.track_id == drop.target_track_id)?;
    let moving: std::collections::BTreeSet<sustain_app_runtime::TrackId> =
        drop.dragged_track_ids.iter().copied().collect();
    if moving.is_empty() {
        return None;
    }
    // Count source tracks that currently sit before the target row; they
    // will be removed first, so the target row's post-removal index drops
    // by that count.
    let source_tracks_before_target = entries
        .iter()
        .take(target_index)
        .filter(|entry| moving.contains(&entry.track_id))
        .count();
    let target_post_removal_index = target_index - source_tracks_before_target;
    let new_position = match drop.position {
        RowDropPosition::Above => target_post_removal_index,
        RowDropPosition::Below => target_post_removal_index + 1,
    };
    u32::try_from(new_position).ok()
}

/// Removes the selected tracks from the shown regular playlist. A refused
/// removal is reported by the dispatch; a committed one reaches the views
/// as a runtime change.
fn remove_from_playlist_callback(
    command_controller: &SharedCommandController,
    sidebar: &PlaylistSidebar,
) -> TrackActionCallback {
    let command_controller = command_controller.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |invocation: TrackActionInvocation| {
        let Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) =
            sidebar.current_selection()
        else {
            return;
        };
        let _result = command_controller.dispatch(ApplicationCommand::RemoveTracksFromPlaylist {
            playlist_id,
            track_ids: invocation.selected_track_ids,
        });
    })
}

fn current_selection_is_regular_playlist(sidebar: &PlaylistSidebar) -> TrackActionVisibility {
    let sidebar = sidebar.clone();
    Rc::new(move |_track_ids| {
        matches!(
            sidebar.current_selection(),
            Some(SidebarSelection::Item(PlaylistItem::Playlist(_)))
        )
    })
}

/// One command for the whole selection: the library drops it in a single
/// transaction.
fn remove_from_library_commands(track_ids: &[TrackId]) -> Vec<ApplicationCommand> {
    vec![ApplicationCommand::RemoveTracksFromLibrary {
        track_ids: track_ids.to_vec(),
    }]
}

/// One command per track: each file is trashed, or kept, on its own.
fn move_to_trash_commands(track_ids: &[TrackId]) -> Vec<ApplicationCommand> {
    track_ids
        .iter()
        .map(|&track_id| ApplicationCommand::MoveTrackToTrash { track_id })
        .collect()
}

/// Runs a library-removing command (Remove from Library, Move to Trash) on
/// the selected tracks. A command can fail for some tracks of the selection
/// and not others; the runtime publishes the tracks that left.
fn track_removal_callback(
    command_controller: &SharedCommandController,
    playback_changed: PlaybackChangedCallback,
    commands: fn(&[TrackId]) -> Vec<ApplicationCommand>,
) -> TrackActionCallback {
    let command_controller = command_controller.clone();

    Rc::new(move |invocation: TrackActionInvocation| {
        let result = command_controller.dispatch_batch(commands(&invocation.selected_track_ids));
        // Removing the playing track stops playback.
        if result.succeeded > 0 {
            playback_changed();
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sustain_app_runtime::{PlaylistEntry, PlaylistId};

    #[test]
    fn drop_above_target_post_removal_collapses_source_tracks_before_target() {
        // Playlist: [1, 2, 3, 4, 5]. Drag [3, 4] (which sit before the
        // target row), drop above row 5. The post-removal list is
        // [1, 2, 5] (len 3); row 5's post-removal index is 2, and "above"
        // resolves to insertion at 2 — landing the [3, 4] block right
        // before 5 in the final order: [1, 2, 3, 4, 5] (no visual change
        // because the block was already contiguous and ends just before
        // the target).
        let entries = playlist_entries(&[1, 2, 3, 4, 5]);
        let drop = RowReorderDrop {
            dragged_track_ids: vec![track_id(3), track_id(4)],
            target_track_id: track_id(5),
            position: RowDropPosition::Above,
        };
        assert_eq!(compute_playlist_reorder_position(&entries, &drop), Some(2));
    }

    #[test]
    fn drop_below_target_adds_one_to_post_removal_index() {
        // Playlist: [1, 2, 3, 4, 5]. Drag [3], drop below row 5.
        // Post-removal list: [1, 2, 4, 5] (len 4); row 5's post-removal
        // index is 3; "below" → insertion at 4, which clamps to len and
        // lands the track at the tail: [1, 2, 4, 5, 3].
        let entries = playlist_entries(&[1, 2, 3, 4, 5]);
        let drop = RowReorderDrop {
            dragged_track_ids: vec![track_id(3)],
            target_track_id: track_id(5),
            position: RowDropPosition::Below,
        };
        assert_eq!(compute_playlist_reorder_position(&entries, &drop), Some(4));
    }

    #[test]
    fn drop_above_target_when_no_sources_precede_it_keeps_index_unchanged() {
        // Playlist: [1, 2, 3, 4]. Drag [4], drop above row 2.
        // No source tracks before row 2; row 2 is at index 1, stays at
        // post-removal index 1. "Above" → insertion at 1 — final order:
        // [1, 4, 2, 3].
        let entries = playlist_entries(&[1, 2, 3, 4]);
        let drop = RowReorderDrop {
            dragged_track_ids: vec![track_id(4)],
            target_track_id: track_id(2),
            position: RowDropPosition::Above,
        };
        assert_eq!(compute_playlist_reorder_position(&entries, &drop), Some(1));
    }

    #[test]
    fn missing_target_rejects_the_move() {
        let entries = playlist_entries(&[1, 2, 3]);
        let drop = RowReorderDrop {
            dragged_track_ids: vec![track_id(1)],
            target_track_id: track_id(99),
            position: RowDropPosition::Above,
        };
        assert_eq!(compute_playlist_reorder_position(&entries, &drop), None);
    }

    #[test]
    fn empty_dragged_set_rejects_the_move() {
        let entries = playlist_entries(&[1, 2, 3]);
        let drop = RowReorderDrop {
            dragged_track_ids: Vec::new(),
            target_track_id: track_id(2),
            position: RowDropPosition::Above,
        };
        assert_eq!(compute_playlist_reorder_position(&entries, &drop), None);
    }

    fn playlist_entries(track_ids: &[i64]) -> Vec<PlaylistEntry> {
        let playlist_id = PlaylistId::new(1).expect("positive id");
        track_ids
            .iter()
            .enumerate()
            .map(|(position, id)| PlaylistEntry {
                playlist_id,
                track_id: track_id(*id),
                position: u32::try_from(position).expect("position fits in u32"),
            })
            .collect()
    }

    fn track_id(value: i64) -> TrackId {
        TrackId::new(value).expect("positive track id")
    }
}
