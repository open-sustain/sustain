// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::rc::Rc;

use gtk::glib;
use gtk::prelude::*;

use super::super::{
    ApplicationCommand, command_controller::SharedCommandController,
    date_format::format_system_time_short,
};
use super::{HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS};
use sustain_app_runtime::{SmartShuffleEntropy, SmartShuffleIndexMetadata};

pub(super) fn build(
    window: &gtk::Window,
    command_controller: SharedCommandController,
) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    content.append(&build_intro_paragraph());

    let intro_separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    intro_separator.add_css_class("preference-section-separator");
    content.append(&intro_separator);

    let initial = command_controller.runtime().borrow().settings().playback;

    let status_section = build_index_status_section(window, command_controller.clone());
    content.append(&status_section);

    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.add_css_class("preference-section-separator");
    content.append(&separator);

    let entropy_section = build_entropy_section(command_controller, initial.smart_shuffle_entropy);
    content.append(&entropy_section);

    content.upcast()
}

/// Preamble at the top of the Shuffle preferences tab. The Smart
/// Shuffle feature is unusual enough — and divisive enough, since
/// classical pure-random shuffle is what most users expect from a
/// shuffle button — that it earns a few sentences of context here.
/// Frame the *why* (preserving the mood or thread you are already in,
/// not chasing variety for its own sake), the *what* (each next track
/// chosen as a continuation of the one playing, by a fixed perceptual
/// match — there is no learning), and the *where* (everything runs on
/// your own machine).
fn build_intro_paragraph() -> gtk::Widget {
    let label = gtk::Label::new(Some(
        "Smart Shuffle tries to follow your mood by using the current track's \
         genre, tempo, key, era and discovery period to choose the next track. \
         Turning on audio analysis allows for even better picks.",
    ));
    label.add_css_class("preference-helper");
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    label.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    label.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    label.upcast()
}

fn build_index_status_section(
    window: &gtk::Window,
    command_controller: SharedCommandController,
) -> gtk::Widget {
    // Read-only status, centred as a small summary block. The index
    // rebuilds itself on the events that actually change it (library scan,
    // audio-analysis coverage, app launch), so there is nothing to
    // schedule and no button to press — this just reports the current
    // state. The badge mirrors the Pioneer export panel's vocabulary (see
    // `analysis_status_row` in `device_panel`): a green tick when the index
    // is ready, an amber mark when there is none yet.
    let container = gtk::Box::new(gtk::Orientation::Vertical, 4);
    container.set_halign(gtk::Align::Center);

    let status_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    status_row.set_halign(gtk::Align::Center);

    let badge = gtk::Image::new();
    badge.set_pixel_size(18);
    badge.set_valign(gtk::Align::Center);
    status_row.append(&badge);

    let headline = gtk::Label::new(None);
    headline.add_css_class("shuffle-index-headline");
    status_row.append(&headline);
    container.append(&status_row);

    let stats = gtk::Label::new(None);
    stats.add_css_class("shuffle-index-stats");
    stats.set_justify(gtk::Justification::Center);
    container.append(&stats);

    let muted = gtk::Label::new(None);
    muted.add_css_class("preference-helper");
    muted.set_justify(gtk::Justification::Center);
    muted.set_wrap(true);
    muted.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    muted.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    muted.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    container.append(&muted);

    // Live state refresh, driven by the runtime's smart-shuffle state
    // observer. Each fire goes through `idle_add_local_once` because
    // the runtime is mid-borrow when its `apply_smart_shuffle_rebuild_result`
    // emits the signal — we must not re-borrow synchronously.
    let runtime_for_refresh = command_controller.runtime();
    let badge_for_refresh = badge.clone();
    let headline_for_refresh = headline.clone();
    let stats_for_refresh = stats.clone();
    let muted_for_refresh = muted.clone();
    let refresh: Rc<dyn Fn()> = Rc::new(move || {
        let runtime = runtime_for_refresh.borrow();
        let is_rebuilding = runtime.smart_shuffle_is_rebuilding();
        let metadata = runtime.smart_shuffle_metadata();
        let index_loaded = runtime.smart_shuffle_index_is_loaded();
        drop(runtime);
        apply_index_status(
            &badge_for_refresh,
            &headline_for_refresh,
            &stats_for_refresh,
            &muted_for_refresh,
            index_status(is_rebuilding, index_loaded, metadata),
        );
    });
    refresh();

    let runtime_for_install = command_controller.runtime();
    let refresh_for_observer = refresh.clone();
    runtime_for_install
        .borrow_mut()
        .set_smart_shuffle_state_observer(Box::new(move || {
            let refresh = refresh_for_observer.clone();
            glib::idle_add_local_once(move || {
                refresh();
            });
        }));

    // Clear the observer when the Preferences window closes so the
    // closure (and the widgets it captures) can be dropped — without
    // this, the runtime would hold a stale Box<dyn Fn> pointing at
    // dead widgets across the application lifetime.
    let runtime_for_close = command_controller.runtime();
    window.connect_close_request(move |_| {
        runtime_for_close
            .borrow_mut()
            .clear_smart_shuffle_state_observer();
        glib::Propagation::Proceed
    });

    container.upcast()
}

fn build_entropy_section(
    command_controller: SharedCommandController,
    initial_entropy: SmartShuffleEntropy,
) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 4);
    container.add_css_class("preference-slider-row");

    let header = gtk::Label::new(Some("Exploration"));
    header.set_xalign(0.0);
    container.append(&header);

    // Three discrete stops (Focused / Balanced / Adventurous)
    // controlling both the candidate-pool width and the softmax
    // temperature applied to candidate scores. Same shape as the
    // analysis tab's resource-usage slider: tick marks at 0/1/2,
    // snap-to-tick via `round_digits = 0`, and a separate label row
    // for the mark text so end-cap labels don't overflow the scale
    // bounds.
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 2.0, 1.0);
    scale.set_round_digits(0);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.add_mark(0.0, gtk::PositionType::Bottom, None);
    scale.add_mark(1.0, gtk::PositionType::Bottom, None);
    scale.add_mark(2.0, gtk::PositionType::Bottom, None);
    scale.set_value(entropy_to_value(initial_entropy));
    container.append(&scale);

    let mark_label_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    mark_label_row.append(&mark_label("Focused", gtk::Align::Start));
    mark_label_row.append(&mark_label("Balanced", gtk::Align::Center));
    mark_label_row.append(&mark_label("Adventurous", gtk::Align::End));
    container.append(&mark_label_row);

    let caption = gtk::Label::new(Some(entropy_caption(initial_entropy)));
    caption.add_css_class("preference-helper");
    caption.set_xalign(0.0);
    caption.set_wrap(true);
    caption.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    caption.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    caption.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    container.append(&caption);

    let controller = command_controller;
    let caption_for_callback = caption.clone();
    scale.connect_value_changed(move |s| {
        let entropy = value_to_entropy(s.value());
        caption_for_callback.set_text(entropy_caption(entropy));
        let mut settings = controller.runtime().borrow().settings().clone();
        if settings.playback.smart_shuffle_entropy == entropy {
            return;
        }
        settings.playback.smart_shuffle_entropy = entropy;
        let _ = controller.dispatch(ApplicationCommand::UpdateSettings(settings));
    });

    container.upcast()
}

fn mark_label(text: &str, align: gtk::Align) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(align);
    label.set_hexpand(true);
    label
}

fn entropy_to_value(entropy: SmartShuffleEntropy) -> f64 {
    match entropy {
        SmartShuffleEntropy::Focused => 0.0,
        SmartShuffleEntropy::Balanced => 1.0,
        SmartShuffleEntropy::Adventurous => 2.0,
    }
}

fn value_to_entropy(value: f64) -> SmartShuffleEntropy {
    let snapped = value.round() as i32;
    match snapped {
        n if n <= 0 => SmartShuffleEntropy::Focused,
        1 => SmartShuffleEntropy::Balanced,
        _ => SmartShuffleEntropy::Adventurous,
    }
}

fn entropy_caption(entropy: SmartShuffleEntropy) -> &'static str {
    match entropy {
        SmartShuffleEntropy::Focused => {
            "Almost always plays the highest-scoring continuation. \
             Predictable picks, less surprise."
        }
        SmartShuffleEntropy::Balanced => {
            "Favours strong continuations but mixes in the occasional \
             looser match. The default."
        }
        SmartShuffleEntropy::Adventurous => {
            "Casts a wider net and spreads picks more evenly across it. \
             More variety, more deep cuts."
        }
    }
}

/// The index readout's visual state, mirroring the Pioneer export
/// panel's badge vocabulary: a green tick when the index is ready, an
/// amber mark when there is none yet, plus a neutral state for the brief
/// window a rebuild is running.
enum IndexBadge {
    Ready,
    Rebuilding,
    Unbuilt,
}

/// A badge variant, a headline, an emphasised one-line summary, and a
/// muted line beneath it. `stats` and `muted` are absent in states that
/// have nothing to put there.
struct IndexStatus {
    badge: IndexBadge,
    headline: String,
    stats: Option<String>,
    muted: Option<String>,
}

/// Compute the index status. Reports reality (§12): whether the index is
/// ready, how many tracks it covers, how much of the library has audio
/// analysis, and when it was last rebuilt — never "trained", because
/// nothing is trained.
fn index_status(
    is_rebuilding: bool,
    index_loaded: bool,
    metadata: Option<SmartShuffleIndexMetadata>,
) -> IndexStatus {
    if is_rebuilding {
        return IndexStatus {
            badge: IndexBadge::Rebuilding,
            headline: "Rebuilding the Smart Shuffle index…".to_owned(),
            stats: None,
            muted: Some("This usually takes a moment.".to_owned()),
        };
    }
    match (index_loaded, metadata) {
        (true, Some(meta)) => IndexStatus {
            badge: IndexBadge::Ready,
            headline: "Smart Shuffle ready.".to_owned(),
            // One emphasised line: track count plus the audio-enhanced
            // coverage in parentheses. Coverage is an *optional
            // enhancement*, not a measure of whether Smart Shuffle works —
            // the core scorer runs on tag features (genre, tempo, key,
            // year, …) at 0% coverage; this percentage is how much of the
            // library also has the heavier audio analysis (loudness,
            // timbre) that sharpens continuity.
            stats: Some(format!(
                "{} tracks indexed ({}% audio profiled)",
                group_thousands(meta.indexed_track_count),
                (meta.analysis_coverage * 100.0).round() as i64
            )),
            muted: format_system_time_short(meta.built_at)
                .map(|when| format!("Last index rebuild: {when}")),
        },
        _ => IndexStatus {
            badge: IndexBadge::Unbuilt,
            headline: "Smart Shuffle index not built yet.".to_owned(),
            stats: None,
            muted: Some(
                "It builds the first time you turn Smart Shuffle on, then refreshes \
                 whenever your library or its analysis changes."
                    .to_owned(),
            ),
        },
    }
}

/// Push an [`IndexStatus`] onto the badge / headline / stats / muted
/// widgets, swapping the badge icon and the semantic colour classes so
/// the row matches the Pioneer panel's tick / mark styling. The stats and
/// muted labels are hidden when the state has no text for them.
fn apply_index_status(
    badge: &gtk::Image,
    headline: &gtk::Label,
    stats: &gtk::Label,
    muted: &gtk::Label,
    status: IndexStatus,
) {
    headline.set_text(&status.headline);
    set_optional_label(stats, status.stats.as_deref());
    set_optional_label(muted, status.muted.as_deref());

    for class in ["device-analysis-badge", "ok", "warn", "dim-label"] {
        badge.remove_css_class(class);
    }
    for class in ["device-analysis-ok", "device-analysis-warn"] {
        headline.remove_css_class(class);
    }

    match status.badge {
        IndexBadge::Ready => {
            badge.set_icon_name(Some("object-select-symbolic"));
            badge.add_css_class("device-analysis-badge");
            badge.add_css_class("ok");
            headline.add_css_class("device-analysis-ok");
        }
        IndexBadge::Unbuilt => {
            badge.set_icon_name(Some("emblem-important-symbolic"));
            badge.add_css_class("device-analysis-badge");
            badge.add_css_class("warn");
            headline.add_css_class("device-analysis-warn");
        }
        IndexBadge::Rebuilding => {
            // No filled disc here: a plain, dim refresh glyph. The badge
            // classes recolour the symbol to the base colour for contrast
            // against the coloured disc, which would make a disc-less icon
            // invisible — so we leave them off.
            badge.set_icon_name(Some("view-refresh-symbolic"));
            badge.add_css_class("dim-label");
        }
    }
}

/// Set a label's text, hiding it entirely when there is none so it
/// claims no vertical space in the column.
fn set_optional_label(label: &gtk::Label, text: Option<&str>) {
    match text {
        Some(text) => {
            label.set_text(text);
            label.set_visible(true);
        }
        None => label.set_visible(false),
    }
}

/// Format an integer with thousands separators (e.g. `10243` →
/// `10,243`) for the indexed-track-count line.
fn group_thousands(value: u32) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    let len = digits.len();
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (len - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::{SmartShuffleEntropy, entropy_to_value, group_thousands, value_to_entropy};

    #[test]
    fn entropy_round_trips_through_slider_value() {
        for entropy in [
            SmartShuffleEntropy::Focused,
            SmartShuffleEntropy::Balanced,
            SmartShuffleEntropy::Adventurous,
        ] {
            assert_eq!(value_to_entropy(entropy_to_value(entropy)), entropy);
        }
    }

    #[test]
    fn entropy_slider_snaps_between_ticks() {
        assert_eq!(value_to_entropy(0.4), SmartShuffleEntropy::Focused);
        assert_eq!(value_to_entropy(0.6), SmartShuffleEntropy::Balanced);
        assert_eq!(value_to_entropy(1.4), SmartShuffleEntropy::Balanced);
        assert_eq!(value_to_entropy(1.6), SmartShuffleEntropy::Adventurous);
        assert_eq!(value_to_entropy(-1.0), SmartShuffleEntropy::Focused);
        assert_eq!(value_to_entropy(99.0), SmartShuffleEntropy::Adventurous);
    }

    #[test]
    fn thousands_grouping_matches_expectations() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(42), "42");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(10_243), "10,243");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }
}
