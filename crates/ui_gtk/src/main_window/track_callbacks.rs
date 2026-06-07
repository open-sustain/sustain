// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Track-table row interactions: inline cell editing, rating clicks, the
//! per-row refresh callback, the Songs and Playlist context-menu action sets,
//! drag-to-reorder within a playlist, and the remove-from-playlist /
//! generic track-mutation helpers.

use super::*;

/// Builds the seed/commit pair the Songs table uses for inline cell
/// editing. The seed reads the field's authoritative value straight from
/// the runtime (so Title is seeded with the real tag, not the file-stem
/// fallback the row displays). The commit funnels through the exact same
/// `UpdateMetadata` write path the File Info dialog uses — SQLite stays
/// authoritative and the file tags are mirrored — then refreshes just the
/// affected row, like the rating click does.
pub(super) fn inline_edit_hooks(
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
    track_row_changed_holder: TrackRowChangedHolder,
) -> InlineEditHooks {
    let seed = {
        let runtime = runtime.clone();
        Rc::new(move |object: &glib::Object, field: EditableField| {
            let track_id = crate::track_table::row_track_id(Some(object.clone()))?;
            runtime
                .borrow()
                .library_tracks()
                .iter()
                .find(|track| track.id == track_id)
                .map(|track| field.seed_value(&track.metadata))
        })
    };

    let commit = {
        let runtime = runtime.clone();
        let command_controller = command_controller.clone();
        Rc::new(
            move |object: &glib::Object, field: EditableField, new_text: String| {
                let Some(track_id) = crate::track_table::row_track_id(Some(object.clone())) else {
                    return false;
                };
                let initial = {
                    let runtime = runtime.borrow();
                    runtime
                        .library_tracks()
                        .iter()
                        .find(|track| track.id == track_id)
                        .map(|track| track.metadata.clone())
                };
                let Some(initial) = initial else {
                    return false;
                };
                let change = field.metadata_change(&initial, &new_text);
                if change == MetadataChange::default() {
                    // Re-typed the same value, or an unparsable number: no
                    // write needed. Report success so the editor just closes.
                    return true;
                }
                if !command_controller.dispatch_succeeded(ApplicationCommand::UpdateMetadata {
                    track_id,
                    change: Box::new(change),
                }) {
                    return false;
                }
                if let Some(callback) = track_row_changed_holder.borrow().as_ref() {
                    callback(track_id, inline_edit_changed_kind(field));
                }
                true
            },
        )
    };

    InlineEditHooks { seed, commit }
}

pub(super) fn rating_changed_callback(
    command_controller: &SharedCommandController,
    track_row_changed_holder: TrackRowChangedHolder,
) -> RatingChangedCallback {
    let command_controller = command_controller.clone();

    Rc::new(move |track_id: TrackId, rating: Rating| {
        if !command_controller
            .dispatch_succeeded(ApplicationCommand::SetRating { track_id, rating })
        {
            return false;
        }
        if let Some(callback) = track_row_changed_holder.borrow().as_ref() {
            callback(track_id, TrackRowChangedKind::TableOnly);
        }
        true
    })
}

/// Targeted refresh path for single-track mutations. Updates only the affected
/// row in the visible tables, applies the cheapest correct Albums refresh for
/// the change kind, and refreshes the status-bar summary. Skips the sidebar
/// tree because row-field mutations do not alter playlist/folder structure.
///
/// When a smart playlist is selected, the Playlists table falls back to a
/// full reflow because the mutation may add/remove the track from the
/// playlist's filtered set — an in-place row update would lie.
pub(super) struct TrackRowChangedContext<'a> {
    pub(super) runtime: &'a SharedRuntime,
    pub(super) songs_table: &'a TrackTable,
    pub(super) albums_view: &'a AlbumsView,
    pub(super) duplicates_view: &'a DuplicatesView,
    pub(super) playlists_table: &'a TrackTable,
    pub(super) playlists_header: &'a PlaylistsHeader,
    pub(super) sidebar: &'a PlaylistSidebar,
    pub(super) content_stack: &'a gtk::Stack,
    pub(super) playlists_dirty: &'a Rc<Cell<bool>>,
    pub(super) visible_summary_refresh: VisibleSummaryRefreshCallback,
    pub(super) current_search_text: &'a Rc<RefCell<String>>,
    pub(super) device_panel: &'a DeviceSyncPanel,
}

pub(super) fn track_row_changed_callback(
    ctx: TrackRowChangedContext<'_>,
) -> TrackRowChangedCallback {
    let runtime = ctx.runtime.clone();
    let songs_table = ctx.songs_table.clone();
    let albums_view = ctx.albums_view.clone();
    let duplicates_view = ctx.duplicates_view.clone();
    let playlists_table = ctx.playlists_table.clone();
    let playlists_header = ctx.playlists_header.clone();
    let sidebar = ctx.sidebar.clone();
    let content_stack = ctx.content_stack.clone();
    let playlists_dirty = ctx.playlists_dirty.clone();
    let current_search_text = ctx.current_search_text.clone();
    let visible_summary_refresh = ctx.visible_summary_refresh;
    let device_panel = ctx.device_panel.clone();

    Rc::new(move |track_id: TrackId, kind: TrackRowChangedKind| {
        let row = {
            let runtime_borrow = runtime.borrow();
            let honor_sort_tags = runtime_borrow.settings().library.honor_sort_tags;
            runtime_borrow
                .library_tracks()
                .iter()
                .find(|track| track.id == track_id)
                .map(|track| TrackTableRow::from_track(track, honor_sort_tags))
        };
        let Some(row) = row else {
            return;
        };

        songs_table.update_row(track_id, row.clone());
        if kind.affects_album_structure() {
            albums_view.replace_tracks();
        } else {
            match kind {
                TrackRowChangedKind::Data | TrackRowChangedKind::DuplicateGrouping => {
                    // In-place per-track refresh — never `replace_tracks`. A
                    // single background completion (Lyrics/Tags/BPM/Key/Waveform,
                    // metadata write) must not collapse the currently-expanded
                    // album or scroll the grid back to the top.
                    albums_view.update_track(track_id);
                }
                TrackRowChangedKind::Artwork => {
                    albums_view.refresh_track_artwork(track_id);
                }
                TrackRowChangedKind::AlbumStructure
                | TrackRowChangedKind::AlbumAndDuplicateGrouping
                | TrackRowChangedKind::TableOnly => {}
            }
        }
        if kind.affects_duplicate_grouping() {
            duplicates_view.refresh_if_active();
        } else {
            duplicates_view.update_track_row(track_id);
        }

        match sidebar.current_selection() {
            Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(smart_id))) => {
                // Smart-playlist *membership* may change with the
                // edit — but in the overwhelmingly common case
                // (BPM/key/waveform scan updating a track that
                // either already matches or already doesn't) the
                // set is unchanged and only the row's data needs to
                // repaint. Use the runtime's per-track status check
                // to tell the two apart and avoid the
                // `replace_rows` that would scroll the user back to
                // the top of a large library on every track update.
                let (status, was_in_table) = {
                    let runtime_borrow = runtime.borrow();
                    let status = runtime_borrow.smart_playlist_track_status(smart_id, track_id);
                    let was_in_table = playlists_table.contains_track(track_id);
                    (status, was_in_table)
                };
                let membership_changed = matches!(
                    (status, was_in_table),
                    (SmartPlaylistTrackStatus::Included, false)
                        | (SmartPlaylistTrackStatus::Excluded, true)
                        | (SmartPlaylistTrackStatus::RequiresFullRebuild, _)
                );
                if membership_changed {
                    schedule_playlists_structural_refresh(
                        runtime.clone(),
                        content_stack.clone(),
                        playlists_table.clone(),
                        playlists_header.clone(),
                        sidebar.clone(),
                        current_search_text.clone(),
                        playlists_dirty.clone(),
                    );
                } else if was_in_table {
                    // Membership unchanged and the row is visible —
                    // refresh the row's data in place.
                    playlists_table.update_row(track_id, row);
                }
                // else: track doesn't match the smart playlist and
                // isn't on screen anyway; no work needed.
            }
            _ => {
                // In-place row update is cheap (one row) and idempotent
                // for a hidden table; no visibility gating needed.
                playlists_table.update_row(track_id, row);
            }
        }

        visible_summary_refresh();
        // A background BPM/key/waveform completion changes the Pioneer
        // export readiness; refresh it when that view is on screen.
        device_panel.refresh_readiness();
    })
}

fn inline_edit_changed_kind(field: EditableField) -> TrackRowChangedKind {
    match field {
        EditableField::Artist | EditableField::Album => {
            TrackRowChangedKind::AlbumAndDuplicateGrouping
        }
        EditableField::Year => TrackRowChangedKind::AlbumStructure,
        EditableField::Title => TrackRowChangedKind::DuplicateGrouping,
        EditableField::Genre
        | EditableField::Bpm
        | EditableField::Key
        | EditableField::TrackNumber => TrackRowChangedKind::Data,
    }
}

fn schedule_playlists_structural_refresh(
    runtime: SharedRuntime,
    content_stack: gtk::Stack,
    playlists_table: TrackTable,
    playlists_header: PlaylistsHeader,
    sidebar: PlaylistSidebar,
    current_search_text: Rc<RefCell<String>>,
    playlists_dirty: Rc<Cell<bool>>,
) {
    if content_stack.visible_child_name().as_deref() != Some(PLAYLISTS_VIEW) {
        playlists_dirty.set(true);
        return;
    }
    if playlists_dirty.replace(true) {
        return;
    }
    glib::idle_add_local_once(move || {
        let search_text = current_search_text.borrow().clone();
        refresh_playlists_view_if_visible(
            &runtime.borrow(),
            &content_stack,
            &playlists_table,
            &playlists_header,
            sidebar.current_selection(),
            &search_text,
            &playlists_dirty,
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
    library_changed_holder: LibraryChangedHolder,
    track_row_changed_holder: TrackRowChangedHolder,
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
        TrackContextAction::get_info(get_info_callback(
            window,
            runtime,
            command_controller,
            &library_changed_holder,
            &track_row_changed_holder,
            artwork_loader,
        )),
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
        TrackContextAction::remove_from_library(track_mutation_callback(
            command_controller,
            playback_changed.clone(),
            library_changed_holder.clone(),
            |track_id| ApplicationCommand::RemoveTrackFromLibrary { track_id },
        )),
        TrackContextAction::move_to_trash(track_mutation_callback(
            command_controller,
            playback_changed,
            library_changed_holder,
            |track_id| ApplicationCommand::MoveTrackToTrash { track_id },
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
    library_changed_holder: LibraryChangedHolder,
    track_row_changed_holder: TrackRowChangedHolder,
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
        TrackContextAction::get_info(get_info_callback(
            window,
            runtime,
            command_controller,
            &library_changed_holder,
            &track_row_changed_holder,
            artwork_loader,
        )),
        TrackContextAction::show_album(
            show_album_callback(show_album_holder),
            track_has_album_visibility(runtime),
        ),
        TrackContextAction::copy_files(copy_files_callback(runtime, window)),
        TrackContextAction::show_in_folder(show_in_folder_callback(runtime, window)),
        TrackContextAction::remove_from_playlist(
            remove_from_playlist_callback(
                command_controller,
                sidebar,
                library_changed_holder.clone(),
            ),
            current_selection_is_regular_playlist(sidebar),
        ),
        TrackContextAction::remove_from_library(track_mutation_callback(
            command_controller,
            playback_changed.clone(),
            library_changed_holder.clone(),
            |track_id| ApplicationCommand::RemoveTrackFromLibrary { track_id },
        )),
        TrackContextAction::move_to_trash(track_mutation_callback(
            command_controller,
            playback_changed,
            library_changed_holder,
            |track_id| ApplicationCommand::MoveTrackToTrash { track_id },
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
/// Post-dispatch the callback rebuilds **only** the playlists table —
/// nothing in the library, the album set, or the sidebar tree changes
/// when a playlist's internal order is shuffled. Calling the global
/// `library_changed` here (the previous approach) re-built the songs
/// table's entire `gio::ListStore` (10k rows + re-sort), the albums
/// view's groupings, and the sidebar — visible as a 1–2 s freeze after
/// every drop. The narrow refresh below touches only the rows the user
/// is looking at, so the new order appears in the next frame.
///
/// `new_position` is the insertion index in the playlist's *post-removal*
/// entries list (see `ApplicationCommand::MovePlaylistEntries`), so the
/// caller pre-shifts by the count of dragged tracks that currently sit
/// before the target row.
pub(super) fn playlist_row_reorder_callback(
    command_controller: &SharedCommandController,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
    playlists_table_holder: &Rc<RefCell<Option<TrackTable>>>,
    current_search_text: &Rc<RefCell<String>>,
) -> RowReorderCallback {
    let command_controller = command_controller.clone();
    let runtime = runtime.clone();
    let sidebar = sidebar.clone();
    let playlists_table_holder = playlists_table_holder.clone();
    let current_search_text = current_search_text.clone();

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

        let dispatched =
            command_controller.dispatch_succeeded(ApplicationCommand::MovePlaylistEntries {
                playlist_id,
                track_ids: drop.dragged_track_ids,
                new_position,
            });
        if !dispatched {
            return false;
        }

        // Targeted rebuild — only the playlist view. Library / albums /
        // sidebar are untouched because a reorder doesn't mutate any of
        // the state those views derive from.
        if let Some(playlists_table) = playlists_table_holder.borrow().as_ref() {
            let search_text = current_search_text.borrow().clone();
            let rows = playlist_table_rows_for(
                &runtime.borrow(),
                sidebar.current_selection(),
                &search_text,
            );
            playlists_table.replace_rows(rows);
        }
        true
    })
}

/// Resolve the (`Above`/`Below`, target-row-track-id) pair from a drop into a
/// post-removal insertion index for `MovePlaylistEntries`.
///
/// Returns `None` when the target row is not in the playlist (shouldn't
/// happen for in-table drops, but the row id is opaque to the cell-level
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

fn remove_from_playlist_callback(
    command_controller: &SharedCommandController,
    sidebar: &PlaylistSidebar,
    library_changed_holder: LibraryChangedHolder,
) -> TrackActionCallback {
    let command_controller = command_controller.clone();
    let sidebar = sidebar.clone();

    Rc::new(move |invocation: TrackActionInvocation| {
        let track_ids = invocation.selected_track_ids;
        let Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) =
            sidebar.current_selection()
        else {
            return;
        };
        if command_controller.dispatch_succeeded(ApplicationCommand::RemoveTracksFromPlaylist {
            playlist_id,
            track_ids,
        }) && let Some(callback) = library_changed_holder.borrow().as_ref()
        {
            callback();
        }
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

fn track_mutation_callback(
    command_controller: &SharedCommandController,
    playback_changed: PlaybackChangedCallback,
    library_changed_holder: LibraryChangedHolder,
    command_builder: impl Fn(TrackId) -> ApplicationCommand + 'static,
) -> TrackActionCallback {
    let command_controller = command_controller.clone();

    Rc::new(move |invocation: TrackActionInvocation| {
        let track_ids = invocation.selected_track_ids;
        let commands = track_ids
            .into_iter()
            .map(&command_builder)
            .collect::<Vec<_>>();
        let result = command_controller.dispatch_batch(commands);
        if result.succeeded == 0 {
            return;
        }
        playback_changed();
        if let Some(callback) = library_changed_holder.borrow().as_ref() {
            callback();
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
