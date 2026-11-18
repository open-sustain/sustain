// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::glib::Propagation;
use gtk::prelude::*;
use sustain_i18n::gettext;

use super::super::{ApplicationCommand, command_controller::SharedCommandController};
use super::switch_row::build_switch_row;

pub(super) fn build(command_controller: SharedCommandController) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let initial = command_controller.runtime().borrow().settings().online;

    let artwork_row = build_switch_row(
        &gettext("Fetch missing artwork"),
        &gettext(
            "Looks up cover art from MusicBrainz and the Cover Art Archive, then Deezer \
         and the iTunes Search API, for tracks that have none. Never replaces \
         artwork that is already present.",
        ),
        initial.artwork,
    );
    wire_online_switch(
        &artwork_row.switch,
        command_controller.clone(),
        OnlineFlag::Artwork,
    );
    content.append(&artwork_row.container);

    let tags_row = build_switch_row(
        &gettext("Fetch missing tags"),
        &gettext(
            "Matches tracks on MusicBrainz by their tags and fills missing fields. \
         Never overwrites a tag that is already set.",
        ),
        initial.tags,
    );
    wire_online_switch(
        &tags_row.switch,
        command_controller.clone(),
        OnlineFlag::Tags,
    );
    content.append(&tags_row.container);

    let lyrics_row = build_switch_row(
        &gettext("Fetch missing lyrics"),
        &gettext(
            "Looks up synchronised and plain lyrics from LRClib for tracks that have none. \
         Never replaces lyrics that are already present.",
        ),
        initial.lyrics,
    );
    wire_online_switch(&lyrics_row.switch, command_controller, OnlineFlag::Lyrics);
    content.append(&lyrics_row.container);

    content.upcast()
}

#[derive(Clone, Copy)]
enum OnlineFlag {
    Artwork,
    Tags,
    Lyrics,
}

fn wire_online_switch(
    switch: &gtk::Switch,
    command_controller: SharedCommandController,
    flag: OnlineFlag,
) {
    switch.connect_state_set(move |_switch, requested_state| {
        let mut settings = command_controller.runtime().borrow().settings().clone();
        match flag {
            OnlineFlag::Artwork => settings.online.artwork = requested_state,
            OnlineFlag::Tags => settings.online.tags = requested_state,
            OnlineFlag::Lyrics => settings.online.lyrics = requested_state,
        }
        if command_controller
            .dispatch(ApplicationCommand::UpdateSettings(settings))
            .is_ok()
        {
            Propagation::Proceed
        } else {
            Propagation::Stop
        }
    });
}
