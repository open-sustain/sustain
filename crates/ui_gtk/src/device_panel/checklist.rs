// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The scrolling check lists of a device's sync choices: its playlists and
//! its artists.
//!
//! A large library has thousands of artists, so the list builds rows only
//! for the entries on screen and reuses them as it scrolls. A row reads
//! its entry by position, so two entries may share a name. The ticked
//! entries live in the list's own set rather than in its rows, which come
//! and go; a row's check follows the set when the row is bound.

use std::{cell::RefCell, collections::HashSet, hash::Hash, rc::Rc};

use gtk::prelude::*;

/// One entry of a check list: what ticking it selects, and how it reads.
pub(super) struct ChecklistEntry<K> {
    pub(super) key: K,
    pub(super) name: String,
    pub(super) icon: &'static str,
}

struct Checklist<K> {
    entries: Vec<ChecklistEntry<K>>,
    ticked: RefCell<HashSet<K>>,
    on_change: Box<dyn Fn(Vec<K>)>,
}

impl<K: Clone + Eq + Hash> Checklist<K> {
    /// Tick or untick the entry at `position`; returns whether that
    /// changed the selection. Binding a row sets its check to the
    /// selection already held, which changes nothing.
    fn set_ticked(&self, position: u32, ticked: bool) -> bool {
        let Some(entry) = usize::try_from(position)
            .ok()
            .and_then(|position| self.entries.get(position))
        else {
            return false;
        };
        let mut selection = self.ticked.borrow_mut();
        if ticked {
            selection.insert(entry.key.clone())
        } else {
            selection.remove(&entry.key)
        }
    }

    /// The ticked keys, in list order.
    fn selection(&self) -> Vec<K> {
        let ticked = self.ticked.borrow();
        self.entries
            .iter()
            .filter(|entry| ticked.contains(&entry.key))
            .map(|entry| entry.key.clone())
            .collect()
    }
}

/// A scrolling list with one check row per entry, ticked for the keys in
/// `ticked`. Each tick or untick hands the new selection, in list order,
/// to `on_change`.
pub(super) fn checklist<K: Clone + Eq + Hash + 'static>(
    entries: Vec<ChecklistEntry<K>>,
    ticked: HashSet<K>,
    on_change: impl Fn(Vec<K>) + 'static,
) -> gtk::ScrolledWindow {
    let names = entries
        .iter()
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    let model = gtk::StringList::new(&names);
    let checklist = Rc::new(Checklist {
        entries,
        ticked: RefCell::new(ticked),
        on_change: Box::new(on_change),
    });

    let factory = gtk::SignalListItemFactory::new();
    {
        let checklist = checklist.clone();
        factory.connect_setup(move |_, list_item| {
            let Some(list_item) = list_item.downcast_ref::<gtk::ListItem>() else {
                return;
            };
            // The check button takes the focus and the clicks; the row
            // around it is only a slot in the list.
            list_item.set_focusable(false);
            list_item.set_activatable(false);
            list_item.set_child(Some(&check_row(list_item, checklist.clone())));
        });
    }
    factory.connect_bind(move |_, list_item| {
        let Some(list_item) = list_item.downcast_ref::<gtk::ListItem>() else {
            return;
        };
        let Some(entry) = usize::try_from(list_item.position())
            .ok()
            .and_then(|position| checklist.entries.get(position))
        else {
            return;
        };
        let Some(check) = list_item.child().and_downcast::<gtk::CheckButton>() else {
            return;
        };
        let icon = check
            .child()
            .and_then(|row| row.first_child())
            .and_downcast::<gtk::Image>();
        let label = icon
            .as_ref()
            .and_then(|icon| icon.next_sibling())
            .and_downcast::<gtk::Label>();
        if let (Some(icon), Some(label)) = (icon, label) {
            icon.set_icon_name(Some(entry.icon));
            label.set_label(&entry.name);
        }
        // Read before setting: the check's toggle handler borrows the
        // selection again.
        let ticked = checklist.ticked.borrow().contains(&entry.key);
        check.set_active(ticked);
    });

    let list = gtk::ListView::new(Some(gtk::NoSelection::new(Some(model))), Some(factory));
    list.add_css_class("device-sync-checklist");

    let scroller = gtk::ScrolledWindow::new();
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_vexpand(true);
    scroller.set_child(Some(&list));
    scroller
}

/// A check button whose child is the entry's icon followed by its name;
/// toggling it ticks or unticks the entry its row is bound to.
fn check_row<K: Clone + Eq + Hash + 'static>(
    list_item: &gtk::ListItem,
    checklist: Rc<Checklist<K>>,
) -> gtk::CheckButton {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    row.append(&gtk::Image::new());
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.append(&label);

    let check = gtk::CheckButton::new();
    check.set_child(Some(&row));
    let list_item = list_item.downgrade();
    check.connect_toggled(move |check| {
        let Some(list_item) = list_item.upgrade() else {
            return;
        };
        if checklist.set_ticked(list_item.position(), check.is_active()) {
            (checklist.on_change)(checklist.selection());
        }
    });
    check
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::time::{Duration, Instant};

    use super::*;

    fn entries(names: &[&str]) -> Vec<ChecklistEntry<usize>> {
        names
            .iter()
            .enumerate()
            .map(|(key, name)| ChecklistEntry {
                key,
                name: (*name).to_owned(),
                icon: "avatar-default-symbolic",
            })
            .collect()
    }

    fn state(names: &[&str], ticked: &[usize]) -> Checklist<usize> {
        Checklist {
            entries: entries(names),
            ticked: RefCell::new(ticked.iter().copied().collect()),
            on_change: Box::new(|_| {}),
        }
    }

    #[test]
    fn ticking_reports_the_selection_in_list_order() {
        let checklist = state(&["A", "B", "C"], &[2]);

        assert!(checklist.set_ticked(0, true));
        assert_eq!(checklist.selection(), vec![0, 2]);
        assert!(checklist.set_ticked(2, false));
        assert_eq!(checklist.selection(), vec![0]);
    }

    #[test]
    fn a_row_bound_to_the_selection_it_shows_changes_nothing() {
        let checklist = state(&["A", "B"], &[1]);

        assert!(!checklist.set_ticked(1, true));
        assert!(!checklist.set_ticked(0, false));
        assert!(!checklist.set_ticked(gtk::INVALID_LIST_POSITION, true));
        assert_eq!(checklist.selection(), vec![1]);
    }

    #[test]
    fn entries_sharing_a_name_tick_apart() {
        let checklist = state(&["Mix", "Mix"], &[]);

        assert!(checklist.set_ticked(1, true));
        assert_eq!(checklist.selection(), vec![1]);
    }

    fn check_buttons(widget: &gtk::Widget, found: &mut Vec<gtk::CheckButton>) {
        if let Some(check) = widget.downcast_ref::<gtk::CheckButton>() {
            found.push(check.clone());
            return;
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            check_buttons(&widget, found);
            child = widget.next_sibling();
        }
    }

    #[test]
    fn a_long_list_builds_only_the_rows_on_screen() {
        let ran = crate::test_support::with_gtk(|| {
            let names = (0..5_000)
                .map(|index| format!("Artist {index}"))
                .collect::<Vec<_>>();
            let names = names.iter().map(String::as_str).collect::<Vec<_>>();
            let reported = Rc::new(RefCell::new(Vec::new()));
            let changes = Rc::new(Cell::new(0));
            let scroller = {
                let reported = reported.clone();
                let changes = changes.clone();
                checklist(entries(&names), HashSet::from([3]), move |selection| {
                    changes.set(changes.get() + 1);
                    *reported.borrow_mut() = selection;
                })
            };
            let window = gtk::Window::new();
            window.set_default_size(300, 300);
            window.set_child(Some(&scroller));
            window.present();

            let context = gtk::glib::MainContext::default();
            let mut rows = Vec::new();
            let deadline = Instant::now() + Duration::from_secs(5);
            while rows.is_empty() && Instant::now() < deadline {
                context.iteration(false);
                rows.clear();
                check_buttons(scroller.upcast_ref(), &mut rows);
            }

            assert!(!rows.is_empty(), "the visible rows are built");
            assert!(
                rows.len() < 500,
                "{} rows built for 5,000 entries",
                rows.len()
            );
            assert!(rows[3].is_active(), "a ticked entry shows ticked");
            assert_eq!(changes.get(), 0, "showing the selection changes nothing");

            rows[0].set_active(true);
            assert_eq!(*reported.borrow(), vec![0, 3]);
            window.destroy();
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
