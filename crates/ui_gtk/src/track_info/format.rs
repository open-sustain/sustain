// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    path::Path,
    time::{Duration, SystemTime},
};
use sustain_i18n::{gettext, ngettext, tr_format};

use crate::date_format::format_system_time_short;

pub(super) fn format_kind(path: Option<&Path>) -> String {
    let Some(path) = path else {
        return String::from("\u{2014}");
    };
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    match extension.as_deref() {
        Some("mp3") => gettext("MPEG audio file"),
        Some("flac") => gettext("FLAC audio file"),
        Some("ogg" | "oga") => gettext("Ogg Vorbis audio file"),
        Some("opus") => gettext("Opus audio file"),
        Some("m4a" | "m4b" | "mp4") => gettext("MPEG-4 audio file"),
        Some("aif" | "aiff" | "aifc") => gettext("AIFF audio file"),
        Some(other) => tr_format!(
            // Translators: {format} is a file extension in capitals, such as "WAV".
            gettext("{format} audio file"),
            format = other.to_ascii_uppercase()
        ),
        None => gettext("Audio file"),
    }
}

pub(super) fn format_duration_label(duration: Option<Duration>) -> String {
    let Some(duration) = duration else {
        return String::from("\u{2014}");
    };
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3600;
    let minutes = (total_seconds % 3600) / 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(super) fn format_size_label(size: Option<u64>) -> String {
    let Some(size) = size else {
        return String::from("\u{2014}");
    };
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let size_f = size as f64;
    if size_f >= GIB {
        format!("{:.2} GB", size_f / GIB)
    } else if size_f >= MIB {
        format!("{:.2} MB", size_f / MIB)
    } else if size_f >= KIB {
        format!("{:.2} KB", size_f / KIB)
    } else {
        format!("{size} B")
    }
}

pub(super) fn format_optional_unit<T: std::fmt::Display>(value: Option<T>, unit: &str) -> String {
    match value {
        Some(value) => format!("{value} {unit}"),
        None => String::from("\u{2014}"),
    }
}

pub(super) fn format_sample_rate(sample_rate_hz: Option<u32>) -> String {
    match sample_rate_hz {
        Some(hz) => {
            let khz = f64::from(hz) / 1000.0;
            if (khz.fract() - 0.0).abs() < f64::EPSILON {
                format!("{khz:.0} kHz")
            } else {
                format!("{khz:.1} kHz")
            }
        }
        None => String::from("\u{2014}"),
    }
}

pub(super) fn format_channels(channels: Option<u8>) -> String {
    match channels {
        Some(1) => gettext("Mono"),
        Some(2) => gettext("Stereo"),
        Some(count) => tr_format!(
            ngettext("{count} channel", "{count} channels", u32::from(count)),
            count = count
        ),
        None => String::from("\u{2014}"),
    }
}

pub(super) fn format_modified(modified: Option<SystemTime>) -> String {
    modified
        .and_then(format_system_time_short)
        .unwrap_or_else(|| String::from("\u{2014}"))
}

#[cfg(test)]
mod tests {
    use super::{
        format_channels, format_duration_label, format_kind, format_sample_rate, format_size_label,
    };
    use std::path::Path;
    use std::time::Duration;

    #[test]
    fn format_kind_recognises_common_extensions() {
        assert_eq!(format_kind(Some(Path::new("song.mp3"))), "MPEG audio file");
        assert_eq!(format_kind(Some(Path::new("song.flac"))), "FLAC audio file");
        assert_eq!(format_kind(None), "\u{2014}");
    }

    #[test]
    fn format_size_label_uses_binary_prefixes() {
        assert_eq!(format_size_label(Some(512)), "512 B");
        assert_eq!(format_size_label(Some(2_048)), "2.00 KB");
        assert_eq!(format_size_label(Some(5_242_880)), "5.00 MB");
        assert_eq!(format_size_label(None), "\u{2014}");
    }

    #[test]
    fn format_duration_label_includes_hours_when_needed() {
        assert_eq!(
            format_duration_label(Some(Duration::from_secs(245))),
            "4:05"
        );
        assert_eq!(
            format_duration_label(Some(Duration::from_secs(3_904))),
            "1:05:04"
        );
        assert_eq!(format_duration_label(None), "\u{2014}");
    }

    #[test]
    fn format_sample_rate_handles_common_rates() {
        assert_eq!(format_sample_rate(Some(44_100)), "44.1 kHz");
        assert_eq!(format_sample_rate(Some(48_000)), "48 kHz");
        assert_eq!(format_sample_rate(None), "\u{2014}");
    }

    #[test]
    fn format_channels_uses_human_labels() {
        assert_eq!(format_channels(Some(1)), "Mono");
        assert_eq!(format_channels(Some(2)), "Stereo");
        assert_eq!(format_channels(Some(6)), "6 channels");
        assert_eq!(format_channels(None), "\u{2014}");
    }
}
