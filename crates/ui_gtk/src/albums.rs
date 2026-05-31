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
    /// Explicit scroll requested by "Show Album" actions. It is consumed from
    /// the width watcher after the Albums page has a visible allocation, so
    /// row-position math uses the final column count.
    pending_scroll_album: Rc<RefCell<Option<AlbumKey>>>,
    visible_columns: Rc<Cell<usize>>,
    last_width: Rc<Cell<i32>>,
    artwork_loader: ArtworkLoader,
    /// Monotonic generation bumped at the start of every full grid
    /// rebuild. Each artwork request snapshot-captures the current value
    /// and the callback drops itself when the view has moved on, so
    /// late-arriving decodes can't touch tiles that have been torn
    /// down. Staleness lives here (per view) instead of in the shared
    /// loader, so an Albums rebuild can't invalidate a concurrent
    /// now-playing request and vice versa.
    artwork_generation: Rc<Cell<u64>>,
    /// Monotonic generation bumped every time the album-detail panel is
    /// (re)built. The detail's artwork request snapshot-captures it and
    /// the callback drops itself when a newer selection has superseded
    /// the panel — critical because the palette provider is installed
    /// display-wide, so a stale install would mis-tint the current
    /// detail. Separate from `artwork_generation` (grid rebuilds) because
    /// a plain selection change rebuilds the detail without rebuilding
    /// the grid.
    detail_generation: Rc<Cell<u64>>,
    /// Switches from `false` to `true` the first time `activate()` is
    /// called. Tile construction, grouping, and the width-watcher tick
    /// callback all key off this — they are skipped while the view is
    /// dormant so the cost stays out of startup.
    activated: Rc<Cell<bool>>,
    playing_track_id: Rc<Cell<Option<TrackId>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AlbumRowViewModel {
    albums: Vec<AlbumViewModel>,
    columns: usize,
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
            all_albums: Rc::new(RefCell::new(Vec::new())),
            albums: Rc::new(RefCell::new(Vec::new())),
            search_text: Rc::new(RefCell::new(String::new())),
            selected_album: Rc::new(RefCell::new(None)),
            realized_rows: Rc::new(RefCell::new(HashMap::new())),
            pending_scroll_album: Rc::new(RefCell::new(None)),
            visible_columns: Rc::new(Cell::new(1)),
            last_width: Rc::new(Cell::new(0)),
            artwork_loader,
            artwork_generation: Rc::new(Cell::new(0)),
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
            self.refresh_row_widget(row_position);
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
            self.refresh_row_widget(row_position);
        }
        if let Some(row_position) = selected_row
            && selected_row != previous_row
        {
            self.refresh_row_widget(row_position);
        }
    }

    fn refresh_selected_row(&self) {
        let selected_row = self
            .selected_album
            .borrow()
            .as_ref()
            .and_then(|album_key| self.row_position_for_album(album_key));
        if let Some(row_position) = selected_row {
            self.refresh_row_widget(row_position);
        }
    }

    fn request_scroll_to_album(&self, album_key: AlbumKey) {
        self.pending_scroll_album.borrow_mut().replace(album_key);
    }

    fn scroll_pending_album_if_ready(&self) {
        if self.scroller.width() <= 0 || self.row_store.n_items() == 0 {
            return;
        }
        let Some(album_key) = self.pending_scroll_album.borrow().clone() else {
            return;
        };
        let Some(row_position) = self.row_position_for_album(&album_key) else {
            self.pending_scroll_album.borrow_mut().take();
            return;
        };
        let scroll_info = gtk::ScrollInfo::new();
        scroll_info.set_enable_horizontal(false);
        scroll_info.set_enable_vertical(true);
        self.list_view.scroll_to(
            row_position as u32,
            gtk::ListScrollFlags::FOCUS,
            Some(scroll_info),
        );
        self.pending_scroll_album.borrow_mut().take();
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
        self.artwork_generation
            .set(self.artwork_generation.get().wrapping_add(1));
        self.realized_rows.borrow_mut().clear();
        let old_len = self.row_store.n_items();

        let columns = self.visible_columns.get().max(1);
        let albums = self.albums.borrow();
        let mut rows = Vec::new();
        if albums.is_empty() {
            rows.push(glib::BoxedAnyObject::new(AlbumRowViewModel {
                albums: Vec::new(),
                columns,
            }));
        } else {
            let mut album_index = 0;
            while album_index < albums.len() {
                let row_start = album_index;
                let row_end = (row_start + columns).min(albums.len());
                rows.push(glib::BoxedAnyObject::new(AlbumRowViewModel {
                    albums: albums[row_start..row_end].to_vec(),
                    columns,
                }));
                album_index = row_end;
            }
        }

        self.row_store.splice(0, old_len, &rows);
    }

    fn refresh_row_widget(&self, row_position: usize) {
        let Some(row) = self.row_model(row_position) else {
            return;
        };
        let Some(row_shell) = self.realized_rows.borrow().get(&row_position).cloned() else {
            return;
        };
        self.render_row_shell(&row_shell, &row);
    }

    fn row_model(&self, row_position: usize) -> Option<AlbumRowViewModel> {
        let columns = self.visible_columns.get().max(1);
        let start = row_position.checked_mul(columns)?;
        let albums = self.albums.borrow();
        if start >= albums.len() {
            return None;
        }
        let end = (start + columns).min(albums.len());
        Some(AlbumRowViewModel {
            albums: albums[start..end].to_vec(),
            columns,
        })
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
        factory.connect_setup(move |_factory, item| {
            let Some(list_item) = item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            let row = gtk::Box::new(gtk::Orientation::Vertical, ALBUM_GRID_ROW_SPACING);
            row.add_css_class("album-row");
            row.set_hexpand(true);
            list_item.set_child(Some(&row));
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
            clear_container(&row_shell);

            let Some(row_object) = list_item
                .item()
                .and_then(|item| item.downcast::<glib::BoxedAnyObject>().ok())
            else {
                return;
            };
            let Ok(row) = row_object.try_borrow::<AlbumRowViewModel>() else {
                return;
            };
            if list_item.position() != gtk::INVALID_LIST_POSITION {
                let mut realized_rows = view_for_bind.realized_rows.borrow_mut();
                realized_rows.retain(|_, shell| shell != &row_shell);
                realized_rows.insert(list_item.position() as usize, row_shell.clone());
            }
            view_for_bind.render_row_shell(&row_shell, &row);
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
                clear_container(&row_shell);
            }
        });

        factory
    }

    fn render_row_shell(&self, row_shell: &gtk::Box, row: &AlbumRowViewModel) {
        clear_container(row_shell);
        if row.albums.is_empty() {
            row_shell.append(&empty_albums_label());
            return;
        }

        let tile_row = self.build_tile_row(row);
        row_shell.append(&tile_row);

        if let Some((selected_column, selected_album)) =
            selected_album_in_row(row, self.selected_album.borrow().as_ref())
        {
            let detail = self.album_detail(selected_album, selected_column, row.columns);
            row_shell.append(&detail);
        }
    }

    fn build_tile_row(&self, row_model: &AlbumRowViewModel) -> gtk::Box {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, ALBUM_GRID_COLUMN_SPACING);
        row.set_homogeneous(true);
        row.set_margin_start(ALBUM_GRID_MARGIN);
        row.set_margin_end(ALBUM_GRID_MARGIN);
        let selected_album = self.selected_album.borrow().clone();

        for offset in 0..row_model.columns {
            if let Some(album) = row_model.albums.get(offset) {
                let is_selected = selected_album
                    .as_ref()
                    .is_some_and(|selected| selected == &album.key);
                let tile = self.album_tile(album, is_selected);
                row.append(&tile);
            } else {
                // Empty placeholder keeps later rows aligned with full-width rows.
                row.append(&empty_tile_placeholder());
            }
        }

        row
    }

    fn album_tile(&self, album: &AlbumViewModel, is_selected: bool) -> gtk::Button {
        let cover_size = if is_selected {
            ALBUM_TILE_COVER_SIZE_EXPANDED
        } else {
            ALBUM_TILE_COVER_SIZE
        };

        // Per-label margins instead of a uniform box spacing so the
        // title→artist gap can be tighter than the cover→title gap.
        let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        content.set_width_request(cover_size);
        content.set_halign(gtk::Align::Center);
        content.set_overflow(gtk::Overflow::Hidden);

        // The cover starts as a placeholder. Artwork loading runs on a
        // background thread; when the result arrives the loader fires
        // the callback below and swaps the placeholder for the decoded
        // image. If the album's representative track can't be resolved
        // (no library root yet, or every track missing), the placeholder
        // stays — which is what the synchronous path used to show too.
        let cover = build_cover_widget(cover_size, "album-cover");
        cover.set_halign(gtk::Align::Start);
        content.append(&cover);
        if let Some(source) = self.album_artwork_source(album) {
            let cover_for_callback = cover.clone();
            let generation_snapshot = self.artwork_generation.get();
            let generation_cell = self.artwork_generation.clone();
            self.artwork_loader.request(
                source,
                Box::new(move |decoded| {
                    if generation_cell.get() != generation_snapshot {
                        return;
                    }
                    apply_cover_texture(&cover_for_callback, decoded.tile_texture, cover_size);
                }),
            );
        }

        let title = gtk::Label::new(Some(&album.title));
        title.add_css_class("album-tile-title");
        title.set_size_request(cover_size, -1);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title.set_xalign(0.0);
        title.set_halign(gtk::Align::Start);
        title.set_margin_top(6);
        content.append(&title);

        let artist = gtk::Label::new(Some(&album.artist));
        artist.add_css_class("album-tile-artist");
        artist.set_size_request(cover_size, -1);
        artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
        artist.set_xalign(0.0);
        artist.set_halign(gtk::Align::Start);
        artist.set_margin_top(1);
        content.append(&artist);

        let button = gtk::Button::new();
        button.add_css_class("album-tile");
        if is_selected {
            button.add_css_class("selected");
        }
        button.set_child(Some(&content));
        button.set_can_shrink(true);
        button.set_width_request(ALBUM_TILE_WIDTH);
        button.set_halign(gtk::Align::Fill);
        button.set_valign(gtk::Align::Start);
        button.set_overflow(gtk::Overflow::Hidden);

        let album_key = album.key.clone();
        let view = self.clone();
        button.connect_clicked(move |_| {
            view.select_album(album_key.clone());
        });

        button
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
            self.artwork_loader.request(
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

        let left = AlbumTrackListView::new(
            &album.tracks[..split_index],
            self.context_menu.clone(),
            self.command_controller.clone(),
            self.playback_changed.clone(),
            playing_track_id,
        );
        let right = AlbumTrackListView::new(
            &album.tracks[split_index..],
            self.context_menu.clone(),
            self.command_controller.clone(),
            self.playback_changed.clone(),
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

fn clear_container(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

fn columns_for_width(width: i32) -> usize {
    let usable_width = width
        .saturating_sub(ALBUM_GRID_MARGIN * 2)
        .max(ALBUM_TILE_MIN_WIDTH);
    ((usable_width + ALBUM_GRID_COLUMN_SPACING)
        / (ALBUM_TILE_MIN_WIDTH + ALBUM_GRID_COLUMN_SPACING))
        .max(1) as usize
}

fn selected_album_in_row<'a>(
    row: &'a AlbumRowViewModel,
    selected_album: Option<&AlbumKey>,
) -> Option<(usize, &'a AlbumViewModel)> {
    let selected_album = selected_album?;
    row.albums
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

fn empty_tile_placeholder() -> gtk::Box {
    let placeholder = gtk::Box::new(gtk::Orientation::Vertical, 0);
    placeholder.set_width_request(ALBUM_TILE_MIN_WIDTH);
    placeholder.set_hexpand(true);
    placeholder
}

fn build_cover_widget(size: i32, css_class: &str) -> gtk::Box {
    let cover = gtk::Box::new(gtk::Orientation::Vertical, 0);
    cover.add_css_class(css_class);
    cover.set_size_request(size, size);
    cover.set_halign(gtk::Align::Center);
    cover.set_valign(gtk::Align::Start);
    cover.set_hexpand(false);
    cover.set_vexpand(false);
    cover.set_overflow(gtk::Overflow::Hidden);
    apply_cover_texture(&cover, None, size);
    cover
}

/// Replaces the cover widget's current contents with either the decoded
/// image or the placeholder icon. Used both at construction time (called
/// with `None` to install the placeholder) and from the artwork loader
/// callback (called with the decoded texture once it arrives).
fn apply_cover_texture(cover: &gtk::Box, texture: Option<gdk::Texture>, size: i32) {
    while let Some(child) = cover.first_child() {
        cover.remove(&child);
    }

    match texture {
        Some(texture) => {
            cover.add_css_class("has-artwork");
            let picture = gtk::Picture::for_paintable(&texture);
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_size_request(size, size);
            picture.set_halign(gtk::Align::Fill);
            picture.set_valign(gtk::Align::Fill);
            picture.set_hexpand(false);
            picture.set_vexpand(false);
            cover.append(&picture);
        }
        None => {
            cover.remove_css_class("has-artwork");
            if let Some(icon) = album_cover_placeholder(size) {
                cover.append(&icon);
            }
        }
    }
}

/// Build a cover widget with an immediately-applied texture. Used by
/// the album detail panel, which resolves artwork synchronously via
/// the loader's cache (or a one-off sync read) and has the texture in
/// hand at construction time.
fn album_cover_with(texture: Option<gdk::Texture>, size: i32, css_class: &str) -> gtk::Box {
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

fn ensure_shuffle_disabled(command_controller: &SharedCommandController) {
    set_shuffle_mode(command_controller, ShuffleMode::Off);
}

fn set_shuffle_mode(command_controller: &SharedCommandController, mode: ShuffleMode) {
    let _result = command_controller.dispatch(ApplicationCommand::Playback(
        PlaybackCommand::SetShuffleMode(mode),
    ));
}

#[cfg(test)]
mod tests {
    use super::{
        ALBUM_GRID_COLUMN_SPACING, ALBUM_GRID_MARGIN, ALBUM_TILE_MIN_WIDTH, columns_for_width,
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
}
