// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The size of a set of tracks, as the status bar and the playlists
//! header show it: how many, how long, how large.

use sustain_app_runtime::Track;

use crate::track_table::TrackTableRow;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TrackSummary {
    pub(crate) track_count: usize,
    pub(crate) duration_seconds: u64,
    pub(crate) size_bytes: u64,
}

impl TrackSummary {
    /// Summarize tracks straight from the library, building no table row
    /// for them: the figures a table of those tracks would show.
    pub(crate) fn of_tracks<'a>(tracks: impl IntoIterator<Item = &'a Track>) -> Self {
        let mut summary = Self::default();
        for track in tracks {
            summary.add(
                TrackTableRow::duration_seconds_of(track),
                TrackTableRow::file_size_bytes_of(track),
            );
        }
        summary
    }

    pub(crate) fn of_rows<'a>(rows: impl IntoIterator<Item = &'a TrackTableRow>) -> Self {
        let mut summary = Self::default();
        for row in rows {
            summary.add_row(row);
        }
        summary
    }

    pub(crate) fn add_row(&mut self, row: &TrackTableRow) {
        self.add(row.duration_seconds, row.file_size_bytes);
    }

    fn add(&mut self, duration_seconds: u64, size_bytes: u64) {
        self.track_count += 1;
        self.duration_seconds += duration_seconds;
        self.size_bytes += size_bytes;
    }
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use sustain_app_runtime::{
        PlayStatistics, Rating, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::*;

    #[test]
    fn tracks_sum_to_what_their_rows_show() {
        let tracks = [
            track(1, Some(Duration::from_millis(61_900)), Some(1_000)),
            track(2, None, None),
            track(3, Some(Duration::from_secs(122)), Some(2_500)),
        ];
        let rows: Vec<_> = tracks
            .iter()
            .map(|track| TrackTableRow::from_track(track, false))
            .collect();

        let summary = TrackSummary::of_tracks(&tracks);

        assert_eq!(summary, TrackSummary::of_rows(&rows));
        assert_eq!(
            summary,
            TrackSummary {
                track_count: 3,
                duration_seconds: 183,
                size_bytes: 3_500,
            },
            "each track counts its whole seconds; unknowns count as zero"
        );
    }

    fn track(id: i64, duration: Option<Duration>, file_size_bytes: Option<u64>) -> Track {
        Track {
            id: TrackId::new(id).expect("positive track id"),
            location: TrackLocation::available(
                TrackRelativePath::new(PathBuf::from(format!("{id}.flac")))
                    .expect("valid relative path"),
            ),
            metadata: TrackMetadata {
                duration,
                ..TrackMetadata::default()
            },
            rating: Rating::default(),
            statistics: PlayStatistics::default(),
            file_size_bytes,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }
}
