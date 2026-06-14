// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::gdk;
use gtk::gdk::prelude::ToplevelExt;
use gtk::prelude::*;

use super::{RESIZE_CORNER_SIZE, RESIZE_EDGE_THICKNESS, WINDOW_SHADOW_MARGIN};

pub(crate) fn install_window_state_chrome(
    window: &gtk::ApplicationWindow,
    window_frame: &gtk::Overlay,
) {
    update_window_state_chrome(window, window_frame);
    install_toplevel_state_observer(window, window_frame);

    let window_frame_for_realize = window_frame.clone();
    window.connect_realize(move |window| {
        install_toplevel_state_observer(window, &window_frame_for_realize);
    });

    let window_frame_for_fullscreen = window_frame.clone();
    window.connect_fullscreened_notify(move |window| {
        update_window_state_chrome(window, &window_frame_for_fullscreen);
    });

    let window_frame_for_maximize = window_frame.clone();
    window.connect_maximized_notify(move |window| {
        update_window_state_chrome(window, &window_frame_for_maximize);
    });
}

fn update_window_state_chrome(window: &gtk::ApplicationWindow, window_frame: &gtk::Overlay) {
    let is_floating = toplevel_for_window(window)
        .map(|toplevel| toplevel_state_is_floating(toplevel.state()))
        .unwrap_or_else(|| !window.is_fullscreen() && !window.is_maximized());
    apply_window_frame_margin(window_frame, is_floating);
}

fn install_toplevel_state_observer(window: &gtk::ApplicationWindow, window_frame: &gtk::Overlay) {
    let Some(toplevel) = toplevel_for_window(window) else {
        return;
    };
    apply_window_frame_margin(window_frame, toplevel_state_is_floating(toplevel.state()));

    let window_frame = window_frame.clone();
    toplevel.connect_state_notify(move |toplevel| {
        apply_window_frame_margin(&window_frame, toplevel_state_is_floating(toplevel.state()));
    });
}

fn toplevel_for_window(window: &gtk::ApplicationWindow) -> Option<gdk::Toplevel> {
    window.surface()?.downcast::<gdk::Toplevel>().ok()
}

fn toplevel_state_is_floating(state: gdk::ToplevelState) -> bool {
    !state.intersects(
        gdk::ToplevelState::FULLSCREEN
            | gdk::ToplevelState::MAXIMIZED
            | gdk::ToplevelState::TILED
            | gdk::ToplevelState::TOP_TILED
            | gdk::ToplevelState::RIGHT_TILED
            | gdk::ToplevelState::BOTTOM_TILED
            | gdk::ToplevelState::LEFT_TILED,
    )
}

fn apply_window_frame_margin(window_frame: &gtk::Overlay, is_floating: bool) {
    let margin = if is_floating {
        window_frame.add_css_class("window-frame");
        WINDOW_SHADOW_MARGIN
    } else {
        window_frame.remove_css_class("window-frame");
        0
    };

    window_frame.set_margin_top(margin);
    window_frame.set_margin_end(margin);
    window_frame.set_margin_bottom(margin);
    window_frame.set_margin_start(margin);
}

pub(crate) fn install_resize_handles(shell: &gtk::Overlay, window: &gtk::ApplicationWindow) {
    for (edge, halign, valign, width, height, cursor) in [
        (
            gdk::SurfaceEdge::North,
            gtk::Align::Fill,
            gtk::Align::Start,
            -1,
            RESIZE_EDGE_THICKNESS,
            "n-resize",
        ),
        (
            gdk::SurfaceEdge::East,
            gtk::Align::End,
            gtk::Align::Fill,
            RESIZE_EDGE_THICKNESS,
            -1,
            "e-resize",
        ),
        (
            gdk::SurfaceEdge::South,
            gtk::Align::Fill,
            gtk::Align::End,
            -1,
            RESIZE_EDGE_THICKNESS,
            "s-resize",
        ),
        (
            gdk::SurfaceEdge::West,
            gtk::Align::Start,
            gtk::Align::Fill,
            RESIZE_EDGE_THICKNESS,
            -1,
            "w-resize",
        ),
        (
            gdk::SurfaceEdge::NorthWest,
            gtk::Align::Start,
            gtk::Align::Start,
            RESIZE_CORNER_SIZE,
            RESIZE_CORNER_SIZE,
            "nw-resize",
        ),
        (
            gdk::SurfaceEdge::NorthEast,
            gtk::Align::End,
            gtk::Align::Start,
            RESIZE_CORNER_SIZE,
            RESIZE_CORNER_SIZE,
            "ne-resize",
        ),
        (
            gdk::SurfaceEdge::SouthEast,
            gtk::Align::End,
            gtk::Align::End,
            RESIZE_CORNER_SIZE,
            RESIZE_CORNER_SIZE,
            "se-resize",
        ),
        // SouthWest is intentionally omitted: the bottom-left corner
        // is occupied by the sidebar collapse / expand toggle in the
        // status bar. The South and West edge handles still cover the
        // rest of the bottom and left sides, so window resize from
        // that quadrant remains possible — just not from the exact
        // corner that hosts the button.
    ] {
        let handle = resize_handle(edge, window, cursor);
        handle.set_halign(halign);
        handle.set_valign(valign);
        handle.set_size_request(width, height);
        shell.add_overlay(&handle);
        shell.set_measure_overlay(&handle, false);
    }
}

fn resize_handle(
    edge: gdk::SurfaceEdge,
    window: &gtk::ApplicationWindow,
    cursor_name: &str,
) -> gtk::Box {
    let handle = gtk::Box::new(gtk::Orientation::Vertical, 0);
    handle.set_cursor_from_name(Some(cursor_name));

    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    let window = window.clone();
    let handle_for_gesture = handle.clone();
    click.connect_pressed(move |click, _n_press, x, y| {
        let Some(surface) = window.surface() else {
            return;
        };
        let Ok(toplevel) = surface.downcast::<gdk::Toplevel>() else {
            return;
        };
        let Some(device) = click.current_event_device() else {
            return;
        };
        let (surface_x, surface_y) = handle_for_gesture
            .compute_point(&window, &gtk::graphene::Point::new(x as f32, y as f32))
            .map(|p| (p.x() as f64, p.y() as f64))
            .unwrap_or((x, y));

        toplevel.begin_resize(
            edge,
            Some(&device),
            click.current_button() as i32,
            surface_x,
            surface_y,
            click.current_event_time(),
        );
    });
    handle.add_controller(click);

    handle
}

#[cfg(test)]
mod tests {
    use gtk::gdk;

    use super::toplevel_state_is_floating;

    #[test]
    fn floating_chrome_only_applies_to_untiled_windows() {
        assert!(toplevel_state_is_floating(gdk::ToplevelState::empty()));
        assert!(!toplevel_state_is_floating(gdk::ToplevelState::MAXIMIZED));
        assert!(!toplevel_state_is_floating(gdk::ToplevelState::FULLSCREEN));
        assert!(!toplevel_state_is_floating(gdk::ToplevelState::TILED));
        assert!(!toplevel_state_is_floating(
            gdk::ToplevelState::LEFT_TILED | gdk::ToplevelState::RIGHT_RESIZABLE
        ));
    }
}
