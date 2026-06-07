// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::Path;

use gtk::gio;
use gtk::prelude::*;

use super::{HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS};

/// Themed application icon, registered by `install_app_icon` in
/// `main_window.rs`. Matches the desktop/MPRIS identity so the About
/// pane shows the same artwork the user sees in their app launcher.
const APP_ICON_NAME: &str = "io.github.open_sustain.sustain";
/// Logical pixels. GTK picks the closest pre-built size from the
/// hicolor theme and scales it with the display scale factor, so this
/// looks crisp on HiDPI screens (256/512 sources are shipped under
/// `data/icons/hicolor`).
const APP_ICON_SIZE: i32 = 96;

const DOCUMENTATION_URL: &str =
    "https://github.com/open-sustain/sustain/blob/main/docs/features.md";
const ISSUES_URL: &str = "https://github.com/open-sustain/sustain/issues";

pub(super) fn build(parent_window: &gtk::Window, database_path: &Path) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let icon = gtk::Image::from_icon_name(APP_ICON_NAME);
    icon.set_pixel_size(APP_ICON_SIZE);
    icon.set_halign(gtk::Align::Center);
    icon.add_css_class("about-app-icon");
    content.append(&icon);

    let name = gtk::Label::new(Some("Sustain"));
    name.set_halign(gtk::Align::Center);
    name.add_css_class("about-app-name");
    content.append(&name);

    content.append(&centered_helper_label(
        "A music library manager and player for the Linux desktop.",
    ));

    let details = gtk::Box::new(gtk::Orientation::Vertical, 2);
    details.set_halign(gtk::Align::Center);
    details.set_margin_top(8);
    details.append(&centered_helper_label(&format!(
        "Version {}",
        env!("CARGO_PKG_VERSION")
    )));
    details.append(&centered_helper_label("Licensed under GPL-3.0-or-later"));
    details.append(&centered_helper_label("© 2026 AnnoyingTechnology"));
    content.append(&details);

    content.append(&build_backup_section(database_path));

    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    actions.set_halign(gtk::Align::Center);
    actions.set_margin_top(16);
    actions.set_homogeneous(true);

    let docs_button = gtk::Button::with_label("Documentation");
    docs_button.set_tooltip_text(Some(DOCUMENTATION_URL));
    let docs_parent = parent_window.clone();
    docs_button.connect_clicked(move |_| {
        launch_uri(&docs_parent, DOCUMENTATION_URL);
    });
    actions.append(&docs_button);

    let issues_button = gtk::Button::with_label("Report a problem");
    issues_button.set_tooltip_text(Some(ISSUES_URL));
    let issues_parent = parent_window.clone();
    issues_button.connect_clicked(move |_| {
        launch_uri(&issues_parent, ISSUES_URL);
    });
    actions.append(&issues_button);

    content.append(&actions);

    content.upcast()
}

/// Backup guidance (issue #119). The SQLite database is the source of
/// truth for every value that exists only in Sustain — ratings, play
/// counts, playlists, and edits the user never wrote back to their files
/// — so a music-folder backup alone would lose all of it. Surface the
/// folder that actually holds the database (resolved at runtime, so it is
/// correct under the `--database` / `--local-scope` dev flags too) and
/// tell the user to back it up alongside their music.
fn build_backup_section(database_path: &Path) -> gtk::Widget {
    let section = gtk::Box::new(gtk::Orientation::Vertical, 4);
    section.set_halign(gtk::Align::Center);
    section.set_margin_top(20);

    section.append(&centered_helper_label(
        "Your ratings, play counts, playlists, and library edits live in \
         Sustain's database, not in your music files. Back up this folder \
         alongside your music collection:",
    ));

    let folder = backup_folder(database_path);
    // Default foreground (not the muted helper class) so the path stands
    // out from the explanatory text above it, while staying theme-correct
    // in both light and dark. Selectable so the user can copy it; wrapped
    // mid-path and width-bounded like every other Preferences label so a
    // long path never widens the pinned panel (see #53).
    let path_label = gtk::Label::new(Some(&folder.display().to_string()));
    path_label.set_halign(gtk::Align::Center);
    path_label.set_justify(gtk::Justification::Center);
    path_label.set_selectable(true);
    path_label.set_wrap(true);
    path_label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    path_label.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    path_label.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    section.append(&path_label);

    section.upcast()
}

/// The folder the user should back up: the directory containing the
/// database file. Falls back to the path itself when it has no usable
/// parent component (e.g. a bare relative `--database name.sqlite`), so
/// the label never shows an empty string.
fn backup_folder(database_path: &Path) -> &Path {
    database_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(database_path)
}

/// Build a centered muted label whose natural width is bounded the same
/// way every other wrapping label in Preferences is bounded
/// (`wrap = true` + `width-chars` + `max-width-chars`, per commit
/// `12a91bb`). Short strings stay on one line; wrap only engages if the
/// text ever exceeds the 56-char ceiling.
fn centered_helper_label(text: &str) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("preference-helper");
    label.set_halign(gtk::Align::Center);
    label.set_justify(gtk::Justification::Center);
    label.set_wrap(true);
    label.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    label.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    label.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    label
}

/// Opens `url` in the user's default browser via `GtkUriLauncher`,
/// which routes through the desktop portal under Flatpak and the
/// `org.freedesktop.DBus`/`xdg-open` path otherwise. Failures are
/// logged: on a properly-configured Linux desktop, this only fails
/// when no default browser is registered, and there is no meaningful
/// fallback Sustain can offer.
fn launch_uri(parent: &gtk::Window, url: &'static str) {
    let launcher = gtk::UriLauncher::new(url);
    launcher.launch(Some(parent), None::<&gio::Cancellable>, move |result| {
        if let Err(error) = result {
            eprintln!("Sustain: failed to open {url} in the default browser ({error:?})");
        }
    });
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::backup_folder;

    #[test]
    fn backup_folder_is_the_database_directory() {
        assert_eq!(
            backup_folder(Path::new("/home/u/.local/share/sustain/library.sqlite")),
            Path::new("/home/u/.local/share/sustain")
        );
    }

    #[test]
    fn backup_folder_falls_back_to_a_bare_relative_path() {
        // An empty parent component would render as a blank label; show the
        // database name itself instead.
        assert_eq!(
            backup_folder(Path::new("library.sqlite")),
            Path::new("library.sqlite")
        );
    }
}
