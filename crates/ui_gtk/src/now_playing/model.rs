// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::time::Duration;

use sustain_app_runtime::{PlaybackState, Track, TrackMetadata};

use crate::util::non_empty_text;

pub(super) fn track_title(track: &Track) -> String {
    crate::util::display_title(track)
}

pub(super) fn artist_album_text(metadata: &TrackMetadata) -> String {
    match (
        non_empty_text(&metadata.artist),
        non_empty_text(&metadata.album),
    ) {
        (Some(artist), Some(album)) => format!("{artist} - {album}"),
        (Some(artist), None) => artist,
        (None, Some(album)) => album,
        (None, None) => String::new(),
    }
}

pub(super) fn playback_position(state: &PlaybackState) -> Option<Duration> {
    match state {
        PlaybackState::Playing { position, .. } | PlaybackState::Paused { position, .. } => {
            Some(*position)
        }
        PlaybackState::Stopped | PlaybackState::Loading { .. } => None,
    }
}

pub(super) fn remaining_time_text(position: Duration, duration: Duration) -> String {
    if duration.is_zero() {
        return String::new();
    }

    format!("-{}", time_text(duration.saturating_sub(position)))
}

pub(super) fn time_text(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let hours = seconds / 3_600;
    let minutes = seconds % 3_600 / 60;
    let seconds = seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(super) fn progress_fraction(position: Duration, duration: Duration) -> f64 {
    if duration.is_zero() {
        return 0.0;
    }

    (position.as_secs_f64() / duration.as_secs_f64()).clamp(0.0, 1.0)
}

pub(super) fn progress_fraction_from_x(x: f64, width: i32) -> Option<f64> {
    if width <= 0 {
        return None;
    }

    Some((x / f64::from(width)).clamp(0.0, 1.0))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sustain_app_runtime::TrackMetadata;

    use super::{
        artist_album_text, progress_fraction, progress_fraction_from_x, remaining_time_text,
        time_text,
    };

    #[test]
    fn artist_album_joins_available_fields() {
        let metadata = TrackMetadata {
            artist: Some("M83".to_owned()),
            album: Some("Hurry Up".to_owned()),
            ..TrackMetadata::default()
        };

        assert_eq!(artist_album_text(&metadata), "M83 - Hurry Up");
    }

    #[test]
    fn time_text_uses_minutes_until_one_hour() {
        assert_eq!(time_text(Duration::from_secs(245)), "4:05");
    }

    #[test]
    fn time_text_uses_hours_when_needed() {
        assert_eq!(time_text(Duration::from_secs(3_665)), "1:01:05");
    }

    #[test]
    fn remaining_time_is_negative_duration_left() {
        assert_eq!(
            remaining_time_text(Duration::from_secs(40), Duration::from_secs(100)),
            "-1:00"
        );
    }

    #[test]
    fn progress_fraction_is_clamped() {
        assert_eq!(
            progress_fraction(Duration::from_secs(150), Duration::from_secs(100)),
            1.0
        );
    }

    #[test]
    fn progress_fraction_from_x_maps_coordinates_to_fraction() {
        assert_eq!(progress_fraction_from_x(25.0, 100), Some(0.25));
    }

    #[test]
    fn progress_fraction_from_x_clamps_outside_coordinates() {
        assert_eq!(progress_fraction_from_x(-10.0, 100), Some(0.0));
        assert_eq!(progress_fraction_from_x(120.0, 100), Some(1.0));
    }

    #[test]
    fn progress_fraction_from_x_ignores_unallocated_width() {
        assert_eq!(progress_fraction_from_x(50.0, 0), None);
    }
}
