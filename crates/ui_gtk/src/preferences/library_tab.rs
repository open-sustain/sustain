// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::PathBuf;

use gtk::prelude::*;
use gtk::{gio, glib::Propagation};
use sustain_app_runtime::{NotificationCategory, NotificationSeverity, runtime_error_text};

use super::super::{
    ApplicationCommand, ApplicationRuntimeError, LibraryManagementMode,
    command_controller::SharedCommandController,
    library_consolidation::LibraryConsolidationRequestedCallback,
    library_scan::LibraryScanRequestedCallback,
};
use super::switch_row::build_switch_row;
use super::{HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS, PATH_ENTRY_VISIBLE_CHARS};

pub(super) fn build(
    parent_window: &gtk::Window,
    command_controller: SharedCommandController,
    scan_requested: LibraryScanRequestedCallback,
    consolidation_requested: LibraryConsolidationRequestedCallback,
) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let label_group = gtk::Box::new(gtk::Orientation::Vertical, 3);

    let library_path_label = gtk::Label::new(Some("Library path"));
    library_path_label.set_xalign(0.0);

    let library_path_help = gtk::Label::new(Some(
        "Files in this folder are scanned into your Sustain library.",
    ));
    library_path_help.add_css_class("preference-helper");
    library_path_help.set_xalign(0.0);
    library_path_help.set_wrap(true);
    library_path_help.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    library_path_help.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    library_path_help.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);

    label_group.append(&library_path_label);
    label_group.append(&library_path_help);

    let path_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let path_entry = gtk::Entry::new();
    path_entry.set_hexpand(true);
    path_entry.set_halign(gtk::Align::Fill);
    // Bound the natural-width request to a semantic visual size; long
    // paths still display and remain editable — `gtk::Text` scrolls
    // them internally — but the field does not push the Library page
    // wider than the panel via `path_row`'s horizontal sum.
    path_entry.set_width_chars(PATH_ENTRY_VISIBLE_CHARS);
    path_entry.set_max_width_chars(PATH_ENTRY_VISIBLE_CHARS);
    path_entry.set_placeholder_text(Some("/home/julien/Music"));
    let library_task_is_running = command_controller
        .runtime()
        .borrow()
        .background_task_status()
        .is_running();
    if let Some(library_path) = command_controller
        .runtime()
        .borrow()
        .settings()
        .library_path()
    {
        path_entry.set_text(&library_path.to_string_lossy());
    }

    let folder_icon = gtk::Image::from_icon_name("folder-open-symbolic");
    let folder_button = gtk::Button::new();
    folder_button.add_css_class("flat");
    folder_button.add_css_class("settings-button");
    folder_button.set_child(Some(&folder_icon));
    folder_button.set_tooltip_text(Some("Choose folder"));

    let scan_button = gtk::Button::with_label("Scan");
    scan_button.set_tooltip_text(Some("Save library path and scan now"));
    path_entry.set_sensitive(!library_task_is_running);
    folder_button.set_sensitive(!library_task_is_running);
    scan_button.set_sensitive(!library_task_is_running);

    let library_path_is_valid = library_path_entry_is_valid(&path_entry);
    let management_mode = command_controller
        .runtime()
        .borrow()
        .settings()
        .library
        .management_mode;
    let keep_organized_row = build_switch_row(
        "Organize my library",
        "New tracks are copied into clean artist, album, and track folders. \
         Existing tracks are organized in the background when this is turned on.",
        library_path_is_valid
            && management_mode == LibraryManagementMode::CopyAddedFilesIntoLibrary,
    );
    let keep_organized_switch = keep_organized_row.switch.clone();
    keep_organized_switch.set_sensitive(library_path_is_valid);

    let command_controller_for_organization = command_controller.clone();
    let consolidation_requested_for_organization = consolidation_requested.clone();
    let path_entry_for_organization = path_entry.clone();
    let path_entry_for_organization_sensitivity = path_entry.clone();
    let folder_button_for_organization_sensitivity = folder_button.clone();
    let scan_button_for_organization_sensitivity = scan_button.clone();
    keep_organized_switch.connect_state_set(move |_switch, requested_state| {
        // The toggle alters two settings at once: the library path (kept
        // in sync with the entry contents) and the management mode. We
        // build the full target settings and dispatch a single
        // UpdateSettings — two dispatches would do every UpdateSettings-
        // side-effect twice on the GTK main loop.
        let path_text = path_entry_for_organization.text().trim().to_owned();
        let library_path = if path_text.is_empty() {
            None
        } else {
            Some(PathBuf::from(path_text))
        };
        let path_is_valid = library_path.as_ref().is_some_and(|path| path.is_dir());

        if requested_state && !path_is_valid {
            notify_library_task(
                &command_controller_for_organization,
                NotificationCategory::LibraryConsolidation,
                NotificationSeverity::Warning,
                runtime_error_text(&ApplicationRuntimeError::LibraryPathUnavailable),
            );
            return Propagation::Stop;
        }

        let mut settings = command_controller_for_organization
            .runtime()
            .borrow()
            .settings()
            .clone();
        settings.library.path = library_path;
        settings.library.management_mode = if requested_state {
            LibraryManagementMode::CopyAddedFilesIntoLibrary
        } else {
            LibraryManagementMode::ReferenceFilesInPlace
        };
        // UpdateSettings failures are reported once by `UiCommandController`;
        // a started/failed consolidation reports through the runtime's
        // LibraryConsolidation notification (and the start-failure path in
        // `consolidation_requested`). This handler only owns the form's
        // control sensitivity and the cancellation acknowledgement.
        match command_controller_for_organization
            .dispatch(ApplicationCommand::UpdateSettings(settings))
        {
            Ok(()) if requested_state => match consolidation_requested_for_organization() {
                Ok(()) => {
                    path_entry_for_organization_sensitivity.set_sensitive(false);
                    folder_button_for_organization_sensitivity.set_sensitive(false);
                    scan_button_for_organization_sensitivity.set_sensitive(false);
                    Propagation::Proceed
                }
                Err(_error) => {
                    // The consolidation start failure was already reported
                    // by `consolidation_requested`; roll the mode back so
                    // the persisted setting matches the toggle's resting
                    // state.
                    let mut settings = command_controller_for_organization
                        .runtime()
                        .borrow()
                        .settings()
                        .clone();
                    settings.library.management_mode = LibraryManagementMode::ReferenceFilesInPlace;
                    let _result = command_controller_for_organization
                        .dispatch(ApplicationCommand::UpdateSettings(settings));
                    Propagation::Stop
                }
            },
            Ok(()) => {
                if command_controller_for_organization
                    .runtime()
                    .borrow()
                    .background_task_status()
                    .is_library_consolidation_running()
                {
                    notify_library_task(
                        &command_controller_for_organization,
                        NotificationCategory::LibraryConsolidation,
                        NotificationSeverity::Info,
                        "Library organization will stop after the current file.".to_owned(),
                    );
                }
                Propagation::Proceed
            }
            Err(_error) => Propagation::Stop,
        }
    });

    let keep_organized_for_path_change = keep_organized_switch.clone();
    let command_controller_for_path_change = command_controller.clone();
    path_entry.connect_changed(move |entry| {
        let path_is_valid = library_path_entry_is_valid(entry);
        let library_task_is_running = command_controller_for_path_change
            .runtime()
            .borrow()
            .background_task_status()
            .is_running();
        keep_organized_for_path_change.set_sensitive(path_is_valid);
        if !library_task_is_running && !path_is_valid && keep_organized_for_path_change.is_active()
        {
            keep_organized_for_path_change.set_active(false);
        }
    });

    let command_controller_for_entry = command_controller.clone();
    path_entry.connect_activate(move |entry| {
        let _result = save_library_path_from_entry(&command_controller_for_entry, entry);
    });

    let parent_for_folder = parent_window.clone();
    let command_controller_for_folder = command_controller.clone();
    let path_entry_for_folder = path_entry.clone();
    folder_button.connect_clicked(move |_| {
        open_library_folder_chooser(
            &parent_for_folder,
            &command_controller_for_folder,
            &path_entry_for_folder,
        );
    });

    let command_controller_for_scan = command_controller.clone();
    let path_entry_for_scan = path_entry.clone();
    let scan_requested_for_scan = scan_requested.clone();
    scan_button.connect_clicked(move |_| {
        if path_entry_for_scan.text().trim().is_empty() {
            notify_library_task(
                &command_controller_for_scan,
                NotificationCategory::LibraryScan,
                NotificationSeverity::Warning,
                runtime_error_text(&ApplicationRuntimeError::LibraryPathUnavailable),
            );
            return;
        }

        // Save failures are reported by `UiCommandController`; scan
        // start/running/outcome (and a start failure) report through the
        // runtime's LibraryScan notifications. Nothing to render here.
        if save_library_path_from_entry(&command_controller_for_scan, &path_entry_for_scan).is_ok()
        {
            let _ = request_library_scan_from_entry(&path_entry_for_scan, &scan_requested_for_scan);
        }
    });

    path_row.append(&path_entry);
    path_row.append(&folder_button);
    path_row.append(&scan_button);

    let sort_tags_enabled = command_controller
        .runtime()
        .borrow()
        .settings()
        .library
        .honor_sort_tags;
    let sort_tags_row = build_switch_row(
        "Sort using sort-order tags",
        "Order the library by the \u{201C}sort as\u{201D} tags some files carry \
         (ID3 TSOP/TSOT, Vorbis ARTISTSORT/TITLESORT, and friends), so \
         \u{201C}The Beatles\u{201D} files under B and \u{201C}Björk\u{201D} sorts as \
         \u{201C}Bjork\u{201D}. Turn off for strict alphabetic-as-shown ordering.",
        sort_tags_enabled,
    );
    let command_controller_for_sort_tags = command_controller.clone();
    sort_tags_row
        .switch
        .connect_state_set(move |_switch, requested_state| {
            let mut settings = command_controller_for_sort_tags
                .runtime()
                .borrow()
                .settings()
                .clone();
            settings.library.honor_sort_tags = requested_state;
            let _result = command_controller_for_sort_tags
                .dispatch(ApplicationCommand::UpdateSettings(settings));
            Propagation::Proceed
        });

    content.append(&label_group);
    content.append(&path_row);
    content.append(&keep_organized_row.container);
    content.append(&sort_tags_row.container);

    content.upcast()
}

/// Push a library-task message onto the shared NotificationCenter lane.
/// Preferences renders no transient operational text of its own (project
/// policy); scan/organization lifecycle and validation all flow through
/// the runtime's notification queue like every other status message.
fn notify_library_task(
    command_controller: &SharedCommandController,
    category: NotificationCategory,
    severity: NotificationSeverity,
    body: String,
) {
    command_controller
        .runtime()
        .borrow_mut()
        .push_ephemeral_notification(category, severity, body);
}

fn open_library_folder_chooser(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
    path_entry: &gtk::Entry,
) {
    let dialog = gtk::FileDialog::builder()
        .title("Choose Library Folder")
        .accept_label("Choose")
        .modal(true)
        .build();

    let current_path = path_entry.text().trim().to_owned();
    if !current_path.is_empty() {
        let current_folder = gio::File::for_path(current_path);
        dialog.set_initial_folder(Some(&current_folder));
    }

    let command_controller = command_controller.clone();
    let path_entry = path_entry.clone();
    dialog.select_folder(Some(parent), None::<&gio::Cancellable>, move |result| {
        let Ok(file) = result else {
            return;
        };
        let Some(folder_path) = file.path() else {
            return;
        };
        path_entry.set_text(&folder_path.to_string_lossy());
        let _result = save_library_path_from_entry(&command_controller, &path_entry);
    });
}

fn save_library_path_from_entry(
    command_controller: &SharedCommandController,
    path_entry: &gtk::Entry,
) -> Result<(), ApplicationRuntimeError> {
    let path_text = path_entry.text().trim().to_owned();
    let library_path = if path_text.is_empty() {
        None
    } else {
        Some(PathBuf::from(path_text))
    };
    let mut settings = command_controller.runtime().borrow().settings().clone();
    settings.library.path = library_path;
    if settings
        .library
        .path
        .as_ref()
        .is_none_or(|path| !path.is_dir())
    {
        settings.library.management_mode = LibraryManagementMode::ReferenceFilesInPlace;
    }

    command_controller.dispatch(ApplicationCommand::UpdateSettings(settings))
}

fn library_path_entry_is_valid(path_entry: &gtk::Entry) -> bool {
    let path_text = path_entry.text().trim().to_owned();
    !path_text.is_empty() && PathBuf::from(path_text).is_dir()
}

fn request_library_scan_from_entry(
    path_entry: &gtk::Entry,
    scan_requested: &LibraryScanRequestedCallback,
) -> Result<(), ApplicationRuntimeError> {
    let path_text = path_entry.text().trim().to_owned();
    if path_text.is_empty() {
        return Err(ApplicationRuntimeError::LibraryScanFailed);
    }

    scan_requested(PathBuf::from(path_text))
}
