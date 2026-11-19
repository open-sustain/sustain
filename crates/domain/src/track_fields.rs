// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Which parts of a [`Track`] differ between two versions of it (#272).
//!
//! A change is computed from the track before and after a commit, never
//! declared by whoever made it, so an edit typed by the user and one an
//! online lookup committed in the background describe themselves the same
//! way. What a difference means to a given view is that view's concern.

use std::ops::{BitOr, BitOrAssign};

use crate::{Track, TrackMetadata};

/// A set of [`Track`] fields. A value that means nothing without its
/// companion, such as a BPM and whether the user set it, shares its
/// member; every value that can change on its own has a member of its own,
/// so a change names exactly what changed.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct TrackFields(u32);

impl TrackFields {
    pub const NONE: Self = Self(0);
    pub const TITLE: Self = Self(1 << 0);
    pub const ARTIST: Self = Self(1 << 1);
    pub const ALBUM: Self = Self(1 << 2);
    pub const ALBUM_ARTIST: Self = Self(1 << 3);
    pub const COMPOSER: Self = Self(1 << 4);
    pub const GROUPING: Self = Self(1 << 5);
    pub const GENRE: Self = Self(1 << 6);
    pub const TRACK_NUMBER: Self = Self(1 << 7);
    pub const TRACK_TOTAL: Self = Self(1 << 8);
    pub const DISC_NUMBER: Self = Self(1 << 9);
    pub const DISC_TOTAL: Self = Self(1 << 10);
    pub const YEAR: Self = Self(1 << 11);
    pub const COMPILATION: Self = Self(1 << 12);
    /// The BPM and whether the user set it.
    pub const BPM: Self = Self(1 << 13);
    /// The key and where it came from.
    pub const KEY: Self = Self(1 << 14);
    pub const COMMENTS: Self = Self(1 << 15);
    pub const LYRICS: Self = Self(1 << 16);
    pub const TITLE_SORT: Self = Self(1 << 17);
    pub const ARTIST_SORT: Self = Self(1 << 18);
    pub const ALBUM_SORT: Self = Self(1 << 19);
    pub const ALBUM_ARTIST_SORT: Self = Self(1 << 20);
    pub const COMPOSER_SORT: Self = Self(1 << 21);
    pub const DURATION: Self = Self(1 << 22);
    /// Bitrate, sample rate, bit depth and channels.
    pub const AUDIO_FORMAT: Self = Self(1 << 23);
    pub const RATING: Self = Self(1 << 24);
    /// Play and skip counts and their dates.
    pub const STATISTICS: Self = Self(1 << 25);
    /// The file's path and whether it is there.
    pub const LOCATION: Self = Self(1 << 26);
    /// The file's size and modification time.
    pub const FILE: Self = Self(1 << 27);
    /// Whether the file embeds artwork.
    pub const EMBEDDED_ARTWORK: Self = Self(1 << 28);
    pub const ALL: Self = Self((1 << 29) - 1);
    /// Every "sort as" name.
    pub const SORT_NAMES: Self = Self::TITLE_SORT
        .union(Self::ARTIST_SORT)
        .union(Self::ALBUM_SORT)
        .union(Self::ALBUM_ARTIST_SORT)
        .union(Self::COMPOSER_SORT);
    /// The metadata a file's tags carry: every editable field from the
    /// title to the "sort as" names. A tag mirror writes these, and the
    /// rating, to the file.
    pub const TAG_METADATA: Self = Self((1 << 22) - 1);

    /// The fields that differ between `old` and `new`, two versions of one
    /// track.
    pub fn between(old: &Track, new: &Track) -> Self {
        // Exhaustive destructuring: a field added to `Track` or
        // `TrackMetadata` does not compile until it is classified here.
        let Track {
            id: _,
            location,
            metadata,
            rating,
            statistics,
            file_size_bytes,
            has_embedded_artwork,
            file_modified_at,
        } = old;
        let mut fields = Self::metadata_between(metadata, &new.metadata);
        fields.include_if(Self::RATING, *rating != new.rating);
        fields.include_if(Self::STATISTICS, *statistics != new.statistics);
        fields.include_if(Self::LOCATION, *location != new.location);
        fields.include_if(
            Self::FILE,
            *file_size_bytes != new.file_size_bytes || *file_modified_at != new.file_modified_at,
        );
        fields.include_if(
            Self::EMBEDDED_ARTWORK,
            *has_embedded_artwork != new.has_embedded_artwork,
        );
        fields
    }

    fn metadata_between(old: &TrackMetadata, new: &TrackMetadata) -> Self {
        let TrackMetadata {
            title,
            artist,
            album,
            album_artist,
            composer,
            grouping,
            genre,
            track_number,
            track_total,
            disc_number,
            disc_total,
            year,
            compilation,
            bpm,
            bpm_set_by_user,
            key,
            key_origin,
            comments,
            lyrics,
            title_sort,
            artist_sort,
            album_sort,
            album_artist_sort,
            composer_sort,
            duration,
            bitrate_kbps,
            sample_rate_hz,
            bit_depth,
            channels,
        } = old;
        let mut fields = Self::NONE;
        fields.include_if(Self::TITLE, *title != new.title);
        fields.include_if(Self::ARTIST, *artist != new.artist);
        fields.include_if(Self::ALBUM, *album != new.album);
        fields.include_if(Self::ALBUM_ARTIST, *album_artist != new.album_artist);
        fields.include_if(Self::COMPOSER, *composer != new.composer);
        fields.include_if(Self::GROUPING, *grouping != new.grouping);
        fields.include_if(Self::GENRE, *genre != new.genre);
        fields.include_if(Self::TRACK_NUMBER, *track_number != new.track_number);
        fields.include_if(Self::TRACK_TOTAL, *track_total != new.track_total);
        fields.include_if(Self::DISC_NUMBER, *disc_number != new.disc_number);
        fields.include_if(Self::DISC_TOTAL, *disc_total != new.disc_total);
        fields.include_if(Self::YEAR, *year != new.year);
        fields.include_if(Self::COMPILATION, *compilation != new.compilation);
        fields.include_if(
            Self::BPM,
            *bpm != new.bpm || *bpm_set_by_user != new.bpm_set_by_user,
        );
        fields.include_if(Self::KEY, *key != new.key || *key_origin != new.key_origin);
        fields.include_if(Self::COMMENTS, *comments != new.comments);
        fields.include_if(Self::LYRICS, *lyrics != new.lyrics);
        fields.include_if(Self::TITLE_SORT, *title_sort != new.title_sort);
        fields.include_if(Self::ARTIST_SORT, *artist_sort != new.artist_sort);
        fields.include_if(Self::ALBUM_SORT, *album_sort != new.album_sort);
        fields.include_if(
            Self::ALBUM_ARTIST_SORT,
            *album_artist_sort != new.album_artist_sort,
        );
        fields.include_if(Self::COMPOSER_SORT, *composer_sort != new.composer_sort);
        fields.include_if(Self::DURATION, *duration != new.duration);
        fields.include_if(
            Self::AUDIO_FORMAT,
            *bitrate_kbps != new.bitrate_kbps
                || *sample_rate_hz != new.sample_rate_hz
                || *bit_depth != new.bit_depth
                || *channels != new.channels,
        );
        fields
    }

    /// Every field of either set; `|` in constant expressions.
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The fields both sets hold.
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether the two sets share a field.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether every field of `other` is in this set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    fn include_if(&mut self, field: Self, differs: bool) {
        if differs {
            *self |= field;
        }
    }
}

impl BitOr for TrackFields {
    type Output = Self;

    fn bitor(self, other: Self) -> Self {
        self.union(other)
    }
}

impl BitOrAssign for TrackFields {
    fn bitor_assign(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

#[cfg(test)]
#[path = "track_fields_tests.rs"]
mod tests;
