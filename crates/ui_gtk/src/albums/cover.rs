// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::gdk;
use gtk::prelude::*;

const ALBUM_COVER_PLACEHOLDER_ICON: &str = "image-missing-symbolic";

pub(super) fn build_cover_widget(size: i32, css_class: &str) -> gtk::Overlay {
    let cover = gtk::Overlay::new();
    cover.add_css_class(css_class);
    cover.set_size_request(size, size);
    cover.set_halign(gtk::Align::Center);
    cover.set_valign(gtk::Align::Start);
    cover.set_hexpand(false);
    cover.set_vexpand(false);
    cover.set_overflow(gtk::Overflow::Hidden);

    // GtkWidget::set_size_request is a minimum, not a fixed size. Keep
    // placeholder/picture requisitions out of the cover's measurement so an
    // asynchronously loaded texture can never grow the cover and push the
    // tile labels down. With no main child, this explicit square request is
    // the overlay's only geometry source.
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_can_shrink(true);
    picture.set_size_request(size, size);
    picture.set_halign(gtk::Align::Fill);
    picture.set_valign(gtk::Align::Fill);
    picture.set_hexpand(false);
    picture.set_vexpand(false);
    picture.set_visible(false);
    cover.add_overlay(&picture);
    cover.set_clip_overlay(&picture, true);
    cover.set_measure_overlay(&picture, false);

    if let Some(icon) = album_cover_placeholder(size) {
        cover.add_overlay(&icon);
        cover.set_clip_overlay(&icon, true);
        cover.set_measure_overlay(&icon, false);
    }
    cover
}

/// Replaces the cover widget's current contents with either the decoded
/// image or the placeholder icon. Used both at construction time (called
/// with `None` to install the placeholder) and from the artwork loader
/// callback (called with the decoded texture once it arrives).
pub(super) fn apply_cover_texture(cover: &gtk::Overlay, texture: Option<gdk::Texture>, size: i32) {
    resize_cover_widget(cover, size);
    let Some(picture) = cover
        .first_child()
        .and_then(|child| child.downcast::<gtk::Picture>().ok())
    else {
        return;
    };
    picture.set_paintable(texture.as_ref());
    picture.set_visible(texture.is_some());
    if let Some(icon) = picture
        .next_sibling()
        .and_then(|child| child.downcast::<gtk::Image>().ok())
    {
        icon.set_visible(texture.is_none());
    }

    if texture.is_some() {
        cover.add_css_class("has-artwork");
    } else {
        cover.remove_css_class("has-artwork");
    }
}

pub(super) fn resize_cover_widget(cover: &gtk::Overlay, size: i32) {
    cover.set_size_request(size, size);
    if let Some(picture) = cover
        .first_child()
        .and_then(|child| child.downcast::<gtk::Picture>().ok())
    {
        picture.set_size_request(size, size);
        if let Some(icon) = picture
            .next_sibling()
            .and_then(|child| child.downcast::<gtk::Image>().ok())
        {
            icon.set_pixel_size((size / 3).max(32));
            icon.set_size_request(size, size);
        }
    }
}

/// Build a cover widget with an immediately-applied texture. Used by
/// the album detail panel, which resolves artwork synchronously via
/// the loader's cache (or a one-off sync read) and has the texture in
/// hand at construction time.
pub(super) fn album_cover_with(
    texture: Option<gdk::Texture>,
    size: i32,
    css_class: &str,
) -> gtk::Overlay {
    let cover = build_cover_widget(size, css_class);
    if texture.is_some() {
        apply_cover_texture(&cover, texture, size);
    }
    cover
}

pub(super) fn album_cover_placeholder(size: i32) -> Option<gtk::Image> {
    let display = gtk::gdk::Display::default()?;
    let theme = gtk::IconTheme::for_display(&display);
    if !theme.has_icon(ALBUM_COVER_PLACEHOLDER_ICON) {
        return None;
    }

    let icon = gtk::Image::from_icon_name(ALBUM_COVER_PLACEHOLDER_ICON);
    icon.add_css_class("album-cover-placeholder-icon");
    icon.set_pixel_size((size / 3).max(32));
    icon.set_size_request(size, size);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    icon.set_hexpand(false);
    icon.set_vexpand(false);
    Some(icon)
}
