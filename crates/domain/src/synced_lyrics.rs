// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Time-coded lyrics representation and a tolerant LRC parser.
//!
//! LRClib (and most other lyrics providers worth talking to) ships
//! synced lyrics as LRC: a line-oriented text format where each line
//! starts with one or more `[mm:ss.xx]` timestamps and ends with the
//! lyric text. Tags like `[ti:Title]` and `[ar:Artist]` may appear in
//! the header — we ignore those, they duplicate information Sustain
//! already has on the track.
//!
//! The parser exists at the domain layer because:
//!
//! 1. The library store needs to serialize/deserialize lyric lines
//!    without pulling the remote-metadata crate.
//! 2. The UI eventually needs to render and seek through them without
//!    re-running a parse on every load.
//!
//! Storage is JSON via serde so future schema additions (per-line
//! romanisation, word-level offsets) are forward-compatible without
//! touching the parser surface.

use serde::{Deserialize, Serialize};

/// One timestamped line of lyrics. Timestamp is an absolute offset
/// from the start of the track, in milliseconds. Empty `text` is a
/// valid line — LRC files use blank lines as pauses or musical
/// interludes and the player relies on the timing to advance the
/// highlight.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncedLyricsLine {
    pub at_ms: u32,
    pub text: String,
}

/// The full set of timestamped lines for a single track, in
/// chronological order. Two lines may share a timestamp (provider
/// quirk) — we preserve the order they appeared in the source.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SyncedLyrics {
    pub lines: Vec<SyncedLyricsLine>,
}

impl SyncedLyrics {
    /// Parse an LRC document. Tolerant: malformed lines are skipped,
    /// metadata tags (`[ti:...]`, `[ar:...]`, `[length:...]`) are
    /// ignored. The result is sorted by timestamp ascending; LRC
    /// allows multiple timestamps per line, which expand into one
    /// [`SyncedLyricsLine`] per timestamp.
    ///
    /// Returns `None` if no timestamped lines were found — callers
    /// treat this as "the provider returned LRC-shaped text but no
    /// usable timing", which is distinct from "no synced lyrics
    /// available".
    pub fn parse_lrc(source: &str) -> Option<Self> {
        let mut lines: Vec<SyncedLyricsLine> = Vec::new();
        for raw in source.lines() {
            let trimmed = raw.trim_end_matches(['\r', '\n']);
            collect_line(trimmed, &mut lines);
        }
        if lines.is_empty() {
            return None;
        }
        lines.sort_by_key(|line| line.at_ms);
        Some(Self { lines })
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

fn collect_line(raw: &str, lines: &mut Vec<SyncedLyricsLine>) {
    let mut cursor = raw.trim_start();
    let mut timestamps: Vec<u32> = Vec::new();

    while cursor.starts_with('[') {
        let Some(end) = cursor.find(']') else {
            return;
        };
        let tag = &cursor[1..end];
        cursor = cursor[end + 1..].trim_start();

        if let Some(at_ms) = parse_timestamp(tag) {
            timestamps.push(at_ms);
        }
        // Non-timestamp tags ([ti:Foo], [length:03:42], etc.) are
        // silently dropped — they don't contribute lyric content and
        // Sustain already knows everything they describe.
    }

    if timestamps.is_empty() {
        return;
    }

    let text = cursor.to_owned();
    for at_ms in timestamps {
        lines.push(SyncedLyricsLine {
            at_ms,
            text: text.clone(),
        });
    }
}

/// Parse `mm:ss.xx`, `mm:ss.xxx`, or `mm:ss` into milliseconds.
/// Returns `None` for anything else — including bare `mm:ss` with a
/// non-numeric component, which keeps metadata tags like `ti:Title`
/// from being misread as timing.
fn parse_timestamp(raw: &str) -> Option<u32> {
    let (minutes_part, rest) = raw.split_once(':')?;
    let minutes: u32 = minutes_part.parse().ok()?;

    let (seconds_part, fraction_part) = match rest.split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (rest, None),
    };
    let seconds: u32 = seconds_part.parse().ok()?;
    if seconds >= 60 {
        return None;
    }

    let fraction_ms: u32 = match fraction_part {
        None | Some("") => 0,
        Some(digits) => {
            if !digits.chars().all(|c| c.is_ascii_digit()) {
                return None;
            }
            // Normalize to milliseconds: ".12" → 120, ".123" → 123,
            // ".1234" → 123 (truncate, do not round — LRC tooling
            // rarely emits more than 3 fractional digits).
            let truncated = &digits[..digits.len().min(3)];
            let value: u32 = truncated.parse().ok()?;
            match truncated.len() {
                1 => value * 100,
                2 => value * 10,
                _ => value,
            }
        }
    };

    minutes
        .checked_mul(60_000)?
        .checked_add(seconds * 1_000)?
        .checked_add(fraction_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_minimal_lrc_document() {
        let source = "[00:01.50]Hello\n[00:03.00]World\n";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse succeeds");
        assert_eq!(
            parsed.lines,
            vec![
                SyncedLyricsLine {
                    at_ms: 1_500,
                    text: "Hello".to_owned()
                },
                SyncedLyricsLine {
                    at_ms: 3_000,
                    text: "World".to_owned()
                },
            ]
        );
    }

    #[test]
    fn expands_lines_with_multiple_timestamps() {
        let source = "[00:01.00][00:05.00]Refrain";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].at_ms, 1_000);
        assert_eq!(parsed.lines[1].at_ms, 5_000);
        assert!(parsed.lines.iter().all(|line| line.text == "Refrain"));
    }

    #[test]
    fn ignores_metadata_tags() {
        let source = "[ti:Yesterday]\n[ar:Beatles]\n[length:02:05]\n[00:02.00]Words";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].at_ms, 2_000);
    }

    #[test]
    fn sorts_by_timestamp() {
        let source = "[00:10.00]B\n[00:05.00]A";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines[0].text, "A");
        assert_eq!(parsed.lines[1].text, "B");
    }

    #[test]
    fn returns_none_for_text_without_timestamps() {
        assert!(SyncedLyrics::parse_lrc("Just a plain lyric line").is_none());
        assert!(SyncedLyrics::parse_lrc("").is_none());
        assert!(SyncedLyrics::parse_lrc("[ti:Only metadata]").is_none());
    }

    #[test]
    fn empty_text_lines_are_kept() {
        let source = "[00:00.50]\n[00:02.00]Line";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines.len(), 2);
        assert_eq!(parsed.lines[0].text, "");
        assert_eq!(parsed.lines[1].text, "Line");
    }

    #[test]
    fn parses_three_digit_fraction() {
        let source = "[00:00.123]Foo";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines[0].at_ms, 123);
    }

    #[test]
    fn parses_one_digit_fraction() {
        let source = "[00:00.5]Foo";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines[0].at_ms, 500);
    }

    #[test]
    fn parses_timestamp_without_fraction() {
        let source = "[01:30]Foo";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines[0].at_ms, 90_000);
    }

    #[test]
    fn a_timestamp_past_what_milliseconds_can_hold_is_skipped() {
        // 71582:47.000 is the last whole second a `u32` of milliseconds
        // holds; the fraction is what carries it past the end.
        let source = "[71582:47.999]Too far\n[71582:47.295]Last\n[00:01.00]Real";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(
            parsed
                .lines
                .iter()
                .map(|line| line.text.as_str())
                .collect::<Vec<_>>(),
            ["Real", "Last"]
        );
        assert_eq!(parsed.lines[1].at_ms, u32::MAX);
    }

    #[test]
    fn malformed_timestamps_are_skipped_not_panicked() {
        let source = "[xx:yy.zz]Garbage\n[00:01.00]Real";
        let parsed = SyncedLyrics::parse_lrc(source).expect("parse");
        assert_eq!(parsed.lines.len(), 1);
        assert_eq!(parsed.lines[0].text, "Real");
    }

    #[test]
    fn round_trips_through_serde_json() {
        let original = SyncedLyrics {
            lines: vec![
                SyncedLyricsLine {
                    at_ms: 1_000,
                    text: "Hello".to_owned(),
                },
                SyncedLyricsLine {
                    at_ms: 2_500,
                    text: "World".to_owned(),
                },
            ],
        };
        let json = serde_json::to_string(&original).expect("serialize");
        let back: SyncedLyrics = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, original);
    }
}
