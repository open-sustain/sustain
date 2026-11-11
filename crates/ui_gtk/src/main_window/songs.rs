// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Songs view: rebuilds of the Songs table, deferred while another view
//! is visible.

use super::*;

/// Rebuilds the Songs table from the library and the current search, or
/// defers the rebuild until the Songs view is next shown.
///
/// A search typed over a playlist, or a library change made while another
/// view is visible, would otherwise rebuild the hidden table too: a
/// whole-library rebuild for a table nobody sees, on every keystroke. The
/// table is marked dirty instead and rebuilt once when the Songs view is
/// shown. That rebuild runs inside the stack switch, so the table is
/// current before anything reads it — `reveal_track` right after
/// `select_music`, the Songs-only shortcuts.
///
/// In-place row updates (`update_rows`, missing flags, the playing track)
/// still reach the hidden table: they are cheap, and keep a clean table
/// clean.
#[derive(Clone)]
pub(super) struct SongsViewRefresh {
    content_stack: gtk::Stack,
    songs_table: TrackTable,
    runtime: SharedRuntime,
    current_search_text: Rc<RefCell<String>>,
    dirty: Rc<Cell<bool>>,
}

impl SongsViewRefresh {
    /// Starts clean: startup hydration fills the Songs table whichever
    /// view is visible.
    pub(super) fn install(
        content_stack: &gtk::Stack,
        songs_table: &TrackTable,
        runtime: &SharedRuntime,
        current_search_text: &Rc<RefCell<String>>,
    ) -> Self {
        let refresh = Self {
            content_stack: content_stack.clone(),
            songs_table: songs_table.clone(),
            runtime: runtime.clone(),
            current_search_text: current_search_text.clone(),
            dirty: Rc::new(Cell::new(false)),
        };
        let activator = refresh.clone();
        content_stack.connect_visible_child_name_notify(move |_| {
            if activator.is_visible() && activator.dirty.get() {
                activator.rebuild();
            }
        });
        refresh
    }

    pub(super) fn is_visible(&self) -> bool {
        self.content_stack.visible_child_name().as_deref() == Some(SONGS_VIEW)
    }

    /// Rebuild the table now if it is visible, otherwise on its next
    /// showing.
    pub(super) fn refresh(&self) {
        if self.is_visible() {
            self.rebuild();
        } else {
            self.dirty.set(true);
        }
    }

    /// The play queue the Songs list defines: the library, or the search
    /// results, in the order the list shows them — brought up to date
    /// first when it went stale while hidden.
    pub(super) fn queue_request(&self) -> PlaybackQueueRequest {
        if self.dirty.get() {
            self.rebuild();
        }
        PlaybackQueueRequest {
            source: songs_queue_source(&self.current_search_text.borrow()),
            ordered_track_ids: self.songs_table.ordered_track_ids(),
        }
    }

    fn rebuild(&self) {
        let profile = sustain_profiler::ProfileScope::start();
        let search_text = self.current_search_text.borrow().clone();
        let rows = runtime_library_table_rows(&self.runtime.borrow(), &search_text);
        let row_count = rows.len();
        self.songs_table.replace_rows(rows);
        self.dirty.set(false);
        sustain_profiler::profile_mark!(profile, "Songs table rebuilt ({} rows)", row_count);
    }
}

#[cfg(test)]
mod tests {
    use sustain_app_runtime::ApplicationRuntime;

    use super::*;
    use crate::track_table::build_track_table;

    #[test]
    fn a_hidden_songs_table_rebuilds_once_it_is_shown() {
        crate::test_support::with_gtk(|| {
            let songs_table = build_track_table(Vec::new(), None, None, None, None, None);
            let content_stack = gtk::Stack::new();
            content_stack.add_named(&songs_table.widget(), Some(SONGS_VIEW));
            content_stack.add_named(&gtk::Box::default(), Some(PLAYLISTS_VIEW));
            let runtime: SharedRuntime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let refresh = SongsViewRefresh::install(
                &content_stack,
                &songs_table,
                &runtime,
                &Rc::new(RefCell::new(String::new())),
            );
            // Rows the empty library does not have, so a rebuild shows.
            songs_table.replace_rows(vec![TrackTableRow::test_row(1)]);

            content_stack.set_visible_child_name(PLAYLISTS_VIEW);
            refresh.refresh();
            assert_eq!(
                songs_table.summary().track_count,
                1,
                "a hidden table waits to be shown"
            );

            content_stack.set_visible_child_name(SONGS_VIEW);
            assert_eq!(
                songs_table.summary().track_count,
                0,
                "showing the table rebuilds it"
            );
        });
    }

    /// Play pressed over a view without a track list queues the Songs
    /// list, so a list gone stale while hidden is rebuilt first.
    #[test]
    fn a_hidden_songs_table_rebuilds_before_it_defines_a_queue() {
        crate::test_support::with_gtk(|| {
            let songs_table = build_track_table(Vec::new(), None, None, None, None, None);
            let content_stack = gtk::Stack::new();
            content_stack.add_named(&songs_table.widget(), Some(SONGS_VIEW));
            content_stack.add_named(&gtk::Box::default(), Some(PLAYLISTS_VIEW));
            let runtime: SharedRuntime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let refresh = SongsViewRefresh::install(
                &content_stack,
                &songs_table,
                &runtime,
                &Rc::new(RefCell::new(String::new())),
            );
            songs_table.replace_rows(vec![TrackTableRow::test_row(1)]);
            content_stack.set_visible_child_name(PLAYLISTS_VIEW);
            refresh.refresh();

            assert_eq!(
                refresh.queue_request(),
                PlaybackQueueRequest {
                    source: PlaybackQueueSource::Library,
                    ordered_track_ids: Vec::new(),
                }
            );
        });
    }
}
