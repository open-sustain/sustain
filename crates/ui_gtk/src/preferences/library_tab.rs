// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::PathBuf;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gio, glib};
use sustain_app_runtime::{NotificationCategory, NotificationSeverity, runtime_error_text};
use sustain_i18n::gettext;

use super::super::{
    ApplicationCommand, ApplicationRuntimeError, LibraryManagementMode,
    command_controller::SharedCommandController,
    library_consolidation::LibraryConsolidationRequestedCallback,
    library_scan::LibraryScanRequestedCallback,
};
use super::switch_row::{build_switch_row, connect_setting_switch};
use super::{
    HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS, PATH_ENTRY_VISIBLE_CHARS,
    SortTagsChangedCallback,
};

pub(super) fn build(
    parent_window: &gtk::Window,
    command_controller: SharedCommandController,
    scan_requested: LibraryScanRequestedCallback,
    consolidation_requested: LibraryConsolidationRequestedCallback,
    sort_tags_changed: SortTagsChangedCallback,
) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let label_group = gtk::Box::new(gtk::Orientation::Vertical, 3);

    let library_path_label = gtk::Label::new(Some(&gettext("Library path")));
    library_path_label.set_xalign(0.0);

    let library_path_help = gtk::Label::new(Some(&gettext(
        "Files in this folder are scanned into your Sustain library.",
    )));
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
    // Suggest the user's own XDG music folder, the usual library root.
    if let Some(music_dir) = glib::user_special_dir(glib::UserDirectory::Music) {
        path_entry.set_placeholder_text(Some(&music_dir.to_string_lossy()));
    }
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
    folder_button.set_tooltip_text(Some(&gettext("Choose folder")));

    let scan_button = gtk::Button::with_label(&gettext("Scan"));
    scan_button.set_tooltip_text(Some(&gettext("Save library path and scan now")));
    lock_path_controls_while_library_task_runs(
        parent_window,
        &command_controller,
        [
            path_entry.clone().upcast(),
            folder_button.clone().upcast(),
            scan_button.clone().upcast(),
        ],
    );

    let saved_root_usable = saved_root_is_a_folder(&command_controller);
    let management_mode = command_controller
        .runtime()
        .borrow()
        .settings()
        .library
        .management_mode;
    let keep_organized_row = build_switch_row(
        &gettext("Organize my library"),
        &gettext(
            "New tracks are copied into clean artist, album, and track folders. \
         Existing tracks are organized in the background when this is turned on.",
        ),
        management_mode == LibraryManagementMode::CopyAddedFilesIntoLibrary,
    );
    let keep_organized_switch = keep_organized_row.switch.clone();
    // The switch organizes the saved library folder, never the entry's
    // draft, and turning it on or off changes the mode alone (#371). It
    // can be used while the entry shows that folder and it exists: a
    // draft must be committed first, so the folder the switch acts on is
    // always the one in view.
    keep_organized_switch.set_sensitive(saved_root_usable);

    let command_controller_for_organization = command_controller.clone();
    let consolidation_requested_for_organization = consolidation_requested.clone();
    let command_controller_for_organized = command_controller.clone();
    // Whatever the handler below does, the switch shows the mode the
    // application ends up holding.
    let organized = move || {
        command_controller_for_organized
            .runtime()
            .borrow()
            .settings()
            .library
            .management_mode
            == LibraryManagementMode::CopyAddedFilesIntoLibrary
    };
    let organize = move |requested_state: bool| {
        if requested_state && !saved_root_is_a_folder(&command_controller_for_organization) {
            notify_library_task(
                &command_controller_for_organization,
                NotificationCategory::LibraryConsolidation,
                NotificationSeverity::Warning,
                runtime_error_text(&ApplicationRuntimeError::LibraryPathUnavailable),
            );
            return;
        }

        let mut settings = command_controller_for_organization
            .runtime()
            .borrow()
            .settings()
            .clone();
        settings.library.management_mode = if requested_state {
            LibraryManagementMode::CopyAddedFilesIntoLibrary
        } else {
            LibraryManagementMode::ReferenceFilesInPlace
        };
        // UpdateSettings failures are reported once by `UiCommandController`;
        // a started/failed consolidation reports through the runtime's
        // LibraryConsolidation notification (and the start-failure path in
        // `consolidation_requested`). This handler only owns the
        // cancellation acknowledgement; the path controls follow the
        // library task through `lock_path_controls_while_library_task_runs`.
        match command_controller_for_organization
            .dispatch(ApplicationCommand::UpdateSettings(settings))
        {
            Ok(()) if requested_state => {
                if consolidation_requested_for_organization().is_err() {
                    // The consolidation start failure was already reported
                    // by `consolidation_requested`; roll the mode back so
                    // organization is not left on with nothing organizing.
                    let mut settings = command_controller_for_organization
                        .runtime()
                        .borrow()
                        .settings()
                        .clone();
                    settings.library.management_mode = LibraryManagementMode::ReferenceFilesInPlace;
                    let _result = command_controller_for_organization
                        .dispatch(ApplicationCommand::UpdateSettings(settings));
                }
            }
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
                        gettext("Library organization will stop after the current file."),
                    );
                }
            }
            Err(_error) => {}
        }
    };
    let organize_switch = connect_setting_switch(&keep_organized_switch, organize, organized);

    // Enter, the folder chooser and Scan commit the entry's draft. The
    // commit may change the mode too (an emptied entry ends organization),
    // so the switch then shows what was saved, and can be used if the
    // saved folder exists.
    let commit_path: CommitLibraryPath = {
        let command_controller = command_controller.clone();
        let keep_organized_switch = keep_organized_switch.downgrade();
        Rc::new(move |entry: &gtk::Entry| {
            let result = save_library_path_from_entry(&command_controller, entry);
            organize_switch.show_held();
            if let Some(switch) = keep_organized_switch.upgrade() {
                switch.set_sensitive(organize_switch_usable(&command_controller, entry));
            }
            result
        })
    };

    // The entry's text is a draft: typing it saves nothing, and only
    // decides whether the switch can be used (#371).
    let keep_organized_for_draft = keep_organized_switch.downgrade();
    let command_controller_for_draft = command_controller.clone();
    path_entry.connect_changed(move |entry| {
        if let Some(switch) = keep_organized_for_draft.upgrade() {
            switch.set_sensitive(organize_switch_usable(&command_controller_for_draft, entry));
        }
    });
    let commit_path_for_entry = commit_path.clone();
    path_entry.connect_activate(move |entry| {
        let _result = commit_path_for_entry(entry);
    });

    // The chooser opens over the window the button is in, found when it is
    // clicked: capturing that window would keep it alive once closed
    // (#370). The entry is reached weakly for the same reason.
    let commit_path_for_folder = commit_path.clone();
    let path_entry_for_folder = path_entry.downgrade();
    folder_button.connect_clicked(move |button| {
        let (Some(parent), Some(path_entry)) = (
            button.root().and_downcast::<gtk::Window>(),
            path_entry_for_folder.upgrade(),
        ) else {
            return;
        };
        open_library_folder_chooser(&parent, &commit_path_for_folder, &path_entry);
    });

    let command_controller_for_scan = command_controller.clone();
    let commit_path_for_scan = commit_path;
    let path_entry_for_scan = path_entry.downgrade();
    let scan_requested_for_scan = scan_requested.clone();
    scan_button.connect_clicked(move |_| {
        let Some(path_entry_for_scan) = path_entry_for_scan.upgrade() else {
            return;
        };
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
        if commit_path_for_scan(&path_entry_for_scan).is_ok() {
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
        &gettext("Sort using sort-order tags"),
        &gettext(
            "Order the library by the \u{201C}sort as\u{201D} tags some files carry \
         (ID3 TSOP/TSOT, Vorbis ARTISTSORT/TITLESORT, and friends) plus \
         Sustain's generated leading-article sort values, so \u{201C}The Beatles\u{201D} \
         files under B. Turn off to order every name as it is shown.",
        ),
        sort_tags_enabled,
    );
    let command_controller_for_held_sort_tags = command_controller.clone();
    let honors_sort_tags = move || {
        command_controller_for_held_sort_tags
            .runtime()
            .borrow()
            .settings()
            .library
            .honor_sort_tags
    };
    let command_controller_for_sort_tags = command_controller.clone();
    let honors_sort_tags_for_apply = honors_sort_tags.clone();
    connect_setting_switch(
        &sort_tags_row.switch,
        move |requested_state| {
            let mut settings = command_controller_for_sort_tags
                .runtime()
                .borrow()
                .settings()
                .clone();
            settings.library.honor_sort_tags = requested_state;
            // A refusal is reported once by `UiCommandController`.
            let _result = command_controller_for_sort_tags
                .dispatch(ApplicationCommand::UpdateSettings(settings));
            if honors_sort_tags_for_apply() == requested_state {
                sort_tags_changed();
            }
        },
        honors_sort_tags,
    );

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

/// Saves the library path the entry holds, as Enter, the folder chooser
/// and Scan do.
type CommitLibraryPath = Rc<dyn Fn(&gtk::Entry) -> Result<(), ApplicationRuntimeError>>;

/// Whether the organize switch can be used: the entry shows the saved
/// library folder, uncommitted text aside, and it is a folder on disk.
fn organize_switch_usable(
    command_controller: &SharedCommandController,
    entry: &gtk::Entry,
) -> bool {
    let draft = entry.text().trim().to_owned();
    let shows_saved_folder = command_controller
        .runtime()
        .borrow()
        .settings()
        .library_path()
        .is_some_and(|saved| !draft.is_empty() && saved == std::path::Path::new(&draft));
    shows_saved_folder && saved_root_is_a_folder(command_controller)
}

/// Whether the saved library folder is a folder on disk, which the
/// organize switch needs.
fn saved_root_is_a_folder(command_controller: &SharedCommandController) -> bool {
    command_controller
        .runtime()
        .borrow()
        .settings()
        .library_path()
        .is_some_and(std::path::Path::is_dir)
}

fn open_library_folder_chooser(
    parent: &gtk::Window,
    commit_path: &CommitLibraryPath,
    path_entry: &gtk::Entry,
) {
    let dialog = gtk::FileDialog::builder()
        .title(gettext("Choose Library Folder"))
        .accept_label(gettext("Choose"))
        .modal(true)
        .build();

    let current_path = path_entry.text().trim().to_owned();
    if !current_path.is_empty() {
        let current_folder = gio::File::for_path(current_path);
        dialog.set_initial_folder(Some(&current_folder));
    }

    let commit_path = commit_path.clone();
    let path_entry = path_entry.clone();
    dialog.select_folder(Some(parent), None::<&gio::Cancellable>, move |result| {
        let Ok(file) = result else {
            return;
        };
        let Some(folder_path) = file.path() else {
            return;
        };
        path_entry.set_text(&folder_path.to_string_lossy());
        let _result = commit_path(&path_entry);
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
    // An emptied entry removes the library folder, and a library without
    // one cannot be kept organized. A path that is not a folder is
    // refused by the runtime, which reports it and keeps the settings.
    if library_path.is_none() {
        settings.library.management_mode = LibraryManagementMode::ReferenceFilesInPlace;
    }
    settings.library.path = library_path;

    command_controller.dispatch(ApplicationCommand::UpdateSettings(settings))
}

/// Keep `controls` insensitive exactly while a library task runs. A scan,
/// import or organization started from this page, or from anywhere else,
/// locks them, and they unlock when it ends, even with the window left
/// open. The runtime observer is cleared when `window` closes, which
/// releases the controls.
fn lock_path_controls_while_library_task_runs(
    window: &gtk::Window,
    command_controller: &SharedCommandController,
    controls: [gtk::Widget; 3],
) {
    let runtime = command_controller.runtime();
    let refresh: Rc<dyn Fn()> = {
        let runtime = runtime.clone();
        Rc::new(move || {
            let running = runtime.borrow().background_task_status().is_running();
            for control in &controls {
                control.set_sensitive(!running);
            }
        })
    };
    refresh();
    // The runtime is mid-borrow when the observer fires, so the refresh
    // waits for the main loop.
    runtime
        .borrow_mut()
        .set_library_task_observer(Box::new(move || {
            let refresh = refresh.clone();
            glib::idle_add_local_once(move || refresh());
        }));
    window.connect_close_request(move |_| {
        runtime.borrow_mut().clear_library_task_observer();
        glib::Propagation::Proceed
    });
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

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use sustain_app_runtime::ApplicationRuntime;

    use super::*;
    use crate::command_controller::UiCommandController;

    fn switch_labelled(root: &gtk::Widget, label_text: &str) -> Option<gtk::Switch> {
        let mut child = root.first_child();
        let mut labelled = false;
        while let Some(widget) = child {
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                labelled = label.text() == label_text;
            } else if let Some(switch) = widget.downcast_ref::<gtk::Switch>() {
                if labelled {
                    return Some(switch.clone());
                }
            } else if let Some(found) = switch_labelled(&widget, label_text) {
                return Some(found);
            }
            child = widget.next_sibling();
        }
        None
    }

    /// The Library tab over an organized library in `root`, with its
    /// path entry and organize switch.
    fn organized_library_tab(
        root: &std::path::Path,
    ) -> (crate::SharedRuntime, gtk::Window, gtk::Entry, gtk::Switch) {
        let mut settings =
            sustain_app_runtime::UserSettings::with_library_path(Some(root.to_path_buf()));
        settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
        let runtime = Rc::new(RefCell::new(
            ApplicationRuntime::with_settings_store(Box::new(
                sustain_settings::InMemorySettingsStore::new(settings),
            ))
            .expect("load settings"),
        ));
        let command_controller: SharedCommandController =
            Rc::new(UiCommandController::new(runtime.clone()));
        let window = gtk::Window::new();
        let page = build(
            &window,
            command_controller,
            Rc::new(|_| Ok(())),
            Rc::new(|| Ok(())),
            Rc::new(|| {}),
        );
        let entry = crate::test_support::descendants(page.upcast_ref())
            .into_iter()
            .find_map(|widget| widget.downcast::<gtk::Entry>().ok())
            .expect("the library path entry");
        let organize = switch_labelled(&page, "Organize my library").expect("the organize switch");
        (runtime, window, entry, organize)
    }

    #[test]
    fn a_typed_library_path_is_a_draft_until_it_is_committed() {
        crate::test_support::with_gtk(|| {
            let root = std::env::temp_dir();
            let (runtime, window, entry, organize) = organized_library_tab(&root);
            let saved = runtime.borrow().settings().clone();

            entry.set_text("/surely/not/a/folder/yet");
            assert_eq!(runtime.borrow().settings(), &saved, "typing saves nothing");
            assert!(organize.is_active(), "the switch shows the saved mode");
            assert!(!organize.is_sensitive(), "a draft must be committed first");

            entry.emit_activate();
            assert_eq!(
                runtime.borrow().settings(),
                &saved,
                "Enter on a path that is not a folder is refused"
            );

            // Another existing folder, typed but not committed: the switch
            // cannot act until the entry shows the saved folder again.
            let other = root.join(format!("sustain_draft_{}", std::process::id()));
            std::fs::create_dir_all(&other).expect("create another folder");
            entry.set_text(&other.to_string_lossy());
            assert!(!organize.is_sensitive());
            entry.set_text(&root.to_string_lossy());
            assert!(organize.is_sensitive(), "the saved folder is in view again");
            organize.set_active(false);
            assert_eq!(runtime.borrow().settings().library.path, saved.library.path);
            assert_eq!(
                runtime.borrow().settings().library.management_mode,
                LibraryManagementMode::ReferenceFilesInPlace
            );
            std::fs::remove_dir(&other).expect("remove the other folder");
            window.destroy();
        });
    }

    #[test]
    fn an_offline_organized_library_shows_its_mode_but_cannot_be_switched() {
        crate::test_support::with_gtk(|| {
            let offline = std::env::temp_dir().join("sustain_offline_drive_not_mounted");
            let (_runtime, window, _entry, organize) = organized_library_tab(&offline);

            assert!(organize.is_active(), "the saved mode is shown");
            assert!(
                !organize.is_sensitive(),
                "its folder cannot be organized now"
            );
            window.destroy();
        });
    }

    #[test]
    fn clearing_the_library_folder_shows_organization_ended() {
        crate::test_support::with_gtk(|| {
            let root = std::env::temp_dir();
            let (runtime, window, entry, organize) = organized_library_tab(&root);

            entry.set_text("");
            entry.emit_activate();

            assert_eq!(runtime.borrow().settings().library.path, None);
            assert_eq!(
                runtime.borrow().settings().library.management_mode,
                LibraryManagementMode::ReferenceFilesInPlace
            );
            assert!(!organize.is_active(), "the switch shows what was saved");
            assert!(!organize.is_sensitive(), "there is no folder to organize");

            entry.set_text(&root.to_string_lossy());
            entry.emit_activate();

            assert!(organize.is_sensitive(), "the folder can be organized again");
            assert!(!organize.is_active(), "but is not until asked");
            window.destroy();
        });
    }

    #[test]
    fn flipping_the_sort_tag_switch_saves_it_and_re_files_the_lists() {
        crate::test_support::with_gtk(|| {
            let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let command_controller: SharedCommandController =
                Rc::new(UiCommandController::new(runtime.clone()));
            let re_filed = Rc::new(Cell::new(0));
            let re_filed_by_tab = re_filed.clone();
            let window = gtk::Window::new();
            let page = build(
                &window,
                command_controller,
                Rc::new(|_| Ok(())),
                Rc::new(|| Ok(())),
                Rc::new(move || re_filed_by_tab.set(re_filed_by_tab.get() + 1)),
            );
            let switch =
                switch_labelled(&page, "Sort using sort-order tags").expect("the sort-tag switch");
            assert!(switch.is_active(), "sort tags are honored by default");

            switch.set_active(false);

            assert!(!runtime.borrow().settings().library.honor_sort_tags);
            assert_eq!(re_filed.get(), 1);
        });
    }
}
