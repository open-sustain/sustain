// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The editable fields every native tag carries, read and written through
//! one definition per format.
//!
//! A format implements [`NativeFields`]: how it holds each field. Reading
//! a tag's metadata and applying a change to it are written once here,
//! on top of that, so a field is written exactly where it is read from,
//! and only when the file does not already read as the value to write.

use lofty::{config::ParsingMode, picture::Picture, tag::items::Timestamp};
use sustain_domain::{Bpm, FieldChange, MetadataChange, MusicalKey, Rating, TrackMetadata};

use super::changed;
use crate::MetadataResult;

/// An editable field a tag holds as text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TextField {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Composer,
    Grouping,
    Genre,
    TitleSort,
    ArtistSort,
    AlbumSort,
    AlbumArtistSort,
    ComposerSort,
    /// The musical key, as notation [`MusicalKey`] parses.
    Key,
    Comments,
    Lyrics,
}

/// A number held together with the total it counts towards.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NumberPair {
    Track,
    Disc,
}

/// A number and its total, either of which may be absent.
pub(super) type Numbers = (Option<u32>, Option<u32>);

/// How a tag format holds Sustain's editable fields, its rating, and its
/// cover. Each reader returns what the file holds; each writer replaces
/// the field's items with the one value given, or removes them for
/// `None`, and touches nothing else.
pub(super) trait NativeFields {
    fn text(&self, field: TextField) -> Option<String>;
    fn set_text(&mut self, field: TextField, value: Option<&str>);
    fn numbers(&self, pair: NumberPair) -> Numbers;
    fn set_numbers(&mut self, pair: NumberPair, numbers: Numbers);
    fn year(&self) -> Option<i32>;
    /// Writes a year the tag can hold (see [`Carriage`]).
    fn set_year(&mut self, year: Option<u16>);
    fn compilation(&self) -> Option<bool>;
    fn set_compilation(&mut self, compilation: Option<bool>);
    fn bpm(&self) -> Option<Bpm>;
    /// Writes a tempo the tag can hold (see [`Carriage`]).
    fn set_bpm(&mut self, bpm: Option<Bpm>);
    fn rating(&self) -> Rating;
    /// Writes a rating other than the one the tag already reads as.
    fn set_rating(&mut self, rating: Rating);
    fn cover(&self) -> Option<&Picture>;
    /// Makes `cover` the tag's own cover, or with `None` removes its own,
    /// leaving every picture that is not its own. Returns whether the tag
    /// changed: a tag whose own cover already is `cover` is left alone.
    fn set_cover(&mut self, cover: Option<Picture>) -> MetadataResult<bool>;
}

/// The editable metadata `tag` holds, every other field left at its
/// default.
pub(super) fn read_metadata(tag: &impl NativeFields) -> TrackMetadata {
    let (track_number, track_total) = tag.numbers(NumberPair::Track);
    let (disc_number, disc_total) = tag.numbers(NumberPair::Disc);
    let mut metadata = TrackMetadata {
        title: tag.text(TextField::Title),
        artist: tag.text(TextField::Artist),
        album: tag.text(TextField::Album),
        album_artist: tag.text(TextField::AlbumArtist),
        composer: tag.text(TextField::Composer),
        grouping: tag.text(TextField::Grouping),
        genre: tag.text(TextField::Genre),
        track_number,
        track_total,
        disc_number,
        disc_total,
        year: tag.year(),
        compilation: tag.compilation(),
        bpm: tag.bpm(),
        // A key tag that is not a key in any notation (free text, a URL)
        // is no key.
        key: tag
            .text(TextField::Key)
            .as_deref()
            .and_then(MusicalKey::parse),
        comments: tag.text(TextField::Comments),
        lyrics: tag.text(TextField::Lyrics),
        title_sort: tag.text(TextField::TitleSort),
        artist_sort: tag.text(TextField::ArtistSort),
        album_sort: tag.text(TextField::AlbumSort),
        album_artist_sort: tag.text(TextField::AlbumArtistSort),
        composer_sort: tag.text(TextField::ComposerSort),
        ..TrackMetadata::default()
    };
    metadata.normalize_text_fields();
    metadata
}

/// The change that sets each field `metadata` holds, and leaves every
/// field it does not hold.
pub(super) fn change_setting(metadata: &TrackMetadata) -> MetadataChange {
    fn set<T: Clone>(value: &Option<T>) -> FieldChange<T> {
        value
            .clone()
            .map_or(FieldChange::Unchanged, FieldChange::Set)
    }
    MetadataChange {
        title: set(&metadata.title),
        artist: set(&metadata.artist),
        album: set(&metadata.album),
        album_artist: set(&metadata.album_artist),
        composer: set(&metadata.composer),
        grouping: set(&metadata.grouping),
        genre: set(&metadata.genre),
        track_number: set(&metadata.track_number),
        track_total: set(&metadata.track_total),
        disc_number: set(&metadata.disc_number),
        disc_total: set(&metadata.disc_total),
        year: set(&metadata.year),
        compilation: set(&metadata.compilation),
        bpm: set(&metadata.bpm),
        key: set(&metadata.key),
        comments: set(&metadata.comments),
        lyrics: set(&metadata.lyrics),
        title_sort: set(&metadata.title_sort),
        artist_sort: set(&metadata.artist_sort),
        album_sort: set(&metadata.album_sort),
        album_artist_sort: set(&metadata.album_artist_sort),
        composer_sort: set(&metadata.composer_sort),
    }
}

/// Applies `change` — already as the tag can hold it, see
/// [`Carriage::carried_change`] — writing each field it touches only when
/// the tag does not already read as the field's new value. Returns
/// whether anything was written.
pub(super) fn apply_change(tag: &mut impl NativeFields, change: &MetadataChange) -> bool {
    let current = read_metadata(tag);
    let mut written = false;

    let texts = [
        (TextField::Title, &current.title, &change.title),
        (TextField::Artist, &current.artist, &change.artist),
        (TextField::Album, &current.album, &change.album),
        (
            TextField::AlbumArtist,
            &current.album_artist,
            &change.album_artist,
        ),
        (TextField::Composer, &current.composer, &change.composer),
        (TextField::Grouping, &current.grouping, &change.grouping),
        (TextField::Genre, &current.genre, &change.genre),
        (
            TextField::TitleSort,
            &current.title_sort,
            &change.title_sort,
        ),
        (
            TextField::ArtistSort,
            &current.artist_sort,
            &change.artist_sort,
        ),
        (
            TextField::AlbumSort,
            &current.album_sort,
            &change.album_sort,
        ),
        (
            TextField::AlbumArtistSort,
            &current.album_artist_sort,
            &change.album_artist_sort,
        ),
        (
            TextField::ComposerSort,
            &current.composer_sort,
            &change.composer_sort,
        ),
        (TextField::Comments, &current.comments, &change.comments),
        (TextField::Lyrics, &current.lyrics, &change.lyrics),
    ];
    for (field, value, change) in texts {
        let target = changed(value, change);
        if &target != value {
            tag.set_text(field, target.as_deref());
            written = true;
        }
    }

    let key = changed(&current.key, &change.key);
    if key != current.key {
        tag.set_text(TextField::Key, key.map(MusicalKey::short_code));
        written = true;
    }

    let pairs = [
        (
            NumberPair::Track,
            (current.track_number, current.track_total),
            (&change.track_number, &change.track_total),
        ),
        (
            NumberPair::Disc,
            (current.disc_number, current.disc_total),
            (&change.disc_number, &change.disc_total),
        ),
    ];
    for (pair, (number, total), (number_change, total_change)) in pairs {
        let target = (
            changed(&number, number_change),
            changed(&total, total_change),
        );
        if target != (number, total) {
            tag.set_numbers(pair, target);
            written = true;
        }
    }

    let year = changed(&current.year, &change.year);
    if year != current.year {
        // A carried change holds only years a tag can hold.
        tag.set_year(year.and_then(|year| u16::try_from(year).ok()));
        written = true;
    }
    let compilation = changed(&current.compilation, &change.compilation);
    if compilation != current.compilation {
        tag.set_compilation(compilation);
        written = true;
    }
    let bpm = changed(&current.bpm, &change.bpm);
    if bpm != current.bpm {
        tag.set_bpm(bpm);
        written = true;
    }
    written
}

/// What a tag format can hold of each field's value: reading a field
/// back after writing a value returns the value as carried. A value the
/// format holds in a coarser form (a whole-beat tempo) is carried in that
/// form; a value it cannot hold at all (a year past 9999) is carried as
/// no value, so the write clears the field rather than leave a value the
/// library no longer holds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Carriage {
    /// The tag holds tempo in whole beats.
    pub(super) whole_bpm: bool,
    /// The largest track or disc number or total the tag holds.
    pub(super) largest_number: u32,
    /// The tag cannot tell a number of zero from no number.
    pub(super) zero_is_no_number: bool,
    /// A text value ends at its first NUL, where ID3v2.4 separates values.
    pub(super) text_ends_at_nul: bool,
    /// A genre that names an ID3v1 genre by its number (`17`, `(17)`)
    /// reads as that genre's name.
    pub(super) genre_names_id3v1_numbers: bool,
}

/// The largest year a tag's date holds: four digits.
const LARGEST_YEAR: i32 = 9999;

impl Carriage {
    /// `change`, with every value it sets as the tag can hold it.
    pub(crate) fn carried_change(self, change: MetadataChange) -> MetadataChange {
        let text = |change: FieldChange<String>| self.carried(change, |text| self.text(text));
        let number = |change: FieldChange<u32>| self.carried(change, |number| self.number(number));
        MetadataChange {
            title: text(change.title),
            artist: text(change.artist),
            album: text(change.album),
            album_artist: text(change.album_artist),
            composer: text(change.composer),
            grouping: text(change.grouping),
            genre: self.carried(change.genre, |genre| {
                let genre = self.text(genre)?;
                if self.genre_names_id3v1_numbers {
                    super::id3::genre_as_read(&genre)
                } else {
                    Some(genre)
                }
            }),
            track_number: number(change.track_number),
            track_total: number(change.track_total),
            disc_number: number(change.disc_number),
            disc_total: number(change.disc_total),
            year: self.carried(change.year, |year| {
                (0..=LARGEST_YEAR).contains(&year).then_some(year)
            }),
            compilation: change.compilation,
            bpm: self.carried(change.bpm, |bpm| {
                if self.whole_bpm {
                    Bpm::from_whole(bpm.whole())
                } else {
                    Some(bpm)
                }
            }),
            key: change.key,
            comments: text(change.comments),
            lyrics: text(change.lyrics),
            title_sort: text(change.title_sort),
            artist_sort: text(change.artist_sort),
            album_sort: text(change.album_sort),
            album_artist_sort: text(change.album_artist_sort),
            composer_sort: text(change.composer_sort),
        }
    }

    fn carried<T>(
        self,
        change: FieldChange<T>,
        carry: impl FnOnce(T) -> Option<T>,
    ) -> FieldChange<T> {
        match change {
            FieldChange::Set(value) => carry(value).map_or(FieldChange::Clear, FieldChange::Set),
            other => other,
        }
    }

    fn text(self, mut text: String) -> Option<String> {
        if self.text_ends_at_nul
            && let Some(end) = text.find('\0')
        {
            text.truncate(end);
        }
        // Every format's text reads back without trailing NULs, which
        // lofty takes for a C string's terminator.
        text.truncate(text.trim_end_matches('\0').len());
        (!text.trim().is_empty()).then_some(text)
    }

    fn number(self, number: u32) -> Option<u32> {
        (number <= self.largest_number && !(self.zero_is_no_number && number == 0))
            .then_some(number)
    }
}

/// The number and total in `text`: `"3"`, `"3/12"`, or `"/12"`. A side
/// that is not a number is absent.
pub(super) fn parse_numbers(text: &str) -> Numbers {
    let number = |text: &str| text.trim().parse::<u32>().ok();
    match text.split_once('/') {
        Some((value, total)) => (number(value), number(total)),
        None => (number(text), None),
    }
}

/// The date `text` holds, as lofty reads a tag's date.
pub(super) fn parse_date(text: &str) -> Option<Timestamp> {
    Timestamp::parse(&mut text.as_bytes(), ParsingMode::Relaxed)
        .ok()
        .flatten()
}

/// `date`, or a date of its own, in `year`: a date keeps its month, day,
/// and time when only the year is edited.
pub(super) fn date_in_year(date: Option<Timestamp>, year: u16) -> Timestamp {
    let date = Timestamp {
        year,
        ..date.unwrap_or_default()
    };
    // A 29 February moved to a year without one keeps only its month.
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    if date.month == Some(2) && date.day == Some(29) && !leap {
        return Timestamp {
            year,
            month: date.month,
            ..Timestamp::default()
        };
    }
    date
}

/// Whether `text` reads as a flag, and which way.
pub(super) fn parse_flag(text: &str) -> Option<bool> {
    crate::read::parse_flag(text)
}

/// The text a flag is written as.
pub(super) fn flag_text(flag: bool) -> &'static str {
    if flag { "1" } else { "0" }
}
