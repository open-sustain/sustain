// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The pages of the main content column, one per mode, of which only the
//! page on show is visible.
//!
//! A `gtk::Stack` keeps its other pages visible, only unmapped, and their
//! style still counts. That matters because GTK restyles a window's every
//! visible node each time the pointer crosses from one widget to another,
//! as it does on every wheel step over a list, whose rows move under the
//! pointer. A table page off screen still holds its couple of hundred
//! realized rows, thousands of nodes, and made each of those steps restyle
//! them all. A hidden page is left out until it is shown again.

use gtk::glib;
use gtk::prelude::*;

use super::{
    ALBUMS_VIEW, CD_IMPORT_VIEW, DEVICES_VIEW, DUPLICATES_VIEW, PLAYLISTS_VIEW, SONGS_VIEW,
    STATISTICS_VIEW,
};

#[derive(Clone, Debug)]
pub(crate) struct ContentStack {
    stack: gtk::Stack,
}

impl ContentStack {
    pub(crate) fn new(
        songs_view: &impl IsA<gtk::Widget>,
        albums_view: &impl IsA<gtk::Widget>,
        duplicates_view: &impl IsA<gtk::Widget>,
        statistics_view: &impl IsA<gtk::Widget>,
        playlists_view: &impl IsA<gtk::Widget>,
        devices_view: &impl IsA<gtk::Widget>,
        cd_import_view: &impl IsA<gtk::Widget>,
    ) -> Self {
        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        // Hidden pages must not contribute to the window's minimum size. The
        // Albums page can legitimately have a wider temporary natural width
        // while it is reflowing between column counts.
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);

        for (page, name) in [
            (songs_view.upcast_ref::<gtk::Widget>(), SONGS_VIEW),
            (albums_view.upcast_ref(), ALBUMS_VIEW),
            (duplicates_view.upcast_ref(), DUPLICATES_VIEW),
            (statistics_view.upcast_ref(), STATISTICS_VIEW),
            (playlists_view.upcast_ref(), PLAYLISTS_VIEW),
            (devices_view.upcast_ref(), DEVICES_VIEW),
            (cd_import_view.upcast_ref(), CD_IMPORT_VIEW),
        ] {
            // Hidden before it is added, so the stack never switches to it
            // on its own.
            page.set_visible(name == SONGS_VIEW);
            stack.add_named(page, Some(name));
        }

        Self { stack }
    }

    /// A content stack whose pages other than Songs are empty.
    #[cfg(test)]
    pub(crate) fn with_songs_view(songs_view: &impl IsA<gtk::Widget>) -> Self {
        let empty = || gtk::Box::new(gtk::Orientation::Vertical, 0);
        Self::new(
            songs_view,
            &empty(),
            &empty(),
            &empty(),
            &empty(),
            &empty(),
            &empty(),
        )
    }

    /// The stack, to place in the window.
    pub(crate) fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    /// Show the page named `name` and hide the one it replaces.
    pub(crate) fn set_visible_child_name(&self, name: &str) {
        let Some(page) = self.stack.child_by_name(name) else {
            return;
        };
        let previous = self.stack.visible_child();
        if previous.as_ref() == Some(&page) {
            return;
        }
        // The stack only switches to a visible page.
        page.set_visible(true);
        self.stack.set_visible_child(&page);
        if let Some(previous) = previous {
            previous.set_visible(false);
        }
    }

    pub(crate) fn visible_child_name(&self) -> Option<glib::GString> {
        self.stack.visible_child_name()
    }

    /// Call `f` each time another page is shown.
    pub(crate) fn connect_visible_child_name_notify(
        &self,
        f: impl Fn(&Self) + 'static,
    ) -> glib::SignalHandlerId {
        // The handler gets its own handle on the stack, so the stack does
        // not hold one to itself through it.
        self.stack.connect_visible_child_name_notify(move |stack| {
            f(&Self {
                stack: stack.clone(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages() -> Vec<gtk::Box> {
        (0..7)
            .map(|_| gtk::Box::new(gtk::Orientation::Vertical, 0))
            .collect()
    }

    fn content_stack(pages: &[gtk::Box]) -> ContentStack {
        ContentStack::new(
            &pages[0], &pages[1], &pages[2], &pages[3], &pages[4], &pages[5], &pages[6],
        )
    }

    fn visible_pages(pages: &[gtk::Box]) -> Vec<usize> {
        (0..pages.len())
            .filter(|&index| pages[index].is_visible())
            .collect()
    }

    #[test]
    fn only_the_page_on_show_is_visible() {
        crate::test_support::with_gtk(|| {
            let pages = pages();
            let stack = content_stack(&pages);
            assert_eq!(stack.visible_child_name().as_deref(), Some(SONGS_VIEW));
            assert_eq!(visible_pages(&pages), [0]);

            stack.set_visible_child_name(DEVICES_VIEW);
            assert_eq!(stack.visible_child_name().as_deref(), Some(DEVICES_VIEW));
            assert_eq!(visible_pages(&pages), [5]);

            stack.set_visible_child_name(SONGS_VIEW);
            assert_eq!(stack.visible_child_name().as_deref(), Some(SONGS_VIEW));
            assert_eq!(visible_pages(&pages), [0]);
        });
    }

    #[test]
    fn a_switch_notifies_once_the_new_page_is_on_show() {
        crate::test_support::with_gtk(|| {
            let pages = pages();
            let stack = content_stack(&pages);
            let shown = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
            {
                let shown = shown.clone();
                stack.connect_visible_child_name_notify(move |stack| {
                    shown
                        .borrow_mut()
                        .push(stack.visible_child_name().map(|name| name.to_string()));
                });
            }

            stack.set_visible_child_name(ALBUMS_VIEW);
            stack.set_visible_child_name(ALBUMS_VIEW);

            assert_eq!(*shown.borrow(), [Some(ALBUMS_VIEW.to_owned())]);
        });
    }
}
