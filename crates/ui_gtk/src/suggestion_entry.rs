// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Free-text autocomplete for a [`gtk::Entry`].
//!
//! GTK4's `GtkEntryCompletion` is deprecated (removed in GTK5) with no
//! drop-in replacement yet, so this is the gtk4-demo "suggestion entry"
//! pattern instead: a non-autohiding [`gtk::Popover`] holding a filtered
//! [`gtk::ListView`] anchored under the entry. Crucially every widget in
//! the popover is `can_focus = false`, so focus never leaves the entry —
//! the user keeps typing while the list filters, and selection is driven
//! entirely from the entry's own key controller. That sidesteps the
//! focus-handoff fragility that keeps the inline table editor on a knife's
//! edge; here the entry stays the sole focus owner throughout.
//!
//! Used by the Get Info genre field so the user reuses an existing genre
//! spelling instead of coining a near-duplicate (#160).

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

/// Tallest the suggestion list grows before it scrolls, in rows.
const MAX_VISIBLE_ROWS: u32 = 8;
/// Approximate row height (px) used to cap the popover height.
const ROW_HEIGHT: i32 = 30;

/// Attach a case-insensitive substring autocomplete to `entry`, offering
/// the given `candidates`. A no-op when there are no candidates.
pub(crate) fn attach_suggestions(entry: &gtk::Entry, candidates: Vec<String>) {
    if candidates.is_empty() {
        return;
    }

    let model = gtk::StringList::new(&candidates.iter().map(String::as_str).collect::<Vec<_>>());

    // Match every candidate that *contains* the typed text (case-insensitive),
    // excluding the verbatim value the user already has so a complete entry
    // doesn't suggest itself.
    let entry_for_filter = entry.downgrade();
    let filter = gtk::CustomFilter::new(move |object| {
        let Some(entry) = entry_for_filter.upgrade() else {
            return false;
        };
        let needle = entry.text().to_lowercase();
        let needle = needle.trim();
        if needle.is_empty() {
            return false;
        }
        let Some(candidate) = object.downcast_ref::<gtk::StringObject>() else {
            return false;
        };
        let haystack = candidate.string().to_lowercase();
        haystack.contains(needle) && haystack != needle
    });
    let filter_model = gtk::FilterListModel::new(Some(model), Some(filter.clone()));
    let selection = gtk::SingleSelection::new(Some(filter_model.clone()));
    // No row is highlighted until the user presses Down — typing alone never
    // commits a value, it only narrows the list.
    selection.set_autoselect(false);
    selection.set_can_unselect(true);
    selection.set_selected(gtk::INVALID_LIST_POSITION);

    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let label = gtk::Label::new(None);
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let Some(item) = item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(label) = item.child().and_downcast::<gtk::Label>() else {
            return;
        };
        let text = item
            .item()
            .and_downcast::<gtk::StringObject>()
            .map(|object| object.string())
            .unwrap_or_default();
        label.set_text(&text);
    });

    let list = gtk::ListView::new(Some(selection.clone()), Some(factory));
    list.add_css_class("suggestion-list");
    list.set_single_click_activate(true);
    list.set_can_focus(false);

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_max_content_height(MAX_VISIBLE_ROWS as i32 * ROW_HEIGHT);
    scroller.set_can_focus(false);
    scroller.set_child(Some(&list));

    let popover = gtk::Popover::new();
    // Non-autohide so the popover never grabs the focus the entry needs to
    // keep accepting keystrokes; we own its show/hide lifecycle below.
    popover.set_autohide(false);
    popover.set_has_arrow(false);
    popover.set_can_focus(false);
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_halign(gtk::Align::Start);
    popover.add_css_class("suggestion-popover");
    popover.set_parent(entry);
    popover.set_child(Some(&scroller));

    // Re-filter and show/hide as the text changes. Gate the *show* on the
    // entry actually having focus so a programmatic `set_text` (navigating
    // between tracks in Get Info) never pops the list open unbidden.
    let popover_for_changed = popover.clone();
    let filter_for_changed = filter.clone();
    let filter_model_for_changed = filter_model.clone();
    let selection_for_changed = selection.clone();
    entry.connect_changed(move |entry| {
        filter_for_changed.changed(gtk::FilterChange::Different);
        selection_for_changed.set_selected(gtk::INVALID_LIST_POSITION);
        if filter_model_for_changed.n_items() > 0 && entry.has_focus() {
            if !popover_for_changed.is_visible() {
                popover_for_changed.popup();
            }
        } else {
            popover_for_changed.popdown();
        }
    });

    // Click a row → accept it. Rows are non-focusable, so the click never
    // pulls focus off the entry.
    let entry_for_activate = entry.clone();
    let popover_for_activate = popover.clone();
    list.connect_activate(move |list, position| {
        if let Some(value) = string_at(list.model().as_ref(), position) {
            apply_suggestion(&entry_for_activate, &value);
        }
        popover_for_activate.popdown();
    });

    // Drive selection and acceptance from the entry's own keys. Capture phase
    // so Down/Up/Enter/Escape are intercepted before the entry's default text
    // handling (and before the dialog's window-level Escape-to-close).
    let key = gtk::EventControllerKey::new();
    key.set_propagation_phase(gtk::PropagationPhase::Capture);
    let popover_for_key = popover.clone();
    let selection_for_key = selection.clone();
    let filter_model_for_key = filter_model.clone();
    let entry_for_key = entry.clone();
    key.connect_key_pressed(move |_, keyval, _, _| {
        let count = filter_model_for_key.n_items();
        match keyval {
            gdk::Key::Down => {
                if count == 0 {
                    return glib::Propagation::Proceed;
                }
                if !popover_for_key.is_visible() {
                    popover_for_key.popup();
                }
                let next = match selection_for_key.selected() {
                    gtk::INVALID_LIST_POSITION => 0,
                    current if current + 1 < count => current + 1,
                    current => current,
                };
                selection_for_key.set_selected(next);
                glib::Propagation::Stop
            }
            gdk::Key::Up => {
                if !popover_for_key.is_visible() || count == 0 {
                    return glib::Propagation::Proceed;
                }
                match selection_for_key.selected() {
                    gtk::INVALID_LIST_POSITION | 0 => {
                        selection_for_key.set_selected(gtk::INVALID_LIST_POSITION);
                    }
                    current => selection_for_key.set_selected(current - 1),
                }
                glib::Propagation::Stop
            }
            gdk::Key::Return | gdk::Key::KP_Enter => {
                if !popover_for_key.is_visible() {
                    return glib::Propagation::Proceed;
                }
                let selected = selection_for_key.selected();
                popover_for_key.popdown();
                if selected != gtk::INVALID_LIST_POSITION
                    && let Some(value) = string_at(Some(&selection_for_key), selected)
                {
                    apply_suggestion(&entry_for_key, &value);
                    // Swallow Enter so it doesn't also fire the dialog's OK.
                    return glib::Propagation::Stop;
                }
                // No highlighted row: closing the list is enough; let Enter
                // fall through to commit the dialog as usual.
                glib::Propagation::Proceed
            }
            gdk::Key::Escape if popover_for_key.is_visible() => {
                // Dismiss the suggestions without closing the whole dialog.
                popover_for_key.popdown();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    entry.add_controller(key);

    // Hide the suggestions when the entry loses focus (e.g. Tab to the next
    // field). Clicks inside the popover keep focus on the entry, so this does
    // not fire for them.
    let focus = gtk::EventControllerFocus::new();
    let popover_for_focus = popover.clone();
    focus.connect_leave(move |_| popover_for_focus.popdown());
    entry.add_controller(focus);

    // A popover with an explicit parent must be unparented before that parent
    // is destroyed, or GTK logs a finalize-time warning.
    let popover_for_destroy = popover.clone();
    entry.connect_destroy(move |_| popover_for_destroy.unparent());
}

/// Read the candidate string at `position` from a list model of
/// [`gtk::StringObject`]s.
fn string_at(model: Option<&impl IsA<gio::ListModel>>, position: u32) -> Option<String> {
    model?
        .item(position)
        .and_downcast::<gtk::StringObject>()
        .map(|object| object.string().to_string())
}

/// Replace the entry's text with the accepted suggestion and park the caret
/// at the end.
fn apply_suggestion(entry: &gtk::Entry, value: &str) {
    entry.set_text(value);
    entry.set_position(-1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_suggestions_constructs_without_panicking() {
        if !crate::test_support::with_gtk(|| {
            // Empty candidates: nothing is attached, and no popover is parented.
            let empty = gtk::Entry::new();
            attach_suggestions(&empty, Vec::new());

            // With candidates: attaching, then a programmatic edit (which the
            // unfocused entry must NOT pop the list open for), must not panic.
            let entry = gtk::Entry::new();
            attach_suggestions(&entry, vec!["Rock".to_owned(), "Hip-Hop".to_owned()]);
            entry.set_text("ro");
            assert!(!entry.has_focus());
        }) {
            eprintln!("SMOKE: no display, skipping");
        }
    }
}
