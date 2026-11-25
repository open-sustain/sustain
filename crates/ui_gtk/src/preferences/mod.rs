// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cell::RefCell;
use std::path::PathBuf;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use sustain_i18n::{gettext, pgettext};

use super::{
    PREFERENCES_WIDTH, WINDOW_SHADOW_MARGIN, command_controller::SharedCommandController,
    library_consolidation::LibraryConsolidationRequestedCallback,
    library_scan::LibraryScanRequestedCallback, shortcuts::register_preferences_accelerator,
};

mod about_tab;
pub(crate) use about_tab::open_documentation;
mod analysis_tab;
mod encoding_tab;
mod library_tab;
mod online_tab;
mod setting_scale;
mod shuffle_tab;
mod switch_row;
mod views_tab;

thread_local! {
    /// The open Preferences window, if any. Preferences is one window:
    /// its tabs install the runtime's single-slot observers (library task,
    /// Smart Shuffle state), so a second window would take them over from
    /// the first. Cleared when the window closes.
    static OPEN_PREFERENCES: RefCell<Option<OpenPreferences>> = const { RefCell::new(None) };
}

/// The open Preferences window and the tab strip that switches its pages.
#[derive(Clone)]
struct OpenPreferences {
    window: gtk::Window,
    tabs: PreferencesTabs,
}

/// The grouped tab buttons, by the page each shows.
#[derive(Clone)]
struct PreferencesTabs {
    buttons: Vec<(&'static str, gtk::ToggleButton)>,
}

impl PreferencesTabs {
    /// Show the page named `tab`, or Library for a name that is not a
    /// tab. Activating the grouped toggle fires its handler, which sets
    /// the visible stack page.
    fn show(&self, tab: &str) {
        let button = self
            .buttons
            .iter()
            .find(|(name, _)| *name == tab)
            .or_else(|| self.buttons.iter().find(|(name, _)| *name == TAB_LIBRARY));
        if let Some((_, button)) = button {
            button.set_active(true);
        }
    }
}

/// Re-applies the Views preferences to the live UI (sidebar rows, Now
/// Playing chips) the moment a toggle changes, so the user doesn't have
/// to relaunch. Created in the main window where those widgets live and
/// threaded down to the Views tab.
pub(crate) type ViewSettingsChangedCallback = std::rc::Rc<dyn Fn()>;

/// Re-files the library lists the moment the sort-tag preference
/// changes, so the switch shows its effect while Preferences is open.
pub(crate) type SortTagsChangedCallback = std::rc::Rc<dyn Fn()>;

/// What Preferences asks of the main window, where the widgets a
/// preference acts on live.
#[derive(Clone)]
pub(crate) struct PreferencesHooks {
    pub(crate) scan_requested: LibraryScanRequestedCallback,
    pub(crate) consolidation_requested: LibraryConsolidationRequestedCallback,
    pub(crate) view_settings_changed: ViewSettingsChangedCallback,
    pub(crate) sort_tags_changed: SortTagsChangedCallback,
}

/// Pixel size of the icon stacked above each tab label. The strip is
/// intentionally taller than the GTK default chrome — matching the
/// integrated top bar convention noted in CLAUDE.md and giving the
/// dominant icon room to breathe.
const TAB_ICON_SIZE: i32 = 32;

/// Wrap-width budget (in average character widths) for muted helper text
/// and any wrapping label inside the Preferences window. Picked so the
/// helper text wraps within the pinned `PREFERENCES_WIDTH` panel rather
/// than pushing the window wider — labels with `wrap = true` request
/// their natural width otherwise, and the stack's `hhomogeneous = true`
/// would propagate that to every tab.
pub(super) const HELPER_MAX_WIDTH_CHARS: i32 = 56;

/// Baseline width-in-characters target for wrapping labels. Required
/// alongside `max_width_chars`: GTK4's `Label::max-width-chars` only
/// bounds the *natural* width request once `width-chars` is set,
/// otherwise it behaves as a ceiling for ellipsize-mode only and the
/// label requests its full unwrapped single-line width — silently
/// widening the Stack page and the whole window. A low value (10) lets
/// the label minimum stay tiny while the ceiling kicks in for wrapping.
pub(super) const HELPER_MIN_WIDTH_CHARS: i32 = 10;

/// Semantic visual width of the library-path entry, in average character
/// widths. The entry remains editable for arbitrarily long paths —
/// `gtk::Text` scrolls horizontally inside the field — but the field's
/// natural-width request is bounded to this many chars, so a long
/// path does not push the Library page (and therefore the window)
/// wider than the panel.
pub(super) const PATH_ENTRY_VISIBLE_CHARS: i32 = 36;

const TAB_LIBRARY: &str = "library";
const TAB_ENCODING: &str = "encoding";
const TAB_ANALYSIS: &str = "analysis";
const TAB_ONLINE: &str = "online";
const TAB_SHUFFLE: &str = "shuffle";
const TAB_VIEWS: &str = "views";
const TAB_ABOUT: &str = "about";

pub(crate) fn install_preferences_action(
    app: &gtk::Application,
    window: &gtk::ApplicationWindow,
    command_controller: SharedCommandController,
    database_path: PathBuf,
    hooks: PreferencesHooks,
) {
    if app.lookup_action("preferences").is_some() {
        return;
    }

    // The action carries the tab to land on as a string target, so any
    // widget can open Preferences at a specific page (e.g. the CD page's
    // "Encoding Settings" button → the Encoding tab). Ctrl+, lands on the
    // Library tab.
    let preferences = gio::SimpleAction::new("preferences", Some(glib::VariantTy::STRING));
    let window = window.clone();
    let command_controller = command_controller.clone();
    preferences.connect_activate(move |_action, parameter| {
        let initial_tab = parameter
            .and_then(glib::Variant::str)
            .filter(|tab| TABS.contains(tab))
            .unwrap_or(TAB_LIBRARY);
        open_preferences_window(
            &window,
            command_controller.clone(),
            database_path.clone(),
            hooks.clone(),
            initial_tab,
        );
    });
    app.add_action(&preferences);
    register_preferences_accelerator(app);
}

/// Every valid Preferences tab id, so an action target naming an unknown tab
/// falls back to the Library landing rather than a blank stack page.
const TABS: [&str; 7] = [
    TAB_LIBRARY,
    TAB_ENCODING,
    TAB_ANALYSIS,
    TAB_ONLINE,
    TAB_SHUFFLE,
    TAB_VIEWS,
    TAB_ABOUT,
];

/// Build the status bar's "cog + Settings" entry.
///
/// Lives in the status bar's bottom-left cluster, to the right of the
/// sidebar collapse toggle (#164). Icon-plus-label so a user with no
/// iTunes muscle memory can find Preferences without hunting; power
/// users still have the Ctrl+, accelerator registered by
/// [`install_preferences_action`]. Flat chrome — no border or button
/// background until `:hover` / `:active` — so it reads as status-bar
/// furniture rather than a primary action.
pub(crate) fn settings_button(
    window: &gtk::ApplicationWindow,
    command_controller: SharedCommandController,
    database_path: PathBuf,
    hooks: PreferencesHooks,
) -> gtk::Button {
    let icon = gtk::Image::from_icon_name("preferences-system-symbolic");
    icon.set_pixel_size(14);
    icon.add_css_class("status-bar-settings-icon");

    let label = gtk::Label::new(Some(&gettext("Settings")));
    label.set_xalign(0.0);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    content.append(&icon);
    content.append(&label);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("status-bar-settings-button");
    button.set_child(Some(&content));
    button.set_tooltip_text(Some(&gettext("Preferences (Ctrl+,)")));

    let window = window.clone();
    let command_controller = command_controller.clone();
    button.connect_clicked(move |_| {
        open_preferences_window(
            &window,
            command_controller.clone(),
            database_path.clone(),
            hooks.clone(),
            TAB_LIBRARY,
        );
    });

    button
}

fn open_preferences_window(
    parent: &impl IsA<gtk::Window>,
    command_controller: SharedCommandController,
    database_path: PathBuf,
    hooks: PreferencesHooks,
    initial_tab: &str,
) {
    // Another request while Preferences is open (it is modal, so only the
    // exported action can make one) brings the open window forward, on
    // the tab it asks for.
    if let Some(open) = OPEN_PREFERENCES.with_borrow(Clone::clone) {
        open.tabs.show(initial_tab);
        open.window.present();
        return;
    }
    let window = gtk::Window::builder()
        .title(gettext("Preferences"))
        .decorated(false)
        .transient_for(parent)
        .modal(true)
        .default_width(PREFERENCES_WIDTH + WINDOW_SHADOW_MARGIN * 2)
        .resizable(false)
        .build();
    window.add_css_class("app-window");

    let frame = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame.add_css_class("preferences-frame");
    frame.set_hexpand(true);
    frame.set_vexpand(true);
    frame.set_margin_top(WINDOW_SHADOW_MARGIN);
    frame.set_margin_end(WINDOW_SHADOW_MARGIN);
    frame.set_margin_bottom(WINDOW_SHADOW_MARGIN);
    frame.set_margin_start(WINDOW_SHADOW_MARGIN);
    // Width is pinned here, and this floor is the SOLE reason the chrome
    // stays a stable width across tab switches: every page is built to fit
    // within `PREFERENCES_WIDTH`, so this minimum dominates them all. The
    // stack is intentionally not `hhomogeneous` (see the warning below it),
    // so do not weaken or remove this floor without restoring some other
    // width pin. Height is free (`-1`) so the window auto-sizes to the
    // visible tab, per the issue #17 spec.
    frame.set_size_request(PREFERENCES_WIDTH, -1);

    let panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
    panel.add_css_class("preferences-panel");
    panel.set_hexpand(true);
    panel.set_vexpand(true);
    panel.set_overflow(gtk::Overflow::Hidden);

    let stack = gtk::Stack::new();
    stack.add_css_class("preferences-stack");
    // Both dimensions are deliberately NON-homogeneous. Variable height
    // (`vhomogeneous = false`) lets the window snap to the visible page's
    // natural height (issue #17); stable width comes from the frame's
    // `PREFERENCES_WIDTH` floor above, not from the stack.
    //
    // ⚠ DO NOT set `hhomogeneous = true`. Combined with
    // `vhomogeneous = false` it drives a GTK4 `Stack` width-for-height
    // feedback that balloons this `resizable(false)` window to the full
    // monitor width when a page far shorter than the others (About)
    // becomes visible — but ONLY on a fractional-scaled (HiDPI) Wayland
    // surface. It is invisible at 1x, so `cargo test` and headless CI will
    // never catch a regression here: any change to the stack/frame sizing
    // must be checked on a real HiDPI display. See #53.
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);
    stack.set_transition_type(gtk::StackTransitionType::None);

    let library_page = library_tab::build(
        window.upcast_ref(),
        command_controller.clone(),
        hooks.scan_requested,
        hooks.consolidation_requested,
        hooks.sort_tags_changed,
    );
    stack.add_named(&library_page, Some(TAB_LIBRARY));

    let encoding_page = encoding_tab::build(command_controller.clone());
    stack.add_named(&encoding_page, Some(TAB_ENCODING));

    let analysis_page = analysis_tab::build(command_controller.clone());
    stack.add_named(&analysis_page, Some(TAB_ANALYSIS));

    let online_page = online_tab::build(command_controller.clone());
    stack.add_named(&online_page, Some(TAB_ONLINE));

    let shuffle_page = shuffle_tab::build(window.upcast_ref(), command_controller.clone());
    stack.add_named(&shuffle_page, Some(TAB_SHUFFLE));

    let views_page = views_tab::build(command_controller, hooks.view_settings_changed);
    stack.add_named(&views_page, Some(TAB_VIEWS));

    let about_page = about_tab::build(&database_path);
    stack.add_named(&about_page, Some(TAB_ABOUT));

    let (tab_strip, tabs) = build_tab_strip(&stack, &window);
    tabs.show(initial_tab);

    panel.append(&tab_strip);
    panel.append(&stack);
    frame.append(&panel);
    window.set_child(Some(&frame));

    // The controller belongs to the window and reaches it through itself,
    // not a capture (#370).
    let key_controller = gtk::EventControllerKey::new();
    key_controller.connect_key_pressed(move |controller, key, _keycode, _state| {
        if key != gdk::Key::Escape {
            return glib::Propagation::Proceed;
        }
        if let Some(window) = controller.widget().and_downcast::<gtk::Window>() {
            window.close();
        }
        glib::Propagation::Stop
    });
    window.add_controller(key_controller);

    OPEN_PREFERENCES.set(Some(OpenPreferences {
        window: window.clone(),
        tabs,
    }));
    window.connect_close_request(|_| {
        OPEN_PREFERENCES.set(None);
        glib::Propagation::Proceed
    });

    window.present();
    focus_visible_preferences_page(&stack);
}

/// Builds the headerless drag surface that combines the three icon-above-label
/// tab buttons and the close button on the trailing end.
fn build_tab_strip(stack: &gtk::Stack, window: &gtk::Window) -> (gtk::Widget, PreferencesTabs) {
    let library_button = build_tab_button("folder-music-symbolic", &gettext("Library"));
    let encoding_button = build_tab_button("media-optical-symbolic", &gettext("Encoding"));
    let analysis_button = build_tab_button("applications-science-symbolic", &gettext("Analysis"));
    let online_button = build_tab_button("network-transmit-receive-symbolic", &gettext("Online"));
    let shuffle_button = build_tab_button(
        "media-playlist-shuffle-symbolic",
        &pgettext("preferences tab", "Shuffle"),
    );
    let views_button = build_tab_button("view-list-symbolic", &gettext("Views"));
    let about_button = build_tab_button("help-about-symbolic", &gettext("About"));

    // Group them all so exactly one is active at a time.
    encoding_button.set_group(Some(&library_button));
    analysis_button.set_group(Some(&library_button));
    online_button.set_group(Some(&library_button));
    shuffle_button.set_group(Some(&library_button));
    views_button.set_group(Some(&library_button));
    about_button.set_group(Some(&library_button));

    wire_tab_button(&library_button, stack, TAB_LIBRARY);
    wire_tab_button(&encoding_button, stack, TAB_ENCODING);
    wire_tab_button(&analysis_button, stack, TAB_ANALYSIS);
    wire_tab_button(&online_button, stack, TAB_ONLINE);
    wire_tab_button(&shuffle_button, stack, TAB_SHUFFLE);
    wire_tab_button(&views_button, stack, TAB_VIEWS);
    wire_tab_button(&about_button, stack, TAB_ABOUT);

    let tab_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab_box.add_css_class("preferences-tab-buttons");
    tab_box.set_halign(gtk::Align::Center);
    tab_box.append(&library_button);
    tab_box.append(&encoding_button);
    tab_box.append(&analysis_button);
    tab_box.append(&online_button);
    tab_box.append(&shuffle_button);
    tab_box.append(&views_button);
    tab_box.append(&about_button);

    let close_icon = gtk::Image::from_icon_name("window-close-symbolic");
    close_icon.set_pixel_size(14);

    let close_button = gtk::Button::new();
    close_button.add_css_class("flat");
    close_button.add_css_class("preference-close-button");
    close_button.set_child(Some(&close_icon));
    close_button.set_tooltip_text(Some(&gettext("Close")));
    close_button.set_valign(gtk::Align::Center);
    close_button.set_margin_end(8);

    let window_for_close = window.downgrade();
    close_button.connect_clicked(move |_| {
        if let Some(window) = window_for_close.upgrade() {
            window.close();
        }
    });

    let strip = gtk::CenterBox::new();
    // `titlebar` is the same built-in GTK class the main window's top bar
    // uses (`Titlebar::widget`), so the strip's background follows the
    // theme's headerbar shade in both light and dark mode without us
    // hard-coding a colour.
    strip.add_css_class("titlebar");
    strip.add_css_class("preferences-tab-strip");
    strip.set_center_widget(Some(&tab_box));
    strip.set_end_widget(Some(&close_button));

    // Wrap in a WindowHandle so clicks on empty regions drag the window.
    // Clicks on the tab buttons and close button are consumed by those
    // widgets first, so they continue to work normally.
    let handle = gtk::WindowHandle::new();
    handle.set_child(Some(&strip));
    let tabs = PreferencesTabs {
        buttons: vec![
            (TAB_LIBRARY, library_button),
            (TAB_ENCODING, encoding_button),
            (TAB_ANALYSIS, analysis_button),
            (TAB_ONLINE, online_button),
            (TAB_SHUFFLE, shuffle_button),
            (TAB_VIEWS, views_button),
            (TAB_ABOUT, about_button),
        ],
    };
    (handle.upcast(), tabs)
}

fn build_tab_button(icon_name: &str, label_text: &str) -> gtk::ToggleButton {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(TAB_ICON_SIZE);
    icon.add_css_class("preferences-tab-icon");

    let label = gtk::Label::new(Some(label_text));
    label.add_css_class("preferences-tab-label");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.set_halign(gtk::Align::Center);
    content.append(&icon);
    content.append(&label);

    let button = gtk::ToggleButton::new();
    button.add_css_class("flat");
    button.add_css_class("preferences-tab-button");
    button.set_focus_on_click(false);
    button.set_child(Some(&content));
    button
}

fn focus_visible_preferences_page(stack: &gtk::Stack) {
    // Weak: a window closed before the idle runs has released its stack,
    // and there is nothing left to focus.
    let stack = stack.downgrade();
    glib::idle_add_local_once(move || {
        if let Some(page) = stack.upgrade().and_then(|stack| stack.visible_child()) {
            let _focused = page.child_focus(gtk::DirectionType::TabForward);
        }
    });
}

fn wire_tab_button(button: &gtk::ToggleButton, stack: &gtk::Stack, page_name: &'static str) {
    let stack = stack.downgrade();
    button.connect_toggled(move |btn| {
        // Only the newly-activated radio fires this callback with
        // `is_active() == true`; the previously-active one fires with
        // `false` and we ignore it so we don't switch pages twice.
        if btn.is_active()
            && let Some(stack) = stack.upgrade()
        {
            stack.set_visible_child_name(page_name);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::rc::Rc;

    use sustain_app_runtime::ApplicationRuntime;

    use super::*;
    use crate::command_controller::UiCommandController;

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

    fn open_windows_titled(title: &str) -> Vec<gtk::Window> {
        gtk::Window::list_toplevels()
            .into_iter()
            .filter_map(|widget| widget.downcast::<gtk::Window>().ok())
            .filter(|window| window.is_visible() && window.title().as_deref() == Some(title))
            .collect()
    }

    #[test]
    fn the_closed_preferences_window_is_released() {
        crate::test_support::with_gtk(|| {
            let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let command_controller: SharedCommandController =
                Rc::new(UiCommandController::new(runtime));
            let hooks = PreferencesHooks {
                scan_requested: Rc::new(|_| Ok(())),
                consolidation_requested: Rc::new(|| Ok(())),
                view_settings_changed: Rc::new(|| {}),
                sort_tags_changed: Rc::new(|| {}),
            };
            let parent = gtk::Window::new();
            open_preferences_window(
                &parent,
                command_controller,
                PathBuf::from("/nonexistent/library.sqlite3"),
                hooks,
                TAB_LIBRARY,
            );
            crate::test_support::assert_closes_and_disposes("Preferences");
            parent.destroy();
        });
    }

    #[test]
    fn a_second_request_brings_the_open_preferences_window_forward() {
        crate::test_support::with_gtk(|| {
            let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let command_controller: SharedCommandController =
                Rc::new(UiCommandController::new(runtime));
            let hooks = PreferencesHooks {
                scan_requested: Rc::new(|_| Ok(())),
                consolidation_requested: Rc::new(|| Ok(())),
                view_settings_changed: Rc::new(|| {}),
                sort_tags_changed: Rc::new(|| {}),
            };
            let parent = gtk::Window::new();
            let open = |tab| {
                open_preferences_window(
                    &parent,
                    command_controller.clone(),
                    PathBuf::from("/nonexistent/library.sqlite3"),
                    hooks.clone(),
                    tab,
                );
            };

            open(TAB_LIBRARY);
            open(TAB_ENCODING);
            let windows = open_windows_titled("Preferences");
            assert_eq!(windows.len(), 1);
            let stack = descendants(windows[0].upcast_ref())
                .into_iter()
                .find_map(|widget| widget.downcast::<gtk::Stack>().ok())
                .expect("the window holds its pages in a stack");
            assert_eq!(
                stack.visible_child_name().as_deref(),
                Some(TAB_ENCODING),
                "the open window shows the asked-for tab"
            );

            // Once closed, the next request opens a fresh window.
            windows[0].close();
            open(TAB_LIBRARY);
            let reopened = open_windows_titled("Preferences");
            assert_eq!(reopened.len(), 1);
            assert_ne!(reopened[0], windows[0]);
            reopened[0].close();
        });
    }
}
