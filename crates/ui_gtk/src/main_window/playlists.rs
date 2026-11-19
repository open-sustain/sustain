// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Playlists view: the lazy view activator and dirty-flag rebuild, the
//! playlists-header state projection, the per-playlist/smart-playlist row
//! construction, and the add-to-playlist menu provider and callback.

use std::{collections::HashSet, time::SystemTime};

use sustain_app_runtime::CollationKey;

use super::*;
use crate::{
    library_view_update::LibraryViewUpdate, local_time_zone_monitor::LocalTimeZoneMonitor,
    wall_clock_alarm::WallClockAlarm,
};

#[derive(Clone)]
pub(super) struct PlaylistsViewRefreshContext {
    content_stack: ContentStack,
    playlists_table: TrackTable,
    playlists_header: PlaylistsHeader,
    status_bar: StatusBar,
    playlists_dirty: Rc<Cell<bool>>,
    /// Rings when the time alone may change the shown smart playlist's
    /// tracks: one leaves a relative date rule's window ("played in the
    /// last 7 days"), which each rebuild sets it to; the wall clock is
    /// set; or the local time zone, which calendar rules ("before
    /// 2024-01-01") read dates in, changes.
    reevaluation_alarm: WallClockAlarm,
    /// Rings the alarm when the local time zone changes.
    _time_zone_monitor: Rc<LocalTimeZoneMonitor>,
}

impl PlaylistsViewRefreshContext {
    pub(super) fn new(
        content_stack: &ContentStack,
        playlists_table: &TrackTable,
        playlists_header: &PlaylistsHeader,
        status_bar: &StatusBar,
        playlists_dirty: &Rc<Cell<bool>>,
    ) -> Self {
        let reevaluation_alarm = WallClockAlarm::new()
            .expect("create the timer that re-evaluates the shown smart playlist");
        let alarm = reevaluation_alarm.clone();
        let time_zone_monitor =
            LocalTimeZoneMonitor::new(move || alarm.ring_now()).expect("watch the local time zone");
        Self {
            content_stack: content_stack.clone(),
            playlists_table: playlists_table.clone(),
            playlists_header: playlists_header.clone(),
            status_bar: status_bar.clone(),
            playlists_dirty: playlists_dirty.clone(),
            reevaluation_alarm,
            _time_zone_monitor: Rc::new(time_zone_monitor),
        }
    }

    /// Re-evaluate the shown smart playlist at `at` at the latest, after
    /// an in-place update to tracks whose verdict time changes then.
    pub(super) fn reevaluate_no_later_than(&self, at: SystemTime) {
        self.reevaluation_alarm.ring_no_later_than(at);
    }

    pub(super) fn mark_dirty(&self) {
        self.playlists_dirty.set(true);
    }

    pub(super) fn dirty(&self) -> bool {
        self.playlists_dirty.get()
    }

    pub(super) fn is_visible(&self) -> bool {
        self.content_stack.visible_child_name().as_deref() == Some(PLAYLISTS_VIEW)
    }

    pub(super) fn mark_dirty_and_was_dirty(&self) -> bool {
        self.playlists_dirty.replace(true)
    }

    /// Show `item`'s current name in the header, after a rename of the
    /// shown item. Its rows are unchanged, so the table is left alone.
    pub(super) fn retitle(&self, runtime: &ApplicationRuntime, item: PlaylistItem) {
        if let Some(title) = playlist_item_title(runtime, item) {
            self.playlists_header.set_title(&title);
        }
    }
}

/// Mirror of `install_albums_view_activator` for the Playlists view.
/// The table is built empty and stays empty while another page is
/// visible; library-change / selection-changed / search rebuilds
/// flip a `dirty` flag instead of running `replace_rows`. When the
/// user picks a playlist row in the sidebar, the activator pays the
/// rebuild cost once with the current state and clears the flag.
/// Music is the default landing page, so in the common case the
/// playlists table is never populated for a session that does not
/// visit a playlist.
pub(super) fn install_playlists_view_activator(
    refresh_context: &PlaylistsViewRefreshContext,
    runtime: &SharedRuntime,
    sidebar: &PlaylistSidebar,
    current_search_text: &Rc<RefCell<String>>,
) {
    // Time passing re-derives the shown smart playlist the way a track
    // change that moves its membership does. Like the playback heartbeat,
    // the alarm lives as long as the main window.
    {
        let runtime = runtime.clone();
        let context = refresh_context.clone();
        let sidebar = sidebar.clone();
        let current_search_text = current_search_text.clone();
        refresh_context.reevaluation_alarm.connect_ring(move || {
            // Only a smart playlist whose rules read the time can change
            // with it; any other stays as it is (a random pick included).
            let Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(smart_id))) =
                sidebar.current_selection()
            else {
                return;
            };
            let reads_the_time = runtime
                .borrow()
                .smart_playlists()
                .iter()
                .find(|smart_playlist| smart_playlist.id == smart_id)
                .is_some_and(|smart_playlist| smart_playlist.rules.reads_the_time());
            if !reads_the_time {
                return;
            }
            schedule_playlists_structural_refresh(
                runtime.clone(),
                context.clone(),
                sidebar.clone(),
                current_search_text.clone(),
            );
        });
    }
    let runtime = runtime.clone();
    let refresh_context = refresh_context.clone();
    let sidebar = sidebar.clone();
    let current_search_text = current_search_text.clone();
    let content_stack = refresh_context.content_stack.clone();
    content_stack.connect_visible_child_name_notify(move |stack| {
        if stack.visible_child_name().as_deref() != Some(PLAYLISTS_VIEW) {
            return;
        }
        if !refresh_context.dirty() {
            return;
        }
        let search_text = current_search_text.borrow().clone();
        rebuild_playlists_view(
            &runtime,
            &refresh_context,
            sidebar.current_selection(),
            &search_text,
            RowSetUpdate::Replace,
        );
        refresh_context.playlists_dirty.set(false);
    });
}

/// Rebuild the playlists table only when the user is actually looking
/// at it. Triggers that fire while another view is visible (library
/// scan completion, search keystrokes, sidebar selection change) just
/// flip the dirty flag; `install_playlists_view_activator` runs the
/// rebuild on the next visit. See its doc-comment for the rationale.
pub(super) fn refresh_playlists_view_if_visible(
    runtime: &SharedRuntime,
    refresh_context: &PlaylistsViewRefreshContext,
    sidebar_selection: Option<SidebarSelection>,
    search_text: &str,
    update: RowSetUpdate,
) -> bool {
    if refresh_context.is_visible() {
        rebuild_playlists_view(
            runtime,
            refresh_context,
            sidebar_selection,
            search_text,
            update,
        );
        refresh_context.playlists_dirty.set(false);
        true
    } else {
        refresh_context.mark_dirty();
        false
    }
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

/// Brings the playlists view up to date with a change to `update`'s
/// tracks: `rows` holds the row of each the library still holds, and
/// `departed` those it no longer does. A shown regular
/// playlist whose entries changed reconciles with the runtime, as does a
/// shown smart playlist whose membership may have changed; otherwise the
/// rows refresh in place, and the re-evaluation alarm is brought forward
/// to when time passing next changes a touched track's verdict.
pub(super) fn update_playlists_view_tracks(
    runtime: &SharedRuntime,
    refresh_context: &PlaylistsViewRefreshContext,
    sidebar: &PlaylistSidebar,
    current_search_text: &Rc<RefCell<String>>,
    update: &LibraryViewUpdate,
    rows: &HashMap<TrackId, TrackTableRow>,
    departed: &HashSet<TrackId>,
) {
    refresh_context.playlists_table.remove_rows(departed);
    let selection = sidebar.current_selection();
    // Whether the rows change, and, when they stay, when time passing
    // next changes a verdict on a touched track.
    let (playlist_rows_changed, changes_at) = match selection {
        Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) => {
            (update.playlist_entries.contains(&playlist_id), None)
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
            let visible_track_ids = refresh_context
                .playlists_table
                .contained_track_ids(&update.track_ids.iter().copied().collect());
            let verdicts = runtime
                .borrow()
                .smart_playlist_track_verdicts(smart_id, &update.track_ids);
            let rows_changed =
                update
                    .track_ids
                    .iter()
                    .zip(verdicts.statuses)
                    .any(|(track_id, status)| {
                        let was_in_table = visible_track_ids.contains(track_id);
                        matches!(
                            (status, was_in_table),
                            (SmartPlaylistTrackStatus::Included, false)
                                | (SmartPlaylistTrackStatus::Excluded, true)
                                | (SmartPlaylistTrackStatus::RequiresFullRebuild, _)
                        )
                    });
            (rows_changed, verdicts.changes_at)
        }
        _ => (false, None),
    };
    if playlist_rows_changed {
        schedule_playlists_structural_refresh(
            runtime.clone(),
            refresh_context.clone(),
            sidebar.clone(),
            current_search_text.clone(),
        );
    } else {
        // In-place row updates are idempotent for a hidden table; no
        // visibility gating needed.
        refresh_context.playlists_table.update_rows(rows);
        // The rows stay, but a touched track's new date may leave a
        // relative date rule's window before any shown track does.
        if let Some(at) = changes_at {
            refresh_context.reevaluate_no_later_than(at);
        }
    }
}

/// Unconditional rebuild of the playlists view (header + track table)
/// from the current selection + search filter. Header summary is derived
/// from the same row set fed to the table, so the visible "N songs, X
/// duration" text always matches what's drawn below it. The
/// re-evaluation alarm is set to when time passing next changes the rows.
///
/// The runtime is borrowed only to derive the rows: showing them can call
/// back into it, as a re-sort does to reorder the play queue.
fn rebuild_playlists_view(
    runtime: &SharedRuntime,
    refresh_context: &PlaylistsViewRefreshContext,
    sidebar_selection: Option<SidebarSelection>,
    search_text: &str,
    update: RowSetUpdate,
) {
    let rows_profile = sustain_profiler::ProfileScope::start();
    let (PlaylistTableRows { rows, changes_at }, header_state) = {
        let runtime = runtime.borrow();
        let rows = playlist_table_rows_for(&runtime, sidebar_selection, search_text);
        let summary = TrackSummary::of_rows(&rows.rows);
        let header_state = playlists_header_state_for(&runtime, sidebar_selection, summary);
        (rows, header_state)
    };
    let row_count = rows.len();
    sustain_profiler::profile_mark!(rows_profile, "playlist rows built ({} rows)", row_count);
    refresh_context.reevaluation_alarm.set(changes_at);
    let summary = TrackSummary::of_rows(&rows);
    refresh_context.playlists_header.set_state(header_state);
    refresh_context.status_bar.update_summary(summary);
    let show_profile = sustain_profiler::ProfileScope::start();
    refresh_context.playlists_table.show_rows(rows, update);
    sustain_profiler::profile_mark!(
        show_profile,
        "playlist table rows shown ({update:?}, {} rows)",
        row_count
    );
}

fn playlists_header_state_for(
    runtime: &ApplicationRuntime,
    selection: Option<SidebarSelection>,
    summary: TrackSummary,
) -> Option<PlaylistsHeaderState> {
    // Music / Albums selections do not render the playlists header at
    // all (the stack shows a different child).
    let Some(SidebarSelection::Item(item)) = selection else {
        return None;
    };
    let title = playlist_item_title(runtime, item)?;
    Some(PlaylistsHeaderState { title, summary })
}

/// The name the playlists header shows for a sidebar item. Folders
/// aggregate their children in the sidebar but are not themselves a
/// playable track set, so the header has nothing meaningful to show.
fn playlist_item_title(runtime: &ApplicationRuntime, item: PlaylistItem) -> Option<String> {
    match item {
        PlaylistItem::Playlist(id) => runtime
            .playlists()
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name.clone()),
        PlaylistItem::SmartPlaylist(id) => runtime
            .smart_playlists()
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name.clone()),
        PlaylistItem::Folder(_) => None,
    }
}

/// The rows the playlists table shows for a sidebar selection.
#[derive(Default)]
struct PlaylistTableRows {
    rows: Vec<TrackTableRow>,
    /// When time passing alone next changes `rows`: a smart playlist's
    /// track leaving a relative date rule's window.
    changes_at: Option<SystemTime>,
}

fn playlist_table_rows_for(
    runtime: &ApplicationRuntime,
    selection: Option<SidebarSelection>,
    search_text: &str,
) -> PlaylistTableRows {
    // Carry the playlist_position alongside each Track so the row built
    // below mirrors PlaylistEntry::position one-to-one for the regular-
    // playlist branch. Library / Smart Playlist selections never have an
    // authoritative play-order, so their pairs hold None — those rows
    // collate equal under the status column sorter and are unaffected by
    // the play-order sort.
    let (candidates, changes_at): (Vec<(Track, Option<u32>)>, _) = match selection {
        // The playlists table mirrors the Music view's rows when the
        // Music entry is selected — same library track set, no
        // play-position. (PLAYLISTS_VIEW is not actually shown for
        // Music / Albums, but the table-rebuild path is shared.)
        Some(SidebarSelection::Music) => (
            runtime
                .library_tracks()
                .iter()
                .map(|track| (track.clone(), None))
                .collect(),
            None,
        ),
        Some(SidebarSelection::Item(PlaylistItem::Playlist(playlist_id))) => {
            let Some(playlist) = runtime
                .playlists()
                .iter()
                .find(|playlist| playlist.id == playlist_id)
            else {
                return PlaylistTableRows::default();
            };
            let mut entries: Vec<&PlaylistEntry> = playlist.entries.iter().collect();
            entries.sort_by_key(|entry| entry.position);
            let candidates = entries
                .into_iter()
                .filter_map(|entry| {
                    runtime
                        .library_track(entry.track_id)
                        .cloned()
                        .map(|track| (track, Some(entry.position)))
                })
                .collect();
            (candidates, None)
        }
        Some(SidebarSelection::Item(PlaylistItem::SmartPlaylist(smart_playlist_id))) => {
            let selection = runtime.smart_playlist_selection(smart_playlist_id);
            let candidates = selection
                .tracks
                .into_iter()
                .map(|track| (track.clone(), None))
                .collect();
            (candidates, selection.changes_at)
        }
        _ => return PlaylistTableRows::default(),
    };

    let honor_sort_tags = runtime.settings().library.honor_sort_tags;
    let normalized = normalize_query(search_text);
    let rows = candidates
        .into_iter()
        .filter(|(track, _)| normalized.is_empty() || runtime.search_matches(track.id, &normalized))
        .map(|(track, position)| {
            TrackTableRow::from_track(&track, honor_sort_tags).with_playlist_position(position)
        })
        .collect();
    PlaylistTableRows { rows, changes_at }
}

pub(super) fn add_to_playlist_provider(runtime: &SharedRuntime) -> AddToPlaylistProvider {
    let runtime = runtime.clone();
    Rc::new(move || {
        let runtime = runtime.borrow();
        let folders: HashMap<PlaylistFolderId, &PlaylistFolder> = runtime
            .playlist_folders()
            .iter()
            .map(|folder| (folder.id, folder))
            .collect();
        let mut entries: Vec<AddToPlaylistEntry> = runtime
            .playlists()
            .iter()
            .map(|playlist| AddToPlaylistEntry {
                playlist_id: playlist.id,
                display_path: playlist_display_path(playlist, &folders),
            })
            .collect();
        entries.sort_by_cached_key(|entry| CollationKey::new(&entry.display_path));
        entries
    })
}

/// Adds tracks to a playlist. A refused addition is reported by the
/// dispatch; a committed one reaches the views as a runtime change.
pub(super) fn add_to_playlist_callback(
    command_controller: &SharedCommandController,
) -> AddToPlaylistCallback {
    let command_controller = command_controller.clone();

    Rc::new(move |playlist_id, track_ids| {
        if track_ids.is_empty() {
            return;
        }
        let _result = command_controller.dispatch(ApplicationCommand::AddTracksToPlaylist {
            playlist_id,
            track_ids,
        });
    })
}

fn playlist_display_path(
    playlist: &Playlist,
    folders: &HashMap<PlaylistFolderId, &PlaylistFolder>,
) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut current = playlist.parent_folder_id;
    while let Some(folder_id) = current {
        let Some(folder) = folders.get(&folder_id) else {
            break;
        };
        segments.push(folder.name.clone());
        current = folder.parent_folder_id;
    }
    segments.reverse();
    segments.push(playlist.name.clone());
    segments.join(" / ")
}

#[cfg(test)]
#[path = "playlists_tests.rs"]
mod tests;
