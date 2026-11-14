// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The list of files an import left out (#298).
//!
//! Every other import message goes through the notification lane. This
//! list is the maintainer's one exception to that rule: once an import has
//! skipped files, the user needs to read which ones and why, which a
//! transient status line cannot hold.

use std::path::Path;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use sustain_app_runtime::{ImportFailure, import_failure_reason_text};

/// Lists `failures` in a window over `parent`. The list is virtualized, so
/// an import that left out thousands of files opens it as quickly.
pub(crate) fn show_import_failures(parent: &gtk::Window, failures: Vec<ImportFailure>) {
    let window = gtk::Window::builder()
        .title("Files Not Imported")
        .transient_for(parent)
        .modal(true)
        .default_width(640)
        .default_height(420)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_end(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);

    let count = failures.len();
    let heading = gtk::Label::new(Some(&if count == 1 {
        "1 file could not be imported. The rest of the import went ahead.".to_owned()
    } else {
        format!("{count} files could not be imported. The rest of the import went ahead.")
    }));
    heading.set_xalign(0.0);
    heading.set_wrap(true);
    content.append(&heading);

    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    for failure in failures {
        store.append(&glib::BoxedAnyObject::new(failure));
    }
    let list = gtk::ListView::new(
        Some(gtk::NoSelection::new(Some(store))),
        Some(failure_row_factory()),
    );
    list.add_css_class("import-failure-list");
    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_has_frame(true);
    scroller.set_child(Some(&list));
    content.append(&scroller);

    let close = gtk::Button::with_label("OK");
    close.add_css_class("suggested-action");
    close.set_halign(gtk::Align::End);
    let window_for_close = window.downgrade();
    close.connect_clicked(move |_| {
        if let Some(window) = window_for_close.upgrade() {
            window.close();
        }
    });
    content.append(&close);

    let escape = gtk::EventControllerKey::new();
    let window_for_escape = window.downgrade();
    escape.connect_key_pressed(move |_, key, _, _| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(window) = window_for_escape.upgrade() {
            window.close();
        }
        glib::Propagation::Stop
    });
    window.add_controller(escape);

    window.set_child(Some(&content));
    window.set_default_widget(Some(&close));
    window.present();
    close.grab_focus();
}

/// One failure per row: the file name, then its folder and the reason,
/// both selectable so a path can be copied.
fn failure_row_factory() -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let row = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.set_margin_top(6);
        row.set_margin_bottom(6);
        row.set_margin_start(8);
        row.set_margin_end(8);
        for dim in [false, true] {
            let label = gtk::Label::new(None);
            label.set_xalign(0.0);
            label.set_selectable(true);
            label.set_wrap(true);
            label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
            if dim {
                label.add_css_class("dim-label");
            }
            row.append(&label);
        }
        item.set_child(Some(&row));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(object) = item.item().and_downcast::<glib::BoxedAnyObject>() else {
            return;
        };
        let Ok(failure) = object.try_borrow::<ImportFailure>() else {
            return;
        };
        let Some(name) = item.child().and_then(|row| row.first_child()) else {
            return;
        };
        let Some(detail) = name.next_sibling() else {
            return;
        };
        let (name_text, detail_text) = failure_row_text(&failure);
        if let Some(name) = name.downcast_ref::<gtk::Label>() {
            name.set_text(&name_text);
        }
        if let Some(detail) = detail.downcast_ref::<gtk::Label>() {
            detail.set_text(&detail_text);
        }
    });
    factory
}

/// The two lines a failure's row shows: its file (or folder) name, then
/// the folder it is in and why it was left out.
fn failure_row_text(failure: &ImportFailure) -> (String, String) {
    let path: &Path = &failure.path;
    let name = path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    );
    let reason = import_failure_reason_text(failure.reason);
    let detail = match path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        Some(folder) => format!("{} — {reason}", folder.display()),
        None => reason.to_owned(),
    };
    (name, detail)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sustain_app_runtime::ImportFailureReason;

    use super::*;

    #[test]
    fn a_row_names_the_file_then_its_folder_and_the_reason() {
        let failure = ImportFailure {
            path: PathBuf::from("/music/Album/01 Song.flac"),
            reason: ImportFailureReason::UnreadableTags,
        };
        assert_eq!(
            failure_row_text(&failure),
            (
                "01 Song.flac".to_owned(),
                "/music/Album — Its tags could not be read.".to_owned()
            )
        );
    }

    #[test]
    fn the_dialog_lists_every_failure() {
        if !crate::test_support::with_gtk(|| {
            let parent = gtk::Window::new();
            let failures = (0..3)
                .map(|index| ImportFailure {
                    path: PathBuf::from(format!("/music/{index}.flac")),
                    reason: ImportFailureReason::Unreadable,
                })
                .collect();
            show_import_failures(&parent, failures);
            let dialog = gtk::Window::list_toplevels()
                .into_iter()
                .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
                .find(|window| window.title().as_deref() == Some("Files Not Imported"))
                .expect("the dialog is open");
            let list = descendants(dialog.upcast_ref())
                .into_iter()
                .find_map(|widget| widget.downcast::<gtk::ListView>().ok())
                .expect("the dialog holds a list");
            assert_eq!(list.model().map(|model| model.n_items()), Some(3));
            dialog.destroy();
            parent.destroy();
        }) {
            eprintln!("SMOKE: no display, skipping");
        }
    }

    fn descendants(widget: &gtk::Widget) -> Vec<gtk::Widget> {
        let mut found = Vec::new();
        let mut child = widget.first_child();
        while let Some(current) = child {
            found.push(current.clone());
            found.extend(descendants(&current));
            child = current.next_sibling();
        }
        found
    }
}
