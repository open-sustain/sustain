// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::gdk;
use gtk::prelude::*;

use crate::artwork_color::ArtworkPalette;

const ALBUM_GRID_MARGIN: i32 = super::ALBUM_GRID_MARGIN;
const ALBUM_GRID_COLUMN_SPACING: i32 = super::ALBUM_GRID_COLUMN_SPACING;
const ALBUM_DETAIL_ARROW_WIDTH: i32 = 36;
pub(super) const ALBUM_DETAIL_ARROW_HEIGHT: i32 = 18;

// One-pixel bleed below the triangle's base. The arrow row is laid out as
// an overlay on top of the detail panel so this bleed extends one row of
// arrow-coloured pixels into the panel's opaque background. Any sub-pixel
// transparency at the arrow texture's bottom edge — common when the
// scroller lands on a fractional offset — should then composite onto the
// panel's same-coloured pixels instead of revealing the window
// background.
// NOTE: Claude failed to fully eliminate the seam — a faint line under
// the arrow still appears intermittently, especially during scrolling.
const ALBUM_DETAIL_ARROW_BLEED: i32 = 1;

pub(super) fn album_detail_arrow_row(selected_column: usize, columns: usize) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, ALBUM_GRID_COLUMN_SPACING);
    row.set_homogeneous(true);
    row.set_margin_start(ALBUM_GRID_MARGIN);
    row.set_margin_end(ALBUM_GRID_MARGIN);
    row.set_height_request(ALBUM_DETAIL_ARROW_HEIGHT + ALBUM_DETAIL_ARROW_BLEED);

    for column in 0..columns {
        let cell = gtk::Box::new(gtk::Orientation::Vertical, 0);
        cell.set_halign(gtk::Align::Fill);
        cell.set_hexpand(true);

        if column == selected_column {
            cell.append(&album_detail_arrow());
        }

        row.append(&cell);
    }

    row
}

fn album_detail_arrow() -> gtk::DrawingArea {
    let arrow = gtk::DrawingArea::new();
    arrow.add_css_class("album-detail-arrow");
    apply_palette_class(&arrow, "album-detail-palette-arrow");
    arrow.set_content_width(ALBUM_DETAIL_ARROW_WIDTH);
    arrow.set_content_height(ALBUM_DETAIL_ARROW_HEIGHT + ALBUM_DETAIL_ARROW_BLEED);
    arrow.set_halign(gtk::Align::Center);
    arrow.set_valign(gtk::Align::End);
    arrow.set_draw_func(|area, context, width, _height| {
        let color = area.color();
        let arrow_width = f64::from(width);
        let arrow_height = f64::from(ALBUM_DETAIL_ARROW_HEIGHT);
        let bleed = f64::from(ALBUM_DETAIL_ARROW_BLEED);

        // The arrow color is driven by CSS so it stays in sync with the
        // panel: `.album-detail-arrow` matches the default panel tint, and
        // `.album-detail-palette-arrow` (applied when artwork yields a
        // palette) matches the palette background. Alpha is forced to 1.0
        // so the panel's 1px overlap below can't composite onto a
        // translucent fill and produce a darker stripe at the seam.
        context.set_source_rgba(
            f64::from(color.red()),
            f64::from(color.green()),
            f64::from(color.blue()),
            1.0,
        );

        context.move_to(arrow_width / 2.0, 0.0);
        context.line_to(arrow_width, arrow_height);
        context.line_to(0.0, arrow_height);
        context.close_path();
        let _result = context.fill();

        context.rectangle(0.0, arrow_height, arrow_width, bleed);
        let _result = context.fill();
    });

    arrow
}

pub(super) fn album_detail_palette_provider(palette: ArtworkPalette) -> gtk::CssProvider {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&album_detail_palette_css(palette));
    provider
}

/// Tag `widget` with an album-detail palette class. The class is inert
/// until the artwork-derived palette provider is installed display-wide
/// (asynchronously, once the cover decodes — #107); albums without
/// artwork never install one, so the class resolves to the default
/// theme. Classes are therefore always added; only the provider install
/// is conditional on a palette being available.
pub(super) fn apply_palette_class(widget: &impl IsA<gtk::Widget>, css_class: &str) {
    widget.as_ref().add_css_class(css_class);
}

pub(super) fn install_palette_provider(
    widget: &impl IsA<gtk::Widget>,
    provider: Option<&gtk::CssProvider>,
) {
    let (Some(display), Some(provider)) = (gdk::Display::default(), provider) else {
        return;
    };

    gtk::style_context_add_provider_for_display(
        &display,
        provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
    );

    let provider = provider.clone();
    widget.as_ref().connect_destroy(move |_| {
        gtk::style_context_remove_provider_for_display(&display, &provider);
    });
}

fn album_detail_palette_css(palette: ArtworkPalette) -> String {
    let background = palette.background_css();
    let foreground = palette.foreground_css();
    let secondary = palette.secondary_css();

    format!(
        r#"
        .album-detail-dominant-color {{
            background-color: {background};
            border: none;
            color: {foreground};
        }}

        .album-detail-palette-arrow {{
            color: {background};
        }}

        .album-detail-palette-primary,
        button.album-detail-palette-button,
        image.album-detail-palette-primary {{
            color: {foreground};
        }}

        /* Artist name (subtitle), track number, and duration share the
           artwork-derived secondary colour so the muted text reads as
           part of the cover's palette instead of as a uniformly faded
           white/black. The track-playing speaker icon is intentionally
           not in this set: it keeps the strict-contrast foreground so
           the "now playing" cue is unmissable on any artwork. */
        .album-detail-palette-secondary,
        .album-detail-palette-muted {{
            color: {secondary};
        }}

        .album-detail-palette-surface {{
            background-color: alpha({foreground}, 0.12);
        }}

        button.album-detail-palette-button:hover,
        button.album-detail-palette-button:active,
        button.album-detail-palette-button:focus {{
            background-color: alpha({foreground}, 0.14);
        }}

        /* Status icons keep the strict-contrast foreground on artwork:
           a cover dominated by the theme's warning orange would hide the
           missing-file mark. The glyph itself tells the states apart. */
        .album-track-table .track-table-status-playing,
        .album-track-table .track-table-status-missing {{
            color: {foreground};
        }}

        listview.album-track-table > row:focus-visible {{
            outline-color: {foreground};
        }}
        "#,
    )
}

pub(super) fn detail_icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(18);
    apply_palette_class(&icon, "album-detail-palette-primary");

    let button = gtk::Button::new();
    button.add_css_class("album-detail-icon-button");
    apply_palette_class(&button, "album-detail-palette-button");
    button.set_child(Some(&icon));
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);
    button
}

#[cfg(test)]
mod tests {
    use gtk::gdk;
    use gtk::prelude::*;

    use super::album_detail_palette_provider;
    use crate::artwork_color::ArtworkPalette;

    /// The warning orange of the default GTK theme, filling a whole cover.
    const WARNING_ORANGE: u32 = 0xe661_00ff;

    #[test]
    fn the_missing_file_mark_stays_readable_on_a_cover_of_its_own_colour() {
        crate::test_support::with_gtk(|| {
            let cover =
                gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 8, 8)
                    .expect("allocate cover");
            cover.fill(WARNING_ORANGE);
            let palette = ArtworkPalette::from_pixbuf(&cover).expect("cover palette");
            let display = gdk::Display::default().expect("test display");
            // A theme whose warning colour is the cover's own orange.
            let theme = gtk::CssProvider::new();
            theme.load_from_string(".track-table-status-missing { color: #e66100; }");
            gtk::style_context_add_provider_for_display(
                &display,
                &theme,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            let provider = album_detail_palette_provider(palette);
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
            );

            let table = gtk::Box::new(gtk::Orientation::Vertical, 0);
            table.add_css_class("album-track-table");
            let mark = gtk::Image::from_icon_name("dialog-warning-symbolic");
            mark.add_css_class("track-table-status-missing");
            table.append(&mark);
            let window = gtk::Window::new();
            window.set_child(Some(&table));

            let shown = mark.color();
            gtk::style_context_remove_provider_for_display(&display, &provider);
            gtk::style_context_remove_provider_for_display(&display, &theme);
            window.destroy();

            let foreground =
                gdk::RGBA::parse(palette.foreground_css()).expect("palette foreground");
            assert_eq!(shown, foreground);
            assert_ne!(shown, palette.background_rgba());
        });
    }
}
