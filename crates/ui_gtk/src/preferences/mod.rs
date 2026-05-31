// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::PathBuf;

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use super::{
    PREFERENCES_WIDTH, WINDOW_SHADOW_MARGIN, command_controller::SharedCommandController,
    library_consolidation::LibraryConsolidationRequestedCallback,
    library_scan::LibraryScanRequestedCallback,
};

mod about_tab;
mod analysis_tab;
mod library_tab;
mod online_tab;
mod shuffle_tab;
mod switch_row;

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
const TAB_ANALYSIS: &str = "analysis";
const TAB_ONLINE: &str = "online";
const TAB_SHUFFLE: &str = "shuffle";
const TAB_ABOUT: &str = "about";

pub(crate) fn install_preferences_action(
    app: &gtk::Application,
    window: &gtk::ApplicationWindow,
    command_controller: SharedCommandController,
    database_path: PathBuf,
    scan_requested: LibraryScanRequestedCallback,
    consolidation_requested: LibraryConsolidationRequestedCallback,
) {
    if app.lookup_action("preferences").is_some() {
        return;
    }

    let preferences = gio::SimpleAction::new("preferences", None);
    let window = window.clone();
    let command_controller = command_controller.clone();
    let scan_requested = scan_requested.clone();
    let consolidation_requested = consolidation_requested.clone();
    preferences.connect_activate(move |_action, _parameter| {
        open_preferences_window(
            &window,
            command_controller.clone(),
            database_path.clone(),
            scan_requested.clone(),
            consolidation_requested.clone(),
        );
    });
    app.add_action(&preferences);
    app.set_accels_for_action("app.preferences", &["<Primary>comma"]);
}

/// Build the sidebar footer's "cog + Settings" entry.
///
/// Lives in the sidebar's bottom footer Box. Icon-plus-label so a user
/// with no iTunes muscle memory can find Preferences without hunting;
/// power users still have the Ctrl+, accelerator registered by
/// [`install_preferences_action`]. Sized to its content so the hover
/// surface only covers the button itself, not the whole footer width.
pub(crate) fn settings_button(
    window: &gtk::ApplicationWindow,
    command_controller: SharedCommandController,
    database_path: PathBuf,
    scan_requested: LibraryScanRequestedCallback,
    consolidation_requested: LibraryConsolidationRequestedCallback,
) -> gtk::Button {
    let icon = gtk::Image::from_icon_name("preferences-system-symbolic");
    icon.set_pixel_size(16);
    icon.add_css_class("sidebar-settings-icon");

    let label = gtk::Label::new(Some("Settings"));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    content.append(&icon);
    content.append(&label);

    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("sidebar-settings-button");
    button.set_child(Some(&content));
    button.set_tooltip_text(Some("Preferences (Ctrl+,)"));
    // hexpand=true asks the footer Box for all available horizontal
    // space; halign=Center then centers the natural-width button
    // within that allocation. The hover surface stays glued to the
    // icon + label — GTK paints the button background within its
    // *aligned* size, not its allocation.
    button.set_hexpand(true);
    button.set_halign(gtk::Align::Center);

    let window = window.clone();
    let command_controller = command_controller.clone();
    let scan_requested = scan_requested.clone();
    let consolidation_requested = consolidation_requested.clone();
    button.connect_clicked(move |_| {
        open_preferences_window(
            &window,
            command_controller.clone(),
            database_path.clone(),
            scan_requested.clone(),
            consolidation_requested.clone(),
        );
    });

    button
}

fn open_preferences_window(
    parent: &gtk::ApplicationWindow,
    command_controller: SharedCommandController,
    database_path: PathBuf,
    scan_requested: LibraryScanRequestedCallback,
    consolidation_requested: LibraryConsolidationRequestedCallback,
) {
    let window = gtk::Window::builder()
        .title("Preferences")
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
        scan_requested,
        consolidation_requested,
    );
    stack.add_named(&library_page, Some(TAB_LIBRARY));

    let analysis_page = analysis_tab::build(command_controller.clone());
    stack.add_named(&analysis_page, Some(TAB_ANALYSIS));

    let online_page = online_tab::build(command_controller.clone());
    stack.add_named(&online_page, Some(TAB_ONLINE));

    let shuffle_page = shuffle_tab::build(window.upcast_ref(), command_controller);
    stack.add_named(&shuffle_page, Some(TAB_SHUFFLE));

    let about_page = about_tab::build(&window, &database_path);
    stack.add_named(&about_page, Some(TAB_ABOUT));

    let tab_strip = build_tab_strip(&stack, &window);

    panel.append(&tab_strip);
    panel.append(&stack);
    frame.append(&panel);
    window.set_child(Some(&frame));

    let key_controller = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    window.present();
}

/// Builds the headerless drag surface that combines the three icon-above-label
/// tab buttons and the close button on the trailing end.
fn build_tab_strip(stack: &gtk::Stack, window: &gtk::Window) -> gtk::Widget {
    let library_button = build_tab_button("folder-music-symbolic", "Library");
    let analysis_button = build_tab_button("applications-science-symbolic", "Analysis");
    let online_button = build_tab_button("network-transmit-receive-symbolic", "Online");
    let shuffle_button = build_tab_button("media-playlist-shuffle-symbolic", "Shuffle");
    let about_button = build_tab_button("help-about-symbolic", "About");

    // Group all five so exactly one is active at a time. The Library tab
    // is the default landing.
    analysis_button.set_group(Some(&library_button));
    online_button.set_group(Some(&library_button));
    shuffle_button.set_group(Some(&library_button));
    about_button.set_group(Some(&library_button));
    library_button.set_active(true);

    wire_tab_button(&library_button, stack, TAB_LIBRARY);
    wire_tab_button(&analysis_button, stack, TAB_ANALYSIS);
    wire_tab_button(&online_button, stack, TAB_ONLINE);
    wire_tab_button(&shuffle_button, stack, TAB_SHUFFLE);
    wire_tab_button(&about_button, stack, TAB_ABOUT);

    let tab_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    tab_box.add_css_class("preferences-tab-buttons");
    tab_box.set_halign(gtk::Align::Center);
    tab_box.append(&library_button);
    tab_box.append(&analysis_button);
    tab_box.append(&online_button);
    tab_box.append(&shuffle_button);
    tab_box.append(&about_button);

    let close_icon = gtk::Image::from_icon_name("window-close-symbolic");
    close_icon.set_pixel_size(14);

    let close_button = gtk::Button::new();
    close_button.add_css_class("flat");
    close_button.add_css_class("preference-close-button");
    close_button.set_child(Some(&close_icon));
    close_button.set_tooltip_text(Some("Close"));
    close_button.set_valign(gtk::Align::Center);
    close_button.set_margin_end(8);

    let window_for_close = window.clone();
    close_button.connect_clicked(move |_| {
        window_for_close.close();
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
    handle.upcast()
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
    button.set_child(Some(&content));
    button
}

fn wire_tab_button(button: &gtk::ToggleButton, stack: &gtk::Stack, page_name: &'static str) {
    let stack = stack.clone();
    button.connect_toggled(move |btn| {
        // Only the newly-activated radio fires this callback with
        // `is_active() == true`; the previously-active one fires with
        // `false` and we ignore it so we don't switch pages twice.
        if btn.is_active() {
            stack.set_visible_child_name(page_name);
        }
    });
}
