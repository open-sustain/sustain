// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use image::codecs::jpeg::JpegEncoder;
use image::{ExtendedColorType, ImageEncoder, RgbImage};

use super::{CoverSet, Images, Shard, database, shards, slot};
use crate::model::{FileType, IpodLibrary, IpodTrack};
use crate::profile::{CoverFormat, Family, Profile};
use crate::record::WriteError;

/// A `width` × `height` JPEG of one colour.
fn jpeg(width: u32, height: u32, rgb: [u8; 3]) -> Vec<u8> {
    let image = RgbImage::from_pixel(width, height, image::Rgb(rgb));
    let mut bytes = Vec::new();
    JpegEncoder::new_with_quality(&mut bytes, 95)
        .write_image(image.as_raw(), width, height, ExtendedColorType::Rgb8)
        .expect("encodes");
    bytes
}

fn classic() -> Profile {
    Family::Classic2.profile()
}

fn covers() -> CoverSet {
    CoverSet::new(classic().cover_edge().expect("the classic shows covers"))
}

fn track(dbid: u64, cover: Option<super::CoverId>) -> IpodTrack {
    IpodTrack {
        dbid,
        location: format!("iPod_Control/Music/F00/S{dbid}.mp3"),
        file_type: FileType::Mp3,
        title: Some(format!("Track {dbid}")),
        artist: None,
        album: None,
        album_artist: None,
        genre: None,
        composer: None,
        comment: None,
        sort_title: None,
        sort_artist: None,
        sort_album: None,
        sort_album_artist: None,
        sort_composer: None,
        compilation: false,
        rating_stars: 0,
        track_number: None,
        track_total: None,
        disc_number: None,
        disc_total: None,
        year: None,
        duration_ms: 1000,
        size_bytes: 1000,
        bitrate_kbps: None,
        sample_rate_hz: 44_100,
        date_added: None,
        cover,
    }
}

// ---------------------------------------------------------------------
// A reader for the tests
// ---------------------------------------------------------------------

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("two bytes"))
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("eight bytes"))
}

/// A record at `offset`, checked to carry `tag`: (header length, total).
fn record(bytes: &[u8], offset: usize, tag: &[u8; 4]) -> (usize, usize) {
    assert_eq!(&bytes[offset..offset + 4], tag, "at {offset:#x}");
    (
        u32_at(bytes, offset + 4) as usize,
        u32_at(bytes, offset + 8) as usize,
    )
}

#[derive(Debug, Eq, PartialEq)]
struct Thumbnail {
    format: u32,
    file: String,
    offset: u32,
    size: u32,
    width: u16,
    height: u16,
}

#[derive(Debug)]
struct WalkedImage {
    id: u32,
    song: u64,
    thumbnails: Vec<Thumbnail>,
}

struct Walked {
    next_id: u32,
    images: Vec<WalkedImage>,
    files: Vec<(u32, u32)>,
}

/// Walk an `ArtworkDB`, checking every length on the way.
fn walk(bytes: &[u8]) -> Walked {
    let (header, total) = record(bytes, 0, b"mhfd");
    assert_eq!(total, bytes.len());
    assert_eq!(u32_at(bytes, 0x14), 3);
    let next_id = u32_at(bytes, 0x1c);
    let mut offset = header;
    let mut kinds = Vec::new();
    let mut images = Vec::new();
    let mut files = Vec::new();
    while offset < total {
        let (header, dataset_total) = record(bytes, offset, b"mhsd");
        let kind = u16_at(bytes, offset + 0x0c);
        kinds.push(kind);
        let list = offset + header;
        let count = u32_at(bytes, list + 8) as usize;
        let mut child = list + u32_at(bytes, list + 4) as usize;
        for _ in 0..count {
            match kind {
                1 => {
                    let (image, length) = walk_image(bytes, child);
                    images.push(image);
                    child += length;
                }
                3 => {
                    let (_, length) = record(bytes, child, b"mhif");
                    files.push((u32_at(bytes, child + 0x10), u32_at(bytes, child + 0x14)));
                    child += length;
                }
                _ => unreachable!("dataset {kind} lists {count} children"),
            }
        }
        assert_eq!(child, offset + dataset_total, "dataset {kind} length");
        offset += dataset_total;
    }
    assert_eq!(kinds, [1, 2, 3]);
    Walked {
        next_id,
        images,
        files,
    }
}

fn walk_image(bytes: &[u8], start: usize) -> (WalkedImage, usize) {
    let (header, total) = record(bytes, start, b"mhii");
    let count = u32_at(bytes, start + 0x0c);
    let mut offset = start + header;
    let mut thumbnails = Vec::new();
    for _ in 0..count {
        let (container_header, container) = record(bytes, offset, b"mhod");
        assert_eq!(u16_at(bytes, offset + 0x0c), 2);
        let mhni = offset + container_header;
        let (header, length) = record(bytes, mhni, b"mhni");
        assert_eq!(header + mhod_length(bytes, mhni + header), length);
        assert_eq!(container_header + length, container);
        let name = mhni + header;
        assert_eq!(u16_at(bytes, name + 0x0c), 3);
        assert_eq!(bytes[name + 0x1c], 2, "UTF-16");
        let text_len = u32_at(bytes, name + 0x18) as usize;
        let text: Vec<u16> = bytes[name + 0x24..name + 0x24 + text_len]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        thumbnails.push(Thumbnail {
            format: u32_at(bytes, mhni + 0x10),
            file: String::from_utf16(&text).expect("UTF-16"),
            offset: u32_at(bytes, mhni + 0x14),
            size: u32_at(bytes, mhni + 0x18),
            height: u16_at(bytes, mhni + 0x20),
            width: u16_at(bytes, mhni + 0x22),
        });
        offset += container;
    }
    assert_eq!(offset, start + total, "image length");
    let image = WalkedImage {
        id: u32_at(bytes, start + 0x10),
        song: u64_at(bytes, start + 0x14),
        thumbnails,
    };
    (image, total)
}

/// A file-name `mhod`'s total length, which must be a multiple of four.
fn mhod_length(bytes: &[u8], offset: usize) -> usize {
    let (header, total) = record(bytes, offset, b"mhod");
    assert_eq!(header, 0x18);
    assert_eq!(total % 4, 0);
    total
}

// ---------------------------------------------------------------------
// Covers
// ---------------------------------------------------------------------

#[test]
fn identical_covers_are_prepared_once() {
    let mut covers = covers();
    let red = jpeg(600, 600, [200, 0, 0]);
    let first = covers.add(&red).expect("decodes");
    let again = covers.add(&red).expect("decodes");
    let blue = covers.add(&jpeg(600, 600, [0, 0, 200])).expect("decodes");
    assert_eq!(first, again);
    assert_ne!(first, blue);
    assert_eq!(covers.len(), 2);
    assert_eq!(first.source_bytes() as usize, red.len());
}

#[test]
fn an_undecodable_cover_is_no_cover() {
    let mut covers = covers();
    assert_eq!(covers.add(b"not an image"), None);
    assert!(covers.is_empty());
}

#[test]
fn a_cover_is_prepared_as_a_square_jpeg_with_a_wide_image_letterboxed() {
    let mut covers = covers();
    covers
        .add(&jpeg(640, 320, [250, 250, 250]))
        .expect("decodes");
    let prepared = image::load_from_memory(&covers.covers[0])
        .expect("a JPEG")
        .to_rgb8();
    assert_eq!(prepared.dimensions(), (320, 320));
    // Black above, the image in the middle.
    assert!(prepared.get_pixel(160, 10).0.iter().all(|&c| c < 16));
    assert!(prepared.get_pixel(160, 160).0.iter().all(|&c| c > 230));
}

// ---------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------

#[test]
fn tracks_with_one_cover_share_an_image_on_a_sparse_device() {
    let mut covers = covers();
    let a = covers.add(&jpeg(32, 32, [200, 0, 0]));
    let b = covers.add(&jpeg(32, 32, [0, 200, 0]));
    let tracks = [track(10, a), track(11, a), track(12, None), track(13, b)];
    let images = Images::new(&tracks, classic()).expect("fits");
    assert_eq!(images.track_links, [100, 100, 0, 101]);
    let songs: Vec<u64> = images.images.iter().map(|image| image.song_dbid).collect();
    assert_eq!(songs, [10, 13]);
}

#[test]
fn each_track_has_its_own_image_on_an_older_device() {
    let video = Family::Video2.profile();
    let mut covers = CoverSet::new(video.cover_edge().expect("shows covers"));
    let a = covers.add(&jpeg(32, 32, [200, 0, 0]));
    let tracks = [track(10, a), track(11, a), track(12, None)];
    let images = Images::new(&tracks, video).expect("fits");
    assert_eq!(images.track_links, [100, 101, 0]);
    // Both images point at the cover's one copy of the pixels.
    let bytes = database(&tracks, &covers, video).expect("writes");
    let walked = walk(&bytes);
    assert_eq!(walked.images.len(), 2);
    assert_eq!(walked.images[0].thumbnails, walked.images[1].thumbnails);
    assert_eq!(walked.images[1].song, 11);
}

#[test]
fn a_device_without_covers_links_no_track() {
    let mut covers = covers();
    let a = covers.add(&jpeg(32, 32, [200, 0, 0]));
    let images = Images::new(&[track(10, a)], Family::Mini2.profile()).expect("fits");
    assert_eq!(images.track_links, [0]);
    assert!(images.images.is_empty());
}

// ---------------------------------------------------------------------
// .ithmb files
// ---------------------------------------------------------------------

/// A set of `count` (at most 256) distinct covers, cheap to build: tiny
/// black images, each with a different pixel lit.
fn many_covers(count: u32) -> CoverSet {
    let mut covers = CoverSet::new(8);
    for index in 0..count {
        let mut image = RgbImage::new(16, 16);
        image.put_pixel(index % 16, index / 16, image::Rgb([255, 255, 255]));
        let mut bytes = Vec::new();
        JpegEncoder::new_with_quality(&mut bytes, 95)
            .write_image(image.as_raw(), 16, 16, ExtendedColorType::Rgb8)
            .expect("encodes");
        covers.add(&bytes).expect("decodes");
    }
    assert_eq!(covers.len(), count as usize);
    covers
}

#[test]
fn covers_fill_each_file_up_to_its_cap_format_by_format() {
    let covers = many_covers(200);
    let shards = shards(&covers, classic());
    let summary: Vec<(String, std::ops::Range<u32>)> = shards
        .iter()
        .map(|shard| (shard.file_name(), shard.covers.clone()))
        .collect();
    // 32 MiB holds 163 covers of 320 × 320; the smaller formats fit in one.
    assert_eq!(
        summary,
        [
            ("F1061_1.ithmb".to_owned(), 0..200),
            ("F1055_1.ithmb".to_owned(), 0..200),
            ("F1060_1.ithmb".to_owned(), 0..163),
            ("F1060_2.ithmb".to_owned(), 163..200),
        ]
    );
    let large = classic().cover_formats[2];
    assert_eq!(slot(large, 162), (1, 162 * 204_800));
    assert_eq!(slot(large, 163), (2, 0));
}

#[test]
fn no_cover_means_no_file() {
    assert_eq!(shards(&covers(), classic()), Vec::<Shard>::new());
}

#[test]
fn the_smallest_classic_format_is_55_pixels_in_112_byte_rows() {
    let mut covers = covers();
    covers.add(&jpeg(100, 100, [255, 0, 0])).expect("decodes");
    let small = shards(&covers, classic())
        .into_iter()
        .find(|shard| shard.format.id == 1061)
        .expect("an F1061 file");
    let bytes = small.render(&covers).expect("renders");
    assert_eq!(bytes.len(), 6_160);
    for row in bytes.chunks_exact(112) {
        let (pixels, padding) = row.split_at(110);
        assert_eq!(padding, [0, 0]);
        for pixel in pixels.chunks_exact(2) {
            let value = u16::from_le_bytes([pixel[0], pixel[1]]);
            // Red at full scale, no green or blue to speak of.
            assert!(value >> 11 >= 30, "{value:#06x}");
            assert!(value & 0x07ff < 0x0100, "{value:#06x}");
        }
    }
}

#[test]
fn a_rendered_file_holds_its_covers_back_to_back_in_order() {
    let mut covers = CoverSet::new(16);
    for rgb in [[0, 0, 255], [255, 0, 0], [0, 255, 0]] {
        covers.add(&jpeg(16, 16, rgb)).expect("decodes");
    }
    let format = CoverFormat {
        id: 1,
        width: 4,
        height: 4,
        row_bytes: 8,
    };
    let shard = Shard {
        format,
        number: 1,
        covers: 1..3,
    };
    let bytes = shard.render(&covers).expect("renders");
    assert_eq!(bytes.len(), 2 * 32);
    // Red, then green, each filling its 32 bytes.
    let top_bits = |slot: &[u8]| -> Vec<u16> {
        slot.chunks_exact(2)
            .map(|pixel| u16::from_le_bytes([pixel[0], pixel[1]]) & 0xc610)
            .collect()
    };
    assert_eq!(top_bits(&bytes[..32]), [0xc000; 16]);
    assert_eq!(top_bits(&bytes[32..]), [0x0600; 16]);
}

// ---------------------------------------------------------------------
// ArtworkDB
// ---------------------------------------------------------------------

#[test]
fn every_thumbnail_lies_inside_its_file() {
    let profile = classic();
    let covers = many_covers(170);
    let tracks: Vec<IpodTrack> = (0..170)
        .map(|index| {
            let cover = Some(super::CoverId {
                index,
                source_bytes: 1000,
            });
            track(u64::from(index) + 1, cover)
        })
        .collect();
    let bytes = database(&tracks, &covers, profile).expect("writes");
    let walked = walk(&bytes);
    assert_eq!(walked.images.len(), 170);
    assert_eq!(walked.next_id, 100 + 169);
    let file_sizes: std::collections::HashMap<String, usize> = shards(&covers, profile)
        .iter()
        .map(|shard| {
            let size = shard.covers.len() * shard.format.image_bytes() as usize;
            (format!(":{}", shard.file_name()), size)
        })
        .collect();
    for image in &walked.images {
        assert_eq!(image.thumbnails.len(), 3);
        for thumbnail in &image.thumbnails {
            let size = file_sizes[&thumbnail.file];
            assert!(
                (thumbnail.offset + thumbnail.size) as usize <= size,
                "{thumbnail:?} in {size} bytes"
            );
            assert_eq!(thumbnail.offset % thumbnail.size, 0);
        }
    }
    let last = &walked.images[169].thumbnails;
    assert_eq!(
        last[2],
        Thumbnail {
            format: 1060,
            file: ":F1060_2.ithmb".to_owned(),
            offset: 6 * 204_800,
            size: 204_800,
            width: 320,
            height: 320,
        }
    );
    assert_eq!(
        walked.files,
        [(1061, 6_160), (1055, 32_768), (1060, 204_800)]
    );
}

#[test]
fn an_empty_cover_set_still_describes_the_formats() {
    let bytes = database(&[track(1, None)], &covers(), classic()).expect("writes");
    let walked = walk(&bytes);
    assert!(walked.images.is_empty());
    assert_eq!(walked.next_id, 0);
    assert_eq!(walked.files.len(), 3);
}

#[test]
fn a_cover_outside_the_set_is_refused() {
    let mut other = covers();
    let foreign = other.add(&jpeg(16, 16, [1, 2, 3]));
    assert_eq!(
        database(&[track(1, None), track(2, foreign)], &covers(), classic()),
        Err(WriteError::UnknownCover { track: 1 })
    );
}

#[test]
fn the_track_record_links_the_image_the_artwork_lists() {
    let mut covers = covers();
    let cover = covers.add(&jpeg(32, 32, [9, 9, 9]));
    let tracks = vec![track(7, None), track(8, cover)];
    let library = IpodLibrary {
        name: "iPod".to_owned(),
        library_id: 1,
        master_playlist_id: 1,
        tracks: tracks.clone(),
        playlists: Vec::new(),
    };
    let profile = classic();
    let catalogue = crate::itunesdb::write(&library, profile, Some(guid())).expect("writes");
    let artwork = walk(&database(&tracks, &covers, profile).expect("writes"));
    let mhits: Vec<usize> = catalogue
        .windows(4)
        .enumerate()
        .filter(|(_, window)| *window == b"mhit")
        .map(|(offset, _)| offset)
        .collect();
    assert_eq!(mhits.len(), 2);
    let without = mhits[0];
    assert_eq!(u32_at(&catalogue, without + 0x160), 0);
    assert_eq!(catalogue[without + 0xa4], 0);
    let with = mhits[1];
    assert_eq!(u32_at(&catalogue, with + 0x160), artwork.images[0].id);
    assert_eq!(u16_at(&catalogue, with + 0x7c), 1);
    assert_eq!(catalogue[with + 0xa4], 1);
    assert_eq!(
        u32_at(&catalogue, with + 0x80),
        cover.expect("decodes").source_bytes()
    );
    assert_eq!(artwork.images[0].song, 8);
}

fn guid() -> crate::FirewireGuid {
    crate::FirewireGuid::parse("000A27001C0FFEE5").expect("a GUID")
}
