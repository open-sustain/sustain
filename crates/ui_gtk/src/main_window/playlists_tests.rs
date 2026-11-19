// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::BTreeSet,
    num::NonZeroU32,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use sustain_app_runtime::{
    ApplicationCommand, Rating, SmartPlaylistDateField, SmartPlaylistMatchKind,
    SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet, Track,
};

use super::*;
use crate::{
    library_view_update::{AlbumsUpdate, DuplicatesUpdate},
    test_support::{SettableClock, runtime_with_tracks_and_clock, track, with_gtk},
};

const DAY: Duration = Duration::from_secs(86_400);
const NANOSECOND: Duration = Duration::from_nanos(1);

/// The present the tests' runtime reads: a whole second (the library
/// stores dates to the second) far enough ahead of the wall clock that
/// the alarm, set to instants after it, never rings on its own during a
/// test. A test rings it when it means to.
fn test_now() -> SystemTime {
    let ahead = SystemTime::now() + 1_000 * DAY;
    let seconds = ahead
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

/// The shown Playlists view of a runtime holding `tracks` and one smart
/// playlist matching any of `rules`, its time read from `clock`.
struct ShownSmartPlaylist {
    runtime: SharedRuntime,
    view: PlaylistsViewRefreshContext,
    sidebar: PlaylistSidebar,
    search_text: Rc<RefCell<String>>,
}

impl ShownSmartPlaylist {
    fn new(tracks: Vec<Track>, rules: Vec<SmartPlaylistRule>, clock: Arc<SettableClock>) -> Self {
        let runtime = runtime_with_tracks_and_clock(tracks, clock);
        runtime
            .borrow_mut()
            .handle_command(ApplicationCommand::CreateSmartPlaylist {
                name: "Smart".to_owned(),
                parent_folder_id: None,
                rules: SmartPlaylistRuleSet {
                    match_kind: SmartPlaylistMatchKind::Any,
                    rules,
                    limit: None,
                },
            })
            .expect("create the smart playlist");
        let playlists_table = build_track_table(Vec::new(), None, None, None, None, None);
        let pages: Vec<gtk::Box> = (0..6)
            .map(|_| gtk::Box::new(gtk::Orientation::Vertical, 0))
            .collect();
        let content_stack = ContentStack::new(
            &pages[0],
            &pages[1],
            &pages[2],
            &pages[3],
            &playlists_table.widget(),
            &pages[4],
            &pages[5],
        );
        let view = PlaylistsViewRefreshContext::new(
            &content_stack,
            &playlists_table,
            &PlaylistsHeader::new(),
            &StatusBar::new(Rc::new(|| {})),
            &Rc::new(Cell::new(true)),
            &LocalTimeZoneMonitor::new().expect("watch the local time zone"),
        );
        let sidebar = PlaylistSidebar::new(runtime.clone(), false);
        // The one created above, after the starter set a library opens with.
        let created = runtime
            .borrow()
            .smart_playlists()
            .iter()
            .find(|smart_playlist| smart_playlist.name == "Smart")
            .expect("the created smart playlist")
            .id;
        sidebar.select_item(PlaylistItem::SmartPlaylist(created));
        let search_text = Rc::new(RefCell::new(String::new()));
        install_playlists_view_activator(&view, &runtime, &sidebar, &search_text);
        content_stack.set_visible_child_name(PLAYLISTS_VIEW);
        Self {
            runtime,
            view,
            sidebar,
            search_text,
        }
    }

    /// The tracks the view shows, in id order.
    fn shown(&self) -> Vec<TrackId> {
        let mut shown = self.view.playlists_table.ordered_track_ids();
        shown.sort();
        shown
    }

    fn alarm_rings_at(&self) -> Option<SystemTime> {
        self.view.reevaluation_alarm.rings_at()
    }

    /// Runs the main loop until the view shows `expected`, for at most
    /// five seconds.
    fn run_until_shown(&self, expected: &[TrackId]) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.shown() != expected && Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

fn played_at(id: i64, last_played_at: SystemTime) -> Track {
    let mut track = track(id, "Played");
    track.statistics.last_played_at = Some(last_played_at);
    track
}

fn played_in_last_two_days() -> SmartPlaylistRule {
    SmartPlaylistRule::DateInLast {
        field: SmartPlaylistDateField::LastPlayed,
        days: NonZeroU32::new(2).expect("positive days"),
    }
}

#[test]
fn a_shown_smart_playlist_is_re_evaluated_when_a_track_leaves_its_window() {
    with_gtk(|| {
        let now = test_now();
        let clock = Arc::new(SettableClock::new(now));
        // Leaves the two-day window ten seconds from now; the other one
        // a day later.
        let leaving = played_at(1, now - 2 * DAY + Duration::from_secs(10));
        let staying = played_at(2, now - DAY);
        let shown = ShownSmartPlaylist::new(
            vec![leaving.clone(), staying.clone()],
            vec![played_in_last_two_days()],
            clock.clone(),
        );
        assert_eq!(shown.shown(), [leaving.id, staying.id]);
        let first_exit = now + Duration::from_secs(10) + NANOSECOND;
        assert_eq!(shown.alarm_rings_at(), Some(first_exit));

        // The instant comes, and the alarm rings.
        clock.set(first_exit);
        shown.view.reevaluation_alarm.ring_now();
        shown.run_until_shown(&[staying.id]);

        assert_eq!(shown.shown(), [staying.id]);
        assert_eq!(shown.alarm_rings_at(), Some(now + DAY + NANOSECOND));
    });
}

#[test]
fn a_ring_brings_back_a_track_the_clock_was_set_back_for() {
    with_gtk(|| {
        let now = test_now();
        let clock = Arc::new(SettableClock::new(now));
        // Left the two-day window a minute ago: nothing to wait for.
        let left = played_at(1, now - 2 * DAY - Duration::from_secs(60));
        let shown = ShownSmartPlaylist::new(
            vec![left.clone()],
            vec![played_in_last_two_days()],
            clock.clone(),
        );
        assert_eq!(shown.shown(), []);
        assert_eq!(shown.alarm_rings_at(), None);

        // The wall clock is set two minutes back, which rings the alarm.
        clock.set(now - Duration::from_secs(120));
        shown.view.reevaluation_alarm.ring_now();
        shown.run_until_shown(&[left.id]);

        assert_eq!(shown.shown(), [left.id]);
    });
}

#[test]
fn a_track_kept_in_place_brings_the_re_evaluation_forward() {
    with_gtk(|| {
        let now = test_now();
        let clock = Arc::new(SettableClock::new(now));
        // Five stars, and leaving the two-day window ten seconds from
        // now: shown either way, until it loses its stars.
        let mut rated = played_at(1, now - 2 * DAY + Duration::from_secs(10));
        rated.rating = Rating::new(5).expect("rating in range");
        let shown = ShownSmartPlaylist::new(
            vec![rated.clone()],
            vec![
                SmartPlaylistRule::Rating {
                    operator: SmartPlaylistNumberOperator::Equal,
                    value: Rating::new(5).expect("rating in range"),
                },
                played_in_last_two_days(),
            ],
            clock,
        );
        assert_eq!(shown.shown(), [rated.id]);
        // As if the window's first exit were another track's, later.
        let later = now + 60 * DAY;
        shown.view.reevaluation_alarm.set(Some(later));

        // Unrated, it is shown only for its play, which leaves the window
        // before the alarm rings.
        shown
            .runtime
            .borrow_mut()
            .handle_command(ApplicationCommand::SetRating {
                track_id: rated.id,
                rating: Rating::unrated(),
            })
            .expect("unrate the track");
        let unrated = shown
            .runtime
            .borrow()
            .library_track(rated.id)
            .cloned()
            .expect("the library holds the track");
        update_playlists_view_tracks(
            &shown.runtime,
            &shown.view,
            &shown.sidebar,
            &shown.search_text,
            &LibraryViewUpdate {
                track_ids: vec![rated.id],
                albums: AlbumsUpdate::Unaffected,
                duplicates: DuplicatesUpdate::Rows,
                statistics: false,
                playlist_entries: BTreeSet::new(),
                analysed: BTreeSet::new(),
            },
            &HashMap::from([(rated.id, TrackTableRow::from_track(&unrated, false))]),
            &HashSet::new(),
        );

        assert!(
            !shown.view.dirty(),
            "the rows stay in place, without a rebuild"
        );
        assert_eq!(shown.shown(), [rated.id]);
        assert_eq!(
            shown.alarm_rings_at(),
            Some(now + Duration::from_secs(10) + NANOSECOND)
        );
    });
}

#[test]
fn a_ring_leaves_a_smart_playlist_that_does_not_read_the_time_alone() {
    with_gtk(|| {
        let clock = Arc::new(SettableClock::new(test_now()));
        let mut rated = track(1, "Rated");
        rated.rating = Rating::new(5).expect("rating in range");
        let unrated = track(2, "Unrated");
        let shown = ShownSmartPlaylist::new(
            vec![rated.clone(), unrated.clone()],
            vec![SmartPlaylistRule::Rating {
                operator: SmartPlaylistNumberOperator::Equal,
                value: Rating::new(5).expect("rating in range"),
            }],
            clock,
        );
        assert_eq!(shown.shown(), [rated.id]);
        // The library changes behind the view, which only a
        // re-evaluation would show.
        shown
            .runtime
            .borrow_mut()
            .handle_command(ApplicationCommand::SetRating {
                track_id: unrated.id,
                rating: Rating::new(5).expect("rating in range"),
            })
            .expect("rate the track");

        shown.view.reevaluation_alarm.ring_now();
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_millis(300);
        while Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(shown.alarm_rings_at(), None, "the alarm rang");
        assert_eq!(shown.shown(), [rated.id], "and the view stayed as it was");
    });
}
