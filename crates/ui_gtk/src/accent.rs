// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cell::RefCell;

use gtk::prelude::*;
use gtk::{gdk, gio};

const INTERFACE_SCHEMA: &str = "org.gnome.desktop.interface";
const ACCENT_COLOR_KEY: &str = "accent-color";

thread_local! {
    static ACCENT_WATCH: RefCell<Option<AccentWatch>> = const { RefCell::new(None) };
}

struct AccentWatch {
    _settings: gio::Settings,
    _provider: gtk::CssProvider,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AccentPalette {
    background: &'static str,
    foreground: &'static str,
}

pub(crate) fn install_accent_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let Some(settings) = accent_settings() else {
        install_static_accent_css(&display, accent_palette("blue"));
        return;
    };

    let provider = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    update_accent_css(&provider, &settings);

    let provider_for_settings = provider.clone();
    settings.connect_changed(Some(ACCENT_COLOR_KEY), move |settings, _key| {
        update_accent_css(&provider_for_settings, settings);
    });

    ACCENT_WATCH.with(|watch| {
        watch.replace(Some(AccentWatch {
            _settings: settings,
            _provider: provider,
        }));
    });
}

fn accent_settings() -> Option<gio::Settings> {
    let schema_source = gio::SettingsSchemaSource::default()?;
    let schema = schema_source.lookup(INTERFACE_SCHEMA, true)?;
    Some(gio::Settings::new_full(
        &schema,
        gio::SettingsBackend::NONE,
        None,
    ))
}

fn install_static_accent_css(display: &gdk::Display, palette: AccentPalette) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&accent_css(palette));
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
}

fn update_accent_css(provider: &gtk::CssProvider, settings: &gio::Settings) {
    let accent_name = settings.string(ACCENT_COLOR_KEY);
    provider.load_from_string(&accent_css(accent_palette(&accent_name)));
}

fn accent_palette(accent_name: &str) -> AccentPalette {
    match accent_name {
        "teal" => AccentPalette {
            background: "#2190a4",
            foreground: "#ffffff",
        },
        "green" => AccentPalette {
            background: "#3a944a",
            foreground: "#ffffff",
        },
        "yellow" => AccentPalette {
            background: "#c88800",
            foreground: "#000000",
        },
        "orange" => AccentPalette {
            background: "#ed5b00",
            foreground: "#ffffff",
        },
        "red" => AccentPalette {
            background: "#e62d42",
            foreground: "#ffffff",
        },
        "pink" => AccentPalette {
            background: "#d56199",
            foreground: "#ffffff",
        },
        "purple" => AccentPalette {
            background: "#9141ac",
            foreground: "#ffffff",
        },
        "slate" => AccentPalette {
            background: "#6f8396",
            foreground: "#ffffff",
        },
        _ => AccentPalette {
            background: "#3584e4",
            foreground: "#ffffff",
        },
    }
}

fn accent_css(palette: AccentPalette) -> String {
    format!(
        r#"
        .now-playing-side-icon-active {{
            color: {background};
            opacity: 1;
        }}

        .track-table-status-playing {{
            color: {background};
        }}

        columnview.track-table listview row:selected,
        columnview.track-table listview row:selected cell {{
            background-color: {background};
            background-image: none;
        }}

        columnview.track-table listview row:selected .track-table-cell,
        columnview.track-table listview row:selected .track-table-cell.track-table-row-even,
        columnview.track-table listview row:selected .track-table-cell.track-table-row-odd,
        .track-table-cell.track-table-row-selected {{
            background-color: {background};
            background-image: none;
        }}

        columnview.track-table listview row:selected .track-table-cell > label,
        columnview.track-table listview row:selected .track-table-cell > image,
        columnview.track-table listview row:selected .track-table-cell .rating-stars button.rating-star,
        .track-table-cell.track-table-row-selected > label,
        .track-table-cell.track-table-row-selected > image,
        .track-table-cell.track-table-row-selected .rating-stars button.rating-star {{
            color: {foreground};
        }}

        columnview.track-table listview row:selected .track-table-status-playing,
        .track-table-cell.track-table-row-selected .track-table-status-playing {{
            color: {foreground};
        }}

        button.suggested-action:not(:disabled) {{
            background-color: {background};
            background-image: none;
            color: {foreground};
        }}

        button.suggested-action:not(:disabled):hover {{
            background-color: shade({background}, 1.10);
            background-image: none;
            color: {foreground};
        }}

        button.suggested-action:not(:disabled):active,
        button.suggested-action:not(:disabled):checked {{
            background-color: shade({background}, 0.90);
            background-image: none;
            color: {foreground};
        }}

        /* Sidebar selection. Two row populations need the accent fill:
           the standalone "Music" entry under LIBRARY (a `gtk::Box` that
           carries `.playlist-sidebar-row` and gets a `.selected` class
           toggled in code), and every row under PLAYLISTS (each lives
           inside a `gtk::ListView` whose `row:selected` pseudoclass is
           managed by GTK). Painting both with the same accent keeps the
           two sections visually uniform and matches the track table's
           selection colour. */
        .playlist-sidebar-row.selected {{
            background-color: {background};
            color: {foreground};
        }}

        .playlist-sidebar-row.selected .playlist-sidebar-icon,
        .playlist-sidebar-row.selected label {{
            color: {foreground};
        }}

        listview.playlist-sidebar-list > row:selected {{
            background-color: {background};
            color: {foreground};
        }}

        listview.playlist-sidebar-list > row:selected .playlist-sidebar-icon,
        listview.playlist-sidebar-list > row:selected label {{
            color: {foreground};
        }}

        /* A playlist popover is parented below its anchor row, so the broad
           selected-row label rule above also reaches into the menu. Restore
           theme foreground inside the popover: selected-row contrast belongs
           to the row, not to the separate popover surface. */
        .playlist-sidebar-row.selected popover,
        .playlist-sidebar-row.selected popover label,
        listview.playlist-sidebar-list > row:selected popover,
        listview.playlist-sidebar-list > row:selected popover label {{
            color: @theme_fg_color;
        }}

        .playlist-sidebar-row:drop(active) {{
            background-color: transparent;
            background-image: none;
        }}

        .playlist-sidebar-row.sidebar-drop-above {{
            box-shadow: inset 0 2px 0 0 {background};
        }}

        .playlist-sidebar-row.sidebar-drop-below {{
            box-shadow: inset 0 -2px 0 0 {background};
        }}

        .playlist-sidebar-row.sidebar-drop-into {{
            background-color: transparent;
            background-image: none;
            box-shadow: inset 0 0 0 2px {background};
        }}

        .library-drop-indicator.library-drop-active {{
            box-shadow: inset 0 0 0 2px {background};
        }}
        "#,
        background = palette.background,
        foreground = palette.foreground,
    )
}

#[cfg(test)]
mod tests {
    use super::{AccentPalette, accent_css, accent_palette};

    #[test]
    fn accent_palette_uses_gnome_green() {
        assert_eq!(
            accent_palette("green"),
            AccentPalette {
                background: "#3a944a",
                foreground: "#ffffff",
            }
        );
    }

    #[test]
    fn accent_palette_falls_back_to_gnome_blue() {
        assert_eq!(
            accent_palette("unknown"),
            AccentPalette {
                background: "#3584e4",
                foreground: "#ffffff",
            }
        );
    }

    #[test]
    fn accent_css_styles_active_icon_and_selected_table_cells() {
        let css = accent_css(accent_palette("green"));

        assert!(css.contains(".now-playing-side-icon-active"));
        assert!(css.contains(".track-table-cell.track-table-row-selected"));
        assert!(css.contains(".track-table-status-playing"));
        assert!(css.contains("#3a944a"));
    }

    #[test]
    fn accent_css_paints_sidebar_selection() {
        let css = accent_css(accent_palette("green"));

        assert!(css.contains(".playlist-sidebar-row.selected"));
        assert!(css.contains("listview.playlist-sidebar-list > row:selected"));
        assert!(css.contains("#3a944a"));
        assert!(css.contains("#ffffff"));
    }

    #[test]
    fn accent_css_restores_theme_text_inside_selected_sidebar_popovers() {
        let css = accent_css(accent_palette("green"));

        assert!(css.contains("listview.playlist-sidebar-list > row:selected popover label"));
        assert!(css.contains("color: @theme_fg_color"));
    }

    #[test]
    fn accent_css_paints_suggested_action_buttons() {
        let css = accent_css(accent_palette("green"));

        assert!(css.contains("button.suggested-action:not(:disabled)"));
        assert!(css.contains("button.suggested-action:not(:disabled):hover"));
        assert!(css.contains("button.suggested-action:not(:disabled):active"));
        assert!(css.contains("#3a944a"));
    }
}
