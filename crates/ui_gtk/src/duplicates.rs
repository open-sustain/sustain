// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::mpsc,
    time::Duration,
};

use gtk::glib;
use gtk::prelude::*;
use sustain_app_runtime::{
    DuplicateMatchMode, NotificationCategory, NotificationSeverity, runtime_error_text,
};

use crate::{
    SharedRuntime,
    track_context::TrackRowContextMenu,
    track_table::{
        InlineEditHooks, RatingChangedCallback, TrackActivatedCallback, TrackTable, TrackTableRow,
        build_track_table,
    },
};

#[derive(Clone)]
pub(crate) struct DuplicatesView {
    root: gtk::Box,
    table: Rc<RefCell<Option<TrackTable>>>,
    context_menu: TrackRowContextMenu,
    track_activated: TrackActivatedCallback,
    rating_changed: RatingChangedCallback,
    inline_edit: InlineEditHooks,
    runtime: SharedRuntime,
    strict: gtk::Switch,
    status: gtk::Label,
    generation: Rc<Cell<u64>>,
    active: Rc<Cell<bool>>,
}

impl DuplicatesView {
    pub(crate) fn new(
        runtime: SharedRuntime,
        context_menu: TrackRowContextMenu,
        track_activated: TrackActivatedCallback,
        rating_changed: RatingChangedCallback,
        inline_edit: InlineEditHooks,
    ) -> Self {
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        root.set_hexpand(true);
        root.set_vexpand(true);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        header.set_margin_top(8);
        header.set_margin_end(10);
        header.set_margin_bottom(8);
        header.set_margin_start(10);
        let status = gtk::Label::new(Some("Open Duplicates to scan the library."));
        status.set_xalign(0.0);
        status.set_hexpand(true);
        let strict_label = gtk::Label::new(Some("Strict matching"));
        let strict = gtk::Switch::new();
        strict.set_valign(gtk::Align::Center);
        header.append(&status);
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
            strict,
            status,
            generation: Rc::new(Cell::new(0)),
            active: Rc::new(Cell::new(false)),
        };
        let view_for_toggle = view.clone();
        view.strict.connect_active_notify(move |_| {
            if view_for_toggle.active.get() {
                view_for_toggle.scan();
            }
        });
        view
    }

    pub(crate) fn widget(&self) -> gtk::Box {
        self.root.clone()
    }

    pub(crate) fn set_active(&self, active: bool) {
        let was_active = self.active.replace(active);
        if active && !was_active {
            self.scan();
        } else if !active && was_active {
            self.generation.set(self.generation.get().wrapping_add(1));
        }
    }

    pub(crate) fn refresh_if_active(&self) {
        if self.active.get() {
            self.scan();
        }
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

    pub(crate) fn update_track_row(&self, track_id: sustain_app_runtime::TrackId) {
        let row = {
            let runtime = self.runtime.borrow();
            let honor_sort_tags = runtime.settings().library.honor_sort_tags;
            runtime
                .library_track(track_id)
                .map(|track| TrackTableRow::from_track(track, honor_sort_tags))
        };
        let Some(row) = row else {
            return;
        };
        if let Some(table) = self.table.borrow().as_ref() {
            table.update_row(track_id, row);
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

    fn scan(&self) {
        self.ensure_table();
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        self.status.set_text("Scanning for duplicate tracks...");
        let mode = if self.strict.is_active() {
            DuplicateMatchMode::Strict
        } else {
            DuplicateMatchMode::Loose
        };
        let task = match self.runtime.borrow().duplicate_groups_task(mode) {
            Ok(task) => task,
            Err(error) => {
                self.status.set_text("Duplicate scan unavailable.");
                self.runtime.borrow_mut().push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    NotificationSeverity::Error,
                    runtime_error_text(&error).to_owned(),
                );
                return;
            }
        };

        let (sender, receiver) = mpsc::sync_channel(1);
        if std::thread::Builder::new()
            .name("sustain-duplicate-groups".to_owned())
            .spawn(move || {
                let _ = sender.send(task.run());
            })
            .is_err()
        {
            self.status.set_text("Duplicate scan could not be started.");
            self.runtime.borrow_mut().push_ephemeral_notification(
                NotificationCategory::DuplicateConsolidation,
                NotificationSeverity::Error,
                "Duplicate scan could not be started.".to_owned(),
            );
            return;
        }

        let view = self.clone();
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let result = match receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Err(sustain_app_runtime::ApplicationRuntimeError::LibraryStoreFailed)
                }
            };
            if view.active.get() && view.generation.get() == generation {
                view.apply_groups(result);
            }
            glib::ControlFlow::Break
        });
    }

    fn apply_groups(
        &self,
        result: sustain_app_runtime::ApplicationRuntimeResult<
            Vec<Vec<sustain_app_runtime::TrackId>>,
        >,
    ) {
        match result {
            Ok(groups) => {
                let rows = {
                    let runtime = self.runtime.borrow();
                    let honor_sort_tags = runtime.settings().library.honor_sort_tags;
                    let mut rows = Vec::new();
                    for (group_index, group) in groups.iter().enumerate() {
                        for track_id in group {
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
                self.ensure_table().replace_rows(rows);
                self.status.set_text(&format!(
                    "{} duplicate group(s), {} track(s).",
                    groups.len(),
                    row_count
                ));
            }
            Err(error) => {
                self.status.set_text("Duplicate scan failed.");
                self.runtime.borrow_mut().push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    NotificationSeverity::Error,
                    runtime_error_text(&error).to_owned(),
                );
            }
        }
    }
}
