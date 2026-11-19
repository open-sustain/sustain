// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Native tag edits on real files, read back through lofty's own decoder
//! of each format rather than through Sustain's field definitions, so a
//! write that lands somewhere Sustain also reads from cannot pass.

use std::{
    borrow::Cow,
    fs::{self, File},
    path::{Path, PathBuf},
};

use lofty::{
    TextEncoding,
    config::{ParseOptions, WriteOptions},
    file::AudioFile,
    flac::FlacFile,
    id3::{
        v1::{GENRES, Id3v1Tag},
        v2::{
            CommentFrame, ExtendedTextFrame, Frame, FrameId, Id3v2Tag, PopularimeterFrame,
            TextInformationFrame, UnsynchronizedTextFrame,
        },
    },
    mp4::{Atom, AtomData, AtomIdent, DataType, Ilst, Mp4File},
    mpeg::MpegFile,
    ogg::{OggPictureStorage, tag::VorbisComments},
    picture::{MimeType, Picture, PictureType},
    tag::TagExt,
};
use sustain_domain::{Bpm, FieldChange, MetadataChange};

use crate::tests::{
    id3v2_3_text_frame, id3v2_4_frame, mp3_with_id3v2_3_frames, mp3_with_id3v2_4_frames,
    mpeg_audio_frames, synchsafe_size, tiny_png, unique_test_directory,
};
use crate::{LoftyMetadataService, MetadataError, MetadataService, Rating};

const COVER: AtomIdent<'static> = AtomIdent::Fourcc(*b"covr");

fn rating(stars: u8) -> Rating {
    Rating::new(stars).expect("rating")
}

fn testdata(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// An MP3 at `path` holding `tag`.
fn write_mp3(path: &Path, tag: &Id3v2Tag) {
    let mut bytes = Vec::new();
    tag.dump_to(&mut bytes, WriteOptions::default())
        .expect("dump ID3v2");
    bytes.extend_from_slice(&mpeg_audio_frames(4));
    fs::write(path, bytes).expect("write MP3");
}

/// A FLAC at `path` holding `items` as its comments, in order.
fn write_flac(path: &Path, items: &[(&str, &str)]) {
    fs::copy(testdata("../analysis/testdata/sine.flac"), path).expect("copy FLAC");
    let mut comments = VorbisComments::new();
    for (key, value) in items {
        comments.push((*key).to_owned(), (*value).to_owned());
    }
    comments
        .save_to_path(path, WriteOptions::default())
        .expect("save comments");
}

/// An M4A at `path` holding `ilst`.
fn write_m4a(path: &Path, ilst: &Ilst) {
    fs::copy(testdata("../analysis/testdata/sine-aac.m4a"), path).expect("copy M4A");
    ilst.save_to_path(path, WriteOptions::default())
        .expect("save ilst");
}

fn id3v2(path: &Path) -> Id3v2Tag {
    let mut file = File::open(path).expect("open MP3");
    MpegFile::read_from(&mut file, ParseOptions::new())
        .expect("read MP3")
        .id3v2()
        .cloned()
        // lofty writes no tag that holds nothing.
        .unwrap_or_else(Id3v2Tag::new)
}

fn vorbis_items(path: &Path) -> Vec<(String, String)> {
    let mut file = File::open(path).expect("open FLAC");
    FlacFile::read_from(&mut file, ParseOptions::new().implicit_conversions(false))
        .expect("read FLAC")
        .vorbis_comments()
        .expect("comments")
        .items()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn ilst(path: &Path) -> Ilst {
    let mut file = File::open(path).expect("open M4A");
    Mp4File::read_from(&mut file, ParseOptions::new().implicit_conversions(false))
        .expect("read M4A")
        .ilst()
        .cloned()
        .unwrap_or_else(Ilst::new)
}

fn items(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// Every `POPM` frame of `tag`: owner, rating byte, play counter.
fn popms(tag: &Id3v2Tag) -> Vec<(String, u8, u64)> {
    tag.iter()
        .filter_map(|frame| match frame {
            Frame::Popularimeter(popm) => Some((popm.email.to_string(), popm.rating, popm.counter)),
            _ => None,
        })
        .collect()
}

/// Every `COMM` frame of `tag`: description and text.
fn comments(tag: &Id3v2Tag) -> Vec<(String, String)> {
    tag.iter()
        .filter_map(|frame| match frame {
            Frame::Comment(comment) => {
                Some((comment.description.to_string(), comment.content.to_string()))
            }
            _ => None,
        })
        .collect()
}

fn text_frame(tag: &Id3v2Tag, id: &str) -> Option<String> {
    tag.iter().find_map(|frame| match frame {
        Frame::Text(text) if text.id().as_str() == id => Some(text.value.to_string()),
        _ => None,
    })
}

fn with_directory(test: impl FnOnce(&Path)) {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    test(&root);
    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn an_id3_edit_leaves_other_tools_frames_as_they_were() {
    with_directory(|root| {
        let path = root.join("foreign.mp3");
        let mut tag = Id3v2Tag::new();
        let _ = tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Borrowed("TPE1")),
            TextEncoding::UTF8,
            "First\0Second",
        )));
        let _ = tag.insert(Frame::Comment(CommentFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            "iTunNORM",
            " 0000044E 00000000",
        )));
        let _ = tag.insert(Frame::Comment(CommentFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            "",
            "Old comment",
        )));
        let _ = tag.insert(Frame::Popularimeter(PopularimeterFrame::new(
            "MusicBee", 128, 5,
        )));
        let _ = tag.insert(Frame::Popularimeter(PopularimeterFrame::new(
            "other@example.org",
            64,
            9,
        )));
        write_mp3(&path, &tag);
        let service = LoftyMetadataService;
        assert_eq!(
            service.read_persisted_tags(&path).expect("read").rating,
            rating(3)
        );

        service
            .write_metadata(
                &path,
                MetadataChange {
                    title: FieldChange::Set("Title".to_owned()),
                    comments: FieldChange::Set("New comment".to_owned()),
                    ..MetadataChange::default()
                },
            )
            .expect("write metadata");
        service.write_rating(&path, rating(5)).expect("rate");

        let written = id3v2(&path);
        assert_eq!(text_frame(&written, "TIT2").as_deref(), Some("Title"));
        assert_eq!(
            text_frame(&written, "TPE1").as_deref(),
            Some("First\0Second"),
            "an artist the edit did not touch keeps both values"
        );
        assert_eq!(
            comments(&written),
            [
                ("iTunNORM".to_owned(), " 0000044E 00000000".to_owned()),
                (String::new(), "New comment".to_owned()),
            ]
        );
        assert_eq!(
            popms(&written),
            [
                ("MusicBee".to_owned(), 255, 5),
                ("other@example.org".to_owned(), 64, 9),
            ]
        );

        service
            .write_rating(&path, Rating::unrated())
            .expect("clear rating");
        assert_eq!(
            popms(&id3v2(&path)),
            [
                ("MusicBee".to_owned(), 0, 5),
                ("other@example.org".to_owned(), 64, 9),
            ],
            "clearing keeps the frame at unknown while another owner's would read"
        );
        assert_eq!(
            service.read_persisted_tags(&path).expect("read").rating,
            Rating::unrated()
        );

        service
            .write_metadata(
                &path,
                MetadataChange {
                    comments: FieldChange::Clear,
                    ..MetadataChange::default()
                },
            )
            .expect("clear comments");
        assert_eq!(
            comments(&id3v2(&path)),
            [("iTunNORM".to_owned(), " 0000044E 00000000".to_owned())],
            "clearing the comment leaves a described one"
        );
    });
}

#[test]
fn a_popm_frame_of_zero_is_unrated_and_rating_a_file_without_one_adds_musicbees() {
    with_directory(|root| {
        let path = root.join("unrated.mp3");
        let mut tag = Id3v2Tag::new();
        let _ = tag.insert(Frame::Popularimeter(PopularimeterFrame::new(
            "Windows Media Player 9 Series",
            0,
            0,
        )));
        write_mp3(&path, &tag);
        let service = LoftyMetadataService;
        assert_eq!(
            service.read_persisted_tags(&path).expect("read").rating,
            Rating::unrated()
        );

        let bare = root.join("bare.mp3");
        write_mp3(&bare, &Id3v2Tag::new());
        let before = fs::read(&bare).expect("read bytes");
        service
            .write_rating(&bare, Rating::unrated())
            .expect("clear an unrated file");
        assert_eq!(
            fs::read(&bare).expect("read bytes"),
            before,
            "a write that changes nothing leaves the file alone"
        );
        service.write_rating(&bare, rating(2)).expect("rate");
        assert_eq!(popms(&id3v2(&bare)), [("MusicBee".to_owned(), 64, 0)]);
        service
            .write_rating(&bare, Rating::unrated())
            .expect("clear rating");
        assert_eq!(
            popms(&id3v2(&bare)),
            [],
            "a counterless frame alone is removed when cleared"
        );
    });
}

#[test]
fn a_vorbis_edit_rewrites_only_the_comments_of_the_fields_it_changes() {
    with_directory(|root| {
        let path = root.join("foreign.flac");
        write_flac(
            &path,
            &[
                ("ARTIST", "First"),
                ("CUESHEET", "FILE \"a.wav\" WAVE"),
                ("ARTIST", "Second"),
                ("LYRICS", "Old lyrics"),
                ("MOOD", "Happy"),
                ("RATING", "80"),
            ],
        );
        let service = LoftyMetadataService;
        assert_eq!(
            service.read_persisted_tags(&path).expect("read").rating,
            rating(4)
        );

        service
            .write_metadata(
                &path,
                MetadataChange {
                    lyrics: FieldChange::Set("New lyrics".to_owned()),
                    ..MetadataChange::default()
                },
            )
            .expect("write lyrics");
        service.write_rating(&path, rating(2)).expect("rate");
        assert_eq!(
            vorbis_items(&path),
            items(&[
                ("ARTIST", "First"),
                ("CUESHEET", "FILE \"a.wav\" WAVE"),
                ("ARTIST", "Second"),
                ("MOOD", "Happy"),
                ("RATING", "40"),
                ("LYRICS", "New lyrics"),
            ])
        );

        service
            .write_rating(&path, Rating::unrated())
            .expect("clear rating");
        assert!(
            !vorbis_items(&path).iter().any(|(key, _)| key == "RATING"),
            "a lone rating comment is removed when cleared"
        );
    });
}

#[test]
fn a_vorbis_rating_keeps_its_owners_key_and_scale() {
    with_directory(|root| {
        let path = root.join("fraction.flac");
        write_flac(
            &path,
            &[("RATING:user@example.org", "0.6"), ("RATING", "20")],
        );
        let service = LoftyMetadataService;
        assert_eq!(
            service.read_persisted_tags(&path).expect("read").rating,
            rating(3)
        );

        service.write_rating(&path, rating(5)).expect("rate");
        assert_eq!(
            vorbis_items(&path),
            items(&[("RATING:user@example.org", "1.0"), ("RATING", "20")])
        );
        service
            .write_rating(&path, Rating::unrated())
            .expect("clear rating");
        assert_eq!(
            vorbis_items(&path),
            items(&[("RATING:user@example.org", "0.0"), ("RATING", "20")]),
            "clearing keeps the comment at zero while another would read"
        );
        assert_eq!(
            service.read_persisted_tags(&path).expect("read").rating,
            Rating::unrated()
        );
    });
}

#[test]
fn a_tempo_is_carried_as_each_format_holds_it() {
    with_directory(|root| {
        let tempo = Bpm::from_hundredths(11_886).expect("tempo");
        let change = MetadataChange {
            bpm: FieldChange::Set(tempo),
            ..MetadataChange::default()
        };
        let service = LoftyMetadataService;

        let mp3 = root.join("tempo.mp3");
        write_mp3(&mp3, &Id3v2Tag::new());
        let flac = root.join("tempo.flac");
        write_flac(&flac, &[]);
        for (path, held) in [(&mp3, Bpm::from_whole(119)), (&flac, Some(tempo))] {
            assert_eq!(
                service
                    .carried_change(path, change.clone())
                    .expect("carry")
                    .bpm,
                held.map_or(FieldChange::Clear, FieldChange::Set),
                "{path:?}"
            );
            service
                .write_metadata(path, change.clone())
                .expect("write tempo");
            assert_eq!(
                service
                    .read_persisted_tags(path)
                    .expect("read")
                    .metadata
                    .bpm,
                held,
                "{path:?}"
            );
        }
        assert_eq!(text_frame(&id3v2(&mp3), "TBPM").as_deref(), Some("119"));
        assert!(vorbis_items(&flac).contains(&("BPM".to_owned(), "118.86".to_owned())));
    });
}

/// A valid PNG distinct from [`tiny_png`]: an image's dimensions are read
/// from its header, so bytes past its end change only its identity.
fn other_png(marker: u8) -> Vec<u8> {
    let mut png = tiny_png();
    png.push(marker);
    png
}

fn covr_images(tag: &Ilst) -> Vec<Vec<u8>> {
    tag.get(&COVER)
        .map(|atom| {
            atom.data()
                .filter_map(|data| match data {
                    AtomData::Picture(picture) => Some(picture.data().to_vec()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn an_m4a_cover_replaces_the_first_covr_image_and_a_clear_removes_them_all() {
    with_directory(|root| {
        let path = root.join("cover.m4a");
        let picture = |data: Vec<u8>| {
            AtomData::Picture(Picture::unchecked(data).mime_type(MimeType::Png).build())
        };
        let mut tag = Ilst::new();
        tag.insert(
            Atom::from_collection(COVER, vec![picture(other_png(1)), picture(other_png(2))])
                .expect("covr atom"),
        );
        write_m4a(&path, &tag);
        let service = LoftyMetadataService;
        assert_eq!(
            service.read_artwork(&path).expect("read"),
            Some(other_png(1))
        );

        service
            .write_artwork(&path, Some(tiny_png()))
            .expect("replace cover");
        assert_eq!(covr_images(&ilst(&path)), [tiny_png(), other_png(2)]);
        assert_eq!(service.read_artwork(&path).expect("read"), Some(tiny_png()));

        service.write_artwork(&path, None).expect("clear cover");
        assert_eq!(covr_images(&ilst(&path)), Vec::<Vec<u8>>::new());
        assert_eq!(service.read_artwork(&path).expect("read"), None);

        let bare = root.join("bare.m4a");
        write_m4a(&bare, &Ilst::new());
        service
            .write_artwork(&bare, Some(tiny_png()))
            .expect("add cover");
        assert_eq!(covr_images(&ilst(&bare)), [tiny_png()]);
        assert_eq!(service.read_artwork(&bare).expect("read"), Some(tiny_png()));
        service.write_artwork(&bare, None).expect("clear cover");
        assert_eq!(covr_images(&ilst(&bare)), Vec::<Vec<u8>>::new());
        assert_eq!(service.read_artwork(&bare).expect("read"), None);
    });
}

#[test]
fn an_m4a_cover_spread_over_several_covr_atoms_is_one_list_of_images() {
    with_directory(|root| {
        let path = root.join("atoms.m4a");
        let picture = |data: Vec<u8>| {
            AtomData::Picture(Picture::unchecked(data).mime_type(MimeType::Png).build())
        };
        let mut tag = Ilst::new();
        tag.insert(Atom::new(COVER, picture(other_png(1))));
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"cvr2"),
            picture(other_png(2)),
        ));
        write_m4a(&path, &tag);
        rename_atom(&path, b"cvr2", b"covr");
        let cleared = root.join("cleared.m4a");
        fs::copy(&path, &cleared).expect("copy M4A");
        let service = LoftyMetadataService;
        assert_eq!(
            service.read_artwork(&path).expect("read"),
            Some(other_png(1))
        );

        service
            .write_artwork(&path, Some(tiny_png()))
            .expect("replace cover");
        assert_eq!(covr_images(&ilst(&path)), [tiny_png(), other_png(2)]);

        service.write_artwork(&cleared, None).expect("clear cover");
        assert_eq!(covr_images(&ilst(&cleared)), Vec::<Vec<u8>>::new());
        assert_eq!(service.read_artwork(&cleared).expect("read"), None);
    });
}

#[test]
fn an_m4a_cover_is_read_from_a_later_covr_atom_when_the_first_holds_none() {
    with_directory(|root| {
        let path = root.join("atoms.m4a");
        let picture = |data: Vec<u8>| {
            AtomData::Picture(Picture::unchecked(data).mime_type(MimeType::Png).build())
        };
        let mut tag = Ilst::new();
        tag.insert(Atom::new(COVER, picture(b"not an image".to_vec())));
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"cvr2"),
            picture(other_png(2)),
        ));
        write_m4a(&path, &tag);
        rename_atom(&path, b"cvr2", b"covr");
        assert_eq!(
            LoftyMetadataService.read_artwork(&path).expect("read"),
            Some(other_png(2))
        );
    });
}

/// Renames the atom `from` of the M4A at `path` to `to`: lofty merges the
/// atoms of one ident, so a file holding two is written under a
/// placeholder ident first.
fn rename_atom(path: &Path, from: &[u8; 4], to: &[u8; 4]) {
    let mut bytes = fs::read(path).expect("read M4A");
    let at = bytes
        .windows(4)
        .position(|window| window == from)
        .expect("placeholder atom");
    bytes[at..at + 4].copy_from_slice(to);
    fs::write(path, bytes).expect("rename atom");
}

#[test]
fn sort_fields_land_in_the_comments_other_applications_read() {
    with_directory(|root| {
        let path = root.join("sort.flac");
        write_flac(&path, &[]);
        LoftyMetadataService
            .write_metadata(
                &path,
                MetadataChange {
                    title_sort: FieldChange::Set("Title".to_owned()),
                    artist_sort: FieldChange::Set("Artist".to_owned()),
                    album_sort: FieldChange::Set("Album".to_owned()),
                    album_artist_sort: FieldChange::Set("Album Artist".to_owned()),
                    composer_sort: FieldChange::Set("Composer".to_owned()),
                    ..MetadataChange::default()
                },
            )
            .expect("write sort fields");
        let mut written = vorbis_items(&path);
        written.sort();
        assert_eq!(
            written,
            items(&[
                ("ALBUMARTISTSORT", "Album Artist"),
                ("ALBUMSORT", "Album"),
                ("ARTISTSORT", "Artist"),
                ("COMPOSERSORT", "Composer"),
                ("TITLESORT", "Title"),
            ])
        );
    });
}

fn change(edit: impl FnOnce(&mut MetadataChange)) -> MetadataChange {
    let mut change = MetadataChange::default();
    edit(&mut change);
    change
}

/// Every `USLT` frame of `tag`: description and text.
fn lyrics_frames(tag: &Id3v2Tag) -> Vec<(String, String)> {
    tag.iter()
        .filter_map(|frame| match frame {
            Frame::UnsynchronizedText(lyrics) => {
                Some((lyrics.description.to_string(), lyrics.content.to_string()))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn id3_lyrics_with_a_description_belong_to_another_tool() {
    with_directory(|root| {
        let path = root.join("lyrics.mp3");
        let mut tag = Id3v2Tag::new();
        let _ = tag.insert(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
            TextEncoding::UTF8,
            *b"fra",
            "translation".to_owned(),
            "Traduction".to_owned(),
        )));
        let _ = tag.insert(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            String::new(),
            "Words".to_owned(),
        )));
        write_mp3(&path, &tag);
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .lyrics
                .as_deref(),
            Some("Words"),
            "the lyrics without a description are the track's own"
        );

        service
            .write_metadata(&path, change(|change| change.lyrics = FieldChange::Clear))
            .expect("clear lyrics");
        assert_eq!(
            lyrics_frames(&id3v2(&path)),
            [("translation".to_owned(), "Traduction".to_owned())]
        );

        service
            .write_metadata(
                &path,
                change(|change| change.lyrics = FieldChange::Set("New".to_owned())),
            )
            .expect("set lyrics");
        let mut written = lyrics_frames(&id3v2(&path));
        written.sort();
        assert_eq!(
            written,
            [
                (String::new(), "New".to_owned()),
                ("translation".to_owned(), "Traduction".to_owned()),
            ]
        );
    });
}

#[test]
fn an_id3_album_artist_in_a_user_text_frame_is_read_and_replaced() {
    with_directory(|root| {
        let path = root.join("album-artist.mp3");
        let mut tag = Id3v2Tag::new();
        let _ = tag.insert(Frame::UserText(ExtendedTextFrame::new(
            TextEncoding::UTF8,
            "AlbumArtist".to_owned(),
            "Alias".to_owned(),
        )));
        write_mp3(&path, &tag);
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .album_artist
                .as_deref(),
            Some("Alias")
        );

        service
            .write_metadata(
                &path,
                change(|change| change.album_artist = FieldChange::Set("New".to_owned())),
            )
            .expect("write album artist");
        let written = id3v2(&path);
        assert_eq!(text_frame(&written, "TPE2").as_deref(), Some("New"));
        assert!(
            !written
                .iter()
                .any(|frame| matches!(frame, Frame::UserText(_))),
            "the alias does not linger with the old value"
        );
    });
}

#[test]
fn an_id3_number_pair_reads_either_separator_and_keeps_a_lone_total() {
    with_directory(|root| {
        let path = root.join("numbers.mp3");
        let mut tag = Id3v2Tag::new();
        let _ = tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Borrowed("TRCK")),
            TextEncoding::UTF8,
            "3\u{0}12",
        )));
        write_mp3(&path, &tag);
        let service = LoftyMetadataService;
        let metadata = service.read_persisted_tags(&path).expect("read").metadata;
        assert_eq!(
            (metadata.track_number, metadata.track_total),
            (Some(3), Some(12))
        );

        service
            .write_metadata(
                &path,
                change(|change| change.track_number = FieldChange::Clear),
            )
            .expect("clear track number");
        assert_eq!(text_frame(&id3v2(&path), "TRCK").as_deref(), Some("0/12"));
        let metadata = service.read_persisted_tags(&path).expect("read").metadata;
        assert_eq!(
            (metadata.track_number, metadata.track_total),
            (None, Some(12))
        );
    });
}

#[test]
fn an_id3_genre_typed_as_a_number_is_carried_as_its_name() {
    with_directory(|root| {
        let path = root.join("genre.mp3");
        write_mp3(&path, &Id3v2Tag::new());
        let service = LoftyMetadataService;
        let typed = change(|change| change.genre = FieldChange::Set("17".to_owned()));
        assert_eq!(
            service
                .carried_change(&path, typed.clone())
                .expect("carry")
                .genre,
            FieldChange::Set("Rock".to_owned())
        );
        service.write_metadata(&path, typed).expect("write genre");
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .genre
                .as_deref(),
            Some("Rock")
        );
    });
}

#[test]
fn an_id3v1_only_mp3_keeps_its_values_through_an_edit() {
    with_directory(|root| {
        let path = root.join("v1.mp3");
        let mut v1 = Id3v1Tag::new();
        v1.title = Some("Old Title".to_owned());
        v1.artist = Some("Old Artist".to_owned());
        v1.year = Some(1999);
        let mut bytes = mpeg_audio_frames(4);
        v1.dump_to(&mut bytes, WriteOptions::default())
            .expect("dump ID3v1");
        fs::write(&path, bytes).expect("write MP3");
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .year,
            Some(1999)
        );

        service
            .write_metadata(
                &path,
                change(|change| change.genre = FieldChange::Set("Jazz".to_owned())),
            )
            .expect("write genre");
        let metadata = service.read_persisted_tags(&path).expect("read").metadata;
        assert_eq!(metadata.genre.as_deref(), Some("Jazz"));
        assert_eq!(metadata.title.as_deref(), Some("Old Title"));
        assert_eq!(metadata.artist.as_deref(), Some("Old Artist"));
        assert_eq!(metadata.year, Some(1999));
    });
}

#[test]
fn an_m4a_key_and_numbered_genre_are_read_as_other_tools_write_them() {
    with_directory(|root| {
        let path = root.join("aliases.m4a");
        let mut tag = Ilst::new();
        tag.insert(Atom::new(
            AtomIdent::Freeform {
                mean: Cow::Borrowed("com.apple.iTunes"),
                name: Cow::Borrowed("INITIALKEY"),
            },
            AtomData::UTF8("Am".to_owned()),
        ));
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"gnre"),
            AtomData::Unknown {
                code: DataType::Reserved,
                data: vec![0, 18],
            },
        ));
        write_m4a(&path, &tag);
        let service = LoftyMetadataService;
        let metadata = service.read_persisted_tags(&path).expect("read").metadata;
        assert_eq!(
            metadata.key.map(|key| key.to_string()).as_deref(),
            Some("Am")
        );
        assert_eq!(metadata.genre.as_deref(), Some(GENRES[17]));

        service
            .write_metadata(
                &path,
                change(|change| change.genre = FieldChange::Set("Jazz".to_owned())),
            )
            .expect("write genre");
        let written = ilst(&path);
        assert!(written.get(&AtomIdent::Fourcc(*b"gnre")).is_none());
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .genre
                .as_deref(),
            Some("Jazz")
        );
    });
}

#[test]
fn m4a_utf16_text_survives_an_unrelated_edit() {
    with_directory(|root| {
        let path = root.join("utf16.m4a");
        let mut tag = Ilst::new();
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"\xa9nam"),
            AtomData::UTF8("\0A\0B".to_owned()),
        ));
        write_m4a(&path, &tag);
        // Retype the title's data from UTF-8 (1) to UTF-16 (2): its bytes
        // are then "AB" in big-endian UTF-16, as some taggers write it.
        let mut bytes = fs::read(&path).expect("read M4A");
        let utf8 = b"data\0\0\0\x01\0\0\0\0\0A\0B";
        let at = bytes
            .windows(utf8.len())
            .position(|window| window == utf8)
            .expect("title data");
        bytes[at + 7] = 2;
        fs::write(&path, bytes).expect("retype title");
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .title
                .as_deref(),
            Some("AB")
        );

        service
            .write_metadata(
                &path,
                change(|change| change.artist = FieldChange::Set("Artist".to_owned())),
            )
            .expect("write artist");
        let metadata = service.read_persisted_tags(&path).expect("read").metadata;
        assert_eq!(metadata.title.as_deref(), Some("AB"));
        assert_eq!(metadata.artist.as_deref(), Some("Artist"));
    });
}

#[test]
fn vorbis_values_are_read_in_file_order_and_untouched_repeats_survive() {
    with_directory(|root| {
        let path = root.join("order.flac");
        write_flac(
            &path,
            &[
                ("ALBUM ARTIST", "First"),
                ("ALBUMARTIST", "Second"),
                ("TRACKNUMBER", "1"),
                ("TRACKNUMBER", "2"),
            ],
        );
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .album_artist
                .as_deref(),
            Some("First")
        );

        service
            .write_metadata(
                &path,
                change(|change| change.title = FieldChange::Set("Title".to_owned())),
            )
            .expect("write title");
        assert_eq!(
            vorbis_items(&path),
            items(&[
                ("ALBUM ARTIST", "First"),
                ("ALBUMARTIST", "Second"),
                ("TRACKNUMBER", "1"),
                ("TRACKNUMBER", "2"),
                ("TITLE", "Title"),
            ])
        );
    });
}

#[test]
fn an_edit_refuses_content_its_extension_does_not_name() {
    with_directory(|root| {
        let path = root.join("flac.oga");
        write_flac(&path, &[("TITLE", "Title")]);
        assert!(matches!(
            LoftyMetadataService.write_metadata(
                &path,
                change(|change| change.title = FieldChange::Set("New".to_owned())),
            ),
            Err(MetadataError::ContainerFormatMismatch { .. })
        ));
    });
}

#[test]
fn an_id3_edit_heals_a_language_inside_a_chapter() {
    with_directory(|root| {
        let path = root.join("chapter.mp3");
        let mut comment = vec![0x00, 0, 0, 0, 0x00];
        comment.extend_from_slice(b"note");
        let mut chapter = b"ch0\0".to_vec();
        chapter.extend_from_slice(&0u32.to_be_bytes());
        chapter.extend_from_slice(&1000u32.to_be_bytes());
        chapter.extend_from_slice(&u32::MAX.to_be_bytes());
        chapter.extend_from_slice(&u32::MAX.to_be_bytes());
        chapter.extend_from_slice(&id3v2_4_frame("COMM", &comment));
        fs::write(
            &path,
            mp3_with_id3v2_4_frames(&[id3v2_4_frame("CHAP", &chapter)]),
        )
        .expect("write MP3");
        let service = LoftyMetadataService;
        service
            .write_metadata(
                &path,
                change(|change| change.title = FieldChange::Set("Title".to_owned())),
            )
            .expect("an edit saves the chapter it keeps");
        let written = id3v2(&path);
        assert_eq!(text_frame(&written, "TIT2").as_deref(), Some("Title"));
        let children: Vec<_> = written
            .iter()
            .filter_map(|frame| match frame {
                Frame::Chapter(chapter) => Some(&chapter.children),
                _ => None,
            })
            .flat_map(|children| children.iter())
            .filter_map(|frame| match frame {
                Frame::Comment(comment) => Some((
                    comment.language,
                    comment.description.to_string(),
                    comment.content.to_string(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(
            children,
            [(*b"XXX", String::new(), "note".to_owned())],
            "the chapter keeps its comment, its language healed"
        );
    });
}

fn flac_pictures(path: &Path) -> Vec<(PictureType, Vec<u8>)> {
    let mut file = File::open(path).expect("open FLAC");
    FlacFile::read_from(&mut file, ParseOptions::new())
        .expect("read FLAC")
        .pictures()
        .iter()
        .map(|(picture, _)| (picture.pic_type(), picture.data().to_vec()))
        .collect()
}

fn add_flac_picture(path: &Path, picture_type: PictureType, data: Vec<u8>) {
    let mut file = File::open(path).expect("open FLAC");
    let mut flac = FlacFile::read_from(&mut file, ParseOptions::new()).expect("read FLAC");
    let picture = Picture::unchecked(data)
        .pic_type(picture_type)
        .mime_type(MimeType::Png)
        .build();
    flac.insert_picture(picture, None).expect("insert picture");
    flac.save_to_path(path, WriteOptions::default())
        .expect("save FLAC");
}

#[test]
fn a_cover_edit_changes_only_the_front_covers() {
    with_directory(|root| {
        let service = LoftyMetadataService;
        let back = root.join("back.flac");
        write_flac(&back, &[]);
        add_flac_picture(&back, PictureType::CoverBack, other_png(1));
        assert_eq!(
            service.read_artwork(&back).expect("read"),
            Some(other_png(1)),
            "a file without a front cover shows its first picture"
        );
        service
            .write_artwork(&back, Some(other_png(1)))
            .expect("set the cover it shows");
        assert_eq!(
            flac_pictures(&back),
            [
                (PictureType::CoverBack, other_png(1)),
                (PictureType::CoverFront, other_png(1)),
            ]
        );

        let fronts = root.join("fronts.flac");
        write_flac(&fronts, &[]);
        add_flac_picture(&fronts, PictureType::CoverFront, other_png(1));
        add_flac_picture(&fronts, PictureType::CoverFront, other_png(2));
        service
            .write_artwork(&fronts, Some(other_png(1)))
            .expect("set the cover it shows");
        assert_eq!(
            flac_pictures(&fronts),
            [(PictureType::CoverFront, other_png(1))],
            "the cover is the only front cover once set"
        );

        let invalid = root.join("invalid.flac");
        write_flac(&invalid, &[]);
        add_flac_picture(&invalid, PictureType::CoverFront, b"not an image".to_vec());
        add_flac_picture(&invalid, PictureType::CoverBack, other_png(2));
        service.write_artwork(&invalid, None).expect("clear cover");
        assert_eq!(
            flac_pictures(&invalid),
            [(PictureType::CoverBack, other_png(2))]
        );
    });
}

#[test]
fn an_id3_edit_keeps_a_frame_with_a_data_length_indicator_readable() {
    with_directory(|root| {
        let path = root.join("indicator.mp3");
        let mut content = vec![0x03];
        content.extend_from_slice(b"Artist");
        // A `TPE1` frame flagged with its data length, which precedes the
        // content.
        let mut frame = b"TPE1".to_vec();
        frame.extend_from_slice(&synchsafe_size(4 + content.len() as u32));
        frame.extend_from_slice(&[0x00, 0x01]);
        frame.extend_from_slice(&synchsafe_size(content.len() as u32));
        frame.extend_from_slice(&content);
        fs::write(&path, mp3_with_id3v2_4_frames(&[frame])).expect("write MP3");
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .artist
                .as_deref(),
            Some("Artist")
        );

        service.write_rating(&path, rating(4)).expect("rate");
        let tags = service.read_persisted_tags(&path).expect("read");
        assert_eq!(tags.metadata.artist.as_deref(), Some("Artist"));
        assert_eq!(tags.rating, rating(4));
    });
}

#[test]
fn a_vorbis_total_edit_leaves_the_number_comments_alone() {
    with_directory(|root| {
        let service = LoftyMetadataService;
        let total = |total| change(|change| change.track_total = FieldChange::Set(total));

        let unreadable = root.join("unreadable.flac");
        write_flac(&unreadable, &[("TRACKNUMBER", "A1")]);
        service
            .write_metadata(&unreadable, total(12))
            .expect("write total");
        assert_eq!(
            vorbis_items(&unreadable),
            items(&[("TRACKNUMBER", "A1"), ("TRACKTOTAL", "12")])
        );

        let repeated = root.join("repeated.flac");
        write_flac(&repeated, &[("TRACKNUMBER", "1"), ("TRACKNUMBER", "2")]);
        service
            .write_metadata(&repeated, total(12))
            .expect("write total");
        assert_eq!(
            vorbis_items(&repeated),
            items(&[
                ("TRACKNUMBER", "1"),
                ("TRACKNUMBER", "2"),
                ("TRACKTOTAL", "12")
            ])
        );

        // A number holding its total holds both: the total moves out.
        let inline = root.join("inline.flac");
        write_flac(&inline, &[("TRACKNUMBER", "3/12")]);
        service
            .write_metadata(&inline, total(15))
            .expect("write total");
        assert_eq!(
            vorbis_items(&inline),
            items(&[("TRACKNUMBER", "3"), ("TRACKTOTAL", "15")])
        );
        let number_only = root.join("number.flac");
        write_flac(&number_only, &[("TRACKNUMBER", "3/12")]);
        service
            .write_metadata(
                &number_only,
                change(|change| change.track_number = FieldChange::Set(5)),
            )
            .expect("write number");
        assert_eq!(
            vorbis_items(&number_only),
            items(&[("TRACKNUMBER", "5"), ("TRACKTOTAL", "12")])
        );
    });
}

#[test]
fn a_vorbis_date_reads_before_a_year_and_a_year_edit_keeps_its_day() {
    with_directory(|root| {
        let path = root.join("date.flac");
        write_flac(&path, &[("YEAR", "1999"), ("DATE", "2001-04-05")]);
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .year,
            Some(2001)
        );

        service
            .write_metadata(&path, change(|change| change.year = FieldChange::Set(2020)))
            .expect("write year");
        assert_eq!(vorbis_items(&path), items(&[("DATE", "2020-04-05")]));
    });
}

/// `count` ADTS frames of AAC-LC, as a raw `.aac` holds them.
fn adts_frames(count: usize) -> Vec<u8> {
    const LENGTH: u32 = 32;
    let header = [
        0xFF,
        0xF1,
        0x50,
        0x80 | ((LENGTH >> 11) & 0x03) as u8,
        ((LENGTH >> 3) & 0xFF) as u8,
        (((LENGTH & 0x07) << 5) | 0x1F) as u8,
        0xFC,
    ];
    let mut frame = header.to_vec();
    frame.resize(LENGTH as usize, 0);
    frame.repeat(count)
}

#[test]
fn an_edit_refuses_a_container_sustain_does_not_read_behind_an_ogg_name() {
    with_directory(|root| {
        let path = root.join("aac.oga");
        let bytes = adts_frames(8);
        fs::write(&path, &bytes).expect("write AAC");
        assert!(
            LoftyMetadataService
                .write_metadata(
                    &path,
                    change(|change| change.title = FieldChange::Set("New".to_owned())),
                )
                .is_err()
        );
        assert_eq!(
            fs::read(&path).expect("read AAC"),
            bytes,
            "the file is untouched"
        );
    });
}

#[test]
fn text_is_carried_without_the_trailing_nuls_no_format_keeps() {
    with_directory(|root| {
        let service = LoftyMetadataService;
        let title = change(|change| change.title = FieldChange::Set("Title\0".to_owned()));
        let flac = root.join("nul.flac");
        write_flac(&flac, &[]);
        let m4a = root.join("nul.m4a");
        write_m4a(&m4a, &Ilst::new());
        for path in [flac, m4a] {
            assert_eq!(
                service
                    .carried_change(&path, title.clone())
                    .expect("carry")
                    .title,
                FieldChange::Set("Title".to_owned())
            );
            service
                .write_metadata(&path, title.clone())
                .expect("write title");
            assert_eq!(
                service
                    .read_persisted_tags(&path)
                    .expect("read")
                    .metadata
                    .title
                    .as_deref(),
                Some("Title")
            );
        }
    });
}

#[test]
fn a_flac_holding_only_an_id3_tag_reads_it_as_id3_and_keeps_it_through_an_edit() {
    with_directory(|root| {
        let path = root.join("id3.flac");
        write_flac(&path, &[]);
        let mut tag = Id3v2Tag::new();
        let _ = tag.insert(Frame::Text(TextInformationFrame::new(
            FrameId::Valid(Cow::Borrowed("TBPM")),
            TextEncoding::UTF8,
            "120",
        )));
        let _ = tag.insert(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
            TextEncoding::UTF8,
            *b"eng",
            String::new(),
            "Words".to_owned(),
        )));
        let mut bytes = Vec::new();
        tag.dump_to(&mut bytes, WriteOptions::default())
            .expect("dump ID3v2");
        bytes.extend(fs::read(&path).expect("read FLAC"));
        fs::write(&path, bytes).expect("write FLAC");
        let service = LoftyMetadataService;
        let read = |path: &Path| service.read_persisted_tags(path).expect("read").metadata;
        let metadata = read(&path);
        assert_eq!(metadata.bpm, Bpm::from_whole(120));
        assert_eq!(metadata.lyrics.as_deref(), Some("Words"));

        service
            .write_metadata(
                &path,
                change(|change| change.title = FieldChange::Set("Title".to_owned())),
            )
            .expect("write title");
        let metadata = read(&path);
        assert_eq!(metadata.title.as_deref(), Some("Title"));
        assert_eq!(metadata.bpm, Bpm::from_whole(120));
        assert_eq!(metadata.lyrics.as_deref(), Some("Words"));
    });
}

#[test]
fn an_id3_edit_writes_a_tag_unsynchronised_by_another_tool_as_it_reads() {
    with_directory(|root| {
        let path = root.join("unsynchronised.mp3");
        // A `PRIV` payload of FF 00 01, which an unsynchronised tag holds
        // as FF 00 00 01.
        let mut content = b"owner\0".to_vec();
        content.extend_from_slice(&[0xFF, 0x00, 0x00, 0x01]);
        let mut bytes = mp3_with_id3v2_4_frames(&[id3v2_4_frame("PRIV", &content)]);
        bytes[5] = 0x80;
        fs::write(&path, bytes).expect("write MP3");
        let private = |path: &Path| {
            id3v2(path).iter().find_map(|frame| match frame {
                Frame::Private(private) => Some(private.private_data.to_vec()),
                _ => None,
            })
        };
        assert_eq!(private(&path), Some(vec![0xFF, 0x00, 0x01]));

        LoftyMetadataService
            .write_rating(&path, rating(4))
            .expect("rate");
        assert_eq!(private(&path), Some(vec![0xFF, 0x00, 0x01]));
    });
}

#[test]
fn a_vorbis_total_edit_keeps_each_number_as_written() {
    with_directory(|root| {
        let service = LoftyMetadataService;
        let total = |total| change(|change| change.track_total = FieldChange::Set(total));

        let unreadable = root.join("unreadable.flac");
        write_flac(&unreadable, &[("TRACKNUMBER", "A1/12")]);
        service
            .write_metadata(&unreadable, total(14))
            .expect("write total");
        assert_eq!(
            vorbis_items(&unreadable),
            items(&[("TRACKNUMBER", "A1"), ("TRACKTOTAL", "14")])
        );

        let second = root.join("second.flac");
        write_flac(&second, &[("TRACKNUMBER", "3/12"), ("TRACKNUMBER", "B1")]);
        service
            .write_metadata(&second, total(14))
            .expect("write total");
        assert_eq!(
            vorbis_items(&second),
            items(&[
                ("TRACKNUMBER", "3"),
                ("TRACKNUMBER", "B1"),
                ("TRACKTOTAL", "14")
            ])
        );

        let total_key = root.join("total-key.flac");
        write_flac(
            &total_key,
            &[("TRACKNUMBER", "3/12"), ("TOTALTRACKS", "0012")],
        );
        service
            .write_metadata(
                &total_key,
                change(|change| change.track_number = FieldChange::Set(5)),
            )
            .expect("write number");
        assert_eq!(
            vorbis_items(&total_key),
            items(&[("TOTALTRACKS", "0012"), ("TRACKNUMBER", "5")])
        );
    });
}

#[test]
fn a_year_edit_moves_29_february_to_its_month_in_a_year_without_one() {
    with_directory(|root| {
        let path = root.join("leap.flac");
        write_flac(&path, &[("DATE", "2024-02-29")]);
        let service = LoftyMetadataService;
        let year = |year| change(|change| change.year = FieldChange::Set(year));
        service
            .write_metadata(&path, year(2023))
            .expect("write year");
        assert_eq!(vorbis_items(&path), items(&[("DATE", "2023-02")]));
        write_flac(&path, &[("DATE", "2024-02-29")]);
        service
            .write_metadata(&path, year(2000))
            .expect("write year");
        assert_eq!(vorbis_items(&path), items(&[("DATE", "2000-02-29")]));
    });
}

#[test]
fn an_m4a_number_pair_is_read_from_the_first_atom_that_holds_one() {
    with_directory(|root| {
        let path = root.join("numbers.m4a");
        let numbers = |data: Vec<u8>| AtomData::Unknown {
            code: DataType::Reserved,
            data,
        };
        let mut tag = Ilst::new();
        tag.insert(Atom::new(AtomIdent::Fourcc(*b"trkn"), numbers(vec![0, 0])));
        tag.insert(Atom::new(
            AtomIdent::Fourcc(*b"trk2"),
            numbers(vec![0, 0, 0, 3, 0, 12, 0, 0]),
        ));
        write_m4a(&path, &tag);
        rename_atom(&path, b"trk2", b"trkn");
        let service = LoftyMetadataService;
        let metadata = service.read_persisted_tags(&path).expect("read").metadata;
        assert_eq!(
            (metadata.track_number, metadata.track_total),
            (Some(3), Some(12))
        );

        service
            .write_metadata(
                &path,
                change(|change| change.track_total = FieldChange::Set(14)),
            )
            .expect("write total");
        let metadata = service.read_persisted_tags(&path).expect("read").metadata;
        assert_eq!(
            (metadata.track_number, metadata.track_total),
            (Some(3), Some(14))
        );
    });
}

#[test]
fn a_flac_holding_only_an_id3v2_3_tag_reads_its_year() {
    with_directory(|root| {
        let path = root.join("v23.flac");
        write_flac(&path, &[]);
        let mp3 = mp3_with_id3v2_3_frames(&[id3v2_3_text_frame("TYER", "1999")]);
        let tag_length = 10
            + mp3[6..10]
                .iter()
                .fold(0usize, |size, byte| (size << 7) | usize::from(*byte));
        let mut bytes = mp3[..tag_length].to_vec();
        bytes.extend(fs::read(&path).expect("read FLAC"));
        fs::write(&path, bytes).expect("write FLAC");
        let service = LoftyMetadataService;
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .year,
            Some(1999)
        );
        service
            .write_metadata(
                &path,
                change(|change| change.title = FieldChange::Set("Title".to_owned())),
            )
            .expect("write title");
        assert_eq!(
            service
                .read_persisted_tags(&path)
                .expect("read")
                .metadata
                .year,
            Some(1999)
        );
    });
}

#[test]
fn an_edit_never_lands_over_another_tools_edit_made_since_the_read() {
    with_directory(|root| {
        let path = root.join("track.flac");
        write_flac(&path, &[("TITLE", "Old")]);
        let parsed = super::parse(&path, ParseOptions::new()).expect("parse");
        let read = parsed.version();
        let mut tag = parsed.into_editable_tag().expect("editable tag");
        assert!(tag.apply_change(&change(|change| {
            change.title = FieldChange::Set("New".to_owned());
        })));

        // Another tagger rewrites the file between Sustain's read and its
        // save.
        write_flac(&path, &[("TITLE", "Old"), ("CUESHEET", "FILE track.wav")]);

        assert_eq!(tag.save(&path, read), Err(MetadataError::SourceChanged));
        assert_eq!(
            vorbis_items(&path),
            items(&[("TITLE", "Old"), ("CUESHEET", "FILE track.wav")])
        );
    });
}

#[test]
fn a_year_no_tag_can_hold_clears_the_year_rather_than_write_another() {
    with_directory(|root| {
        let mp3 = root.join("year.mp3");
        write_mp3(&mp3, &Id3v2Tag::new());
        let flac = root.join("year.flac");
        write_flac(&flac, &[]);
        let m4a = root.join("year.m4a");
        write_m4a(&m4a, &Ilst::default());
        let service = LoftyMetadataService;
        let write_year = |path: &Path, year: i32| {
            let change = MetadataChange {
                year: FieldChange::Set(year),
                ..MetadataChange::default()
            };
            service.write_metadata(path, change).expect("write year");
            service
                .read_persisted_tags(path)
                .expect("read")
                .metadata
                .year
        };

        for path in [&mp3, &flac, &m4a] {
            // Four digits are the bounds a tag's date holds (#368).
            assert_eq!(write_year(path, 0), Some(0), "{path:?}");
            assert_eq!(write_year(path, 9999), Some(9999), "{path:?}");
            for beyond in [10_000, -1, i32::MAX] {
                write_year(path, 1998);
                assert_eq!(write_year(path, beyond), None, "{beyond} in {path:?}");
            }
        }
    });
}
