// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Sustain's fields in an MP4 `ilst` atom (M4A, M4B, MP4).
//!
//! Each field is the atom lofty maps it to; writing a field replaces its
//! atom with one holding one value, and every other atom stays as the
//! file holds it.
//!
//! The rating is the `rate` atom, in its value's own scale (see
//! [`super::rating`]).
//!
//! The cover is the first image of the `covr` atom, the one iTunes shows
//! (the first valid one, when an image does not decode): MP4 pictures
//! carry no type, so no image is marked as the front cover. Replacing the
//! cover replaces only the first image, and every other image stays as it
//! is (#367). Removing the cover removes every image: each would otherwise
//! read as the cover in its turn.

use std::borrow::Cow;

use lofty::{
    id3::v1::GENRES,
    mp4::{Atom, AtomData, AtomIdent, DataType, Ilst},
    picture::Picture,
};
use sustain_domain::{Bpm, Rating};

use super::fields::{
    Carriage, NativeFields, NumberPair, Numbers, TextField, date_in_year, parse_date, parse_flag,
};
use super::{cover_among, rating::TextScale};
use crate::MetadataResult;

/// `trkn` and `disk` hold each number in 16 bits, and 0 for none.
pub(super) const CARRIAGE: Carriage = Carriage {
    whole_bpm: true,
    largest_number: u16::MAX as u32,
    zero_is_no_number: true,
    text_ends_at_nul: false,
    genre_names_id3v1_numbers: false,
};

const TRACK: AtomIdent<'static> = AtomIdent::Fourcc(*b"trkn");
const DISC: AtomIdent<'static> = AtomIdent::Fourcc(*b"disk");
const DATE: AtomIdent<'static> = AtomIdent::Fourcc(*b"\xa9day");
const COMPILATION: AtomIdent<'static> = AtomIdent::Fourcc(*b"cpil");
const BPM: AtomIdent<'static> = AtomIdent::Fourcc(*b"tmpo");
const RATING: AtomIdent<'static> = AtomIdent::Fourcc(*b"rate");
const COVER: AtomIdent<'static> = AtomIdent::Fourcc(*b"covr");
const GENRE_NUMBER: AtomIdent<'static> = AtomIdent::Fourcc(*b"gnre");

fn text_atom(field: TextField) -> AtomIdent<'static> {
    let fourcc = match field {
        TextField::Title => b"\xa9nam",
        TextField::Artist => b"\xa9ART",
        TextField::Album => b"\xa9alb",
        TextField::AlbumArtist => b"aART",
        TextField::Composer => b"\xa9wrt",
        TextField::Grouping => b"\xa9grp",
        TextField::Genre => b"\xa9gen",
        TextField::TitleSort => b"sonm",
        TextField::ArtistSort => b"soar",
        TextField::AlbumSort => b"soal",
        TextField::AlbumArtistSort => b"soaa",
        TextField::ComposerSort => b"soco",
        TextField::Comments => b"\xa9cmt",
        TextField::Lyrics => b"\xa9lyr",
        TextField::Key => {
            return AtomIdent::Freeform {
                mean: Cow::Borrowed("com.apple.iTunes"),
                name: Cow::Borrowed("initialkey"),
            };
        }
    };
    AtomIdent::Fourcc(*fourcc)
}

/// The first text value of the atom `ident`.
/// Every atom `ident` of `tag`, in order: a file may hold one ident in
/// several atoms, as lofty reads them.
fn atoms<'a>(tag: &'a Ilst, ident: &'a AtomIdent<'_>) -> impl Iterator<Item = &'a Atom<'static>> {
    tag.into_iter().filter(move |atom| atom.ident() == ident)
}

/// The first text among the atoms `ident`.
fn first_text<'a>(tag: &'a Ilst, ident: &'a AtomIdent<'_>) -> Option<&'a str> {
    atoms(tag, ident).find_map(atom_text)
}

fn atom_text<'a>(atom: &'a Atom<'_>) -> Option<&'a str> {
    atom.data().find_map(|data| match data {
        AtomData::UTF8(text) | AtomData::UTF16(text) => Some(text.as_str()),
        _ => None,
    })
}

/// Whether `ident` names the atom `field`: a freeform atom's mean and name
/// compare without case, as iTunes and lofty read them
/// (`com.apple.iTunes:INITIALKEY` is the key).
fn same_atom(ident: &AtomIdent<'_>, field: &AtomIdent<'_>) -> bool {
    match (ident, field) {
        (
            AtomIdent::Freeform { mean, name },
            AtomIdent::Freeform {
                mean: field_mean,
                name: field_name,
            },
        ) => mean.eq_ignore_ascii_case(field_mean) && name.eq_ignore_ascii_case(field_name),
        _ => ident == field,
    }
}

/// The genre a `gnre` atom names: an ID3v1 genre number, plus one.
fn numbered_genre(atom: &Atom<'_>) -> Option<String> {
    let number = atom.data().find_map(|data| match data {
        AtomData::Unknown { data, .. } => data
            .get(..2)
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]])),
        data => atom_integer(data).and_then(|value| u16::try_from(value).ok()),
    })?;
    let index = usize::from(number).checked_sub(1)?;
    GENRES.get(index).map(|genre| (*genre).to_owned())
}

/// Replaces the atom `ident` with one holding `data`, or removes it for
/// `None`. lofty's own replacement reorders the atoms it keeps.
fn set_atom(tag: &mut Ilst, ident: AtomIdent<'static>, data: Option<AtomData>) {
    tag.retain(|atom| atom.ident() != &ident);
    if let Some(data) = data {
        tag.insert(Atom::new(ident, data));
    }
}

/// The number and total of a `trkn` or `disk` atom.
/// The number pair of the atoms `ident`: the first of their data long
/// enough to hold one.
fn atom_numbers(tag: &Ilst, ident: &AtomIdent<'_>) -> Numbers {
    let Some(data) = atoms(tag, ident)
        .flat_map(Atom::data)
        .find_map(|data| match data {
            AtomData::Unknown { data, .. } if data.len() >= 6 => Some(data),
            _ => None,
        })
    else {
        return (None, None);
    };
    let at = |offset: usize| {
        data.get(offset..offset + 2)
            .map(|bytes| u32::from(u16::from_be_bytes([bytes[0], bytes[1]])))
            .filter(|number| *number != 0)
    };
    (at(2), at(4))
}

/// A whole number from an integer atom, or from one some taggers write as
/// text.
fn atom_integer(data: &AtomData) -> Option<u32> {
    match data {
        AtomData::SignedInteger(value) => u32::try_from(*value).ok(),
        AtomData::UnsignedInteger(value) => Some(*value),
        AtomData::UTF8(text) | AtomData::UTF16(text) => text.trim().parse().ok(),
        _ => None,
    }
}

impl NativeFields for Ilst {
    fn text(&self, field: TextField) -> Option<String> {
        let ident = text_atom(field);
        let text = self
            .into_iter()
            .filter(|atom| same_atom(atom.ident(), &ident))
            .find_map(atom_text)
            .map(ToOwned::to_owned);
        match field {
            // An old file may name its genre by an ID3v1 number in `gnre`.
            TextField::Genre => text.or_else(|| self.get(&GENRE_NUMBER).and_then(numbered_genre)),
            _ => text,
        }
    }

    fn set_text(&mut self, field: TextField, value: Option<&str>) {
        let ident = text_atom(field);
        if field == TextField::Genre {
            // `gnre` holds the same field.
            self.retain(|atom| atom.ident() != &GENRE_NUMBER);
        }
        self.retain(|atom| !same_atom(atom.ident(), &ident));
        if let Some(value) = value {
            self.insert(Atom::new(ident, AtomData::UTF8(value.to_owned())));
        }
    }

    fn numbers(&self, pair: NumberPair) -> Numbers {
        match pair {
            NumberPair::Track => atom_numbers(self, &TRACK),
            NumberPair::Disc => atom_numbers(self, &DISC),
        }
    }

    fn set_numbers(&mut self, pair: NumberPair, (number, total): Numbers) {
        let ident = match pair {
            NumberPair::Track => TRACK,
            NumberPair::Disc => DISC,
        };
        // A carried change holds only numbers of 16 bits.
        let bytes =
            |value: Option<u32>| u16::try_from(value.unwrap_or(0)).unwrap_or(0).to_be_bytes();
        let data = (number.is_some() || total.is_some()).then(|| {
            let (number, total) = (bytes(number), bytes(total));
            let mut data = vec![0, 0, number[0], number[1], total[0], total[1]];
            if ident == TRACK {
                // `trkn` carries two trailing reserved bytes `disk` lacks.
                data.extend([0, 0]);
            }
            AtomData::Unknown {
                code: DataType::Reserved,
                data,
            }
        });
        set_atom(self, ident, data);
    }

    fn year(&self) -> Option<i32> {
        first_text(self, &DATE)
            .and_then(parse_date)
            .map(|date| i32::from(date.year))
    }

    fn set_year(&mut self, year: Option<u16>) {
        let date = first_text(self, &DATE).and_then(parse_date);
        set_atom(
            self,
            DATE,
            year.map(|year| AtomData::UTF8(date_in_year(date, year).to_string())),
        );
    }

    fn compilation(&self) -> Option<bool> {
        self.get(&COMPILATION)
            .and_then(|atom| atom.data().next())
            .and_then(|data| match data {
                AtomData::Bool(flag) => Some(*flag),
                AtomData::UTF8(text) | AtomData::UTF16(text) => parse_flag(text),
                data => atom_integer(data).map(|value| value != 0),
            })
    }

    fn set_compilation(&mut self, compilation: Option<bool>) {
        set_atom(self, COMPILATION, compilation.map(AtomData::Bool));
    }

    fn bpm(&self) -> Option<Bpm> {
        let data = self.get(&BPM)?.data().next()?;
        match data {
            AtomData::UTF8(text) | AtomData::UTF16(text) => Bpm::parse(text),
            data => atom_integer(data).and_then(Bpm::from_whole),
        }
    }

    fn set_bpm(&mut self, bpm: Option<Bpm>) {
        // `tmpo` holds whole beats in a 16-bit integer, as iTunes writes
        // it; a carried tempo is at most `Bpm::MAX_WHOLE`.
        let data = bpm.map(|bpm| AtomData::Unknown {
            code: DataType::BeSignedInteger,
            data: u16::try_from(bpm.whole())
                .unwrap_or(u16::MAX)
                .to_be_bytes()
                .to_vec(),
        });
        set_atom(self, BPM, data);
    }

    fn rating(&self) -> Rating {
        first_text(self, &RATING)
            .and_then(TextScale::parse)
            .map_or_else(Rating::unrated, |(_, rating)| rating)
    }

    fn set_rating(&mut self, value: Rating) {
        let scale = first_text(self, &RATING)
            .and_then(TextScale::parse)
            .map_or(TextScale::DEFAULT, |(scale, _)| scale);
        let data = (value != Rating::unrated()).then(|| AtomData::UTF8(scale.text(value)));
        set_atom(self, RATING, data);
    }

    fn cover(&self) -> Option<&Picture> {
        cover_among(
            atoms(self, &COVER)
                .flat_map(Atom::data)
                .filter_map(|data| match data {
                    AtomData::Picture(picture) => Some(picture),
                    _ => None,
                }),
        )
    }

    fn set_cover(&mut self, cover: Option<Picture>) -> MetadataResult<bool> {
        // Every `covr` atom's data in order: a file may split its images
        // across several atoms, which become one.
        let mut images: Vec<AtomData> = atoms(self, &COVER).flat_map(Atom::data).cloned().collect();
        let first = images
            .iter()
            .position(|data| matches!(data, AtomData::Picture(_)));
        match cover {
            Some(cover) => {
                if let Some(index) = first {
                    if matches!(&images[index], AtomData::Picture(own) if own.data() == cover.data())
                    {
                        return Ok(false);
                    }
                    images[index] = AtomData::Picture(cover);
                } else {
                    images.insert(0, AtomData::Picture(cover));
                }
            }
            // No image is typed, so every image would read as the cover in
            // turn: no cover is no image.
            None if images.is_empty() => return Ok(false),
            None => images.clear(),
        }
        self.retain(|atom| atom.ident() != &COVER);
        if let Some(atom) = Atom::from_collection(COVER, images) {
            self.insert(atom);
        }
        Ok(true)
    }
}

/// Re-encodes as UTF-8 every text value `tag` holds as UTF-16. lofty
/// writes a UTF-16 value's bytes as they are in memory, UTF-8, under the
/// UTF-16 type, so saving one as read would leave other text in its place;
/// as UTF-8 it keeps its text.
pub(super) fn reencode_utf16_text(tag: &mut Ilst) {
    let holds_utf16 = (&*tag)
        .into_iter()
        .any(|atom| atom.data().any(|data| matches!(data, AtomData::UTF16(_))));
    if !holds_utf16 {
        return;
    }
    let atoms: Vec<Atom<'static>> = std::mem::take(tag).into_iter().collect();
    for atom in atoms {
        let ident = atom.ident().clone().into_owned();
        let data = atom
            .into_data()
            .map(|data| match data {
                AtomData::UTF16(text) => AtomData::UTF8(text),
                data => data,
            })
            .collect();
        if let Some(atom) = Atom::from_collection(ident, data) {
            tag.insert(atom);
        }
    }
}
