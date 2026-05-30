// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{path::Path, time::Duration};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub composer: Option<String>,
    pub grouping: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_total: Option<u32>,
    pub year: Option<i32>,
    pub compilation: Option<bool>,
    pub bpm: Option<u32>,
    pub key: Option<String>,
    pub comments: Option<String>,
    pub lyrics: Option<String>,
    /// Tag-derived "sort as" names — the parallel sort fields some
    /// taggers write next to the display fields (ID3 `TSOT`/`TSOP`/
    /// `TSOA`/`TSO2`/`TSOC`, Vorbis `TITLESORT`/`ARTISTSORT`/`ALBUMSORT`/
    /// `ALBUMARTISTSORT`/`COMPOSERSORT`, MP4 equivalents) so "The Beatles"
    /// sorts under **B** and "Björk" sorts as "Bjork". Captured once at
    /// import like every other tag value (see the persistence policy in
    /// AGENTS.md) and used only for ordering, never displayed. Like the
    /// audio-stream properties below they are read-only: not part of
    /// [`MetadataChange`] and not mirrored back to files — editing sort
    /// fields is deferred (issue #13).
    pub title_sort: Option<String>,
    pub artist_sort: Option<String>,
    pub album_sort: Option<String>,
    pub album_artist_sort: Option<String>,
    pub composer_sort: Option<String>,
    pub duration: Option<Duration>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: Option<u32>,
    pub channels: Option<u8>,
}

impl TrackMetadata {
    pub fn apply_change(&mut self, change: &MetadataChange) {
        apply_field_change(&mut self.title, &change.title);
        apply_field_change(&mut self.artist, &change.artist);
        apply_field_change(&mut self.album, &change.album);
        apply_field_change(&mut self.album_artist, &change.album_artist);
        apply_field_change(&mut self.composer, &change.composer);
        apply_field_change(&mut self.grouping, &change.grouping);
        apply_field_change(&mut self.genre, &change.genre);
        apply_field_change(&mut self.track_number, &change.track_number);
        apply_field_change(&mut self.track_total, &change.track_total);
        apply_field_change(&mut self.disc_number, &change.disc_number);
        apply_field_change(&mut self.disc_total, &change.disc_total);
        apply_field_change(&mut self.year, &change.year);
        apply_field_change(&mut self.compilation, &change.compilation);
        apply_field_change(&mut self.bpm, &change.bpm);
        apply_field_change(&mut self.key, &change.key);
        apply_field_change(&mut self.comments, &change.comments);
        apply_field_change(&mut self.lyrics, &change.lyrics);
    }

    /// Refresh the fields that describe the audio stream itself —
    /// duration, bitrate, sample rate, channel count — from a freshly
    /// scanned copy, leaving every tag-derived field (title, artist,
    /// album, year, bpm, comments, …) untouched. Used during library
    /// rescan: per the persistence policy in AGENTS.md, SQLite is the
    /// source of truth for tag-derived metadata once a track has been
    /// imported, but if the underlying file has been re-encoded the
    /// audio-stream properties need to catch up.
    pub fn refresh_audio_stream_properties_from(&mut self, scanned: &Self) {
        self.duration = scanned.duration;
        self.bitrate_kbps = scanned.bitrate_kbps;
        self.sample_rate_hz = scanned.sample_rate_hz;
        self.channels = scanned.channels;
    }

    /// When a file has no Title tag, promote the source file stem to
    /// the title so the only human-readable name we have is captured
    /// in stable storage. Called once per track at import / first
    /// scan; after that the value lives in SQLite and is no longer
    /// derived from the file's name. This is what stops the managed
    /// library planner from mutating its own input on every run: with
    /// title held in the database, the planner never has to fall back
    /// to `source_path.file_stem()` (which changes after each move),
    /// so the planned destination converges instead of accumulating
    /// track-number prefixes one launch at a time.
    pub fn ensure_title_from_filename(&mut self, path: &Path) {
        if self
            .title
            .as_deref()
            .is_some_and(|title| !title.trim().is_empty())
        {
            return;
        }
        if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
            && !stem.trim().is_empty()
        {
            self.title = Some(stem.to_owned());
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum FieldChange<T> {
    #[default]
    Unchanged,
    Set(T),
    Clear,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MetadataChange {
    pub title: FieldChange<String>,
    pub artist: FieldChange<String>,
    pub album: FieldChange<String>,
    pub album_artist: FieldChange<String>,
    pub composer: FieldChange<String>,
    pub grouping: FieldChange<String>,
    pub genre: FieldChange<String>,
    pub track_number: FieldChange<u32>,
    pub track_total: FieldChange<u32>,
    pub disc_number: FieldChange<u32>,
    pub disc_total: FieldChange<u32>,
    pub year: FieldChange<i32>,
    pub compilation: FieldChange<bool>,
    pub bpm: FieldChange<u32>,
    pub key: FieldChange<String>,
    pub comments: FieldChange<String>,
    pub lyrics: FieldChange<String>,
}

fn apply_field_change<T: Clone>(target: &mut Option<T>, change: &FieldChange<T>) {
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(value) => {
            *target = Some(value.clone());
        }
        FieldChange::Clear => {
            *target = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{FieldChange, MetadataChange, TrackMetadata};

    #[test]
    fn ensure_title_from_filename_promotes_stem_when_title_is_missing() {
        let mut metadata = TrackMetadata::default();

        metadata.ensure_title_from_filename(Path::new("/library/Singles/Track - Artist.mp3"));

        assert_eq!(metadata.title.as_deref(), Some("Track - Artist"));
    }

    #[test]
    fn ensure_title_from_filename_promotes_stem_when_title_is_blank() {
        let mut metadata = TrackMetadata {
            title: Some("   ".to_owned()),
            ..TrackMetadata::default()
        };

        metadata.ensure_title_from_filename(Path::new("/library/foo.flac"));

        assert_eq!(metadata.title.as_deref(), Some("foo"));
    }

    #[test]
    fn ensure_title_from_filename_keeps_existing_title() {
        let mut metadata = TrackMetadata {
            title: Some("Real Title".to_owned()),
            ..TrackMetadata::default()
        };

        metadata.ensure_title_from_filename(Path::new("/library/should-not-be-used.mp3"));

        assert_eq!(metadata.title.as_deref(), Some("Real Title"));
    }

    #[test]
    fn ensure_title_from_filename_is_a_noop_when_path_has_no_filename() {
        let mut metadata = TrackMetadata::default();

        metadata.ensure_title_from_filename(Path::new("/"));

        assert_eq!(metadata.title, None);
    }

    #[test]
    fn metadata_changes_default_to_unchanged() {
        let change = MetadataChange::default();

        assert_eq!(change.title, FieldChange::Unchanged);
        assert_eq!(change.artist, FieldChange::Unchanged);
        assert_eq!(change.track_number, FieldChange::Unchanged);
    }

    #[test]
    fn track_metadata_applies_field_changes() {
        let mut metadata = TrackMetadata {
            title: Some("Old".to_owned()),
            artist: Some("Artist".to_owned()),
            track_number: Some(1),
            ..TrackMetadata::default()
        };
        let change = MetadataChange {
            title: FieldChange::Set("New".to_owned()),
            artist: FieldChange::Clear,
            track_number: FieldChange::Unchanged,
            year: FieldChange::Set(1998),
            ..MetadataChange::default()
        };

        metadata.apply_change(&change);

        assert_eq!(metadata.title.as_deref(), Some("New"));
        assert_eq!(metadata.artist, None);
        assert_eq!(metadata.track_number, Some(1));
        assert_eq!(metadata.year, Some(1998));
    }

    #[test]
    fn track_metadata_applies_extended_field_changes() {
        let mut metadata = TrackMetadata::default();
        let change = MetadataChange {
            grouping: FieldChange::Set("Workout".to_owned()),
            track_total: FieldChange::Set(12),
            disc_total: FieldChange::Set(2),
            compilation: FieldChange::Set(true),
            bpm: FieldChange::Set(128),
            key: FieldChange::Set("Am".to_owned()),
            comments: FieldChange::Set("Note".to_owned()),
            ..MetadataChange::default()
        };

        metadata.apply_change(&change);

        assert_eq!(metadata.grouping.as_deref(), Some("Workout"));
        assert_eq!(metadata.track_total, Some(12));
        assert_eq!(metadata.disc_total, Some(2));
        assert_eq!(metadata.compilation, Some(true));
        assert_eq!(metadata.bpm, Some(128));
        assert_eq!(metadata.key.as_deref(), Some("Am"));
        assert_eq!(metadata.comments.as_deref(), Some("Note"));
    }

    #[test]
    fn track_metadata_clears_extended_field_changes() {
        let mut metadata = TrackMetadata {
            grouping: Some("Old group".to_owned()),
            compilation: Some(true),
            bpm: Some(100),
            ..TrackMetadata::default()
        };
        let change = MetadataChange {
            grouping: FieldChange::Clear,
            compilation: FieldChange::Clear,
            bpm: FieldChange::Clear,
            ..MetadataChange::default()
        };

        metadata.apply_change(&change);

        assert_eq!(metadata.grouping, None);
        assert_eq!(metadata.compilation, None);
        assert_eq!(metadata.bpm, None);
    }
}
