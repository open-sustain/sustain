// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{gio, glib};
use sustain_app_runtime::{
    ApplicationCommand, PlaybackCommand, PlaybackQueueRequest, PlaybackQueueSource, ShuffleMode,
    TrackId, album_matches_search_text,
};

use super::{
    PlaybackChangedCallback, SharedRuntime,
    artwork_loader::{ArtworkLoader, ArtworkSource},
    command_controller::SharedCommandController,
    missing_track::LocateMissingTrackCallback,
    track_context::TrackRowContextMenu,
};
use model::{
    AlbumKey, AlbumTrackReplacement, AlbumViewModel, album_subtitle, album_track, group_albums,
    replace_track_in_album,
};
use track_list::AlbumTrackListView;

mod cover;
mod detail;
mod model;
mod track_list;

use cover::{album_cover_with, apply_cover_texture, build_cover_widget, resize_cover_widget};
use detail::{
    ALBUM_DETAIL_ARROW_HEIGHT, album_detail_arrow_row, album_detail_palette_provider,
    apply_palette_class, detail_icon_button, install_palette_provider,
};

#[derive(Clone)]
pub(crate) struct AlbumsView {
    scroller: gtk::ScrolledWindow,
    content: gtk::Fixed,
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
    /// in place so the manual row pool does not rebuild off-screen tiles.
    realized_rows: Rc<RefCell<HashMap<usize, gtk::Box>>>,
    /// Reusable widget pools owned by the manual row virtualizer.
    /// Scrolling and width breakpoints rebind these slots in place instead of
    /// destroying and reconstructing every visible album tile.
    row_widgets: Rc<RefCell<HashMap<usize, AlbumRowWidgets>>>,
    row_pool: Rc<RefCell<Vec<AlbumRowWidgets>>>,
    row_model_generation: Rc<Cell<u64>>,
    measured_detail_height: Rc<Cell<i32>>,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AlbumDetailKey {
    album_key: AlbumKey,
    selected_column: usize,
    columns: usize,
    playing_track_id: Option<TrackId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum AlbumListRow {
    Tiles { tile_row_index: usize },
    Detail { album_key: AlbumKey },
}

#[derive(Clone, Copy)]
enum AlbumRowRefresh<'a> {
    Rebind,
    Normal,
    Detail(&'a AlbumKey),
    Artwork(&'a AlbumKey),
}

impl AlbumRowRefresh<'_> {
    fn refreshes_detail_for(self, album_key: &AlbumKey) -> bool {
        match self {
            Self::Rebind | Self::Normal => false,
            Self::Detail(key) | Self::Artwork(key) => key == album_key,
        }
    }

    fn refreshes_artwork_for(self, album_key: &AlbumKey) -> bool {
        match self {
            Self::Artwork(key) => key == album_key,
            Self::Rebind | Self::Normal | Self::Detail(_) => false,
        }
    }
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
// A tile's cover + title + artist measures 181 px tall, with the artist
// baseline at the same Y whether the tile is selected or not. These row
// heights are the sole source of truth for the manual virtualizer's vertical
// layout (the tile buttons carry no `min-height`; see app.css), so they must
// stay >= that content height.
//
// The non-selected height keeps 17 px of trailing rhythm above the next grid
// row. The selected row instead hugs its content so the expanded detail panel
// — and its upward arrow — sits just below the selected album's artist line
// rather than floating a full rhythm gap lower.
const ALBUM_TILE_ROW_HEIGHT: i32 = 198;
const ALBUM_TILE_ROW_HEIGHT_SELECTED: i32 = 182;
// Gap between the selected tile row and the expanded panel's arrow tip. Kept
// small so the arrow nearly touches the selected album's artist line, with a
// few pixels of breathing room.
const ALBUM_DETAIL_TOP_GAP: i32 = 9;
const ALBUM_DETAIL_ARTWORK_SIZE: i32 = ALBUM_TILE_COVER_SIZE * 3;
const ALBUM_DETAIL_TEXT_CHARS: i32 = 48;
const ALBUM_DETAIL_HEADER_HEIGHT_ESTIMATE: i32 = 56;
const ALBUM_DETAIL_VERTICAL_PADDING: i32 = 64;
const ALBUM_DETAIL_TRACK_ROW_HEIGHT: i32 = 32;
const ALBUM_SCROLL_REALIZE_BUFFER_PX: f64 = 400.0;

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
        let content = gtk::Fixed::new();
        content.add_css_class("albums-list");
        content.set_hexpand(true);
        content.set_vexpand(false);

        let scroller = gtk::ScrolledWindow::new();
        scroller.add_css_class("albums-view");
        scroller.set_vexpand(true);
        scroller.set_hexpand(true);
        // Rows are rebuilt around the current allocation. Allowing temporary
        // horizontal overflow lets the scroller shrink first; the width watcher
        // then recomputes a smaller column count on the next frame.
        scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
        scroller.set_propagate_natural_width(false);
        scroller.set_child(Some(&content));

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
        Self {
            scroller,
            content,
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
            row_pool: Rc::new(RefCell::new(Vec::new())),
            row_model_generation: Rc::new(Cell::new(0)),
            measured_detail_height: Rc::new(Cell::new(0)),
            pending_scroll_album: Rc::new(RefCell::new(None)),
            visible_columns: Rc::new(Cell::new(1)),
            last_width: Rc::new(Cell::new(0)),
            artwork_loader,
            detail_generation: Rc::new(Cell::new(0)),
            activated: Rc::new(Cell::new(false)),
            playing_track_id: Rc::new(Cell::new(None)),
        }
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
    /// the same album bucket, mirroring the row-level refresh the Songs and
    /// Playlists tables run on `track_data_observer`. Album-structure
    /// metadata edits use `replace_tracks()` instead so regrouping stays
    /// correct. Only the row holding the affected album repaints; the selected
    /// album, scroll position, and every untouched row's artwork stay intact.
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
            (
                album_track(track),
                runtime.settings().library_path().map(ToOwned::to_owned),
            )
        };

        let (new_track_vm, library_root) = new_track_vm;
        let Some(replacement) = replace_track_in_album(
            &mut self.all_albums.borrow_mut(),
            track_id,
            &new_track_vm,
            library_root.as_deref(),
        ) else {
            return;
        };
        let affected_album_key = match replacement {
            AlbumTrackReplacement::Updated(album_key) => album_key,
            AlbumTrackReplacement::Unchanged(_) => return,
        };

        // `albums` is a separately-cloned filtered view of
        // `all_albums`; the on-screen rows read from it, so the same
        // patch has to land here when the album survived the active
        // search.
        let visible_album_present = replace_track_in_album(
            &mut self.albums.borrow_mut(),
            track_id,
            &new_track_vm,
            library_root.as_deref(),
        )
        .is_some();
        if !visible_album_present {
            return;
        }

        self.refresh_album_widgets(
            &affected_album_key,
            AlbumRowRefresh::Detail(&affected_album_key),
        );
    }

    pub(crate) fn refresh_track_artwork(&self, track_id: TrackId) {
        if !self.activated.get() {
            return;
        }
        let album_key = {
            let albums = self.albums.borrow();
            albums
                .iter()
                .find(|album| album.tracks.iter().any(|track| track.id == track_id))
                .map(|album| album.key.clone())
        };
        let Some(album_key) = album_key else {
            return;
        };
        self.refresh_album_widgets(&album_key, AlbumRowRefresh::Artwork(&album_key));
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
            *self.all_albums.borrow_mut() =
                group_albums(runtime.library_tracks(), runtime.settings().library_path());
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

        let previous_detail_tile_row = previous_album
            .as_ref()
            .and_then(|album_key| self.tile_row_index_for_album(album_key));
        let previous_row = previous_album.as_ref().and_then(|album_key| {
            self.tile_list_position_for_album_with_detail(album_key, previous_detail_tile_row)
        });
        let selected_row =
            self.tile_list_position_for_album_with_detail(&album_key, previous_detail_tile_row);

        *self.selected_album.borrow_mut() = Some(album_key.clone());

        if let Some(row_position) = previous_row {
            self.refresh_row_widget(row_position, AlbumRowRefresh::Normal);
        }
        if let Some(row_position) = selected_row
            && selected_row != previous_row
        {
            self.refresh_row_widget(row_position, AlbumRowRefresh::Normal);
        }

        self.sync_detail_row_for_selection(previous_detail_tile_row, &album_key);
    }

    fn refresh_selected_row(&self) {
        let selected_album = self.selected_album.borrow().clone();
        if let Some(album_key) = selected_album.as_ref() {
            self.refresh_album_widgets(album_key, AlbumRowRefresh::Detail(album_key));
        }
    }

    fn request_scroll_to_album(&self, album_key: AlbumKey) {
        self.pending_scroll_album
            .borrow_mut()
            .replace(PendingAlbumScroll { album_key });
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

        let Some(row_y) = self.row_y_for_position(row_position).map(f64::from) else {
            return;
        };
        let adjustment = self.scroller.vadjustment();
        if adjustment.upper() - adjustment.page_size() < row_y {
            return;
        }
        adjustment.set_value(row_y);
        self.pending_scroll_album.borrow_mut().take();
    }

    fn install_width_watcher(&self) {
        let view = self.clone();
        self.scroller.add_tick_callback(move |scroller, _clock| {
            view.sync_columns_for_width(scroller.width());
            view.update_visible_rows();
            if view.pending_scroll_album.borrow().is_some() {
                view.scroll_pending_album_if_ready();
            }

            glib::ControlFlow::Continue
        });
    }

    fn sync_columns_for_width(&self, width: i32) {
        if width <= 0 || self.last_width.replace(width) == width {
            return;
        }
        let columns = columns_for_width(width);
        if self.visible_columns.replace(columns) != columns {
            self.rebuild_rows();
        }
    }

    fn rebuild_rows(&self) {
        if !self.activated.get() {
            return;
        }
        let columns = self.visible_columns.get().max(1);
        let rows = {
            let albums = self.albums.borrow();
            album_list_rows(&albums, columns, self.selected_album.borrow().as_ref())
        };
        self.row_model_generation
            .set(self.row_model_generation.get().wrapping_add(1));
        self.measured_detail_height.set(0);
        replace_row_store(&self.row_store, rows);
        self.clear_realized_rows();
        self.update_visible_rows();
    }

    fn refresh_row_widget(&self, row_position: usize, refresh: AlbumRowRefresh<'_>) {
        let Some(row_shell) = self.realized_rows.borrow().get(&row_position).cloned() else {
            return;
        };
        self.render_row_position(&row_shell, row_position, refresh);
    }

    fn update_visible_rows(&self) {
        if !self.activated.get() || self.scroller.width() <= 0 {
            return;
        }
        self.sync_realized_detail_height();
        let width = self.scroller.width();
        let layouts = self.row_layouts();
        let total_height = layouts
            .last()
            .map(|(_, y, height)| y + height + ALBUM_GRID_MARGIN)
            .unwrap_or(ALBUM_GRID_MARGIN * 2)
            .max(1);
        self.content.set_size_request(width, total_height);

        let adjustment = self.scroller.vadjustment();
        let visible_top = adjustment.value() - ALBUM_SCROLL_REALIZE_BUFFER_PX;
        let visible_bottom =
            adjustment.value() + adjustment.page_size() + ALBUM_SCROLL_REALIZE_BUFFER_PX;
        let visible_positions: std::collections::HashSet<_> = layouts
            .iter()
            .filter_map(|(position, y, height)| {
                let top = f64::from(*y);
                let bottom = f64::from(*y + *height);
                (bottom >= visible_top && top <= visible_bottom).then_some(*position)
            })
            .collect();

        let realized_positions: Vec<_> = self.realized_rows.borrow().keys().copied().collect();
        for position in realized_positions {
            if !visible_positions.contains(&position) {
                self.release_row(position);
            }
        }

        for (position, y, _height) in layouts {
            if visible_positions.contains(&position) {
                self.realize_row(position, y, width);
            }
        }
    }

    fn row_layouts(&self) -> Vec<(usize, i32, i32)> {
        let mut y = ALBUM_GRID_MARGIN;
        let mut layouts = Vec::with_capacity(self.row_store.n_items() as usize);
        for position in 0..self.row_store.n_items() as usize {
            let height = self.row_height(position);
            layouts.push((position, y, height));
            y += height;
        }
        layouts
    }

    fn row_y_for_position(&self, row_position: usize) -> Option<i32> {
        let mut y = ALBUM_GRID_MARGIN;
        for position in 0..self.row_store.n_items() as usize {
            if position == row_position {
                return Some(y);
            }
            y += self.row_height(position);
        }
        None
    }

    fn row_height(&self, row_position: usize) -> i32 {
        match album_list_row_at(&self.row_store, row_position) {
            Some(AlbumListRow::Tiles { tile_row_index }) => self.tile_row_height(tile_row_index),
            Some(AlbumListRow::Detail { album_key }) => self.detail_row_height(&album_key),
            None => ALBUM_TILE_ROW_HEIGHT,
        }
    }

    fn tile_row_height(&self, tile_row_index: usize) -> i32 {
        if self.selected_detail_tile_row_index() == Some(tile_row_index) {
            ALBUM_TILE_ROW_HEIGHT_SELECTED
        } else {
            ALBUM_TILE_ROW_HEIGHT
        }
    }

    fn detail_row_height(&self, album_key: &AlbumKey) -> i32 {
        let is_selected = self
            .selected_album
            .borrow()
            .as_ref()
            .is_some_and(|selected| selected == album_key);
        let measured_height = self.measured_detail_height.get();
        if is_selected && measured_height > 0 {
            return measured_height;
        }

        // Only the track count feeds the estimate — read it through the
        // borrow instead of cloning the whole album (tracks included) on
        // every layout pass that runs before the height is measured.
        let track_count = self
            .albums
            .borrow()
            .iter()
            .find(|album| &album.key == album_key)
            .map(|album| album.tracks.len());
        let Some(track_count) = track_count else {
            return 0;
        };
        let track_rows = track_count.div_ceil(2).max(1) as i32;
        let left_height =
            ALBUM_DETAIL_HEADER_HEIGHT_ESTIMATE + 14 + track_rows * ALBUM_DETAIL_TRACK_ROW_HEIGHT;
        let panel_height =
            left_height.max(ALBUM_DETAIL_ARTWORK_SIZE) + ALBUM_DETAIL_VERTICAL_PADDING;
        ALBUM_DETAIL_TOP_GAP + ALBUM_DETAIL_ARROW_HEIGHT + panel_height + ALBUM_DETAIL_ARROW_HEIGHT
    }

    fn sync_realized_detail_height(&self) {
        let Some(detail_position) = self.selected_detail_position() else {
            self.measured_detail_height.set(0);
            return;
        };
        let Some(detail_shell) = self.realized_rows.borrow().get(&detail_position).cloned() else {
            return;
        };
        let height = detail_shell.height();
        if height > 0 && height != self.measured_detail_height.get() {
            self.measured_detail_height.set(height);
        }
    }

    fn realize_row(&self, row_position: usize, y: i32, width: i32) {
        if let Some(row_shell) = self.realized_rows.borrow().get(&row_position).cloned() {
            self.content.move_(&row_shell, 0.0, f64::from(y));
            row_shell.set_size_request(width, -1);
            self.render_row_position(&row_shell, row_position, AlbumRowRefresh::Rebind);
            return;
        }

        let mut row = self
            .row_pool
            .borrow_mut()
            .pop()
            .unwrap_or_else(AlbumRowWidgets::new);
        row.shell.set_size_request(width, -1);
        row.bound_row = None;
        row.bound_generation = 0;
        let row_shell = row.shell.clone();
        self.content.put(&row_shell, 0.0, f64::from(y));
        self.row_widgets
            .borrow_mut()
            .insert(widget_id(&row_shell), row);
        self.realized_rows
            .borrow_mut()
            .insert(row_position, row_shell.clone());
        self.render_row_position(&row_shell, row_position, AlbumRowRefresh::Rebind);
    }

    fn release_row(&self, row_position: usize) {
        let Some(row_shell) = self.realized_rows.borrow_mut().remove(&row_position) else {
            return;
        };
        self.content.remove(&row_shell);
        if let Some(mut row) = self.row_widgets.borrow_mut().remove(&widget_id(&row_shell)) {
            row.set_detail(None, None);
            row.bound_row = None;
            row.bound_generation = 0;
            self.row_pool.borrow_mut().push(row);
        }
    }

    fn clear_realized_rows(&self) {
        let positions: Vec<_> = self.realized_rows.borrow().keys().copied().collect();
        for position in positions {
            self.release_row(position);
        }
    }

    fn row_position_for_album(&self, album_key: &AlbumKey) -> Option<usize> {
        self.tile_list_position_for_album_with_detail(
            album_key,
            self.selected_detail_tile_row_index(),
        )
    }

    fn tile_row_index_for_album(&self, album_key: &AlbumKey) -> Option<usize> {
        let columns = self.visible_columns.get().max(1);
        self.albums
            .borrow()
            .iter()
            .position(|album| &album.key == album_key)
            .map(|album_index| album_index / columns)
    }

    fn tile_list_position_for_album_with_detail(
        &self,
        album_key: &AlbumKey,
        detail_tile_row_index: Option<usize>,
    ) -> Option<usize> {
        let tile_row_index = self.tile_row_index_for_album(album_key)?;
        Some(tile_list_position_for_tile_row(
            tile_row_index,
            detail_tile_row_index,
        ))
    }

    fn selected_detail_tile_row_index(&self) -> Option<usize> {
        let selected_album = self.selected_album.borrow().clone()?;
        self.tile_row_index_for_album(&selected_album)
    }

    fn selected_detail_position(&self) -> Option<usize> {
        self.selected_detail_tile_row_index()
            .map(detail_position_for_tile_row)
    }

    /// Move the (single) detail row in the row store to follow a new
    /// selection. The realized-row map is *reindexed* for the removal and
    /// insertion instead of being cleared: a detail move only shifts the
    /// list positions after the affected row, so every realized shell
    /// keeps its widget tree and `update_visible_rows` re-renders it in
    /// place (the generation bump invalidates the bind), releasing only
    /// rows that actually leave the viewport. Releasing and re-realizing
    /// all ~40–60 visible rows per album click was the bulk of the
    /// selection stutter.
    fn sync_detail_row_for_selection(
        &self,
        previous_detail_tile_row: Option<usize>,
        selected_album: &AlbumKey,
    ) {
        let Some(selected_tile_row) = self.tile_row_index_for_album(selected_album) else {
            self.selected_album.borrow_mut().take();
            if let Some(previous_tile_row) = previous_detail_tile_row {
                let removed_position = detail_position_for_tile_row(previous_tile_row);
                remove_row_store_item(&self.row_store, removed_position);
                self.shift_realized_rows_for_removal(removed_position);
            }
            self.row_model_generation
                .set(self.row_model_generation.get().wrapping_add(1));
            self.measured_detail_height.set(0);
            self.update_visible_rows();
            return;
        };

        let detail_row = AlbumListRow::Detail {
            album_key: selected_album.clone(),
        };
        if previous_detail_tile_row == Some(selected_tile_row) {
            // Same tile row: the detail stays at its position, only its
            // album changes. No positions shift.
            replace_row_store_item(
                &self.row_store,
                detail_position_for_tile_row(selected_tile_row),
                detail_row,
            );
            self.row_model_generation
                .set(self.row_model_generation.get().wrapping_add(1));
            self.measured_detail_height.set(0);
            self.update_visible_rows();
            return;
        }

        if let Some(previous_tile_row) = previous_detail_tile_row {
            let removed_position = detail_position_for_tile_row(previous_tile_row);
            remove_row_store_item(&self.row_store, removed_position);
            self.shift_realized_rows_for_removal(removed_position);
        }
        let inserted_position = detail_position_for_tile_row(selected_tile_row);
        insert_row_store_item(&self.row_store, inserted_position, detail_row);
        self.shift_realized_rows_for_insertion(inserted_position);
        self.row_model_generation
            .set(self.row_model_generation.get().wrapping_add(1));
        self.measured_detail_height.set(0);
        self.update_visible_rows();
    }

    /// Reindex `realized_rows` after the row store removed the row at
    /// `removed_position`: that row's shell (if realized) is released and
    /// every realized position after it shifts down by one, so each
    /// remaining shell stays keyed to the store row it renders.
    fn shift_realized_rows_for_removal(&self, removed_position: usize) {
        self.release_row(removed_position);
        shift_positions_after_removal(&mut self.realized_rows.borrow_mut(), removed_position);
    }

    /// Reindex `realized_rows` after the row store inserted a row at
    /// `inserted_position`: every realized position at or after it shifts
    /// up by one. The inserted row itself is realized by the next
    /// `update_visible_rows` pass if it is in the viewport.
    fn shift_realized_rows_for_insertion(&self, inserted_position: usize) {
        shift_positions_after_insertion(&mut self.realized_rows.borrow_mut(), inserted_position);
    }

    fn refresh_album_widgets(&self, album_key: &AlbumKey, refresh: AlbumRowRefresh<'_>) {
        if let Some(row_position) = self.row_position_for_album(album_key) {
            self.refresh_row_widget(row_position, refresh);
        }
        let is_selected = self
            .selected_album
            .borrow()
            .as_ref()
            .is_some_and(|selected| selected == album_key);
        if is_selected && let Some(row_position) = self.selected_detail_position() {
            self.refresh_row_widget(row_position, refresh);
        }
    }

    fn render_row_position(
        &self,
        row_shell: &gtk::Box,
        row_position: usize,
        refresh: AlbumRowRefresh<'_>,
    ) {
        let row_id = widget_id(row_shell);
        let mut row_widgets = self.row_widgets.borrow_mut();
        let Some(row) = row_widgets.get_mut(&row_id) else {
            return;
        };
        let Some(list_row) = album_list_row_at(&self.row_store, row_position) else {
            return;
        };
        let model_generation = self.row_model_generation.get();
        if matches!(refresh, AlbumRowRefresh::Rebind)
            && row.bound_generation == model_generation
            && row.bound_row.as_ref() == Some(&list_row)
        {
            return;
        }
        row.bound_generation = model_generation;
        row.bound_row = Some(list_row.clone());
        match list_row {
            AlbumListRow::Tiles { tile_row_index } => {
                self.render_tile_row_position(row, tile_row_index, refresh);
            }
            AlbumListRow::Detail { album_key } => {
                self.render_detail_row(row, &album_key, refresh);
            }
        }
    }

    fn render_tile_row_position(
        &self,
        row: &mut AlbumRowWidgets,
        tile_row_index: usize,
        refresh: AlbumRowRefresh<'_>,
    ) {
        let columns = self.visible_columns.get().max(1);
        let Some(start) = tile_row_index.checked_mul(columns) else {
            return;
        };
        let albums = self.albums.borrow();
        if albums.is_empty() {
            if tile_row_index == 0 {
                row.show_empty_state();
            }
            return;
        }
        if start >= albums.len() {
            return;
        }
        let end = (start + columns).min(albums.len());
        self.render_tile_row_shell(row, &albums[start..end], columns, refresh);
    }

    fn render_tile_row_shell(
        &self,
        row: &mut AlbumRowWidgets,
        albums: &[AlbumViewModel],
        columns: usize,
        refresh: AlbumRowRefresh<'_>,
    ) {
        row.show_tiles();
        row.set_detail(None, None);
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
            let refresh_artwork = refresh.refreshes_artwork_for(&album.key);
            slot.bind(self, album, is_selected, refresh_artwork);
        }
    }

    fn render_detail_row(
        &self,
        row: &mut AlbumRowWidgets,
        album_key: &AlbumKey,
        refresh: AlbumRowRefresh<'_>,
    ) {
        let selected = self
            .selected_album
            .borrow()
            .as_ref()
            .is_some_and(|selected| selected == album_key);
        if !selected {
            row.show_empty_state();
            return;
        }

        let columns = self.visible_columns.get().max(1);
        let Some((selected_column, album)) = self.detail_album(album_key, columns) else {
            row.show_empty_state();
            return;
        };
        row.show_detail();

        let detail_key = AlbumDetailKey {
            album_key: album.key.clone(),
            selected_column,
            columns,
            playing_track_id: self.playing_track_id.get(),
        };
        let force_rebuild = refresh.refreshes_detail_for(&album.key);
        if force_rebuild || row.detail_key.as_ref() != Some(&detail_key) {
            let detail = self.album_detail(&album, selected_column, columns);
            detail.set_margin_top(ALBUM_DETAIL_TOP_GAP);
            row.set_detail(Some(detail), Some(detail_key));
        }
    }

    fn detail_album(
        &self,
        album_key: &AlbumKey,
        columns: usize,
    ) -> Option<(usize, AlbumViewModel)> {
        self.albums
            .borrow()
            .iter()
            .enumerate()
            .find(|(_, album)| &album.key == album_key)
            .map(|(album_index, album)| (album_index % columns, album.clone()))
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
        if let Some(source) = album.artwork_source.clone() {
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
}

struct AlbumRowWidgets {
    shell: gtk::Box,
    tile_row: gtk::Box,
    empty_state: gtk::Label,
    slots: Vec<AlbumTileSlot>,
    detail: Option<gtk::Overlay>,
    detail_key: Option<AlbumDetailKey>,
    bound_row: Option<AlbumListRow>,
    bound_generation: u64,
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
            detail_key: None,
            bound_row: None,
            bound_generation: 0,
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
        self.set_detail(None, None);
    }

    fn show_tiles(&self) {
        self.empty_state.set_visible(false);
        self.tile_row.set_visible(true);
    }

    fn show_detail(&self) {
        self.empty_state.set_visible(false);
        self.tile_row.set_visible(false);
    }

    fn set_detail(&mut self, detail: Option<gtk::Overlay>, detail_key: Option<AlbumDetailKey>) {
        if let Some(previous) = self.detail.take() {
            self.shell.remove(&previous);
        }
        if let Some(detail) = detail {
            self.shell.append(&detail);
            self.detail = Some(detail);
        }
        self.detail_key = detail_key;
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
            album.artwork_source.clone(),
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

#[cfg(test)]
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

fn album_list_rows(
    albums: &[AlbumViewModel],
    columns: usize,
    selected_album: Option<&AlbumKey>,
) -> Vec<AlbumListRow> {
    let columns = columns.max(1);
    let tile_row_count = album_row_count(albums.len(), columns);
    let selected_tile_row =
        selected_album.and_then(|album_key| album_tile_row_index_in(albums, columns, album_key));
    let mut rows = Vec::with_capacity(tile_row_count + usize::from(selected_tile_row.is_some()));

    for tile_row_index in 0..tile_row_count {
        rows.push(AlbumListRow::Tiles { tile_row_index });
        if selected_tile_row == Some(tile_row_index)
            && let Some(album_key) = selected_album
        {
            rows.push(AlbumListRow::Detail {
                album_key: album_key.clone(),
            });
        }
    }
    rows
}

fn album_tile_row_index_in(
    albums: &[AlbumViewModel],
    columns: usize,
    album_key: &AlbumKey,
) -> Option<usize> {
    albums
        .iter()
        .position(|album| &album.key == album_key)
        .map(|album_index| album_index / columns.max(1))
}

fn tile_list_position_for_tile_row(
    tile_row_index: usize,
    detail_tile_row_index: Option<usize>,
) -> usize {
    match detail_tile_row_index {
        Some(detail_tile_row_index) if tile_row_index > detail_tile_row_index => tile_row_index + 1,
        _ => tile_row_index,
    }
}

fn detail_position_for_tile_row(tile_row_index: usize) -> usize {
    tile_row_index + 1
}

fn replace_row_store(store: &gio::ListStore, rows: Vec<AlbumListRow>) {
    let additions: Vec<_> = rows.into_iter().map(glib::BoxedAnyObject::new).collect();
    store.splice(0, store.n_items(), &additions);
}

fn insert_row_store_item(store: &gio::ListStore, position: usize, row: AlbumListRow) {
    let additions = [glib::BoxedAnyObject::new(row)];
    store.splice(position as u32, 0, &additions);
}

fn replace_row_store_item(store: &gio::ListStore, position: usize, row: AlbumListRow) {
    if position >= store.n_items() as usize {
        insert_row_store_item(store, position, row);
        return;
    }
    let additions = [glib::BoxedAnyObject::new(row)];
    store.splice(position as u32, 1, &additions);
}

fn remove_row_store_item(store: &gio::ListStore, position: usize) {
    if position < store.n_items() as usize {
        store.splice(position as u32, 1, &[] as &[glib::BoxedAnyObject]);
    }
}

/// Shift a position-keyed map's keys down by one after the underlying
/// list removed the row at `removed_position`. The caller is expected to
/// have dropped the entry for the removed position itself.
fn shift_positions_after_removal<V>(
    positions: &mut std::collections::HashMap<usize, V>,
    removed_position: usize,
) {
    let entries: Vec<(usize, V)> = positions.drain().collect();
    positions.extend(entries.into_iter().map(|(position, value)| {
        if position > removed_position {
            (position - 1, value)
        } else {
            (position, value)
        }
    }));
}

/// Shift a position-keyed map's keys up by one for every entry at or
/// after `inserted_position`, after the underlying list inserted a row
/// there.
fn shift_positions_after_insertion<V>(
    positions: &mut std::collections::HashMap<usize, V>,
    inserted_position: usize,
) {
    let entries: Vec<(usize, V)> = positions.drain().collect();
    positions.extend(entries.into_iter().map(|(position, value)| {
        if position >= inserted_position {
            (position + 1, value)
        } else {
            (position, value)
        }
    }));
}

fn album_list_row_at(store: &gio::ListStore, position: usize) -> Option<AlbumListRow> {
    let row_object = store
        .item(position as u32)?
        .downcast::<glib::BoxedAnyObject>()
        .ok()?;
    let row = row_object.try_borrow::<AlbumListRow>().ok()?;
    Some(row.clone())
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
        ALBUM_TILE_COVER_SIZE_EXPANDED, ALBUM_TILE_MIN_WIDTH, ALBUM_TILE_WIDTH, AlbumKey,
        AlbumListRow, AlbumViewModel, album_list_row_at, album_list_rows, album_row_count,
        align_widget_to_viewport_top, apply_cover_texture, build_album_tile_content,
        build_cover_widget, columns_for_width, detail_position_for_tile_row, insert_row_store_item,
        remove_row_store_item, replace_row_store, replace_row_store_item,
        shift_positions_after_insertion, shift_positions_after_removal,
        tile_list_position_for_tile_row,
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
    fn album_rows_insert_detail_after_selected_tile_row() {
        let albums = albums_for_test(7);
        let selected = albums[4].key.clone();

        assert_eq!(
            album_list_rows(&albums, 3, Some(&selected)),
            vec![
                AlbumListRow::Tiles { tile_row_index: 0 },
                AlbumListRow::Tiles { tile_row_index: 1 },
                AlbumListRow::Detail {
                    album_key: selected,
                },
                AlbumListRow::Tiles { tile_row_index: 2 },
            ]
        );
    }

    #[test]
    fn album_rows_omit_detail_when_selection_is_not_visible() {
        let albums = albums_for_test(4);
        let selected = album_key_for_test(99);

        assert_eq!(
            album_list_rows(&albums, 2, Some(&selected)),
            vec![
                AlbumListRow::Tiles { tile_row_index: 0 },
                AlbumListRow::Tiles { tile_row_index: 1 },
            ]
        );
    }

    #[test]
    fn tile_positions_account_for_inserted_detail_row() {
        assert_eq!(tile_list_position_for_tile_row(0, None), 0);
        assert_eq!(tile_list_position_for_tile_row(1, None), 1);
        assert_eq!(tile_list_position_for_tile_row(0, Some(1)), 0);
        assert_eq!(tile_list_position_for_tile_row(1, Some(1)), 1);
        assert_eq!(tile_list_position_for_tile_row(2, Some(1)), 3);
        assert_eq!(detail_position_for_tile_row(1), 2);
    }

    #[test]
    fn row_store_splices_detail_rows() {
        let store = gtk::gio::ListStore::new::<gtk::glib::BoxedAnyObject>();
        let detail_key = album_key_for_test(1);
        let replacement_key = album_key_for_test(2);

        replace_row_store(
            &store,
            vec![
                AlbumListRow::Tiles { tile_row_index: 0 },
                AlbumListRow::Tiles { tile_row_index: 1 },
            ],
        );
        insert_row_store_item(
            &store,
            1,
            AlbumListRow::Detail {
                album_key: detail_key.clone(),
            },
        );
        assert_eq!(store.n_items(), 3);
        assert_eq!(
            album_list_row_at(&store, 1),
            Some(AlbumListRow::Detail {
                album_key: detail_key,
            })
        );

        replace_row_store_item(
            &store,
            1,
            AlbumListRow::Detail {
                album_key: replacement_key.clone(),
            },
        );
        assert_eq!(
            album_list_row_at(&store, 1),
            Some(AlbumListRow::Detail {
                album_key: replacement_key,
            })
        );

        remove_row_store_item(&store, 1);
        assert_eq!(store.n_items(), 2);
        assert_eq!(
            album_list_row_at(&store, 1),
            Some(AlbumListRow::Tiles { tile_row_index: 1 })
        );
    }

    #[test]
    fn position_maps_follow_row_removal_and_insertion() {
        // The realized-row map must track a detail-row move the same way
        // the row store does, so a selection change re-renders shells in
        // place instead of releasing every realized row.
        let mut positions: HashMap<usize, &str> =
            [(0, "tiles-0"), (2, "tiles-1"), (3, "tiles-2")].into();

        // Detail removed at position 1 (not realized): later rows close up.
        shift_positions_after_removal(&mut positions, 1);
        assert_eq!(
            positions,
            [(0, "tiles-0"), (1, "tiles-1"), (2, "tiles-2")].into()
        );

        // Detail inserted at position 1: rows at or after it move down.
        shift_positions_after_insertion(&mut positions, 1);
        assert_eq!(
            positions,
            [(0, "tiles-0"), (2, "tiles-1"), (3, "tiles-2")].into()
        );
    }

    #[test]
    fn realized_row_can_be_aligned_to_viewport_top() {
        let ran = crate::test_support::with_gtk(|| {
            let store = gtk::gio::ListStore::new::<gtk::glib::BoxedAnyObject>();
            replace_row_store(
                &store,
                (0..40)
                    .map(|tile_row_index| AlbumListRow::Tiles { tile_row_index })
                    .collect(),
            );
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

    fn albums_for_test(count: usize) -> Vec<AlbumViewModel> {
        (0..count)
            .map(|index| {
                let key = album_key_for_test(index);
                AlbumViewModel {
                    title: format!("Album {index}"),
                    artist: format!("Artist {index}"),
                    key,
                    year: None,
                    tracks: Vec::new(),
                    representative_track_path: None,
                    artwork_source: None,
                }
            })
            .collect()
    }

    fn album_key_for_test(index: usize) -> AlbumKey {
        AlbumKey::for_test(format!("Artist {index}"), format!("Album {index}"))
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
