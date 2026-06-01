// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::artwork_color::ArtworkPalette;

const OVERLAY_SIZE: i32 = 440;
const OVERLAY_ARTWORK_STACK_NAME: &str = "artwork";
const OVERLAY_LYRICS_STACK_NAME: &str = "lyrics";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LyricsOverlayFace {
    Artwork,
    Lyrics,
}

pub(super) fn open(
    parent: &gtk::ApplicationWindow,
    artwork: Option<&gdk::Texture>,
    palette: Option<ArtworkPalette>,
    lyrics: Option<&str>,
    initial_face: LyricsOverlayFace,
) {
    build(parent, artwork, palette, lyrics, initial_face).present();
}

fn build(
    parent: &gtk::ApplicationWindow,
    artwork: Option<&gdk::Texture>,
    palette: Option<ArtworkPalette>,
    lyrics: Option<&str>,
    initial_face: LyricsOverlayFace,
) -> gtk::Window {
    let window = gtk::Window::builder()
        .title("Now Playing")
        .decorated(false)
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .build();
    window.add_css_class("now-playing-overlay-window");

    let surface = gtk::Overlay::new();
    surface.add_css_class("now-playing-overlay-surface");
    surface.set_size_request(OVERLAY_SIZE, OVERLAY_SIZE);

    let stack = gtk::Stack::new();
    stack.set_hhomogeneous(true);
    stack.set_vhomogeneous(true);
    stack.set_transition_type(gtk::StackTransitionType::RotateLeftRight);
    stack.set_transition_duration(260);
    let artwork_face = artwork_face(artwork);
    let (lyrics_face, lyrics_view) = lyrics_face(lyrics);
    stack.add_named(&artwork_face, Some(OVERLAY_ARTWORK_STACK_NAME));
    stack.add_named(&lyrics_face, Some(OVERLAY_LYRICS_STACK_NAME));
    stack.set_visible_child_name(stack_name(initial_face));
    surface.set_child(Some(&stack));

    let close = gtk::Button::from_icon_name("window-close-symbolic");
    close.add_css_class("now-playing-overlay-close");
    close.set_tooltip_text(Some("Close"));
    close.set_halign(gtk::Align::End);
    close.set_valign(gtk::Align::Start);
    close.set_margin_top(8);
    close.set_margin_end(8);
    surface.add_overlay(&close);
    surface.set_measure_overlay(&close, false);

    window.set_child(Some(&surface));
    install_palette_provider(&surface, palette);

    let window_for_close = window.clone();
    close.connect_clicked(move |_| window_for_close.close());

    let key = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    key.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key);

    if lyrics.is_some() && artwork.is_some() {
        install_flip_handler(&artwork_face, &stack, OVERLAY_LYRICS_STACK_NAME);
        install_flip_handler(&lyrics_view, &stack, OVERLAY_ARTWORK_STACK_NAME);
    }

    window
}

fn artwork_face(artwork: Option<&gdk::Texture>) -> gtk::Widget {
    match artwork {
        Some(texture) => {
            let picture = gtk::Picture::new();
            picture.add_css_class("now-playing-overlay-artwork");
            picture.set_paintable(Some(texture));
            picture.set_content_fit(gtk::ContentFit::Contain);
            picture.set_can_shrink(true);
            picture.set_size_request(OVERLAY_SIZE, OVERLAY_SIZE);
            picture.set_halign(gtk::Align::Fill);
            picture.set_valign(gtk::Align::Fill);
            picture.upcast()
        }
        None => {
            let image = gtk::Image::new();
            image.add_css_class("now-playing-overlay-artwork");
            image.set_size_request(OVERLAY_SIZE, OVERLAY_SIZE);
            image.set_halign(gtk::Align::Fill);
            image.set_valign(gtk::Align::Fill);
            image.set_icon_name(Some("image-missing-symbolic"));
            image.set_pixel_size(OVERLAY_SIZE / 3);
            image.upcast()
        }
    }
}

fn lyrics_face(lyrics: Option<&str>) -> (gtk::ScrolledWindow, gtk::TextView) {
    let view = gtk::TextView::new();
    view.add_css_class("now-playing-overlay-lyrics");
    view.set_editable(false);
    view.set_cursor_visible(false);
    view.set_wrap_mode(gtk::WrapMode::WordChar);
    view.set_top_margin(42);
    view.set_bottom_margin(24);
    view.set_left_margin(28);
    view.set_right_margin(28);
    view.buffer().set_text(lyrics.unwrap_or_default());

    let scroller = gtk::ScrolledWindow::new();
    scroller.add_css_class("now-playing-overlay-lyrics-scroll");
    scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroller.set_child(Some(&view));
    (scroller, view)
}

fn install_flip_handler(
    widget: &impl IsA<gtk::Widget>,
    stack: &gtk::Stack,
    destination: &'static str,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    let stack = stack.clone();
    click.connect_released(move |gesture, _n_press, _x, _y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        stack.set_visible_child_name(destination);
    });
    widget.add_controller(click);
}

fn stack_name(face: LyricsOverlayFace) -> &'static str {
    match face {
        LyricsOverlayFace::Artwork => OVERLAY_ARTWORK_STACK_NAME,
        LyricsOverlayFace::Lyrics => OVERLAY_LYRICS_STACK_NAME,
    }
}

fn install_palette_provider(widget: &impl IsA<gtk::Widget>, palette: Option<ArtworkPalette>) {
    let (Some(display), Some(palette)) = (gdk::Display::default(), palette) else {
        return;
    };
    let provider = gtk::CssProvider::new();
    provider.load_from_string(&palette_css(palette));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
    );

    let provider_for_destroy = provider.clone();
    widget.as_ref().connect_destroy(move |_| {
        gtk::style_context_remove_provider_for_display(&display, &provider_for_destroy);
    });
}

fn palette_css(palette: ArtworkPalette) -> String {
    let background = palette.background_css();
    let foreground = palette.foreground_css();
    format!(
        ".now-playing-overlay-surface {{ background-color: {background}; color: {foreground}; }}"
    )
}

#[cfg(test)]
mod tests {
    use super::{LyricsOverlayFace, build, stack_name};
    use gtk::prelude::*;

    #[test]
    fn face_names_match_stack_page_names() {
        assert_eq!(stack_name(LyricsOverlayFace::Artwork), "artwork");
        assert_eq!(stack_name(LyricsOverlayFace::Lyrics), "lyrics");
    }

    #[test]
    fn overlay_builds_as_a_modal_transient_window() {
        if !crate::test_support::with_gtk(|| {
            let parent = gtk::ApplicationWindow::default();
            let overlay = build(
                &parent,
                None,
                None,
                Some("A lyric line"),
                LyricsOverlayFace::Lyrics,
            );

            assert!(overlay.is_modal());
            assert_eq!(overlay.transient_for().as_ref(), Some(parent.upcast_ref()));
            assert!(overlay.child().is_some());
            overlay.close();
            parent.close();
        }) {
            eprintln!("SMOKE: no display, skipping");
        }
    }
}
