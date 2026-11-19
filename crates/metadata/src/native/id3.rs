// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Sustain's fields in an ID3v2 tag (MP3, WAV, AIFF).
//!
//! Each field is the frame lofty maps it to. A field's frame is replaced
//! whole when the field is written, so a field Sustain writes holds one
//! value; a field it does not write keeps every value it held.
//!
//! The frames several applications share follow an owner rule:
//!
//! - comments are the `COMM` frames with an empty description. Sustain
//!   reads the first, writes into it, and clears them all; a described
//!   comment (`iTunNORM`, `iTunSMPB`, …) belongs to the tool that wrote it
//!   and is never touched.
//! - lyrics are the `USLT` frames with an empty description. Sustain
//!   reads the first (or, when the file holds only described lyrics such
//!   as a translation, the first of those), writes into the first of its
//!   own, and clears its own; a described one is never touched.
//! - the album artist is `TPE2`, or else a `TXXX:ALBUMARTIST` (or
//!   `ALBUM ARTIST`) frame some taggers write; writing it replaces both.
//! - the rating is the first `POPM` frame, in its owner's scale (see
//!   [`super::rating`]). Rating it rewrites only that frame's rating byte,
//!   keeping its owner and play counter. Clearing it keeps the frame at
//!   "unknown" (0) while it holds a play counter, or while another
//!   owner's frame would otherwise read as the rating; otherwise it
//!   removes the frame. A file with no frame gets a MusicBee one. Every
//!   other owner's frame is left exactly as it was.
//! - the cover is every front-cover `APIC` frame (see [`super::cover_among`]
//!   for how it is read); other pictures stay.

use std::borrow::Cow;

use lofty::{
    TextEncoding,
    id3::v2::{
        AttachedPictureFrame, CommentFrame, Frame, FrameFlags, FrameId, FrameList, Id3v2Tag,
        Id3v2TagFlags, PopularimeterFrame, TextInformationFrame, TimestampFrame,
        UnsynchronizedTextFrame,
    },
    picture::{Picture, PictureType},
    tag::items::{Lang, UNKNOWN_LANGUAGE},
};
use sustain_domain::{Bpm, Rating};

use super::fields::{
    Carriage, NativeFields, NumberPair, Numbers, TextField, date_in_year, flag_text, parse_date,
    parse_flag, parse_numbers,
};
use super::{cover_among, rating};
use crate::MetadataResult;

/// `TRCK` and `TPOS` hold a total only after a position, so a total
/// without a number is written after a position of 0, which reads as none.
pub(super) const CARRIAGE: Carriage = Carriage {
    whole_bpm: true,
    largest_number: u32::MAX,
    zero_is_no_number: true,
    text_ends_at_nul: true,
    genre_names_id3v1_numbers: true,
};

/// ID3v2.4's separator between a text frame's values.
const VALUE_SEPARATOR: char = '\0';

const fn frame_id(id: &'static str) -> FrameId<'static> {
    FrameId::Valid(Cow::Borrowed(id))
}

const TRACK: FrameId<'static> = frame_id("TRCK");
const DISC: FrameId<'static> = frame_id("TPOS");
const RECORDING_DATE: FrameId<'static> = frame_id("TDRC");
const COMPILATION: FrameId<'static> = frame_id("TCMP");
const BPM: FrameId<'static> = frame_id("TBPM");
const ALBUM_ARTIST: FrameId<'static> = frame_id("TPE2");

/// The text frame a field other than comments and lyrics is held in.
fn text_frame(field: TextField) -> Option<FrameId<'static>> {
    Some(frame_id(match field {
        TextField::Title => "TIT2",
        TextField::Artist => "TPE1",
        TextField::Album => "TALB",
        TextField::AlbumArtist => "TPE2",
        TextField::Composer => "TCOM",
        TextField::Grouping => "TIT1",
        TextField::Genre => "TCON",
        TextField::TitleSort => "TSOT",
        TextField::ArtistSort => "TSOP",
        TextField::AlbumSort => "TSOA",
        TextField::AlbumArtistSort => "TSO2",
        TextField::ComposerSort => "TSOC",
        TextField::Key => "TKEY",
        TextField::Comments | TextField::Lyrics => return None,
    }))
}

/// The first value of the text frame `id`.
fn first_text<'a>(tag: &'a Id3v2Tag, id: &FrameId<'_>) -> Option<&'a str> {
    tag.get_text(id)?.split(VALUE_SEPARATOR).next()
}

/// Replaces every `id` frame with a UTF-8 text frame holding `value`, or
/// removes them for `None`.
fn set_text_frame(tag: &mut Id3v2Tag, id: FrameId<'static>, value: Option<String>) {
    tag.retain(|frame| frame.id() != &id);
    if let Some(value) = value {
        let _replaced = tag.insert(Frame::Text(TextInformationFrame::new(
            id,
            TextEncoding::UTF8,
            value,
        )));
    }
}

fn is_sustain_comment(frame: &Frame<'_>) -> bool {
    matches!(frame, Frame::Comment(comment) if comment.description.is_empty())
}

fn is_sustain_lyrics(frame: &Frame<'_>) -> bool {
    matches!(frame, Frame::UnsynchronizedText(lyrics) if lyrics.description.is_empty())
}

/// A `TXXX` frame some taggers hold the album artist in instead of `TPE2`.
fn is_album_artist_alias(frame: &Frame<'_>) -> bool {
    matches!(frame, Frame::UserText(text)
        if ["ALBUMARTIST", "ALBUM ARTIST"]
            .iter()
            .any(|alias| text.description.eq_ignore_ascii_case(alias)))
}

/// The `USLT` frame Sustain reads lyrics from: the first one without a
/// description, else — for a file whose only lyrics are described, such
/// as a translation — the first one.
fn lyrics_frame(tag: &Id3v2Tag) -> Option<&UnsynchronizedTextFrame<'_>> {
    tag.unsync_text()
        .find(|lyrics| lyrics.description.is_empty())
        .or_else(|| tag.unsync_text().next())
}

/// `genre` as a `TCON` frame holding it reads back: an ID3v1 genre number
/// (`17`, `(17)`) reads as the genre's name.
pub(super) fn genre_as_read(genre: &str) -> Option<String> {
    let mut tag = Id3v2Tag::new();
    let _replaced = tag.insert(Frame::Text(TextInformationFrame::new(
        frame_id("TCON"),
        TextEncoding::UTF8,
        genre.to_owned(),
    )));
    tag.genres()
        .and_then(|mut genres| genres.next())
        .map(ToOwned::to_owned)
}

/// Calls `edit` on the first frame `select` picks, returning whether one
/// was found.
fn edit_first(
    tag: &mut Id3v2Tag,
    mut select: impl FnMut(&Frame<'_>) -> bool,
    edit: impl FnOnce(&mut Frame<'static>),
) -> bool {
    let mut edit = Some(edit);
    tag.retain_mut(|frame| {
        if edit.is_some()
            && select(frame)
            && let Some(edit) = edit.take()
        {
            edit(frame);
        }
        true
    });
    edit.is_none()
}

impl NativeFields for Id3v2Tag {
    fn text(&self, field: TextField) -> Option<String> {
        let text = match field {
            TextField::Comments => self
                .comments()
                .next()
                .and_then(|comment| comment.content.split(VALUE_SEPARATOR).next()),
            TextField::Lyrics => {
                lyrics_frame(self).and_then(|lyrics| lyrics.content.split(VALUE_SEPARATOR).next())
            }
            // A genre may be an ID3v1 genre number, which lofty names.
            TextField::Genre => self.genres().and_then(|mut genres| genres.next()),
            TextField::AlbumArtist => first_text(self, &ALBUM_ARTIST).or_else(|| {
                self.iter().find_map(|frame| match frame {
                    Frame::UserText(text) if is_album_artist_alias(frame) => {
                        text.content.split(VALUE_SEPARATOR).next()
                    }
                    _ => None,
                })
            }),
            field => text_frame(field).and_then(|id| first_text(self, &id)),
        };
        text.map(ToOwned::to_owned)
    }

    fn set_text(&mut self, field: TextField, value: Option<&str>) {
        match field {
            TextField::Comments => match value {
                Some(value) => {
                    let edited = edit_first(self, is_sustain_comment, |frame| {
                        if let Frame::Comment(comment) = frame {
                            comment.encoding = TextEncoding::UTF8;
                            comment.content = Cow::Owned(value.to_owned());
                        }
                    });
                    if !edited {
                        let _replaced = self.insert(Frame::Comment(CommentFrame::new(
                            TextEncoding::UTF8,
                            UNKNOWN_LANGUAGE,
                            String::new(),
                            value.to_owned(),
                        )));
                    }
                }
                None => self.retain(|frame| !is_sustain_comment(frame)),
            },
            TextField::Lyrics => match value {
                Some(value) => {
                    let edited = edit_first(self, is_sustain_lyrics, |frame| {
                        if let Frame::UnsynchronizedText(lyrics) = frame {
                            lyrics.encoding = TextEncoding::UTF8;
                            lyrics.content = Cow::Owned(value.to_owned());
                        }
                    });
                    if !edited {
                        let _replaced =
                            self.insert(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
                                TextEncoding::UTF8,
                                UNKNOWN_LANGUAGE,
                                String::new(),
                                value.to_owned(),
                            )));
                    }
                }
                None => self.retain(|frame| !is_sustain_lyrics(frame)),
            },
            TextField::AlbumArtist => {
                // The alias is the same field: writing it replaces both.
                self.retain(|frame| !is_album_artist_alias(frame));
                set_text_frame(self, ALBUM_ARTIST, value.map(ToOwned::to_owned));
            }
            field => {
                if let Some(id) = text_frame(field) {
                    set_text_frame(self, id, value.map(ToOwned::to_owned));
                }
            }
        }
    }

    fn numbers(&self, pair: NumberPair) -> Numbers {
        let id = match pair {
            NumberPair::Track => TRACK,
            NumberPair::Disc => DISC,
        };
        // ID3v2.4 lets a pair separate its total by a NUL as well as a
        // slash; a position or total of 0 is none.
        let (number, total) = self.get_text(&id).map_or((None, None), |text| {
            parse_numbers(&text.replace(VALUE_SEPARATOR, "/"))
        });
        let some = |value: Option<u32>| value.filter(|value| *value != 0);
        (some(number), some(total))
    }

    fn set_numbers(&mut self, pair: NumberPair, numbers: Numbers) {
        let id = match pair {
            NumberPair::Track => TRACK,
            NumberPair::Disc => DISC,
        };
        let text = match numbers {
            (Some(number), Some(total)) => Some(format!("{number}/{total}")),
            (Some(number), None) => Some(number.to_string()),
            (None, Some(total)) => Some(format!("0/{total}")),
            (None, None) => None,
        };
        set_text_frame(self, id, text);
    }

    fn year(&self) -> Option<i32> {
        recording_date(self).map(|date| i32::from(date.year))
    }

    fn set_year(&mut self, year: Option<u16>) {
        let date = year.map(|year| date_in_year(recording_date(self), year));
        self.retain(|frame| frame.id() != &RECORDING_DATE);
        if let Some(date) = date {
            let _replaced = self.insert(Frame::Timestamp(TimestampFrame::new(
                RECORDING_DATE,
                TextEncoding::UTF8,
                date,
            )));
        }
    }

    fn compilation(&self) -> Option<bool> {
        first_text(self, &COMPILATION).and_then(parse_flag)
    }

    fn set_compilation(&mut self, compilation: Option<bool>) {
        set_text_frame(
            self,
            COMPILATION,
            compilation.map(|flag| flag_text(flag).to_owned()),
        );
    }

    fn bpm(&self) -> Option<Bpm> {
        first_text(self, &BPM).and_then(Bpm::parse)
    }

    fn set_bpm(&mut self, bpm: Option<Bpm>) {
        // `TBPM` holds whole beats.
        set_text_frame(self, BPM, bpm.map(|bpm| bpm.whole().to_string()));
    }

    fn rating(&self) -> Rating {
        first_popm(self).map_or_else(Rating::unrated, |popm| {
            rating::popm_rating(&popm.email, popm.rating)
        })
    }

    fn set_rating(&mut self, value: Rating) {
        let popm_frames = self
            .iter()
            .filter(|frame| matches!(frame, Frame::Popularimeter(_)))
            .count();
        let Some(ours) = first_popm(self) else {
            // An unrated file without a frame already reads as unrated.
            let owner = rating::DEFAULT_POPM_OWNER;
            let _replaced = self.insert(Frame::Popularimeter(PopularimeterFrame::new(
                owner,
                rating::popm_byte(owner, value),
                0,
            )));
            return;
        };
        let remove = value == Rating::unrated() && ours.counter == 0 && popm_frames == 1;
        if remove {
            self.retain(|frame| !matches!(frame, Frame::Popularimeter(_)));
            return;
        }
        let _edited = edit_first(
            self,
            |frame| matches!(frame, Frame::Popularimeter(_)),
            |frame| {
                if let Frame::Popularimeter(popm) = frame {
                    popm.rating = rating::popm_byte(&popm.email, value);
                }
            },
        );
    }

    fn cover(&self) -> Option<&Picture> {
        cover_among(self.iter().filter_map(|frame| match frame {
            Frame::Picture(AttachedPictureFrame { picture, .. }) => Some(picture.as_ref()),
            _ => None,
        }))
    }

    fn set_cover(&mut self, cover: Option<Picture>) -> MetadataResult<bool> {
        let fronts = self.iter().filter_map(|frame| match frame {
            Frame::Picture(AttachedPictureFrame { picture, .. })
                if picture.pic_type() == PictureType::CoverFront =>
            {
                Some(picture.as_ref())
            }
            _ => None,
        });
        if super::covers_are(fronts, cover.as_ref()) {
            return Ok(false);
        }
        self.remove_picture_type(PictureType::CoverFront);
        if let Some(cover) = cover {
            let _replaced = self.insert(Frame::Picture(AttachedPictureFrame::new(
                TextEncoding::UTF8,
                cover,
            )));
        }
        Ok(true)
    }
}

fn first_popm(tag: &Id3v2Tag) -> Option<&PopularimeterFrame<'static>> {
    tag.iter().find_map(|frame| match frame {
        Frame::Popularimeter(popm) => Some(popm),
        _ => None,
    })
}

/// The recording date: a `TDRC` frame lofty parsed as a date, or one it
/// holds as text — as a tag converted from another format does.
fn recording_date(tag: &Id3v2Tag) -> Option<lofty::tag::items::Timestamp> {
    match tag.get(&RECORDING_DATE)? {
        Frame::Timestamp(date) => Some(date.timestamp),
        Frame::Text(text) => parse_date(&text.value),
        _ => None,
    }
}

/// lofty's write-time rule for an ID3v2 language (each of the three bytes
/// ASCII-alphabetic). A value like `\0\0\0`, which some taggers emit and
/// lofty accepts on read but rejects on write, makes the whole frame
/// unserializable.
fn is_valid_language(language: &Lang) -> bool {
    language.iter().all(u8::is_ascii_alphabetic)
}

/// Readies `tag` for lofty's writer, which writes every frame the file
/// held back with it.
///
/// It heals the malformed languages of `COMM` and `USLT` frames, the only
/// frames whose language lofty validates when saving, so a tag another
/// tagger left structurally broken can be saved again (#193). A malformed
/// language becomes the specification's "undefined" `XXX`; the text and
/// description stay, and a valid language is never touched.
///
/// It also clears the tag's unsynchronisation flag, as it does each
/// frame's (see [`prepare_frames`]): lofty writes the frames as decoded.
pub(super) fn prepare_for_save(tag: &mut Id3v2Tag) {
    tag.set_flags(Id3v2TagFlags {
        unsynchronisation: false,
        ..*tag.flags()
    });
    prepare_frames(tag);
}

/// Readies `frames`, and the frames a chapter or table of contents among
/// them embeds, which are saved with it, for lofty's writer: heals each
/// invalid language, and clears the encoding flags of each frame lofty
/// decoded on read. lofty writes a frame's content as decoded, yet keeps
/// its flags, so a kept compression, unsynchronisation, or data length
/// flag would make the next reader misread the content.
fn prepare_frames(frames: &mut FrameList<'_>) {
    frames.retain_mut(|frame| {
        let flags = frame.flags();
        // lofty keeps an encrypted frame's content as it was read.
        if flags.encryption.is_none() {
            frame.set_flags(FrameFlags {
                compression: false,
                unsynchronisation: false,
                data_length_indicator: None,
                ..flags
            });
        }
        let language = match frame {
            Frame::Comment(comment) => &mut comment.language,
            Frame::UnsynchronizedText(lyrics) => &mut lyrics.language,
            Frame::Chapter(chapter) => {
                prepare_frames(&mut chapter.children);
                return true;
            }
            Frame::TableOfContents(contents) => {
                prepare_frames(&mut contents.children);
                return true;
            }
            _ => return true,
        };
        if !is_valid_language(language) {
            *language = UNKNOWN_LANGUAGE;
        }
        true
    });
}
