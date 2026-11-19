// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The one tag Sustain edits in a file, in that tag format's own model.
//!
//! lofty's generic [`Tag`] can hold only what every format has in common,
//! so a file saved from it loses whatever its own tag held beyond that:
//! unmapped Vorbis comments, an ID3 comment's description, a field's
//! second value, another application's rating frame (#329). Sustain
//! instead edits the format's native tag — [`Id3v2Tag`] for MP3, WAV
//! and AIFF, [`VorbisComments`] for FLAC and Ogg, [`Ilst`] for MP4 — and
//! changes only the items for the fields it writes, leaving every other
//! item exactly as the file holds it.
//!
//! The same field definitions serve reading and writing, so what Sustain
//! imports from a file is what it would write there, and a write is
//! skipped field by field when the file already reads as the value to
//! write: an unchanged field keeps its encoding, its other values, and
//! its frames, and a write that changes nothing leaves the file alone.

use std::{
    io::{Seek, SeekFrom},
    path::Path,
};

use lofty::{
    config::{ParseOptions, WriteOptions},
    error::FileEncodingError,
    file::{AudioFile, FileType, TaggedFile, TaggedFileExt},
    flac::FlacFile,
    id3::v2::Id3v2Tag,
    iff::{aiff::AiffFile, wav::WavFile},
    mp4::{Ilst, Mp4File},
    mpeg::MpegFile,
    ogg::{OggPictureStorage, OpusFile, SpeexFile, VorbisFile, tag::VorbisComments},
    picture::{Picture, PictureType},
    probe::Probe,
    tag::{ItemKey, ItemValue, Tag, TagExt, TagItem, TagType},
};
use sustain_artwork::validate_encoded_artwork;
use sustain_domain::{FieldChange, MetadataChange, Rating, TrackMetadata};

use crate::{MetadataError, MetadataResult};

mod fields;
mod id3;
mod mp4;
mod rating;
mod vorbis;

#[cfg(test)]
mod tests;

pub(crate) use fields::Carriage;
use fields::NativeFields;

/// A tag format Sustain edits natively.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TagFamily {
    Id3v2,
    Vorbis,
    Mp4,
}

impl TagFamily {
    /// The family of the tag Sustain edits in a file of `file_type`, if
    /// Sustain edits that container at all.
    fn of_file_type(file_type: FileType) -> Option<Self> {
        match file_type {
            FileType::Mpeg | FileType::Wav | FileType::Aiff => Some(Self::Id3v2),
            FileType::Flac | FileType::Vorbis | FileType::Opus | FileType::Speex => {
                Some(Self::Vorbis)
            }
            FileType::Mp4 => Some(Self::Mp4),
            _ => None,
        }
    }

    /// The family of a tag of `tag_type`, if Sustain edits that format.
    fn of_tag_type(tag_type: TagType) -> Option<Self> {
        match tag_type {
            TagType::Id3v2 => Some(Self::Id3v2),
            TagType::VorbisComments => Some(Self::Vorbis),
            TagType::Mp4Ilst => Some(Self::Mp4),
            _ => None,
        }
    }

    /// What this family's tag can hold of a field value.
    pub(crate) fn carriage(self) -> Carriage {
        match self {
            Self::Id3v2 => id3::CARRIAGE,
            Self::Vorbis => vorbis::CARRIAGE,
            Self::Mp4 => mp4::CARRIAGE,
        }
    }

    fn empty_tag(self) -> EditableTag {
        match self {
            Self::Id3v2 => EditableTag::Id3v2(Id3v2Tag::new()),
            Self::Vorbis => EditableTag::Vorbis(VorbisComments::new()),
            Self::Mp4 => EditableTag::Mp4(Ilst::new()),
        }
    }

    /// `tag` — another tag the file carries, read through lofty's generic
    /// model — as this family's tag, for reading only.
    fn converted(self, mut tag: Tag) -> EditableTag {
        // A tag holding only a year (ID3v1, APE) holds it under a key no
        // ID3v2 or MP4 frame maps; every native format holds a recording
        // date.
        if tag.get(ItemKey::RecordingDate).is_none() {
            let year = tag.take_strings(ItemKey::Year).next();
            if let Some(year) = year {
                // The year's own tag type may map no recording date.
                tag.insert_unchecked(TagItem::new(ItemKey::RecordingDate, ItemValue::Text(year)));
            }
        }
        match self {
            Self::Id3v2 => EditableTag::Id3v2(Id3v2Tag::from(tag)),
            Self::Vorbis => EditableTag::Vorbis(VorbisComments::from(tag)),
            Self::Mp4 => EditableTag::Mp4(Ilst::from(tag)),
        }
    }
}

/// The tag Sustain edits, in its format's own model.
pub(crate) enum EditableTag {
    Id3v2(Id3v2Tag),
    Vorbis(VorbisComments),
    Mp4(Ilst),
}

macro_rules! with_fields {
    ($tag:expr, $fields:ident => $body:expr) => {
        match $tag {
            EditableTag::Id3v2($fields) => $body,
            EditableTag::Vorbis($fields) => $body,
            EditableTag::Mp4($fields) => $body,
        }
    };
}

impl EditableTag {
    fn family(&self) -> TagFamily {
        match self {
            Self::Id3v2(_) => TagFamily::Id3v2,
            Self::Vorbis(_) => TagFamily::Vorbis,
            Self::Mp4(_) => TagFamily::Mp4,
        }
    }

    /// The editable metadata the tag holds; every other field is left at
    /// its default.
    pub(crate) fn metadata(&self) -> TrackMetadata {
        with_fields!(self, tag => fields::read_metadata(tag))
    }

    pub(crate) fn rating(&self) -> Rating {
        with_fields!(self, tag => tag.rating())
    }

    /// The picture Sustain reads as the track's cover.
    pub(crate) fn cover(&self) -> Option<&Picture> {
        with_fields!(self, tag => tag.cover())
    }

    /// Applies `change`, as this tag can hold it, to the fields it
    /// touches. Returns whether the tag changed.
    pub(crate) fn apply_change(&mut self, change: &MetadataChange) -> bool {
        let change = self.family().carriage().carried_change(change.clone());
        with_fields!(self, tag => fields::apply_change(tag, &change))
    }

    /// Sets the rating on Sustain's rating item. Returns whether the tag
    /// changed.
    pub(crate) fn set_rating(&mut self, rating: Rating) -> bool {
        if self.rating() == rating {
            return false;
        }
        with_fields!(self, tag => tag.set_rating(rating));
        true
    }

    /// Replaces, or with `None` removes, the cover. Returns whether the
    /// tag changed.
    pub(crate) fn set_cover(&mut self, cover: Option<Picture>) -> MetadataResult<bool> {
        with_fields!(self, tag => tag.set_cover(cover))
    }

    /// Saves the tag over the file at `path`, which holds a file of the
    /// tag's container.
    pub(crate) fn save(&mut self, path: &Path) -> MetadataResult<()> {
        match self {
            Self::Id3v2(tag) => id3::prepare_for_save(tag),
            Self::Mp4(tag) => mp4::reencode_utf16_text(tag),
            Self::Vorbis(_) => {}
        }
        crate::tag_write::atomic_save_lofty(path, |temp_path| self.save_to_path(temp_path))
    }

    fn save_to_path(&self, path: &Path) -> Result<(), FileEncodingError> {
        let options = WriteOptions::default();
        match self {
            Self::Id3v2(tag) => tag.save_to_path(path, options),
            Self::Vorbis(tag) => tag.save_to_path(path, options),
            Self::Mp4(tag) => tag.save_to_path(path, options),
        }
    }
}

/// A file as Sustain reads it: the tag it edits taken out of the file's
/// own model, and the rest of the file — its audio properties and any
/// other tag — as lofty's generic file.
pub(crate) struct ParsedFile {
    /// The family of the tag Sustain edits in this container.
    family: TagFamily,
    /// The editable tag, absent when the file carries none.
    editable: Option<EditableTag>,
    /// The audio properties and every other tag.
    pub(crate) rest: TaggedFile,
}

impl ParsedFile {
    /// Reads from the tag Sustain reads values from: the editable tag,
    /// or else another tag the file carries (an MP3's ID3v1 or APE tag, a
    /// WAV's RIFF INFO list, an AIFF's text chunks, a FLAC's ID3v2 tag),
    /// read as an editable one.
    pub(crate) fn with_readable_tag<T>(&self, read: impl FnOnce(Option<&EditableTag>) -> T) -> T {
        match &self.editable {
            Some(tag) => read(Some(tag)),
            None => read(self.other_tag().as_ref()),
        }
    }

    /// The file's other tag, read by its own format's rules (a FLAC's
    /// ID3v2 tag as ID3v2), or else as the format Sustain edits.
    fn other_tag(&self) -> Option<EditableTag> {
        let tag = self.rest.primary_tag().or_else(|| self.rest.first_tag())?;
        let family = TagFamily::of_tag_type(tag.tag_type()).unwrap_or(self.family);
        Some(family.converted(tag.clone()))
    }

    /// The tag format Sustain edits in the file.
    pub(crate) fn family(&self) -> TagFamily {
        self.family
    }

    /// The tag Sustain edits. A file without one gets a new one holding
    /// what Sustain reads from its other tag, so the file reads the same
    /// once the new tag, which reads first, is saved.
    pub(crate) fn into_editable_tag(self) -> MetadataResult<EditableTag> {
        if let Some(tag) = self.editable {
            return Ok(tag);
        }
        let mut tag = self.family.empty_tag();
        if let Some(other) = self.other_tag() {
            tag.apply_change(&fields::change_setting(&other.metadata()));
            tag.set_rating(other.rating());
            tag.set_cover(other.cover().cloned())?;
        }
        Ok(tag)
    }
}

/// Reads the file at `path` as the container its extension names. A
/// container Sustain does not edit natively fails with the error lofty's
/// own read would have been reported as.
pub(crate) fn parse(path: &Path, options: ParseOptions) -> MetadataResult<ParsedFile> {
    let unsupported = || {
        crate::read::container_format_mismatch(path, crate::audio_format_from_path(path).ok())
            .unwrap_or(MetadataError::ReadFailed)
    };
    crate::read::apply_read_policy();
    // The container is the one the extension names, as lofty's own read
    // takes it: content of another container behind it fails to parse as
    // that container, and reports the mismatch.
    let mut probe = Probe::open(path)
        .map_err(|_| MetadataError::ReadFailed)?
        .options(options);
    if probe.file_type().is_none() {
        probe = probe
            .guess_file_type()
            .map_err(|_| MetadataError::ReadFailed)?;
    }
    let file_type = probe.file_type().ok_or_else(unsupported)?;
    // A guessed container (behind an extension lofty does not know, such
    // as `.oga`) may be another than the one the extension names, or one
    // Sustain does not read at all.
    let detected =
        crate::read::audio_format_from_lofty_file_type(file_type).ok_or_else(unsupported)?;
    if let Ok(expected) = crate::audio_format_from_path(path)
        && expected != detected
    {
        return Err(MetadataError::ContainerFormatMismatch { expected, detected });
    }
    let family = TagFamily::of_file_type(file_type).ok_or_else(unsupported)?;
    // lofty's conversions on read merge and reorder a Vorbis or MP4 tag's
    // items (a `TRACKNUMBER` of `3/12`, a `gnre` atom), which the save
    // would then write; Sustain reads those forms itself. ID3v2 needs them
    // to read a v2.3 tag as the v2.4 tag it is saved as.
    let id3v2_options = options;
    let options = match family {
        TagFamily::Id3v2 => options,
        TagFamily::Vorbis | TagFamily::Mp4 => {
            let mut options = options;
            options.implicit_conversions(false)
        }
    };
    let mut reader = probe.into_inner();
    let lofty_error = |error| crate::read::metadata_error_from_lofty(path, error);

    let (editable, rest) = match file_type {
        FileType::Mpeg => {
            let mut file = MpegFile::read_from(&mut reader, options).map_err(lofty_error)?;
            (file.remove_id3v2().map(EditableTag::Id3v2), file.into())
        }
        FileType::Wav => {
            let mut file = WavFile::read_from(&mut reader, options).map_err(lofty_error)?;
            (file.remove_id3v2().map(EditableTag::Id3v2), file.into())
        }
        FileType::Aiff => {
            let mut file = AiffFile::read_from(&mut reader, options).map_err(lofty_error)?;
            (file.remove_id3v2().map(EditableTag::Id3v2), file.into())
        }
        FileType::Flac => {
            let mut file = FlacFile::read_from(&mut reader, options).map_err(lofty_error)?;
            // A FLAC file keeps its pictures in blocks of their own, which
            // lofty writes from the comments' pictures: they belong to the
            // tag Sustain edits.
            let pictures = file.remove_pictures();
            let comments = match file.remove_vorbis_comments() {
                Some(comments) => Some(comments),
                None if pictures.is_empty() => None,
                None => Some(VorbisComments::new()),
            };
            let comments = comments.map(|mut comments| {
                for (picture, information) in pictures {
                    // lofty replaces an existing icon on insert, as the
                    // FLAC specification allows only one of each.
                    let _replaced = comments.insert_picture(picture, Some(information));
                }
                comments
            });
            match comments {
                Some(comments) => (Some(EditableTag::Vorbis(comments)), file.into()),
                // The file's other tag, an ID3v2 one, is read as ID3v2
                // reads it: a v2.3 tag holds its year in a frame only the
                // conversions read.
                None => {
                    reader
                        .seek(SeekFrom::Start(0))
                        .map_err(|_| MetadataError::ReadFailed)?;
                    let file =
                        FlacFile::read_from(&mut reader, id3v2_options).map_err(lofty_error)?;
                    (None, file.into())
                }
            }
        }
        FileType::Vorbis => {
            let mut file = VorbisFile::read_from(&mut reader, options).map_err(lofty_error)?;
            let comments = file.remove_vorbis_comments();
            (Some(EditableTag::Vorbis(comments)), file.into())
        }
        FileType::Opus => {
            let mut file = OpusFile::read_from(&mut reader, options).map_err(lofty_error)?;
            let comments = file.remove_vorbis_comments();
            (Some(EditableTag::Vorbis(comments)), file.into())
        }
        FileType::Speex => {
            let mut file = SpeexFile::read_from(&mut reader, options).map_err(lofty_error)?;
            let comments = file.remove_vorbis_comments();
            (Some(EditableTag::Vorbis(comments)), file.into())
        }
        FileType::Mp4 => {
            let mut file = Mp4File::read_from(&mut reader, options).map_err(lofty_error)?;
            (file.remove_ilst().map(EditableTag::Mp4), file.into())
        }
        _ => return Err(unsupported()),
    };
    Ok(ParsedFile {
        family,
        editable,
        rest,
    })
}

/// The cover among `pictures`: the first valid front cover, else the
/// first valid picture of any type.
pub(crate) fn cover_among<'a>(pictures: impl Iterator<Item = &'a Picture>) -> Option<&'a Picture> {
    let valid: Vec<&Picture> = pictures
        .filter(|picture| validate_encoded_artwork(picture.data()).is_ok())
        .collect();
    valid
        .iter()
        .find(|picture| picture.pic_type() == PictureType::CoverFront)
        .or_else(|| valid.first())
        .copied()
}

/// Whether `owned`, a tag's own cover pictures, already are exactly
/// `cover`: the one picture `cover` holds, or none for `None`.
pub(crate) fn covers_are<'a>(
    mut owned: impl Iterator<Item = &'a Picture>,
    cover: Option<&Picture>,
) -> bool {
    match (owned.next(), cover) {
        (None, None) => true,
        (Some(first), Some(cover)) => first.data() == cover.data() && owned.next().is_none(),
        _ => false,
    }
}

/// The value a field has after `change`, given the value it had.
pub(crate) fn changed<T: Clone>(current: &Option<T>, change: &FieldChange<T>) -> Option<T> {
    match change {
        FieldChange::Unchanged => current.clone(),
        FieldChange::Set(value) => Some(value.clone()),
        FieldChange::Clear => None,
    }
}
