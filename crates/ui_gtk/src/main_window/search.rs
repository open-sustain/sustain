// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Search wiring: debounced, in-place rebuilds of whichever view is visible
//! as the user types in the titlebar search entry.

use super::*;

/// Wires the topbar SearchEntry to a debounced callback that re-filters
/// all three content pages (Music, Albums, Playlists) plus the
/// status-bar summary against the new query. The visible page rebuilds
/// at once; a hidden one rebuilds when it is next shown, so switching
/// pages mid-query never shows stale unfiltered content.
///
/// Filtering follows the agreed product semantics:
/// - Music view filters across the 7 track-level fields each track's
///   [`sustain_app_runtime::SearchIndex`] document holds.
/// - Albums view filters by album-level fields only (title, artist,
///   year) via [`AlbumsView::set_search_text`].
/// - Playlist view filters within the currently selected playlist /
///   smart playlist, again on track fields.
///
/// Debouncing belongs to the search entry alone: its search delay (see
/// `titlebar`) restarts on every keystroke, so a typing burst arrives here
/// as one change, and each change rebuilds at once. A second timer here
/// would only add its delay to every search.
pub(super) struct SearchWiringContext {
    pub(super) current_search_text: Rc<RefCell<String>>,
    pub(super) command_controller: SharedCommandController,
    pub(super) runtime: SharedRuntime,
    pub(super) songs_refresh: SongsViewRefresh,
    pub(super) albums_view: AlbumsView,
    pub(super) playlists_refresh: PlaylistsViewRefreshContext,
    pub(super) sidebar: PlaylistSidebar,
    pub(super) content_stack: gtk::Stack,
    pub(super) visible_summary_refresh: VisibleSummaryRefreshCallback,
}

pub(super) fn install_search_wiring(titlebar: &Titlebar, context: SearchWiringContext) {
    let SearchWiringContext {
        current_search_text,
        command_controller,
        runtime,
        songs_refresh,
        albums_view,
        playlists_refresh,
        sidebar,
        content_stack,
        visible_summary_refresh,
    } = context;
    connect_titlebar_search(
        titlebar,
        Rc::new(move |new_text| {
            let change_profile = sustain_profiler::ProfileScope::start();
            if *current_search_text.borrow() == new_text {
                return;
            }
            *current_search_text.borrow_mut() = new_text.clone();

            // Clearing the search widens the play queue back to the current
            // view, so auto-advance resumes through the full list instead of
            // stopping at the end of the now-gone filtered set (#78). The
            // runtime no-ops when nothing is playing or the playing track is
            // not in the widened view.
            if new_text.trim().is_empty() {
                let request = repopulate_request_for_visible_view(
                    &runtime.borrow(),
                    &content_stack,
                    sidebar.current_selection(),
                );
                if let Some(request) = request {
                    let _ = command_controller.dispatch(ApplicationCommand::Playback(
                        PlaybackCommand::RepopulateQueue(request),
                    ));
                }
            }

            sustain_profiler::profile_mark!(change_profile, "search: text changed phase");

            songs_refresh.refresh();

            let albums_profile = sustain_profiler::ProfileScope::start();
            albums_view.set_search_text(new_text.clone());
            sustain_profiler::profile_mark!(
                albums_profile,
                "search: Albums view update phase ({} albums)",
                albums_view.visible_album_count()
            );

            let playlists_profile = sustain_profiler::ProfileScope::start();
            let playlists_refreshed = refresh_playlists_view_if_visible(
                &runtime.borrow(),
                &playlists_refresh,
                sidebar.current_selection(),
                &new_text,
            );
            if playlists_refreshed {
                sustain_profiler::profile_mark!(
                    playlists_profile,
                    "search: Playlists view update phase"
                );
            }

            if !playlists_refreshed {
                visible_summary_refresh();
            }
        }),
    );
}
