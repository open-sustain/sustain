// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use gtk::glib;
use gtk::prelude::*;
use sustain_app_runtime::{
    ApplicationRuntimeError, DuplicateGroup, DuplicateMatchMode, NotificationCategory,
    NotificationSeverity, runtime_error_text,
};
use sustain_i18n::{gettext, npgettext, phrase_list, plural_count, tr_format};

use crate::{
    SharedRuntime,
    command_controller::SharedCommandController,
    main_window::VisibleSummaryRefreshCallback,
    track_context::TrackRowContextMenu,
    track_summary::TrackSummary,
    track_table::{
        BeginEditCallback, RatingChangedCallback, RowSetUpdate, TrackActivatedCallback, TrackTable,
        TrackTableRow, build_track_table,
    },
    verified_duplicates::open_verified_duplicates_preview,
};

#[derive(Clone)]
pub(crate) struct DuplicatesView {
    root: gtk::Box,
    table: Rc<RefCell<Option<TrackTable>>>,
    context_menu: TrackRowContextMenu,
    track_activated: TrackActivatedCallback,
    rating_changed: RatingChangedCallback,
    inline_edit: BeginEditCallback,
    runtime: SharedRuntime,
    command_controller: SharedCommandController,
    strict: gtk::Switch,
    status: gtk::Label,
    merge_verified: gtk::Button,
    /// The groups the rows show, for the verified-merge preview.
    groups: Rc<RefCell<Vec<DuplicateGroup>>>,
    /// Advanced whenever the result of a running scan stops answering
    /// what the view asks for: the view was left, or the user asked for a
    /// fresh start (another match mode, the view opened again).
    generation: Rc<Cell<u64>>,
    /// Whether the next scan result must replace the rows rather than
    /// reconcile them: a scan that asked for a fresh start was superseded
    /// before its result arrived.
    replace_pending: Rc<Cell<bool>>,
    /// At most one scan runs at a time; each reads the whole library.
    scan_running: Rc<Cell<bool>>,
    /// Whether the library changed under the running scan, which must
    /// then be followed by another one.
    rescan_requested: Rc<Cell<bool>>,
    active: Rc<Cell<bool>>,
    on_summary_refresh: Rc<RefCell<Option<VisibleSummaryRefreshCallback>>>,
}

impl DuplicatesView {
    pub(crate) fn new(
        runtime: SharedRuntime,
        command_controller: SharedCommandController,
        context_menu: TrackRowContextMenu,
        track_activated: TrackActivatedCallback,
        rating_changed: RatingChangedCallback,
        inline_edit: BeginEditCallback,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        header.set_margin_top(8);
        header.set_margin_end(10);
        header.set_margin_bottom(8);
        header.set_margin_start(10);
        let status = gtk::Label::new(Some(&gettext("Open Duplicates to scan the library.")));
        status.set_xalign(0.0);
        status.set_hexpand(true);
        let merge_verified = gtk::Button::with_label(&gettext("Merge Verified Duplicates\u{2026}"));
        merge_verified.set_sensitive(false);
        merge_verified.set_tooltip_text(Some(&gettext(
            "Preview and merge the groups whose audio shows them to be copies of one recording",
        )));
        let strict_label = gtk::Label::new(Some(&gettext("Strict matching")));
        let strict = gtk::Switch::new();
        strict.set_valign(gtk::Align::Center);
        header.append(&status);
        header.append(&merge_verified);
        header.append(&strict_label);
        header.append(&strict);
        root.append(&header);

        let view = Self {
            root,
            table: Rc::new(RefCell::new(None)),
            context_menu,
            track_activated,
            rating_changed,
            inline_edit,
            runtime,
            command_controller,
            strict,
            status,
            merge_verified,
            groups: Rc::new(RefCell::new(Vec::new())),
            generation: Rc::new(Cell::new(0)),
            replace_pending: Rc::new(Cell::new(false)),
            scan_running: Rc::new(Cell::new(false)),
            rescan_requested: Rc::new(Cell::new(false)),
            active: Rc::new(Cell::new(false)),
            on_summary_refresh: Rc::new(RefCell::new(None)),
        };
        let view_for_toggle = view.clone();
        view.strict.connect_active_notify(move |_| {
            if view_for_toggle.active.get() {
                view_for_toggle.scan(RowSetUpdate::Replace);
            }
        });
        let view_for_merge = view.clone();
        view.merge_verified
            .connect_clicked(move |_| view_for_merge.preview_verified_merges());
        view
    }

    pub(crate) fn widget(&self) -> gtk::Box {
        self.root.clone()
    }

    pub(crate) fn set_active(&self, active: bool) {
        let was_active = self.active.replace(active);
        if active && !was_active {
            self.scan(RowSetUpdate::Replace);
        } else if !active && was_active {
            self.generation.set(self.generation.get().wrapping_add(1));
        }
    }

    /// Rescan after the library changed under the view — a merge, a
    /// removal, an edit — keeping the user's place in the groups.
    pub(crate) fn refresh_if_active(&self) {
        if self.active.get() {
            self.scan(RowSetUpdate::Reconcile);
        }
    }

    /// Install the callback that recomputes the status-bar summary. The
    /// view fires it whenever a scan changes its rows.
    pub(crate) fn set_summary_refresh(&self, refresh: VisibleSummaryRefreshCallback) {
        self.on_summary_refresh.replace(Some(refresh));
    }

    /// The tracks the view shows: the members of every duplicate group.
    pub(crate) fn summary(&self) -> TrackSummary {
        self.table
            .borrow()
            .as_ref()
            .map(TrackTable::summary)
            .unwrap_or_default()
    }

    pub(crate) fn selected_track_ids(&self) -> Vec<sustain_app_runtime::TrackId> {
        self.table
            .borrow()
            .as_ref()
            .map(TrackTable::selected_track_ids)
            .unwrap_or_default()
    }

    pub(crate) fn ordered_track_ids(&self) -> Vec<sustain_app_runtime::TrackId> {
        self.table
            .borrow()
            .as_ref()
            .map(TrackTable::ordered_track_ids)
            .unwrap_or_default()
    }

    pub(crate) fn select_all(&self) {
        if let Some(table) = self.table.borrow().as_ref() {
            table.select_all();
        }
    }

    /// Repaint the rows of these tracks in place, in one table pass.
    pub(crate) fn update_rows(&self, rows: &HashMap<sustain_app_runtime::TrackId, TrackTableRow>) {
        if let Some(table) = self.table.borrow().as_ref() {
            table.update_rows(rows);
        }
    }

    /// See [`TrackTable::refresh_bound_cells`].
    pub(crate) fn refresh_bound_cells(&self) {
        let table = self.table.borrow().clone();
        if let Some(table) = table {
            table.refresh_bound_cells();
        }
    }

    /// See [`TrackTable::flush_pending_edits`].
    pub(crate) fn flush_pending_edits(&self) {
        let table = self.table.borrow().clone();
        if let Some(table) = table {
            table.flush_pending_edits();
        }
    }

    fn ensure_table(&self) -> TrackTable {
        if let Some(table) = self.table.borrow().as_ref() {
            return table.clone();
        }

        let table = build_track_table(
            Vec::new(),
            Some(self.track_activated.clone()),
            Some(self.context_menu.clone()),
            Some(self.rating_changed.clone()),
            None,
            Some(self.inline_edit.clone()),
        );
        table.disable_sorting_and_column_reordering();
        self.root.append(&table.widget());
        self.table.borrow_mut().replace(table.clone());
        table
    }

    /// Scan the library for duplicate groups, or, while a scan runs,
    /// follow it with another. The running scan's result still shows
    /// unless it no longer answers what the view asks for: it is newer
    /// than the rows on screen, and a library that keeps changing (an
    /// analysis sweep) would otherwise keep every result from showing.
    fn scan(&self, update: RowSetUpdate) {
        self.ensure_table();
        if update == RowSetUpdate::Replace {
            self.replace_pending.set(true);
            self.generation.set(self.generation.get().wrapping_add(1));
        }
        if self.scan_running.get() {
            self.rescan_requested.set(true);
            return;
        }
        let generation = self.generation.get();
        self.status
            .set_text(&gettext("Scanning for duplicate tracks..."));
        self.merge_verified.set_sensitive(false);
        let mode = if self.strict.is_active() {
            DuplicateMatchMode::Strict
        } else {
            DuplicateMatchMode::Loose
        };
        // Release the runtime before the error arm borrows it mutably.
        let task = self.runtime.borrow().duplicate_groups_task(mode);
        let task = match task {
            Ok(task) => task,
            // Opened while the library loads: the views refresh that ends
            // the load scans again.
            Err(ApplicationRuntimeError::LibraryHydrationPending) => return,
            Err(error) => {
                self.status
                    .set_text(&gettext("Duplicate scan unavailable."));
                self.runtime.borrow_mut().push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    NotificationSeverity::Error,
                    runtime_error_text(&error),
                );
                return;
            }
        };

        let (sender, receiver) = async_channel::bounded(1);
        if std::thread::Builder::new()
            .name("sustain-duplicate-groups".to_owned())
            .spawn(move || {
                let _ = sender.send_blocking(task.run());
            })
            .is_err()
        {
            self.status
                .set_text(&gettext("Duplicate scan could not be started."));
            self.runtime.borrow_mut().push_ephemeral_notification(
                NotificationCategory::DuplicateConsolidation,
                NotificationSeverity::Error,
                gettext("Duplicate scan could not be started."),
            );
            return;
        }
        self.scan_running.set(true);

        let view = self.clone();
        glib::MainContext::default().spawn_local(async move {
            // A worker that ends without delivering groups has failed the
            // scan.
            let result = receiver
                .recv()
                .await
                .unwrap_or(Err(ApplicationRuntimeError::LibraryStoreFailed));
            view.scan_running.set(false);
            let rescan = view.rescan_requested.replace(false);
            if !view.active.get() {
                return;
            }
            if view.generation.get() == generation {
                view.apply_groups(result);
            }
            if rescan {
                view.scan(RowSetUpdate::Reconcile);
            }
        });
    }

    fn preview_verified_merges(&self) {
        let merges = self
            .runtime
            .borrow()
            .verified_duplicate_merges(&self.groups.borrow());
        if merges.is_empty() {
            return;
        }
        let parent = self
            .root
            .root()
            .and_then(|root| root.downcast::<gtk::Window>().ok());
        open_verified_duplicates_preview(parent.as_ref(), &self.command_controller, merges);
    }

    fn apply_groups(
        &self,
        result: sustain_app_runtime::ApplicationRuntimeResult<Vec<DuplicateGroup>>,
    ) {
        match result {
            Ok(groups) => {
                let rows = {
                    let runtime = self.runtime.borrow();
                    let honor_sort_tags = runtime.settings().library.honor_sort_tags;
                    let mut rows = Vec::new();
                    for (group_index, group) in groups.iter().enumerate() {
                        for track_id in &group.track_ids {
                            if let Some(track) = runtime.library_track(*track_id) {
                                rows.push(
                                    TrackTableRow::from_track(track, honor_sort_tags)
                                        .with_group_band(group_index % 2 == 0),
                                );
                            }
                        }
                    }
                    rows
                };
                let row_count = rows.len();
                let update = if self.replace_pending.replace(false) {
                    RowSetUpdate::Replace
                } else {
                    RowSetUpdate::Reconcile
                };
                self.ensure_table().show_rows(rows, update);
                let refresh = self.on_summary_refresh.borrow().clone();
                if let Some(refresh) = refresh {
                    refresh();
                }
                let verified = groups.iter().filter(|group| group.same_recording).count();
                self.status
                    .set_text(&duplicate_scan_summary(groups.len(), row_count, verified));
                self.merge_verified.set_sensitive(verified > 0);
                self.groups.replace(groups);
            }
            Err(error) => {
                self.status.set_text(&gettext("Duplicate scan failed."));
                self.runtime.borrow_mut().push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    NotificationSeverity::Error,
                    runtime_error_text(&error),
                );
            }
        }
    }
}
/// The status line after a scan: how many groups and tracks it found, and
/// how many of the groups their audio verified.
fn duplicate_scan_summary(groups: usize, tracks: usize, verified: usize) -> String {
    phrase_list(&[
        tr_format!(
            npgettext(
                "count of duplicate groups",
                "{count} duplicate group",
                "{count} duplicate groups",
                plural_count(groups)
            ),
            count = groups
        ),
        tr_format!(
            npgettext(
                "count of tracks",
                "{count} track",
                "{count} tracks",
                plural_count(tracks)
            ),
            count = tracks
        ),
        tr_format!(
            npgettext(
                "count of duplicate groups",
                "{count} verified by their audio",
                "{count} verified by their audio",
                plural_count(verified)
            ),
            count = verified
        ),
    ])
}
