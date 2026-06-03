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
    // Non-modal so a click anywhere outside the overlay defocuses it; the
    // focus-out handler below then closes it. A modal grab would swallow
    // those outside clicks, and a scrim window can't reliably cover the
    // parent on Wayland — focus-out is the Wayland-safe "click elsewhere
    // closes" (#163).
    let window = gtk::Window::builder()
        .title("Now Playing")
        .decorated(false)
        .transient_for(parent)
        .modal(false)
        .resizable(false)
        .build();
    window.add_css_class("now-playing-overlay-window");

    let surface = gtk::Overlay::new();
    surface.add_css_class("now-playing-overlay-surface");
    surface.set_size_request(OVERLAY_SIZE, OVERLAY_SIZE);
    // Clip the artwork/lyrics faces to the surface's rounded corners, the
    // same way the main window's shell rounds its content.
    surface.set_overflow(gtk::Overflow::Hidden);

    let stack = gtk::Stack::new();
    stack.set_hhomogeneous(true);
    stack.set_vhomogeneous(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(260);
    let artwork_face = artwork_face(artwork);
    let (lyrics_face, lyrics_view) = lyrics_face(lyrics);
    stack.add_named(&artwork_face, Some(OVERLAY_ARTWORK_STACK_NAME));
    stack.add_named(&lyrics_face, Some(OVERLAY_LYRICS_STACK_NAME));
    stack.set_visible_child_name(stack_name(initial_face));
    surface.set_child(Some(&stack));

    window.set_child(Some(&surface));
    install_palette_provider(&surface, palette);
    // Paint the lyrics with the artwork's secondary accent via a buffer tag
    // (authoritative over the inherited CSS colour), so they take an
    // artwork-derived colour instead of the computed black/white that left
    // them black (#165). `ArtworkPalette` is `Copy`, so the provider above
    // still got its own copy.
    if let Some(palette) = palette {
        apply_lyrics_palette(&lyrics_view, palette);
    }

    // Clicking anywhere outside the overlay defocuses it; close as soon as
    // the window goes inactive. Clicks inside the overlay (which flip
    // artwork↔lyrics) keep it focused, so only an "elsewhere" click closes
    // it (#163). present() activates the window first, so the initial
    // notification reports active and is ignored.
    window.connect_is_active_notify(move |window| {
        if !window.is_active() {
            window.close();
        }
    });

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
    view.set_justification(gtk::Justification::Center);
    // Reclaim the top line the previous 42px margin sacrificed to the
    // floating close button: the centred text clears the top-right
    // corner on its own, so a balanced top/bottom margin is enough.
    view.set_top_margin(24);
    view.set_bottom_margin(24);
    view.set_left_margin(28);
    view.set_right_margin(28);
    view.set_pixels_below_lines(7);
    view.set_pixels_inside_wrap(2);
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
    // Both colours come straight from the artwork (#165): the dominant tone
    // tints the surface, and the artwork-derived secondary accent is the text
    // colour — never the stark computed black/white `foreground`, which is
    // what left the lyrics black. The lyrics text is also painted by a buffer
    // tag in `apply_lyrics_palette` (authoritative); this `color` is the
    // inherited fallback for the rest of the surface.
    let background = palette.background_css();
    let secondary = palette.secondary_css();
    format!(
        ".now-playing-overlay-surface {{ background-color: {background}; color: {secondary}; }}"
    )
}

/// Paint the lyrics text with the artwork's secondary accent colour using a
/// buffer-wide [`gtk::TextTag`]. A tag's foreground takes precedence over the
/// inherited CSS `color` for the tagged range, so the lyrics take the artwork
/// colour regardless of how the `GtkTextView` text node resolves its CSS —
/// the inheritance ambiguity that previously left them black (#165).
fn apply_lyrics_palette(view: &gtk::TextView, palette: ArtworkPalette) {
    let buffer = view.buffer();
    let secondary = palette.secondary_css();
    let Some(tag) = buffer.create_tag(None, &[("foreground", &secondary as &dyn ToValue)]) else {
        return;
    };
    let (start, end) = buffer.bounds();
    buffer.apply_tag(&tag, &start, &end);
}

#[cfg(test)]
mod tests {
    use super::{LyricsOverlayFace, apply_lyrics_palette, build, stack_name};
    use crate::artwork_color::{ArtworkPalette, ArtworkPaletteComponents, RgbColorComponents};
    use gtk::prelude::*;

    #[test]
    fn face_names_match_stack_page_names() {
        assert_eq!(stack_name(LyricsOverlayFace::Artwork), "artwork");
        assert_eq!(stack_name(LyricsOverlayFace::Lyrics), "lyrics");
    }

    #[test]
    fn lyrics_take_the_artwork_secondary_colour_not_black_or_white() {
        if !crate::test_support::with_gtk(|| {
            // A near-black dominant cover: the computed `foreground` would be
            // white, so if the lyrics came out as the secondary accent below
            // (a warm orange) we know we used the artwork colour, not the
            // black/white contrast neutral that caused the bug (#165).
            let palette = ArtworkPalette::from_components(ArtworkPaletteComponents {
                background: RgbColorComponents {
                    red: 10,
                    green: 12,
                    blue: 16,
                },
                foreground: RgbColorComponents {
                    red: 255,
                    green: 255,
                    blue: 255,
                },
                secondary: RgbColorComponents {
                    red: 200,
                    green: 120,
                    blue: 60,
                },
            });

            let view = gtk::TextView::new();
            view.buffer().set_text("a lyric line");
            apply_lyrics_palette(&view, palette);

            let buffer = view.buffer();
            let tags = buffer.start_iter().tags();
            assert_eq!(tags.len(), 1, "one colour tag covers the lyrics");
            let rgba = tags[0]
                .foreground_rgba()
                .expect("the tag carries a foreground colour");
            assert!((rgba.red() - 200.0 / 255.0).abs() < 0.01, "red = secondary");
            assert!(
                (rgba.green() - 120.0 / 255.0).abs() < 0.01,
                "green = secondary"
            );
            assert!(
                (rgba.blue() - 60.0 / 255.0).abs() < 0.01,
                "blue = secondary"
            );
        }) {
            eprintln!("SMOKE: no display, skipping");
        }
    }

    #[test]
    fn overlay_builds_as_a_non_modal_transient_window() {
        if !crate::test_support::with_gtk(|| {
            let parent = gtk::ApplicationWindow::default();
            let overlay = build(
                &parent,
                None,
                None,
                Some("A lyric line"),
                LyricsOverlayFace::Lyrics,
            );

            // Non-modal so an outside click can defocus and dismiss it (#163).
            assert!(!overlay.is_modal());
            assert_eq!(overlay.transient_for().as_ref(), Some(parent.upcast_ref()));
            assert!(overlay.child().is_some());
            overlay.close();
            parent.close();
        }) {
            eprintln!("SMOKE: no display, skipping");
        }
    }
}
