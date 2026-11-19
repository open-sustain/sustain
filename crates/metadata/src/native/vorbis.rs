// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Sustain's fields in Vorbis comments (FLAC, Ogg Vorbis, Opus, Speex).
//!
//! Each field is the comment key lofty maps it to, plus `COMPOSERSORT`,
//! which lofty does not map. Where applications disagree on a key
//! (`ALBUMARTIST` or `ALBUM ARTIST`), Sustain reads the first comment
//! under either, in the file's order, and writes the first key. Writing a
//! field replaces every value under all its keys with one value, so a
//! field Sustain writes holds one value, and a field it does not write
//! keeps every value and its order.
//!
//! The rating is the first `RATING` or `RATING:<owner>` comment Sustain
//! can read, in its value's own scale (see [`super::rating`]). Rating it
//! rewrites that comment's value, keeping its key; clearing it removes the
//! comment, or sets it to zero while another rating comment would
//! otherwise read as the rating. A file with none gets a plain `RATING`
//! percentage. Comments Sustain cannot read as a rating are never
//! touched.
//!
//! The cover is every front-cover picture (see [`super::cover_among`] for
//! how it is read); other pictures stay.

use lofty::{
    ogg::{OggPictureStorage, tag::VorbisComments},
    picture::{Picture, PictureType},
    tag::items::Timestamp,
};
use sustain_domain::{Bpm, Rating};

use super::fields::{
    Carriage, NativeFields, NumberPair, Numbers, TextField, date_in_year, flag_text, parse_date,
    parse_flag, parse_numbers,
};
use super::{cover_among, rating::TextScale};
use crate::{MetadataError, MetadataResult};

pub(super) const CARRIAGE: Carriage = Carriage {
    whole_bpm: false,
    largest_number: u32::MAX,
    zero_is_no_number: false,
    text_ends_at_nul: false,
    genre_names_id3v1_numbers: false,
};

const TRACK_NUMBER: &str = "TRACKNUMBER";
const TRACK_TOTAL: [&str; 2] = ["TRACKTOTAL", "TOTALTRACKS"];
const DISC_NUMBER: &str = "DISCNUMBER";
const DISC_TOTAL: [&str; 2] = ["DISCTOTAL", "TOTALDISCS"];
const DATE: [&str; 2] = ["DATE", "YEAR"];
const COMPILATION: &str = "COMPILATION";
const BPM: &str = "BPM";
const RATING: &str = "RATING";

/// The keys a text field is held under, the one Sustain writes first.
fn text_keys(field: TextField) -> &'static [&'static str] {
    match field {
        TextField::Title => &["TITLE"],
        TextField::Artist => &["ARTIST"],
        TextField::Album => &["ALBUM"],
        TextField::AlbumArtist => &["ALBUMARTIST", "ALBUM ARTIST"],
        TextField::Composer => &["COMPOSER"],
        TextField::Grouping => &["GROUPING"],
        TextField::Genre => &["GENRE"],
        TextField::TitleSort => &["TITLESORT"],
        TextField::ArtistSort => &["ARTISTSORT"],
        TextField::AlbumSort => &["ALBUMSORT"],
        TextField::AlbumArtistSort => &["ALBUMARTISTSORT"],
        TextField::ComposerSort => &["COMPOSERSORT"],
        TextField::Key => &["INITIALKEY", "KEY"],
        TextField::Comments => &["COMMENT"],
        TextField::Lyrics => &["LYRICS"],
    }
}

/// The first value, in the comments' order, under any of `keys`.
fn first_value<'a>(tag: &'a VorbisComments, keys: &[&str]) -> Option<&'a str> {
    tag.items()
        .find(|(key, _)| {
            keys.iter()
                .any(|candidate| key.eq_ignore_ascii_case(candidate))
        })
        .map(|(_, value)| value)
}

/// Rewrites the comments' items through `edit`, which sees each in order
/// and returns the items to keep in its place. lofty's own removal
/// reorders the items it keeps, which would put a field's second value
/// first.
fn edit_items(
    tag: &mut VorbisComments,
    mut edit: impl FnMut(String, String) -> Option<(String, String)>,
) {
    let items: Vec<_> = tag.take_items().collect();
    for (key, value) in items {
        if let Some((key, value)) = edit(key, value) {
            tag.push(key, value);
        }
    }
}

/// Replaces every value under any of `keys` with `value` under the first
/// key, or removes them for `None`.
fn set_values(tag: &mut VorbisComments, keys: &[&str], value: Option<String>) {
    edit_items(tag, |key, item| {
        (!keys
            .iter()
            .any(|candidate| key.eq_ignore_ascii_case(candidate)))
        .then_some((key, item))
    });
    if let Some(value) = value {
        tag.push(keys[0].to_owned(), value);
    }
}

/// The date the track was recorded: a `DATE` comment, which holds the
/// full date, before a `YEAR` one, wherever each stands in the comments.
fn recording_date(tag: &VorbisComments) -> Option<Timestamp> {
    DATE.iter()
        .find_map(|key| first_value(tag, &[key]).and_then(parse_date))
}

fn is_rating_key(key: &str) -> bool {
    key.get(..RATING.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(RATING))
        && matches!(key.as_bytes().get(RATING.len()), None | Some(b':'))
}

/// The position among the items of each rating comment Sustain can read,
/// with its scale and rating.
fn readable_ratings(tag: &VorbisComments) -> impl Iterator<Item = (usize, TextScale, Rating)> + '_ {
    tag.items()
        .enumerate()
        .filter(|(_, (key, _))| is_rating_key(key))
        .filter_map(|(index, (_, value))| {
            TextScale::parse(value).map(|(scale, rating)| (index, scale, rating))
        })
}

impl NativeFields for VorbisComments {
    fn text(&self, field: TextField) -> Option<String> {
        first_value(self, text_keys(field)).map(ToOwned::to_owned)
    }

    fn set_text(&mut self, field: TextField, value: Option<&str>) {
        set_values(self, text_keys(field), value.map(ToOwned::to_owned));
    }

    fn numbers(&self, pair: NumberPair) -> Numbers {
        let (number_key, total_keys) = match pair {
            NumberPair::Track => (TRACK_NUMBER, TRACK_TOTAL),
            NumberPair::Disc => (DISC_NUMBER, DISC_TOTAL),
        };
        // A number may carry its total ("3/12") when lofty's read did not
        // split it.
        let (number, inline_total) = self.get(number_key).map_or((None, None), parse_numbers);
        let total = first_value(self, &total_keys)
            .and_then(|total| parse_numbers(total).0)
            .or(inline_total);
        (number, total)
    }

    fn set_numbers(&mut self, pair: NumberPair, (number, total): Numbers) {
        let (number_key, total_keys) = match pair {
            NumberPair::Track => (TRACK_NUMBER, TRACK_TOTAL),
            NumberPair::Disc => (DISC_NUMBER, DISC_TOTAL),
        };
        // The number and the total are separate comments, and only the
        // one that changes is rewritten: an untouched one keeps every
        // value it holds, even one Sustain cannot read ("A1").
        let (current_number, current_total) = self.numbers(pair);
        let total_comment = first_value(self, &total_keys).is_some();
        if number != current_number {
            set_values(self, &[number_key], number.map(|number| number.to_string()));
            // The number held the total ("3/12"), which keeps its own
            // comment from now on.
            if !total_comment && total == current_total {
                set_values(self, &total_keys, total.map(|total| total.to_string()));
            }
        }
        if total != current_total {
            // A number holding the old total keeps only its own part.
            edit_items(self, |key, value| {
                if !key.eq_ignore_ascii_case(number_key) {
                    return Some((key, value));
                }
                match value.split_once('/') {
                    Some((own, _)) if own.trim().is_empty() => None,
                    Some((own, _)) => Some((key, own.trim().to_owned())),
                    None => Some((key, value)),
                }
            });
            set_values(self, &total_keys, total.map(|total| total.to_string()));
        }
    }

    fn year(&self) -> Option<i32> {
        recording_date(self).map(|date| i32::from(date.year))
    }

    fn set_year(&mut self, year: Option<u16>) {
        let date = recording_date(self);
        let value = year.map(|year| date_in_year(date, year).to_string());
        set_values(self, &DATE, value);
    }

    fn compilation(&self) -> Option<bool> {
        self.get(COMPILATION).and_then(parse_flag)
    }

    fn set_compilation(&mut self, compilation: Option<bool>) {
        set_values(
            self,
            &[COMPILATION],
            compilation.map(|flag| flag_text(flag).to_owned()),
        );
    }

    fn bpm(&self) -> Option<Bpm> {
        self.get(BPM).and_then(Bpm::parse)
    }

    fn set_bpm(&mut self, bpm: Option<Bpm>) {
        // `BPM` holds the exact tempo.
        set_values(self, &[BPM], bpm.map(|bpm| bpm.to_string()));
    }

    fn rating(&self) -> Rating {
        readable_ratings(self)
            .next()
            .map_or_else(Rating::unrated, |(_, _, rating)| rating)
    }

    fn set_rating(&mut self, value: Rating) {
        let (ours, others_follow) = {
            let mut readable = readable_ratings(self);
            let ours = readable.next().map(|(index, scale, _)| (index, scale));
            (ours, readable.next().is_some())
        };
        let Some((ours, scale)) = ours else {
            self.push(RATING.to_owned(), TextScale::DEFAULT.text(value));
            return;
        };
        let remove = value == Rating::unrated() && !others_follow;
        let mut index = 0;
        edit_items(self, |key, item| {
            let this = index;
            index += 1;
            if this != ours {
                Some((key, item))
            } else if remove {
                None
            } else {
                Some((key, scale.text(value)))
            }
        });
    }

    fn cover(&self) -> Option<&Picture> {
        cover_among(self.pictures().iter().map(|(picture, _)| picture))
    }

    fn set_cover(&mut self, cover: Option<Picture>) -> MetadataResult<bool> {
        let fronts = self
            .pictures()
            .iter()
            .map(|(picture, _)| picture)
            .filter(|picture| picture.pic_type() == PictureType::CoverFront);
        if super::covers_are(fronts, cover.as_ref()) {
            return Ok(false);
        }
        self.remove_picture_type(PictureType::CoverFront);
        if let Some(cover) = cover {
            let _replaced = self
                .insert_picture(cover, None)
                .map_err(|_| MetadataError::WriteFailed)?;
        }
        Ok(true)
    }
}
