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
    ApplicationCommand, PlaybackCommand, PlaybackQueueRequest, PlaybackQueueSource, ShuffleMode,
    TrackId, album_matches_search_text,
};

use super::{
    PlaybackChangedCallback, SharedRuntime,
    artwork_color::ArtworkPalette,
    artwork_loader::{ArtworkLoader, ArtworkSource},
    command_controller::SharedCommandController,
    missing_track::LocateMissingTrackCallback,
    track_context::TrackRowContextMenu,
};
use model::{
    AlbumKey, AlbumViewModel, album_subtitle, album_track, group_albums, replace_track_in_album,
};
use track_list::AlbumTrackListView;

mod model;
mod track_list;

#[derive(Clone)]
pub(crate) struct AlbumsView {
    scroller: gtk::ScrolledWindow,
    list_view: gtk::ListView,
    row_store: gio::ListStore,
    runtime: SharedRuntime,
    command_controller: SharedCommandController,
    playback_changed: PlaybackChangedCallback,
    context_menu: TrackRowContextMenu,
    locate_missing_track: LocateMissingTrackCallback,
    /// All grouped albums from the most recent group pass, unfiltered.
    /// `albums` (below) is derived from this by `apply_search`. Empty
    /// until the view is activated.
    all_albums: Rc<RefCell<Vec<AlbumViewModel>>>,
    /// Albums currently shown in the grid, after the active search filter.
    /// The renderer, `reveal_album_for_track`, and selection indexing all
    /// operate on this filtered view — selection by index becomes meaningless
    /// across a search change, so `apply_search` clears the selection.
    albums: Rc<RefCell<Vec<AlbumViewModel>>>,
    search_text: Rc<RefCell<String>>,
    selected_album: Rc<RefCell<Option<AlbumKey>>>,
    /// Currently bound virtual rows. Selection changes refresh these widgets
    /// in place so a plain album click does not emit model changes that can
    /// make GtkListView choose a new scroll anchor.
    realized_rows: Rc<RefCell<HashMap<usize, gtk::Box>>>,
    /// Reusable widget pools owned by GtkListView's realized row shells.
    /// Scrolling and width breakpoints rebind these slots in place instead of
    /// destroying and reconstructing every visible album tile.
    row_widgets: Rc<RefCell<HashMap<usize, AlbumRowWidgets>>>,
    /// Explicit scroll requested by "Show Album" actions. It is consumed from
    /// the width watcher after the Albums page has a visible allocation, so
    /// row-position math uses the final column count and live row geometry.
    pending_scroll_album: Rc<RefCell<Option<PendingAlbumScroll>>>,
    visible_columns: Rc<Cell<usize>>,
    last_width: Rc<Cell<i32>>,
    artwork_loader: ArtworkLoader,
    /// Monotonic generation bumped every time the album-detail panel is
    /// (re)built. The detail's artwork request snapshot-captures it and
    /// the callback drops itself when a newer selection has superseded
    /// the panel — critical because the palette provider is installed
    /// display-wide, so a stale install would mis-tint the current
    /// detail.
    detail_generation: Rc<Cell<u64>>,
    /// Switches from `false` to `true` the first time `activate()` is
    /// called. Tile construction, grouping, and the width-watcher tick
    /// callback all key off this — they are skipped while the view is
    /// dormant so the cost stays out of startup.
    activated: Rc<Cell<bool>>,
    playing_track_id: Rc<Cell<Option<TrackId>>>,
}

struct PendingAlbumScroll {
    album_key: AlbumKey,
    visibility_requested: bool,
}

const ALBUM_TILE_WIDTH: i32 = 150;
const ALBUM_TILE_HORIZONTAL_PADDING: i32 = 16;
const ALBUM_TILE_MIN_WIDTH: i32 = ALBUM_TILE_WIDTH + ALBUM_TILE_HORIZONTAL_PADDING;
const ALBUM_TILE_COVER_SIZE: i32 = 132;
// Sized so the title sits at the same vertical position whether the tile is
// selected or not. Non-selected: 8 px button padding + 132 cover = 140. The
// selected tile drops button padding to 0, so the cover size must equal 140
// to land at the same Y.
const ALBUM_TILE_COVER_SIZE_EXPANDED: i32 = 140;
const ALBUM_GRID_MARGIN: i32 = 14;
const ALBUM_GRID_ROW_SPACING: i32 = 12;
const ALBUM_GRID_COLUMN_SPACING: i32 = 16;
const ALBUM_DETAIL_ARTWORK_SIZE: i32 = ALBUM_TILE_COVER_SIZE * 3;
const ALBUM_DETAIL_TEXT_CHARS: i32 = 48;
const ALBUM_DETAIL_ARROW_WIDTH: i32 = 36;
const ALBUM_DETAIL_ARROW_HEIGHT: i32 = 18;
// One-pixel bleed below the triangle's base. The arrow row is laid out as
// an overlay on top of the detail panel so this bleed extends one row of
// arrow-coloured pixels into the panel's opaque background. Any sub-pixel
// transparency at the arrow texture's bottom edge — common when the
// scroller lands on a fractional offset — should then composite onto the
// panel's same-coloured pixels instead of revealing the window
// background.
// NOTE: Claude failed to fully eliminate the seam — a faint line under
// the arrow still appears intermittently, especially during scrolling.
const ALBUM_DETAIL_ARROW_BLEED: i32 = 1;
const ALBUM_COVER_PLACEHOLDER_ICON: &str = "image-missing-symbolic";

impl AlbumsView {
    pub(crate) fn new(
        runtime: SharedRuntime,
        command_controller: SharedCommandController,
        playback_changed: PlaybackChangedCallback,
        context_menu: TrackRowContextMenu,
        locate_missing_track: LocateMissingTrackCallback,
        artwork_loader: ArtworkLoader,
    ) -> Self {
        let row_store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let selection = gtk::NoSelection::new(Some(row_store.clone()));
        let list_view = gtk::ListView::new(Some(selection), None::<gtk::ListItemFactory>);
        list_view.add_css_class("albums-list");
        list_view.set_hexpand(true);
        list_view.set_vexpand(true);
        list_view.set_show_separators(false);

        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("albums-view");
        scroller.set_vexpand(true);
        scroller.set_hexpand(true);
        // Rows are rebuilt around the current allocation. Allowing temporary
        // horizontal overflow lets the scroller shrink first; the width watcher
        // then recomputes a smaller column count on the next frame.
        scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroller.set_propagate_natural_width(false);
        scroller.set_child(Some(&list_view));

        // The loader is constructed once at startup in `main_window` and
        // shared with every view that needs artwork (Albums, now-playing,
        // future zoom modal). Its worker pool sits idle until a view
        // queues its first request, and shuts down when the last `Rc`
        // is dropped at app teardown.
        //
        // The view keeps no track snapshot of its own: the runtime owns the
        // authoritative `library_tracks`, and the deferred first activate()
        // groups straight from it, so cold-start construction does no
        // O(library-size) clone (#102).
        let view = Self {
            scroller,
            list_view,
            row_store,
            runtime,
            command_controller,
            playback_changed,
            context_menu,
            locate_missing_track,
            all_albums: Rc::new(RefCell::new(Vec::new())),
            albums: Rc::new(RefCell::new(Vec::new())),
            search_text: Rc::new(RefCell::new(String::new())),
            selected_album: Rc::new(RefCell::new(None)),
            realized_rows: Rc::new(RefCell::new(HashMap::new())),
            row_widgets: Rc::new(RefCell::new(HashMap::new())),
            pending_scroll_album: Rc::new(RefCell::new(None)),
            visible_columns: Rc::new(Cell::new(1)),
            last_width: Rc::new(Cell::new(0)),
            artwork_loader,
            detail_generation: Rc::new(Cell::new(0)),
            activated: Rc::new(Cell::new(false)),
            playing_track_id: Rc::new(Cell::new(None)),
        };
        view.list_view
            .set_factory(Some(&view.build_album_row_factory()));
        view
    }

    pub(crate) fn widget(&self) -> gtk::ScrolledWindow {
        self.scroller.clone()
    }

    /// Build the grid for the first time. Called when the Albums tab is
    /// selected, either by the user clicking the mode button or by a
    /// reveal request that needs the view populated to find an album.
    /// Idempotent: repeated calls are no-ops, so callers don't need to
    /// track activation state themselves.
    pub(crate) fn activate(&self) {
        if self.activated.replace(true) {
            return;
        }
        self.install_width_watcher();
        self.regroup_and_apply_search();
    }

    /// Re-derive the album grid from the runtime's current library after a
    /// full library change (scan/import/removal). While the view is dormant
    /// this is a no-op — there is no snapshot to refresh and the first
    /// activate() reads the runtime fresh — so a library change does not pay
    /// for an off-screen regroup or any track clone.
    pub(crate) fn replace_tracks(&self) {
        if !self.activated.get() {
            return;
        }
        self.regroup_and_apply_search();
    }

    /// Apply an in-place update for a single track that already lives in
    /// the view, mirroring the row-level refresh the Songs and Playlists
    /// tables run on `track_data_observer`. The album grouping is *not*
    /// recomputed: every Sustain feature is existing-tag-preserving, so
    /// an existing track's album never moves across an update. Only the
    /// row holding the affected album repaints; the selected album,
    /// scroll position, and every untouched row's artwork stay intact.
    ///
    /// A full `replace_tracks` here would tear down the entire grid,
    /// drop the user's expanded album, and scroll back to the top every
    /// time a background lyrics/tags/artwork/BPM/key/waveform job
    /// finished a single track.
    pub(crate) fn update_track(&self, track_id: TrackId) {
        // While dormant there is no grouped state to patch: the runtime
        // owns the authoritative track and the first activate() reads it
        // fresh, so a background per-track update costs nothing here and a
        // full sweep no longer scans an off-screen snapshot per track.
        if !self.activated.get() {
            return;
        }

        // Keyed O(log n) lookup of the refreshed track, not a linear scan
        // of the whole library.
        let new_track_vm = {
            let runtime = self.runtime.borrow();
            let Some(track) = runtime.library_track(track_id) else {
                return;
            };
            album_track(track)
        };

        let Some(affected_album_key) =
            replace_track_in_album(&mut self.all_albums.borrow_mut(), track_id, &new_track_vm)
        else {
            return;
        };

        // `albums` is a separately-cloned filtered view of
        // `all_albums`; the on-screen rows read from it, so the same
        // patch has to land here when the album survived the active
        // search.
        let visible_album_present =
            replace_track_in_album(&mut self.albums.borrow_mut(), track_id, &new_track_vm)
                .is_some();
        if !visible_album_present {
            return;
        }

        if let Some(row_position) = self.row_position_for_album(&affected_album_key) {
            self.refresh_row_widget(row_position, Some(&affected_album_key));
        }
    }

    /// Update the active search filter and re-derive the visible album set.
    /// Calling with the same string as the current one is a no-op.
    pub(crate) fn set_search_text(&self, search_text: String) {
        if *self.search_text.borrow() == search_text {
            return;
        }
        *self.search_text.borrow_mut() = search_text;
        if !self.activated.get() {
            return;
        }
        self.apply_search();
    }

    /// Re-derive `albums` from `all_albums` according to the active search,
    /// clear selection, and rebuild the virtual row model.
    fn apply_search(&self) {
        let search_text = self.search_text.borrow().clone();
        let filtered: Vec<AlbumViewModel> = self
            .all_albums
            .borrow()
            .iter()
            .filter(|album| {
                album_matches_search_text(&album.title, &album.artist, album.year, &search_text)
            })
            .cloned()
            .collect();
        *self.albums.borrow_mut() = filtered;
        self.selected_album.borrow_mut().take();
        self.pending_scroll_album.borrow_mut().take();
        self.rebuild_rows();
    }

    /// Group the stashed tracks into albums and re-derive the visible
    /// set under the current search filter. The width snapshot keeps
    /// the column count consistent with whatever size the scroller
    /// happened to reach before activation.
    fn regroup_and_apply_search(&self) {
        {
            let runtime = self.runtime.borrow();
            *self.all_albums.borrow_mut() = group_albums(runtime.library_tracks());
        }
        self.visible_columns
            .set(columns_for_width(self.scroller.width()));
        self.apply_search();
    }

    /// Resolve the cold-start play request for the Albums view: the
    /// first playable track of the first album in the current
    /// (search-filtered) ordering, with the rest of that album pinned
    /// behind it as the queue. Mirrors [`play_album`] but targets
    /// whichever album currently leads the grid. Returns `None` when no
    /// album is visible or the leading album has no playable track —
    /// used by the top-bar Play button when nothing is loaded yet
    /// (issue #60).
    pub(crate) fn first_album_play_request(&self) -> Option<(TrackId, PlaybackQueueRequest)> {
        let albums = self.albums.borrow();
        let first = albums.first()?;
        let ordered_track_ids: Vec<TrackId> = first
            .tracks
            .iter()
            .filter(|track| !track.is_missing)
            .map(|track| track.id)
            .collect();
        let track_id = *ordered_track_ids.first()?;
        Some((
            track_id,
            PlaybackQueueRequest::Explicit {
                source: PlaybackQueueSource::Album,
                ordered_track_ids,
            },
        ))
    }

    pub(crate) fn set_playing_track_id(&self, playing_track_id: Option<TrackId>) {
        if self.playing_track_id.get() == playing_track_id {
            return;
        }
        self.playing_track_id.set(playing_track_id);
        if self.activated.get() {
            self.refresh_selected_row();
        }
    }

    /// Selects the album containing the given track, expands its detail panel,
    /// and brings the tile into view. Returns `false` when no album in the
    /// current grouping holds the track.
    pub(crate) fn reveal_album_for_track(&self, track_id: TrackId) -> bool {
        self.activate();
        let album_index = {
            let albums = self.albums.borrow();
            albums
                .iter()
                .position(|album| album.tracks.iter().any(|track| track.id == track_id))
        };
        let Some(album_index) = album_index else {
            return false;
        };
        let album_key = self.albums.borrow()[album_index].key.clone();
        self.select_album(album_key.clone());
        self.request_scroll_to_album(album_key);
        true
    }

    fn select_album(&self, album_key: AlbumKey) {
        let previous_album = self.selected_album.borrow().clone();
        if previous_album.as_ref() == Some(&album_key) {
            return;
        }

        let previous_row = previous_album
            .as_ref()
            .and_then(|album_key| self.row_position_for_album(album_key));

        *self.selected_album.borrow_mut() = Some(album_key.clone());

        let selected_row = self.row_position_for_album(&album_key);
        if let Some(row_position) = previous_row {
            self.refresh_row_widget(row_position, None);
        }
        if let Some(row_position) = selected_row
            && selected_row != previous_row
        {
            self.refresh_row_widget(row_position, None);
        }
    }

    fn refresh_selected_row(&self) {
        let selected_row = self
            .selected_album
            .borrow()
            .as_ref()
            .and_then(|album_key| self.row_position_for_album(album_key));
        if let Some(row_position) = selected_row {
            self.refresh_row_widget(row_position, None);
        }
    }

    fn request_scroll_to_album(&self, album_key: AlbumKey) {
        self.pending_scroll_album
            .borrow_mut()
            .replace(PendingAlbumScroll {
                album_key,
                visibility_requested: false,
            });
    }

    fn scroll_pending_album_if_ready(&self) {
        if self.scroller.width() <= 0 || self.row_store.n_items() == 0 {
            return;
        }
        let Some(album_key) = self
            .pending_scroll_album
            .borrow()
            .as_ref()
            .map(|pending| pending.album_key.clone())
        else {
            return;
        };
        let Some(row_position) = self.row_position_for_album(&album_key) else {
            self.pending_scroll_album.borrow_mut().take();
            return;
        };

        let visibility_requested = self
            .pending_scroll_album
            .borrow()
            .as_ref()
            .is_some_and(|pending| pending.visibility_requested);
        if !visibility_requested {
            let scroll_info = gtk::ScrollInfo::new();
            scroll_info.set_enable_horizontal(false);
            scroll_info.set_enable_vertical(true);
            self.list_view.scroll_to(
                row_position as u32,
                gtk::ListScrollFlags::NONE,
                Some(scroll_info),
            );
            if let Some(pending) = self.pending_scroll_album.borrow_mut().as_mut() {
                pending.visibility_requested = true;
            }
            return;
        }

        let Some(row_shell) = self.realized_rows.borrow().get(&row_position).cloned() else {
            return;
        };
        if align_widget_to_viewport_top(&row_shell, &self.list_view, &self.scroller.vadjustment()) {
            self.pending_scroll_album.borrow_mut().take();
        }
    }

    fn install_width_watcher(&self) {
        let view = self.clone();
        self.scroller.add_tick_callback(move |scroller, _clock| {
            let width = scroller.width();
            if width > 0 && view.last_width.replace(width) != width {
                let columns = columns_for_width(width);
                if view.visible_columns.replace(columns) != columns {
                    view.rebuild_rows();
                }
            }
            view.scroll_pending_album_if_ready();

            glib::ControlFlow::Continue
        });
    }

    fn rebuild_rows(&self) {
        if !self.activated.get() {
            return;
        }
        let columns = self.visible_columns.get().max(1);
        let row_count = album_row_count(self.albums.borrow().len(), columns);
        resize_row_store(&self.row_store, row_count);

        // Tokens that survive the tail resize keep their list positions but
        // resolve to a different album slice when the column count changes.
        // Re-render only the virtual rows GTK currently has realized.
        let realized_rows: Vec<_> = self
            .realized_rows
            .borrow()
            .iter()
            .filter(|(position, _)| **position < row_count)
            .map(|(&position, shell)| (position, shell.clone()))
            .collect();
        for (position, shell) in realized_rows {
            self.render_row_position(&shell, position, None);
        }
    }

    fn refresh_row_widget(&self, row_position: usize, refresh_artwork_for: Option<&AlbumKey>) {
        let Some(row_shell) = self.realized_rows.borrow().get(&row_position).cloned() else {
            return;
        };
        self.render_row_position(&row_shell, row_position, refresh_artwork_for);
    }

    fn row_position_for_album(&self, album_key: &AlbumKey) -> Option<usize> {
        let columns = self.visible_columns.get().max(1);
        self.albums
            .borrow()
            .iter()
            .position(|album| &album.key == album_key)
            .map(|album_index| album_index / columns)
    }

    fn build_album_row_factory(&self) -> gtk::SignalListItemFactory {
        let factory = gtk::SignalListItemFactory::new();
        let row_widgets_for_setup = self.row_widgets.clone();
        factory.connect_setup(move |_factory, item| {
            let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let row = AlbumRowWidgets::new();
            list_item.set_child(Some(&row.shell));
            row_widgets_for_setup
                .borrow_mut()
                .insert(widget_id(&row.shell), row);
        });

        let view_for_bind = self.clone();
        factory.connect_bind(move |_factory, item| {
            let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(row_shell) = list_item
                .child()
                .and_then(|child| child.downcast::<gtk::Box>().ok())
            else {
                return;
            };

            if list_item.position() == gtk::INVALID_LIST_POSITION {
                return;
            }
            let position = list_item.position() as usize;
            let mut realized_rows = view_for_bind.realized_rows.borrow_mut();
            realized_rows.retain(|_, shell| shell != &row_shell);
            realized_rows.insert(position, row_shell.clone());
            drop(realized_rows);
            view_for_bind.render_row_position(&row_shell, position, None);
        });

        let view_for_unbind = self.clone();
        factory.connect_unbind(move |_factory, item| {
            let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            if let Some(row_shell) = list_item
                .child()
                .and_then(|child| child.downcast::<gtk::Box>().ok())
            {
                view_for_unbind
                    .realized_rows
                    .borrow_mut()
                    .retain(|_, shell| shell != &row_shell);
            }
        });

        let row_widgets_for_teardown = self.row_widgets.clone();
        factory.connect_teardown(move |_factory, item| {
            let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let Some(row_shell) = list_item
                .child()
                .and_then(|child| child.downcast::<gtk::Box>().ok())
            else {
                return;
            };
            row_widgets_for_teardown
                .borrow_mut()
                .remove(&widget_id(&row_shell));
        });

        factory
    }

    fn render_row_position(
        &self,
        row_shell: &gtk::Box,
        row_position: usize,
        refresh_artwork_for: Option<&AlbumKey>,
    ) {
        let row_id = widget_id(row_shell);
        let mut row_widgets = self.row_widgets.borrow_mut();
        let Some(row) = row_widgets.get_mut(&row_id) else {
            return;
        };
        let columns = self.visible_columns.get().max(1);
        let Some(start) = row_position.checked_mul(columns) else {
            return;
        };
        let albums = self.albums.borrow();
        if albums.is_empty() {
            if row_position == 0 {
                row.show_empty_state();
            }
            return;
        }
        if start >= albums.len() {
            return;
        }
        let end = (start + columns).min(albums.len());
        self.render_row_shell(row, &albums[start..end], columns, refresh_artwork_for);
    }

    fn render_row_shell(
        &self,
        row: &mut AlbumRowWidgets,
        albums: &[AlbumViewModel],
        columns: usize,
        refresh_artwork_for: Option<&AlbumKey>,
    ) {
        row.show_tiles();
        row.ensure_slot_count(columns, self);
        let selected_album = self.selected_album.borrow().clone();
        for (offset, slot) in row.slots.iter_mut().enumerate() {
            slot.container.set_visible(offset < columns);
            let Some(album) = albums.get(offset) else {
                if offset < columns {
                    slot.show_placeholder();
                }
                continue;
            };
            let is_selected = selected_album
                .as_ref()
                .is_some_and(|selected| selected == &album.key);
            let refresh_artwork = refresh_artwork_for == Some(&album.key);
            slot.bind(self, album, is_selected, refresh_artwork);
        }

        if let Some((selected_column, selected_album)) =
            selected_album_in_row(albums, self.selected_album.borrow().as_ref())
        {
            let detail = self.album_detail(selected_album, selected_column, columns);
            row.set_detail(Some(detail));
        } else {
            row.set_detail(None);
        }
    }

    fn album_detail(
        &self,
        album: &AlbumViewModel,
        selected_column: usize,
        columns: usize,
    ) -> gtk::Overlay {
        // Spacing here is the gap between the title-block / track-lists
        // column on the left and the artwork column on the right. Kept in
        // sync with the inter-column spacing of `lists` so the
        // right-half track column sits the same distance from the
        // artwork as the two track columns sit from each other.
        // Bump the detail generation so any artwork request still in
        // flight for a previously-selected album drops itself instead of
        // installing the wrong palette over this panel (the palette
        // provider is display-wide, so a stale install would mis-tint the
        // current detail). Snapshot it for this build's request callback.
        self.detail_generation
            .set(self.detail_generation.get().wrapping_add(1));
        let detail_generation_snapshot = self.detail_generation.get();

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 40);
        content.add_css_class("album-detail");
        content.set_hexpand(true);
        // The palette classes are added unconditionally; the
        // artwork-derived palette provider is installed display-wide by
        // the async callback below once the cover decodes. No blocking
        // read happens on this (GTK) thread (#107).
        apply_palette_class(&content, "album-detail-dominant-color");

        let left = gtk::Box::new(gtk::Orientation::Vertical, 6);
        left.set_hexpand(true);
        left.set_vexpand(true);

        let title_block = gtk::Box::new(gtk::Orientation::Vertical, 2);
        title_block.set_hexpand(true);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.set_hexpand(true);

        let title = gtk::Label::new(Some(&album.title));
        title.add_css_class("album-detail-title");
        apply_palette_class(&title, "album-detail-palette-primary");
        title.set_xalign(0.0);
        title.set_hexpand(false);
        title.set_wrap(true);
        title.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        title.set_lines(2);
        title.set_width_chars(1);
        title.set_max_width_chars(ALBUM_DETAIL_TEXT_CHARS);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        header.append(&title);

        let play_button = detail_icon_button("media-playback-start-symbolic", "Play album");
        let album_for_play = album.clone();
        let command_controller_for_play = self.command_controller.clone();
        let playback_changed_for_play = self.playback_changed.clone();
        play_button.connect_clicked(move |_| {
            ensure_shuffle_disabled(&command_controller_for_play);
            if play_album(&command_controller_for_play, &album_for_play) {
                playback_changed_for_play();
            }
        });
        header.append(&play_button);

        let shuffle_button = detail_icon_button("media-playlist-shuffle-symbolic", "Shuffle album");
        let album_for_shuffle = album.clone();
        let command_controller_for_shuffle = self.command_controller.clone();
        let playback_changed_for_shuffle = self.playback_changed.clone();
        shuffle_button.connect_clicked(move |_| {
            // Album header's Shuffle button always means "pure
            // random over this album" — Smart shuffle's discovery
            // model is meaningless inside a single album, so we
            // pin the queue's mode to Pure regardless of the
            // transport setting.
            set_shuffle_mode(&command_controller_for_shuffle, ShuffleMode::Pure);
            if play_album(&command_controller_for_shuffle, &album_for_shuffle) {
                playback_changed_for_shuffle();
            }
        });
        header.append(&shuffle_button);

        // Trailing spacer absorbs the rest of the header row so title +
        // buttons pile up at the start instead of buttons being pushed
        // against the right edge by an hexpanding title.
        let header_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        header_spacer.set_hexpand(true);
        header.append(&header_spacer);

        title_block.append(&header);

        let subtitle = gtk::Label::new(Some(&album_subtitle(album)));
        subtitle.add_css_class("album-detail-subtitle");
        apply_palette_class(&subtitle, "album-detail-palette-secondary");
        subtitle.set_xalign(0.0);
        subtitle.set_width_chars(1);
        subtitle.set_max_width_chars(ALBUM_DETAIL_TEXT_CHARS);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title_block.append(&subtitle);
        left.append(&title_block);

        let track_lists = self.album_track_lists(album);
        track_lists.set_margin_top(14);
        left.append(&track_lists);

        let artwork_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
        artwork_column.set_halign(gtk::Align::End);
        artwork_column.set_valign(gtk::Align::End);
        // Starts as a placeholder; the decoded cover (and the palette
        // derived from it) lands via the async callback below.
        let detail_cover = album_cover_with(None, ALBUM_DETAIL_ARTWORK_SIZE, "album-detail-cover");
        apply_palette_class(&detail_cover, "album-detail-palette-surface");
        artwork_column.append(&detail_cover);

        // Reserve vertical room above the panel for the arrow. The arrow
        // is rendered as an overlay on top of this region so its texture's
        // bottom edge overlaps the panel's top edge; any sub-pixel
        // sampling artifact in the arrow's bottom row composites over the
        // panel's opaque background (same color) instead of revealing the
        // window's theme background. This holds even when the scroller
        // translates the contents to a fractional pixel offset.
        let arrow_spacer = gtk::Box::new(gtk::Orientation::Vertical, 0);
        arrow_spacer.set_size_request(-1, ALBUM_DETAIL_ARROW_HEIGHT);

        let base = gtk::Box::new(gtk::Orientation::Vertical, 0);
        base.set_hexpand(true);
        base.append(&arrow_spacer);
        base.append(&content);

        let shell = gtk::Overlay::new();
        shell.set_hexpand(true);
        shell.set_margin_bottom(ALBUM_DETAIL_ARROW_HEIGHT);
        shell.set_child(Some(&base));

        let arrow_row = album_detail_arrow_row(selected_column, columns);
        arrow_row.set_valign(gtk::Align::Start);
        arrow_row.set_can_target(false);
        shell.add_overlay(&arrow_row);

        content.append(&left);
        content.append(&artwork_column);

        // All widgets exist; request the cover off the GTK thread. The
        // shared loader fires the callback synchronously on a cache hit
        // (the common case — the tile already resolved this path, so the
        // palette and cover appear with no flash) and from the worker on
        // a cold miss. Either way the heavy tag read + decode + palette
        // extraction never runs here.
        if let Some(source) = self.album_artwork_source(album) {
            let content_for_callback = content.clone();
            let cover_for_callback = detail_cover.clone();
            let generation_cell = self.detail_generation.clone();
            self.artwork_loader.request_detail(
                source,
                Box::new(move |decoded| {
                    // A newer album selection has superseded this panel;
                    // dropping here prevents a stale, display-wide palette
                    // install from mis-tinting the current detail.
                    if generation_cell.get() != detail_generation_snapshot {
                        return;
                    }
                    if let Some(palette) = decoded.palette {
                        let provider = album_detail_palette_provider(palette);
                        install_palette_provider(&content_for_callback, Some(&provider));
                    }
                    apply_cover_texture(
                        &cover_for_callback,
                        decoded.detail_texture,
                        ALBUM_DETAIL_ARTWORK_SIZE,
                    );
                }),
            );
        }

        shell
    }

    fn album_track_lists(&self, album: &AlbumViewModel) -> gtk::Box {
        let lists = gtk::Box::new(gtk::Orientation::Horizontal, 40);
        lists.add_css_class("album-track-lists");
        lists.set_hexpand(true);

        let split_index = album.tracks.len().div_ceil(2);
        let playing_track_id = self.playing_track_id.get();
        let ordered_track_ids: Vec<TrackId> = album.tracks.iter().map(|track| track.id).collect();

        let left = AlbumTrackListView::new(
            &album.tracks[..split_index],
            ordered_track_ids.clone(),
            self.context_menu.clone(),
            self.command_controller.clone(),
            self.playback_changed.clone(),
            self.locate_missing_track.clone(),
            playing_track_id,
        );
        let right = AlbumTrackListView::new(
            &album.tracks[split_index..],
            ordered_track_ids,
            self.context_menu.clone(),
            self.command_controller.clone(),
            self.playback_changed.clone(),
            self.locate_missing_track.clone(),
            playing_track_id,
        );

        let left_widget = left.widget();
        let right_widget = right.widget();
        left_widget.set_hexpand(true);
        right_widget.set_hexpand(true);

        lists.append(&left_widget);
        lists.append(&right_widget);
        lists
    }

    /// Resolves the artwork source the loader should read for an
    /// album cover. Mirrors what the synchronous reader used to do
    /// inline: prefer the first non-missing track, fall back to the
    /// first track of any kind, and turn relative paths into absolute
    /// paths against the configured library root. The source keeps the
    /// original relative path as its cache key so cache rows survive
    /// library-root moves. Returns `None` only when no library root is set
    /// or no representative track exists.
    fn album_artwork_source(&self, album: &AlbumViewModel) -> Option<ArtworkSource> {
        let relative = album.representative_track_path.as_ref()?;
        if relative.is_absolute() {
            return Some(ArtworkSource::embedded_track(
                relative.clone(),
                relative.clone(),
            ));
        }
        let root = self
            .runtime
            .borrow()
            .settings()
            .library_path()?
            .to_path_buf();
        Some(ArtworkSource::embedded_track(
            relative.clone(),
            root.join(relative),
        ))
    }
}

struct AlbumRowWidgets {
    shell: gtk::Box,
    tile_row: gtk::Box,
    empty_state: gtk::Label,
    slots: Vec<AlbumTileSlot>,
    detail: Option<gtk::Overlay>,
}

impl AlbumRowWidgets {
    fn new() -> Self {
        let shell = gtk::Box::new(gtk::Orientation::Vertical, ALBUM_GRID_ROW_SPACING);
        shell.add_css_class("album-row");
        shell.set_hexpand(true);

        let tile_row = gtk::Box::new(gtk::Orientation::Horizontal, ALBUM_GRID_COLUMN_SPACING);
        tile_row.set_homogeneous(true);
        tile_row.set_margin_start(ALBUM_GRID_MARGIN);
        tile_row.set_margin_end(ALBUM_GRID_MARGIN);
        shell.append(&tile_row);

        let empty_state = empty_albums_label();
        empty_state.set_visible(false);
        shell.append(&empty_state);

        Self {
            shell,
            tile_row,
            empty_state,
            slots: Vec::new(),
            detail: None,
        }
    }

    fn ensure_slot_count(&mut self, count: usize, view: &AlbumsView) {
        while self.slots.len() < count {
            let slot = AlbumTileSlot::new(view);
            self.tile_row.append(&slot.container);
            self.slots.push(slot);
        }
    }

    fn show_empty_state(&mut self) {
        self.tile_row.set_visible(false);
        self.empty_state.set_visible(true);
        self.set_detail(None);
    }

    fn show_tiles(&self) {
        self.empty_state.set_visible(false);
        self.tile_row.set_visible(true);
    }

    fn set_detail(&mut self, detail: Option<gtk::Overlay>) {
        if let Some(previous) = self.detail.take() {
            self.shell.remove(&previous);
        }
        if let Some(detail) = detail {
            self.shell.append(&detail);
            self.detail = Some(detail);
        }
    }
}

struct AlbumTileSlot {
    container: gtk::Box,
    button: gtk::Button,
    content: AlbumTileContent,
    bound_album_key: Rc<RefCell<Option<AlbumKey>>>,
    bound_artwork_source: Option<ArtworkSource>,
    artwork_binding_generation: Rc<Cell<u64>>,
    cover_size: Rc<Cell<i32>>,
    is_selected: bool,
}

impl AlbumTileSlot {
    fn new(view: &AlbumsView) -> Self {
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        container.set_width_request(ALBUM_TILE_MIN_WIDTH);
        container.set_hexpand(true);

        let content = build_album_tile_content("", "", ALBUM_TILE_COVER_SIZE);
        let button = gtk::Button::new();
        button.add_css_class("album-tile");
        button.set_child(Some(&content.root));
        button.set_can_shrink(true);
        button.set_width_request(ALBUM_TILE_WIDTH);
        button.set_halign(gtk::Align::Fill);
        button.set_valign(gtk::Align::Start);
        button.set_hexpand(true);
        button.set_overflow(gtk::Overflow::Hidden);
        button.set_visible(false);
        container.append(&button);

        let bound_album_key = Rc::new(RefCell::new(None::<AlbumKey>));
        let bound_album_key_for_click = bound_album_key.clone();
        let view_for_click = view.clone();
        button.connect_clicked(move |_| {
            let Some(album_key) = bound_album_key_for_click.borrow().clone() else {
                return;
            };
            view_for_click.select_album(album_key);
        });

        Self {
            container,
            button,
            content,
            bound_album_key,
            bound_artwork_source: None,
            artwork_binding_generation: Rc::new(Cell::new(0)),
            cover_size: Rc::new(Cell::new(ALBUM_TILE_COVER_SIZE)),
            is_selected: false,
        }
    }

    fn bind(
        &mut self,
        view: &AlbumsView,
        album: &AlbumViewModel,
        is_selected: bool,
        refresh_artwork: bool,
    ) {
        self.button.set_visible(true);
        if self.content.title.text() != album.title {
            self.content.title.set_text(&album.title);
        }
        if self.content.artist.text() != album.artist {
            self.content.artist.set_text(&album.artist);
        }
        if self.bound_album_key.borrow().as_ref() != Some(&album.key) {
            self.bound_album_key.borrow_mut().replace(album.key.clone());
        }
        self.set_selected(is_selected);
        self.bind_artwork(
            &view.artwork_loader,
            view.album_artwork_source(album),
            refresh_artwork,
        );
    }

    fn show_placeholder(&mut self) {
        self.button.set_visible(false);
        self.bound_album_key.borrow_mut().take();
        self.set_selected(false);
        if self.bound_artwork_source.take().is_some() {
            self.bump_artwork_generation();
            apply_cover_texture(&self.content.cover, None, self.cover_size.get());
        }
    }

    fn set_selected(&mut self, is_selected: bool) {
        if self.is_selected == is_selected {
            return;
        }
        self.is_selected = is_selected;
        if is_selected {
            self.button.add_css_class("selected");
        } else {
            self.button.remove_css_class("selected");
        }
        let cover_size = if is_selected {
            ALBUM_TILE_COVER_SIZE_EXPANDED
        } else {
            ALBUM_TILE_COVER_SIZE
        };
        self.cover_size.set(cover_size);
        resize_album_tile_content(&self.content, cover_size);
    }

    fn bind_artwork(
        &mut self,
        loader: &ArtworkLoader,
        source: Option<ArtworkSource>,
        refresh: bool,
    ) {
        if self.bound_artwork_source == source && !refresh {
            return;
        }
        self.bound_artwork_source = source.clone();
        let generation_snapshot = self.bump_artwork_generation();
        let Some(source) = source else {
            apply_cover_texture(&self.content.cover, None, self.cover_size.get());
            return;
        };

        if let Some(decoded) = loader.cached(&source) {
            apply_cover_texture(
                &self.content.cover,
                decoded.tile_texture,
                self.cover_size.get(),
            );
            return;
        }

        apply_cover_texture(&self.content.cover, None, self.cover_size.get());
        let cover = self.content.cover.clone();
        let cover_size = self.cover_size.clone();
        let generation = self.artwork_binding_generation.clone();
        loader.request(
            source,
            Box::new(move |decoded| {
                if generation.get() != generation_snapshot {
                    return;
                }
                apply_cover_texture(&cover, decoded.tile_texture, cover_size.get());
            }),
        );
    }

    fn bump_artwork_generation(&self) -> u64 {
        let generation = self.artwork_binding_generation.get().wrapping_add(1);
        self.artwork_binding_generation.set(generation);
        generation
    }
}

fn widget_id(widget: &gtk::Box) -> usize {
    widget.as_ptr() as usize
}

fn align_widget_to_viewport_top(
    widget: &impl IsA<gtk::Widget>,
    viewport: &impl IsA<gtk::Widget>,
    adjustment: &gtk::Adjustment,
) -> bool {
    let Some(point) = widget
        .as_ref()
        .compute_point(viewport.as_ref(), &gtk::graphene::Point::new(0.0, 0.0))
    else {
        return false;
    };
    let offset = f64::from(point.y());
    if offset.abs() <= 0.5 {
        return true;
    }

    let previous = adjustment.value();
    adjustment.set_value(previous + offset);
    // A clamped adjustment means the row is as close to the top as the
    // remaining content permits, which is the correct result near list end.
    (adjustment.value() - previous).abs() <= f64::EPSILON
}

fn columns_for_width(width: i32) -> usize {
    let usable_width = width
        .saturating_sub(ALBUM_GRID_MARGIN * 2)
        .max(ALBUM_TILE_MIN_WIDTH);
    ((usable_width + ALBUM_GRID_COLUMN_SPACING)
        / (ALBUM_TILE_MIN_WIDTH + ALBUM_GRID_COLUMN_SPACING))
        .max(1) as usize
}

fn album_row_count(album_count: usize, columns: usize) -> usize {
    album_count.div_ceil(columns.max(1)).max(1)
}

fn resize_row_store(store: &gio::ListStore, row_count: usize) {
    let current = store.n_items() as usize;
    match row_count.cmp(&current) {
        std::cmp::Ordering::Less => {
            store.splice(
                row_count as u32,
                (current - row_count) as u32,
                &[] as &[glib::BoxedAnyObject],
            );
        }
        std::cmp::Ordering::Equal => {}
        std::cmp::Ordering::Greater => {
            let additions: Vec<_> = (current..row_count)
                .map(|_| glib::BoxedAnyObject::new(()))
                .collect();
            store.splice(current as u32, 0, &additions);
        }
    }
}

fn selected_album_in_row<'a>(
    albums: &'a [AlbumViewModel],
    selected_album: Option<&AlbumKey>,
) -> Option<(usize, &'a AlbumViewModel)> {
    let selected_album = selected_album?;
    albums
        .iter()
        .enumerate()
        .find(|(_, album)| &album.key == selected_album)
}

fn empty_albums_label() -> gtk::Label {
    let label = gtk::Label::new(Some("No albums"));
    label.add_css_class("album-empty-state");
    label.set_margin_top(24);
    label.set_margin_end(24);
    label.set_margin_bottom(24);
    label.set_margin_start(24);
    label
}

struct AlbumTileContent {
    root: gtk::Box,
    cover: gtk::Overlay,
    title: gtk::Label,
    artist: gtk::Label,
}

/// A tile caption (title or artist), pinned to the cover width and ellipsized
/// within it. `set_size_request` floors both the minimum and natural width to
/// the cover; capping `max-width-chars` keeps a long value's own natural-width
/// request from inflating the centered tile past the cover and misaligning the
/// grid (issue #125). The pixel width — not the character cap — governs the
/// rendered size, so the cap is just a small sentinel.
fn album_tile_label(text: &str, css_class: &str, cover_size: i32, margin_top: i32) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class(css_class);
    label.set_size_request(cover_size, -1);
    label.set_max_width_chars(1);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_xalign(0.0);
    label.set_halign(gtk::Align::Start);
    label.set_margin_top(margin_top);
    label
}

fn build_album_tile_content(
    title_text: &str,
    artist_text: &str,
    cover_size: i32,
) -> AlbumTileContent {
    // Per-label margins instead of a uniform box spacing so the
    // title→artist gap can be tighter than the cover→title gap.
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_width_request(cover_size);
    root.set_halign(gtk::Align::Center);
    root.set_valign(gtk::Align::Start);
    root.set_overflow(gtk::Overflow::Hidden);

    let cover = build_cover_widget(cover_size, "album-cover");
    cover.set_halign(gtk::Align::Start);
    root.append(&cover);

    let title = album_tile_label(title_text, "album-tile-title", cover_size, 6);
    root.append(&title);

    let artist = album_tile_label(artist_text, "album-tile-artist", cover_size, 1);
    root.append(&artist);

    AlbumTileContent {
        root,
        cover,
        title,
        artist,
    }
}

fn resize_album_tile_content(content: &AlbumTileContent, size: i32) {
    content.root.set_width_request(size);
    content.title.set_size_request(size, -1);
    content.artist.set_size_request(size, -1);
    resize_cover_widget(&content.cover, size);
}

fn build_cover_widget(size: i32, css_class: &str) -> gtk::Overlay {
    let cover = gtk::Overlay::new();
    cover.add_css_class(css_class);
    cover.set_size_request(size, size);
    cover.set_halign(gtk::Align::Center);
    cover.set_valign(gtk::Align::Start);
    cover.set_hexpand(false);
    cover.set_vexpand(false);
    cover.set_overflow(gtk::Overflow::Hidden);

    // GtkWidget::set_size_request is a minimum, not a fixed size. Keep
    // placeholder/picture requisitions out of the cover's measurement so an
    // asynchronously loaded texture can never grow the cover and push the
    // tile labels down. With no main child, this explicit square request is
    // the overlay's only geometry source.
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_size_request(size, size);
    picture.set_halign(gtk::Align::Fill);
    picture.set_valign(gtk::Align::Fill);
    picture.set_hexpand(false);
    picture.set_vexpand(false);
    picture.set_visible(false);
    cover.add_overlay(&picture);
    cover.set_clip_overlay(&picture, true);
    cover.set_measure_overlay(&picture, false);

    if let Some(icon) = album_cover_placeholder(size) {
        cover.add_overlay(&icon);
        cover.set_clip_overlay(&icon, true);
        cover.set_measure_overlay(&icon, false);
    }
    cover
}

/// Replaces the cover widget's current contents with either the decoded
/// image or the placeholder icon. Used both at construction time (called
/// with `None` to install the placeholder) and from the artwork loader
/// callback (called with the decoded texture once it arrives).
fn apply_cover_texture(cover: &gtk::Overlay, texture: Option<gdk::Texture>, size: i32) {
    resize_cover_widget(cover, size);
    let Some(picture) = cover
        .first_child()
        .and_then(|child| child.downcast::<gtk::Picture>().ok())
    else {
        return;
    };
    picture.set_paintable(texture.as_ref());
    picture.set_visible(texture.is_some());
    if let Some(icon) = picture
        .next_sibling()
        .and_then(|child| child.downcast::<gtk::Image>().ok())
    {
        icon.set_visible(texture.is_none());
    }

    if texture.is_some() {
        cover.add_css_class("has-artwork");
    } else {
        cover.remove_css_class("has-artwork");
    }
}

fn resize_cover_widget(cover: &gtk::Overlay, size: i32) {
    cover.set_size_request(size, size);
    if let Some(picture) = cover
        .first_child()
        .and_then(|child| child.downcast::<gtk::Picture>().ok())
    {
        picture.set_size_request(size, size);
        if let Some(icon) = picture
            .next_sibling()
            .and_then(|child| child.downcast::<gtk::Image>().ok())
        {
            icon.set_pixel_size((size / 3).max(32));
            icon.set_size_request(size, size);
        }
    }
}

/// Build a cover widget with an immediately-applied texture. Used by
/// the album detail panel, which resolves artwork synchronously via
/// the loader's cache (or a one-off sync read) and has the texture in
/// hand at construction time.
fn album_cover_with(texture: Option<gdk::Texture>, size: i32, css_class: &str) -> gtk::Overlay {
    let cover = build_cover_widget(size, css_class);
    if texture.is_some() {
        apply_cover_texture(&cover, texture, size);
    }
    cover
}

fn album_cover_placeholder(size: i32) -> Option<gtk::Image> {
    let display = gtk::gdk::Display::default()?;
    let theme = gtk::IconTheme::for_display(&display);
    if !theme.has_icon(ALBUM_COVER_PLACEHOLDER_ICON) {
        return None;
    }

    let icon = gtk::Image::from_icon_name(ALBUM_COVER_PLACEHOLDER_ICON);
    icon.add_css_class("album-cover-placeholder-icon");
    icon.set_pixel_size((size / 3).max(32));
    icon.set_size_request(size, size);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    icon.set_hexpand(false);
    icon.set_vexpand(false);
    Some(icon)
}

fn album_detail_arrow_row(selected_column: usize, columns: usize) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, ALBUM_GRID_COLUMN_SPACING);
    row.set_homogeneous(true);
    row.set_margin_start(ALBUM_GRID_MARGIN);
    row.set_margin_end(ALBUM_GRID_MARGIN);
    row.set_height_request(ALBUM_DETAIL_ARROW_HEIGHT + ALBUM_DETAIL_ARROW_BLEED);

    for column in 0..columns {
        let cell = gtk::Box::new(gtk::Orientation::Vertical, 0);
        cell.set_halign(gtk::Align::Fill);
        cell.set_hexpand(true);

        if column == selected_column {
            cell.append(&album_detail_arrow());
        }

        row.append(&cell);
    }

    row
}

fn album_detail_arrow() -> gtk::DrawingArea {
    let arrow = gtk::DrawingArea::new();
    arrow.add_css_class("album-detail-arrow");
    apply_palette_class(&arrow, "album-detail-palette-arrow");
    arrow.set_content_width(ALBUM_DETAIL_ARROW_WIDTH);
    arrow.set_content_height(ALBUM_DETAIL_ARROW_HEIGHT + ALBUM_DETAIL_ARROW_BLEED);
    arrow.set_halign(gtk::Align::Center);
    arrow.set_valign(gtk::Align::End);
    arrow.set_draw_func(|area, context, width, _height| {
        let color = area.color();
        let arrow_width = f64::from(width);
        let arrow_height = f64::from(ALBUM_DETAIL_ARROW_HEIGHT);
        let bleed = f64::from(ALBUM_DETAIL_ARROW_BLEED);

        // The arrow color is driven by CSS so it stays in sync with the
        // panel: `.album-detail-arrow` matches the default panel tint, and
        // `.album-detail-palette-arrow` (applied when artwork yields a
        // palette) matches the palette background. Alpha is forced to 1.0
        // so the panel's 1px overlap below can't composite onto a
        // translucent fill and produce a darker stripe at the seam.
        context.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            1.0,
        );

        context.move_to(arrow_width / 2.0, 0.0);
        context.line_to(arrow_width, arrow_height);
        context.line_to(0.0, arrow_height);
        context.close_path();
        let _result = context.fill();

        context.rectangle(0.0, arrow_height, arrow_width, bleed);
        let _result = context.fill();
    });

    arrow
}

fn album_detail_palette_provider(palette: ArtworkPalette) -> gtk::CssProvider {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&album_detail_palette_css(palette));
    provider
}

/// Tag `widget` with an album-detail palette class. The class is inert
/// until the artwork-derived palette provider is installed display-wide
/// (asynchronously, once the cover decodes — #107); albums without
/// artwork never install one, so the class resolves to the default
/// theme. Classes are therefore always added; only the provider install
/// is conditional on a palette being available.
fn apply_palette_class(widget: &impl IsA<gtk::Widget>, css_class: &str) {
    widget.as_ref().add_css_class(css_class);
}

fn install_palette_provider(widget: &impl IsA<gtk::Widget>, provider: Option<&gtk::CssProvider>) {
    let (Some(display), Some(provider)) = (gdk::Display::default(), provider) else {
        return;
    };

    gtk::style_context_add_provider_for_display(
        &display,
        provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
    );

    let provider = provider.clone();
    widget.as_ref().connect_destroy(move |_| {
        gtk::style_context_remove_provider_for_display(&display, &provider);
    });
}

fn album_detail_palette_css(palette: ArtworkPalette) -> String {
    let background = palette.background_css();
    let foreground = palette.foreground_css();
    let secondary = palette.secondary_css();

    format!(
        r#"
        .album-detail-dominant-color {{
            background-color: {background};
            border: none;
            color: {foreground};
        }}

        .album-detail-palette-arrow {{
            color: {background};
        }}

        .album-detail-palette-primary,
        button.album-detail-palette-button,
        image.album-detail-palette-primary {{
            color: {foreground};
        }}

        /* Artist name (subtitle), track number, and duration share the
           artwork-derived secondary colour so the muted text reads as
           part of the cover's palette instead of as a uniformly faded
           white/black. The track-playing speaker icon is intentionally
           not in this set: it keeps the strict-contrast foreground so
           the "now playing" cue is unmissable on any artwork. */
        .album-detail-palette-secondary,
        .album-detail-palette-muted {{
            color: {secondary};
        }}

        .album-detail-palette-surface {{
            background-color: alpha({foreground}, 0.12);
        }}

        button.album-detail-palette-button:hover,
        button.album-detail-palette-button:active,
        button.album-detail-palette-button:focus {{
            background-color: alpha({foreground}, 0.14);
        }}

        .album-track-table .track-table-status-playing {{
            color: {foreground};
        }}

        listview.album-track-table > row:focus-visible {{
            outline-color: {foreground};
        }}
        "#,
    )
}

fn detail_icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(18);
    apply_palette_class(&icon, "album-detail-palette-primary");

    let button = gtk::Button::new();
    button.add_css_class("album-detail-icon-button");
    apply_palette_class(&button, "album-detail-palette-button");
    button.set_child(Some(&icon));
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);
    button
}

fn play_album(command_controller: &SharedCommandController, album: &AlbumViewModel) -> bool {
    // First playable track is the one that actually starts; the queue
    // pins the rest of the album behind it so auto-advance walks the
    // album in display order instead of leaking into the library.
    let ordered_track_ids: Vec<TrackId> = album
        .tracks
        .iter()
        .filter(|track| !track.is_missing)
        .map(|track| track.id)
        .collect();
    let Some(&track_id) = ordered_track_ids.first() else {
        return false;
    };

    command_controller.dispatch_succeeded(ApplicationCommand::Playback(
        PlaybackCommand::PlayTrack {
            track_id,
            queue: PlaybackQueueRequest::Explicit {
                source: PlaybackQueueSource::Album,
                ordered_track_ids,
            },
        },
    ))
}

pub(crate) fn ensure_shuffle_disabled(command_controller: &SharedCommandController) {
    set_shuffle_mode(command_controller, ShuffleMode::Off);
}

fn set_shuffle_mode(command_controller: &SharedCommandController, mode: ShuffleMode) {
    let _result = command_controller.dispatch(ApplicationCommand::Playback(
        PlaybackCommand::SetShuffleMode(mode),
    ));
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::HashMap, rc::Rc, time::Duration};

    use gtk::prelude::*;

    use super::{
        ALBUM_GRID_COLUMN_SPACING, ALBUM_GRID_MARGIN, ALBUM_TILE_COVER_SIZE,
        ALBUM_TILE_COVER_SIZE_EXPANDED, ALBUM_TILE_MIN_WIDTH, ALBUM_TILE_WIDTH, album_row_count,
        align_widget_to_viewport_top, apply_cover_texture, build_album_tile_content,
        build_cover_widget, columns_for_width, resize_row_store,
    };

    #[test]
    fn columns_follow_available_width() {
        assert_eq!(columns_for_width(120), 1);
        assert_eq!(columns_for_width(520), 2);
        assert_eq!(columns_for_width(1200), 6);
        assert_eq!(columns_for_width(2400), 13);
    }

    #[test]
    fn columns_account_for_spacing_between_tiles() {
        let two_column_width =
            ALBUM_GRID_MARGIN * 2 + ALBUM_TILE_MIN_WIDTH * 2 + ALBUM_GRID_COLUMN_SPACING;

        assert_eq!(columns_for_width(two_column_width - 1), 1);
        assert_eq!(columns_for_width(two_column_width), 2);
    }

    #[test]
    fn row_count_never_drops_the_empty_state_row() {
        assert_eq!(album_row_count(0, 6), 1);
        assert_eq!(album_row_count(1, 6), 1);
        assert_eq!(album_row_count(6, 6), 1);
        assert_eq!(album_row_count(7, 6), 2);
    }

    #[test]
    fn resizing_row_store_preserves_surviving_tokens() {
        let store = gtk::gio::ListStore::new::<gtk::glib::BoxedAnyObject>();
        resize_row_store(&store, 10);
        let first = store.item(0).expect("first row token");

        resize_row_store(&store, 7);
        assert_eq!(store.n_items(), 7);
        assert_eq!(store.item(0).as_ref(), Some(&first));

        resize_row_store(&store, 12);
        assert_eq!(store.n_items(), 12);
        assert_eq!(store.item(0).as_ref(), Some(&first));
    }

    #[test]
    fn realized_row_can_be_aligned_to_viewport_top() {
        let ran = crate::test_support::with_gtk(|| {
            let store = gtk::gio::ListStore::new::<gtk::glib::BoxedAnyObject>();
            resize_row_store(&store, 40);
            let selection = gtk::NoSelection::new(Some(store));
            let rows = Rc::new(RefCell::new(HashMap::<u32, gtk::Box>::new()));
            let factory = gtk::SignalListItemFactory::new();
            factory.connect_setup(move |_factory, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("list item setup");
                let row = gtk::Box::new(gtk::Orientation::Vertical, 0);
                row.set_height_request(48);
                item.set_child(Some(&row));
            });
            let rows_for_bind = rows.clone();
            factory.connect_bind(move |_factory, item| {
                let item = item
                    .downcast_ref::<gtk::ListItem>()
                    .expect("list item bind");
                let row = item
                    .child()
                    .and_then(|child| child.downcast::<gtk::Box>().ok())
                    .expect("row shell");
                rows_for_bind
                    .borrow_mut()
                    .retain(|_, bound_row| bound_row != &row);
                rows_for_bind.borrow_mut().insert(item.position(), row);
            });

            let list = gtk::ListView::new(Some(selection), Some(factory));
            let scroller = gtk::ScrolledWindow::new();
            scroller.set_child(Some(&list));
            let window = gtk::Window::new();
            window.set_default_size(320, 220);
            window.set_child(Some(&scroller));
            window.present();
            pump_gtk();

            let scroll_info = gtk::ScrollInfo::new();
            scroll_info.set_enable_horizontal(false);
            scroll_info.set_enable_vertical(true);
            list.scroll_to(12, gtk::ListScrollFlags::NONE, Some(scroll_info));
            let target = wait_for_row(&rows, 12);
            for _ in 0..20 {
                if align_widget_to_viewport_top(&target, &list, &scroller.vadjustment()) {
                    break;
                }
                pump_gtk();
            }
            pump_gtk();

            assert!(
                widget_y_in_widget(&target, &list).abs() <= 0.5,
                "target row should be aligned to the viewport top"
            );
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn loaded_artwork_cannot_change_cover_requisition() {
        let ran = crate::test_support::with_gtk(|| {
            let cover = build_cover_widget(ALBUM_TILE_COVER_SIZE, "album-cover");
            let before = cover_requisition(&cover);
            let initial_visual = cover.first_child().expect("stable picture overlay");

            apply_cover_texture(&cover, Some(oversized_texture()), ALBUM_TILE_COVER_SIZE);

            assert_eq!(cover_requisition(&cover), before);
            assert!(cover.child().is_none());
            let visual = cover.first_child().expect("artwork overlay");
            assert_eq!(visual, initial_visual);
            assert!(!cover.is_measure_overlay(&visual));

            apply_cover_texture(&cover, None, ALBUM_TILE_COVER_SIZE);
            assert_eq!(
                cover.first_child().as_ref(),
                Some(&initial_visual),
                "texture changes reuse the existing picture widget"
            );
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn album_tile_title_offset_is_stable_across_artwork_and_selection_states() {
        let ran = crate::test_support::with_gtk(|| {
            let (placeholder, placeholder_title) = album_tile_for_alignment_test(false, None);
            let (loaded, loaded_title) =
                album_tile_for_alignment_test(false, Some(oversized_texture()));
            let (selected_placeholder, selected_placeholder_title) =
                album_tile_for_alignment_test(true, None);
            let (selected, selected_title) =
                album_tile_for_alignment_test(true, Some(oversized_texture()));

            let row = gtk::Box::new(gtk::Orientation::Horizontal, ALBUM_GRID_COLUMN_SPACING);
            row.set_homogeneous(true);
            row.append(&placeholder);
            row.append(&loaded);
            row.append(&selected_placeholder);
            row.append(&selected);

            let window = gtk::Window::new();
            window.set_default_size(
                ALBUM_TILE_MIN_WIDTH * 4 + ALBUM_GRID_COLUMN_SPACING * 3,
                260,
            );
            window.set_child(Some(&row));
            window.present();
            let ctx = gtk::glib::MainContext::default();
            for _ in 0..200 {
                while ctx.iteration(false) {}
                if placeholder_title.width() > 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(placeholder_title.width() > 0, "title was not allocated");

            let placeholder_y = widget_y_in_widget(&placeholder_title, &row);
            assert_eq!(placeholder_y, 146.0);
            assert_eq!(widget_y_in_widget(&loaded_title, &row), placeholder_y);
            assert_eq!(
                widget_y_in_widget(&selected_placeholder_title, &row),
                placeholder_y
            );
            assert_eq!(widget_y_in_widget(&selected_title, &row), placeholder_y);

            window.set_child(None::<&gtk::Widget>);
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn album_tile_captions_never_widen_the_tile_past_the_cover() {
        let ran = crate::test_support::with_gtk(|| {
            let long = "A ludicrously long album title that would overflow the tile column";
            for cover_size in [ALBUM_TILE_COVER_SIZE, ALBUM_TILE_COVER_SIZE_EXPANDED] {
                let tile = build_album_tile_content(long, long, cover_size);
                let title_natural = tile.title.measure(gtk::Orientation::Horizontal, -1).1;
                let artist_natural = tile.artist.measure(gtk::Orientation::Horizontal, -1).1;
                assert_eq!(
                    title_natural, cover_size,
                    "long title should stay pinned to the {cover_size}px cover, not grow to fit",
                );
                assert_eq!(
                    artist_natural, cover_size,
                    "long artist should stay pinned to the {cover_size}px cover, not grow to fit",
                );
            }
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    fn cover_requisition(cover: &gtk::Overlay) -> ((i32, i32), (i32, i32)) {
        let horizontal = cover.measure(gtk::Orientation::Horizontal, -1);
        let vertical = cover.measure(gtk::Orientation::Vertical, ALBUM_TILE_COVER_SIZE);
        ((horizontal.0, horizontal.1), (vertical.0, vertical.1))
    }

    fn oversized_texture() -> gtk::gdk::Texture {
        let width = ALBUM_TILE_COVER_SIZE * 3;
        let height = ALBUM_TILE_COVER_SIZE * 4;
        let stride = width as usize * 4;
        let bytes = gtk::glib::Bytes::from_owned(vec![0xff; stride * height as usize]);
        gtk::gdk::MemoryTexture::new(
            width,
            height,
            gtk::gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            stride,
        )
        .upcast()
    }

    fn album_tile_for_alignment_test(
        is_selected: bool,
        texture: Option<gtk::gdk::Texture>,
    ) -> (gtk::Button, gtk::Label) {
        let cover_size = if is_selected {
            ALBUM_TILE_COVER_SIZE_EXPANDED
        } else {
            ALBUM_TILE_COVER_SIZE
        };
        let tile = build_album_tile_content("Album", "Artist", cover_size);
        if let Some(texture) = texture {
            apply_cover_texture(&tile.cover, Some(texture), cover_size);
        }
        let button = gtk::Button::new();
        button.add_css_class("album-tile");
        if is_selected {
            button.add_css_class("selected");
        }
        button.set_child(Some(&tile.root));
        button.set_can_shrink(true);
        button.set_width_request(ALBUM_TILE_WIDTH);
        button.set_halign(gtk::Align::Fill);
        button.set_valign(gtk::Align::Start);
        button.set_overflow(gtk::Overflow::Hidden);
        (button, tile.title)
    }

    fn widget_y_in_widget(widget: &impl IsA<gtk::Widget>, target: &impl IsA<gtk::Widget>) -> f32 {
        widget
            .compute_point(target, &gtk::graphene::Point::new(0.0, 0.0))
            .expect("widgets share a root")
            .y()
    }

    fn pump_gtk() {
        let ctx = gtk::glib::MainContext::default();
        while ctx.iteration(false) {}
    }

    fn wait_for_row(rows: &RefCell<HashMap<u32, gtk::Box>>, position: u32) -> gtk::Box {
        for _ in 0..200 {
            pump_gtk();
            if let Some(row) = rows.borrow().get(&position).cloned() {
                return row;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        rows.borrow()
            .get(&position)
            .cloned()
            .expect("target row was realized")
    }
}
