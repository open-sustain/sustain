// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Search wiring: debounced, in-place rebuilds of whichever view is visible
//! as the user types in the titlebar search entry.

use super::*;

/// Wires the topbar SearchEntry to a debounced callback that re-filters
/// all three content pages (Music, Albums, Playlists) plus the
/// status-bar summary against the new query. All three are rebuilt on
/// each fire so that switching pages mid-query never shows stale
/// unfiltered content.
///
/// Filtering follows the agreed product semantics:
/// - Music view filters across the 7 track-level fields covered by
///   [`track_matches_search_text`].
/// - Albums view filters by album-level fields only (title, artist,
///   year) via [`AlbumsView::set_search_text`].
/// - Playlist view filters within the currently selected playlist /
///   smart playlist, again on track fields.
///
/// Debouncing: rebuilding the visible track table on every keystroke is
/// expensive — not because of the in-memory filter (microseconds) but
/// because [`TrackTable::replace_rows`] rewrites the underlying
/// `gio::ListStore`, which fires GTK list-model events that the sorter
/// and selection model both have to process. The same effect shows up
/// on the album grid. We therefore cancel any in-flight rebuild and
/// schedule a fresh one [`SEARCH_REBUILD_DEBOUNCE`] in the future on
/// every keystroke, collapsing a typing burst into one rebuild when
/// the user pauses. The raw SearchEntry text is saved on close as part of
/// the UI session, so closing inside the debounce window preserves the query
/// even if the last rebuild never runs.
pub(super) struct SearchWiringContext {
    pub(super) current_search_text: Rc<RefCell<String>>,
    pub(super) command_controller: SharedCommandController,
    pub(super) runtime: SharedRuntime,
    pub(super) songs_table: TrackTable,
    pub(super) albums_view: AlbumsView,
    pub(super) playlists_table: TrackTable,
    pub(super) playlists_header: PlaylistsHeader,
    pub(super) sidebar: PlaylistSidebar,
    pub(super) content_stack: gtk::Stack,
    pub(super) playlists_dirty: Rc<Cell<bool>>,
    pub(super) status_bar: StatusBar,
    pub(super) visible_summary_refresh: VisibleSummaryRefreshCallback,
}

pub(super) fn install_search_wiring(titlebar: &Titlebar, context: SearchWiringContext) {
    let SearchWiringContext {
        current_search_text,
        command_controller,
        runtime,
        songs_table,
        albums_view,
        playlists_table,
        playlists_header,
        sidebar,
        content_stack,
        playlists_dirty,
        status_bar,
        visible_summary_refresh,
    } = context;
    let pending_rebuild: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    connect_titlebar_search(
        titlebar,
        Rc::new(move |new_text| {
            if *current_search_text.borrow() == new_text {
                return;
            }
            *current_search_text.borrow_mut() = new_text.clone();

            // Clearing the search widens the play queue back to the current
            // view, so auto-advance resumes through the full list instead of
            // stopping at the end of the now-gone filtered set (#78). Done
            // immediately — it is cheap and must not wait on the table-rebuild
            // debounce below. The runtime no-ops when nothing is playing or
            // the playing track is not in the widened view.
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

            // Cancel any pending rebuild scheduled for the previous
            // keystroke; only the most recent query should run.
            if let Some(previous) = pending_rebuild.borrow_mut().take() {
                previous.remove();
            }

            let runtime = runtime.clone();
            let songs_table = songs_table.clone();
            let albums_view = albums_view.clone();
            let playlists_table = playlists_table.clone();
            let playlists_header = playlists_header.clone();
            let sidebar = sidebar.clone();
            let content_stack = content_stack.clone();
            let playlists_dirty = playlists_dirty.clone();
            let status_bar = status_bar.clone();
            let visible_summary_refresh = visible_summary_refresh.clone();
            let pending_rebuild_clear = pending_rebuild.clone();
            let source_id = glib::timeout_add_local_once(SEARCH_REBUILD_DEBOUNCE, move || {
                pending_rebuild_clear.borrow_mut().take();

                let songs_rows = runtime_library_table_rows(&runtime.borrow(), &new_text);
                // Share one pass: when Songs is the visible view, summarize
                // the rows we just built (count + duration + size) instead
                // of re-filtering and re-materializing the whole table for
                // the status bar. Other views fall back to the generic
                // refresh below, which derives the right rows for them.
                let songs_visible =
                    content_stack.visible_child_name().as_deref() == Some(SONGS_VIEW);
                if songs_visible {
                    status_bar.update_summary(&songs_rows);
                }
                songs_table.replace_rows(songs_rows);

                albums_view.set_search_text(new_text.clone());

                refresh_playlists_view_if_visible(
                    &runtime.borrow(),
                    &content_stack,
                    &playlists_table,
                    &playlists_header,
                    sidebar.current_selection(),
                    &new_text,
                    &playlists_dirty,
                );

                if !songs_visible {
                    visible_summary_refresh();
                }
            });
            *pending_rebuild.borrow_mut() = Some(source_id);
        }),
    );
}

/// Debounce window for search-driven view rebuilds. 100ms is short enough
/// that a single keystroke followed by a pause feels instantaneous, and
/// long enough to swallow a burst of typing at any realistic speed
/// (40ms per keystroke at 25 WPM, ~20ms at very fast typing) into one
/// rebuild when the user stops.
pub(super) const SEARCH_REBUILD_DEBOUNCE: std::time::Duration =
    std::time::Duration::from_millis(100);
