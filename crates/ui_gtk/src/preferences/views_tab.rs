// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::glib::Propagation;
use gtk::prelude::*;

use super::super::{ApplicationCommand, command_controller::SharedCommandController};
use super::ViewSettingsChangedCallback;
use super::switch_row::build_switch_row;

pub(super) fn build(
    command_controller: SharedCommandController,
    view_settings_changed: ViewSettingsChangedCallback,
) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let initial = command_controller.runtime().borrow().settings().ui.clone();

    let duplicates_row = build_switch_row(
        "Show Duplicates in the sidebar",
        "Lists the Duplicates view in the left column's Library section.",
        initial.sidebar_show_duplicates,
    );
    wire_view_switch(
        &duplicates_row.switch,
        command_controller.clone(),
        view_settings_changed.clone(),
        ViewFlag::SidebarDuplicates,
    );
    content.append(&duplicates_row.container);

    let statistics_row = build_switch_row(
        "Show Statistics in the sidebar",
        "Lists the Statistics view in the left column's Library section.",
        initial.sidebar_show_statistics,
    );
    wire_view_switch(
        &statistics_row.switch,
        command_controller,
        view_settings_changed,
        ViewFlag::SidebarStatistics,
    );
    content.append(&statistics_row.container);

    content.upcast()
}

#[derive(Clone, Copy)]
enum ViewFlag {
    SidebarDuplicates,
    SidebarStatistics,
}

fn wire_view_switch(
    switch: &gtk::Switch,
    command_controller: SharedCommandController,
    view_settings_changed: ViewSettingsChangedCallback,
    flag: ViewFlag,
) {
    switch.connect_state_set(move |_switch, requested_state| {
        let mut settings = command_controller.runtime().borrow().settings().clone();
        match flag {
            ViewFlag::SidebarDuplicates => settings.ui.sidebar_show_duplicates = requested_state,
            ViewFlag::SidebarStatistics => settings.ui.sidebar_show_statistics = requested_state,
        }
        if command_controller
            .dispatch(ApplicationCommand::UpdateSettings(settings))
            .is_ok()
        {
            // Apply the change to the live UI (sidebar rows now, the
            // Now Playing chips on its next refresh) without a relaunch.
            view_settings_changed();
            Propagation::Proceed
        } else {
            Propagation::Stop
        }
    });
}
