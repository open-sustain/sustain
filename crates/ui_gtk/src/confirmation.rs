// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::gio;
use gtk::prelude::*;

const CANCEL_BUTTON: i32 = 0;
const CONFIRM_BUTTON: i32 = 1;

/// Show a native GTK confirmation alert with Cancel as both the Escape
/// target and default button. `on_confirm` only runs when the user picks
/// the second button.
pub(crate) fn show_confirmation_alert(
    parent: &impl IsA<gtk::Window>,
    message: &str,
    detail: &str,
    confirm_label: &str,
    on_confirm: impl FnOnce() + 'static,
) {
    let dialog = gtk::AlertDialog::builder()
        .modal(true)
        .message(message)
        .detail(detail)
        .buttons(["Cancel", confirm_label])
        .cancel_button(CANCEL_BUTTON)
        .default_button(CANCEL_BUTTON)
        .build();

    dialog.choose(Some(parent), None::<&gio::Cancellable>, move |response| {
        if matches!(response, Ok(CONFIRM_BUTTON)) {
            on_confirm();
        }
    });
}
