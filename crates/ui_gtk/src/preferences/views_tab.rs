// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::rc::Rc;

use gtk::prelude::*;
use sustain_app_runtime::UserSettings;
use sustain_i18n::gettext;

use super::super::{ApplicationCommand, command_controller::SharedCommandController};
use super::ViewSettingsChangedCallback;
use super::switch_row::{build_switch_row, connect_setting_switch};

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
        &gettext("Show Duplicates in the sidebar"),
        &gettext("Lists the Duplicates view in the left column's Library section."),
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
        &gettext("Show Statistics in the sidebar"),
        &gettext("Lists the Statistics view in the left column's Library section."),
        initial.sidebar_show_statistics,
    );
    wire_view_switch(
        &statistics_row.switch,
        command_controller.clone(),
        view_settings_changed.clone(),
        ViewFlag::SidebarStatistics,
    );
    content.append(&statistics_row.container);

    let waveform_row = build_switch_row(
        &gettext("Show the waveform in Now Playing"),
        &gettext(
            "Draws the playing track's waveform in place of the progress bar, \
         once audio analysis has measured it.",
        ),
        initial.now_playing_waveform,
    );
    wire_view_switch(
        &waveform_row.switch,
        command_controller,
        view_settings_changed,
        ViewFlag::NowPlayingWaveform,
    );
    content.append(&waveform_row.container);

    content.upcast()
}

#[derive(Clone, Copy)]
enum ViewFlag {
    SidebarDuplicates,
    SidebarStatistics,
    NowPlayingWaveform,
}

impl ViewFlag {
    fn of(self, settings: &mut UserSettings) -> &mut bool {
        match self {
            Self::SidebarDuplicates => &mut settings.ui.sidebar_show_duplicates,
            Self::SidebarStatistics => &mut settings.ui.sidebar_show_statistics,
            Self::NowPlayingWaveform => &mut settings.ui.now_playing_waveform,
        }
    }
}

fn wire_view_switch(
    switch: &gtk::Switch,
    command_controller: SharedCommandController,
    view_settings_changed: ViewSettingsChangedCallback,
    flag: ViewFlag,
) {
    let command_controller_for_apply = command_controller.clone();
    let held = Rc::new(move || {
        let mut held = command_controller.runtime().borrow().settings().clone();
        *flag.of(&mut held)
    });
    let held_for_apply = held.clone();
    connect_setting_switch(
        switch,
        move |requested_state| {
            let mut settings = command_controller_for_apply
                .runtime()
                .borrow()
                .settings()
                .clone();
            *flag.of(&mut settings) = requested_state;
            // A refusal is reported once by `UiCommandController`.
            let _result =
                command_controller_for_apply.dispatch(ApplicationCommand::UpdateSettings(settings));
            if held_for_apply() == requested_state {
                // Apply the change to the live UI (sidebar rows now, the
                // Now Playing waveform) without a relaunch.
                view_settings_changed();
            }
        },
        move || held(),
    );
}
