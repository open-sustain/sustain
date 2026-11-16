// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The sidebar collapse/expand controller: owns the toggle button, snaps the
//! GtkPaned position instantly between collapsed and the remembered expanded
//! width, and keeps the toggle icon honest across click, keyboard, and
//! drag-to-close gestures.

use super::*;
use sustain_i18n::gettext;

/// Owns the sidebar collapse / expand state, the toggle button that
/// drives it, and the last manually-set expanded width so a user who
/// drag-resized the sidebar keeps that width on re-expand.
///
/// State transitions snap the [`gtk::Paned`]'s position instantly —
/// the right-hand content column hosts views (Albums grid, track
/// table virtualisation) whose layout cost makes a continuously-
/// resizing animation visibly choppy. An instant flip is also closer
/// to the iTunes 11 sidebar toggle, which had no slide animation.
#[derive(Clone)]
pub(crate) struct SidebarCollapseController {
    inner: Rc<SidebarCollapseControllerInner>,
}

struct SidebarCollapseControllerInner {
    paned: gtk::Paned,
    toggle: gtk::Button,
    /// Last width the sidebar held while expanded. Restored on the next
    /// expand and persisted at shutdown. The `Paned`'s own `position`
    /// (`0` == collapsed) is the single source of truth for *visibility*;
    /// this only remembers where to reopen to.
    last_expanded_position: Cell<i32>,
    /// Collapsed-ness currently reflected in the toggle's icon/tooltip.
    /// A render cache derived from `position` on every change — never an
    /// independent state — so per-pixel drag-resizes don't rebuild the
    /// icon needlessly.
    toggle_shows_collapsed: Cell<bool>,
}

impl SidebarCollapseController {
    pub(super) fn new(
        paned: gtk::Paned,
        initial_collapsed: bool,
        initial_width: Option<u32>,
    ) -> Self {
        let toggle = gtk::Button::new();
        toggle.add_css_class("flat");
        toggle.add_css_class("sidebar-collapse-toggle");
        toggle.set_focus_on_click(false);
        toggle.set_can_focus(false);

        // Clamp the persisted width back into the legal band. The
        // domain stores whatever the user last set; the UI is the
        // authority on min/max, so out-of-band values are silently
        // pulled into range rather than rejected.
        let expanded_width = initial_width
            .map(|width| (width as i32).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH))
            .unwrap_or(SIDEBAR_DEFAULT_WIDTH);

        let inner = Rc::new(SidebarCollapseControllerInner {
            paned: paned.clone(),
            toggle: toggle.clone(),
            last_expanded_position: Cell::new(expanded_width),
            toggle_shows_collapsed: Cell::new(initial_collapsed),
        });

        // Apply the persisted collapsed state.
        if initial_collapsed {
            inner.paned.set_position(0);
        } else {
            inner.paned.set_position(expanded_width);
        }
        sync_collapse_toggle_icon(&toggle, initial_collapsed);

        // The `Paned` position is the single source of truth for sidebar
        // visibility, so every change funnels through here — drag-to-zero,
        // toggle click, keyboard shortcut, restore-from-settings. This is
        // what keeps the toggle's icon honest after a drag-to-close
        // gesture (issue #56), and tracks the chosen width for the next
        // expand.
        let inner_for_position = inner.clone();
        inner.paned.connect_position_notify(move |content_area| {
            let position = content_area.position();
            if position > 0 {
                inner_for_position.last_expanded_position.set(position);
            }
            let collapsed = position == 0;
            if inner_for_position.toggle_shows_collapsed.replace(collapsed) != collapsed {
                sync_collapse_toggle_icon(&inner_for_position.toggle, collapsed);
            }
        });

        let inner_for_click = inner.clone();
        toggle.connect_clicked(move |_| {
            let controller = SidebarCollapseController {
                inner: inner_for_click.clone(),
            };
            controller.toggle();
        });

        Self { inner }
    }

    pub(super) fn toggle_widget(&self) -> gtk::Button {
        self.inner.toggle.clone()
    }

    pub(super) fn is_collapsed(&self) -> bool {
        self.inner.paned.position() == 0
    }

    /// The last manually-set expanded width, in pixels. Used at
    /// shutdown to persist the user's preferred sidebar width. Always
    /// the expanded width — collapsing does not zero this out, so
    /// re-expanding restores the same value on next launch.
    pub(super) fn expanded_width(&self) -> u32 {
        self.inner.last_expanded_position.get().max(0) as u32
    }

    /// No-op when the sidebar is already visible. Used by shortcuts
    /// that need the sidebar on-screen for their UI affordance to be
    /// visible (e.g. Ctrl+N's armed inline rename of a new playlist
    /// row).
    pub(crate) fn expand_if_collapsed(&self) {
        if self.is_collapsed() {
            self.toggle();
        }
    }

    fn toggle(&self) {
        // Move the splitter; the position-notify handler installed in
        // `new` repaints the toggle icon and records the expanded width,
        // so collapse-by-click and collapse-by-drag stay in lockstep.
        let target = if self.is_collapsed() {
            self.inner
                .last_expanded_position
                .get()
                .max(SIDEBAR_MIN_WIDTH)
        } else {
            0
        };
        self.inner.paned.set_position(target);
    }
}

/// Repaint the toggle's icon and tooltip to advertise the action the
/// next click performs.
///
/// - When the sidebar is visible, the click collapses it — show a
///   left-pointing arrow ("Collapse sidebar").
/// - When the sidebar is hidden, the click brings it back — show a
///   right-pointing arrow ("Show sidebar").
fn sync_collapse_toggle_icon(button: &gtk::Button, collapsed: bool) {
    let (icon_name, tooltip) = if collapsed {
        ("go-next-symbolic", gettext("Show sidebar"))
    } else {
        ("go-previous-symbolic", gettext("Collapse sidebar"))
    };
    button.set_icon_name(icon_name);
    button.set_tooltip_text(Some(&tooltip));
}
