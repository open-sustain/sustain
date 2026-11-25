// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Album covers: the `iPod_Control/Artwork/ArtworkDB` catalogue and the
//! `.ithmb` files that hold the pixels.
//!
//! A cover is stored once per [`CoverFormat`] of the device, as
//! little-endian RGB565, in files named `F{format}_{n}.ithmb`. The
//! `ArtworkDB` lists images (`mhii`); each names, per format, the file,
//! offset and size of its pixels (`mhod` 2 → `mhni` → `mhod` 3). A track
//! reaches its image through the image id in its own record.
//!
//! On a device with sparse artwork, the tracks that share a cover share
//! one image. On older devices, the firmware looks a cover up by the track
//! that owns it, so each track with a cover has its own image; those
//! images still point at the cover's one copy of the pixels.
//!
//! The layout follows libgpod's writer (`db-artwork-writer.c`), with the
//! classic's F1061 geometry corrected; see `PROVENANCE.md`.

use std::collections::HashMap;
use std::ops::Range;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::{self, FilterType};
use image::{DynamicImage, ExtendedColorType, ImageEncoder, ImageFormat, RgbImage};
use sha2::{Digest, Sha256};
use sustain_artwork::decode_static_artwork;

use crate::model::IpodTrack;
use crate::profile::{CoverFormat, Profile};
use crate::record::{Header, Open, Out, WriteError, u32_len};

/// Where the artwork catalogue lives, relative to the iPod's root.
pub const ARTWORK_DB_RELATIVE_PATH: &str = "iPod_Control/Artwork/ArtworkDB";
/// The directory of the catalogue and its `.ithmb` files.
pub const ARTWORK_DIRECTORY: &str = "iPod_Control/Artwork";

/// libgpod numbers images from 100.
const FIRST_IMAGE_ID: u32 = 0x64;
/// The largest `.ithmb` file Sustain writes. libgpod stops at 256 MB,
/// after reports of slow devices; Sustain stays well below, because a
/// whole file is rendered in memory before it is written.
const MAX_SHARD_BYTES: u32 = 32 * 1024 * 1024;
/// The prepared covers' JPEG quality, as for Pioneer covers.
const JPEG_QUALITY: u8 = 90;

// ---------------------------------------------------------------------
// Prepared covers
// ---------------------------------------------------------------------

/// A cover registered in a [`CoverSet`]: its place in the set, and the
/// size of the image it was prepared from, which the track record carries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverId {
    index: u32,
    source_bytes: u32,
}

impl CoverId {
    pub(crate) const fn source_bytes(self) -> u32 {
        self.source_bytes
    }
}

/// The distinct covers of a sync, each kept as a square JPEG at the edge
/// of the device's largest format, until the `.ithmb` files are rendered
/// from them. Identical source images are prepared once.
pub struct CoverSet {
    edge: u32,
    by_hash: HashMap<[u8; 32], CoverId>,
    covers: Vec<Vec<u8>>,
}

impl CoverSet {
    /// An empty set for covers of `edge` pixels ([`Profile::cover_edge`]).
    pub fn new(edge: u32) -> Self {
        Self {
            edge,
            by_hash: HashMap::new(),
            covers: Vec::new(),
        }
    }

    /// Register a track's embedded cover, returning its id, or `None` when
    /// the bytes are not an image Sustain can decode: the track then shows
    /// no cover.
    pub fn add(&mut self, encoded: &[u8]) -> Option<CoverId> {
        let hash: [u8; 32] = Sha256::digest(encoded).into();
        if let Some(&id) = self.by_hash.get(&hash) {
            return Some(id);
        }
        let source_bytes = u32::try_from(encoded.len()).ok()?;
        // Indices stay below `u32::MAX`, so the count fits too.
        let index = u32::try_from(self.covers.len())
            .ok()
            .filter(|&index| index < u32::MAX)?;
        let image = decode_static_artwork(encoded).ok()?;
        let prepared = prepare(&image, self.edge)?;
        let id = CoverId {
            index,
            source_bytes,
        };
        self.covers.push(prepared);
        self.by_hash.insert(hash, id);
        Some(id)
    }

    pub fn len(&self) -> usize {
        self.covers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.covers.is_empty()
    }

    fn count(&self) -> u32 {
        u32::try_from(self.covers.len()).expect("CoverSet::add bounds the count to 32 bits")
    }

    fn contains(&self, id: CoverId) -> bool {
        id.index < self.count()
    }
}

/// `image` fitted, centred, into an `edge` × `edge` square, as a JPEG.
/// Every cover format is square, so each is later a plain resize of it.
fn prepare(image: &DynamicImage, edge: u32) -> Option<Vec<u8>> {
    let fitted = image.resize(edge, edge, FilterType::Lanczos3).to_rgb8();
    let mut square = RgbImage::new(edge, edge);
    imageops::overlay(
        &mut square,
        &fitted,
        i64::from((edge - fitted.width()) / 2),
        i64::from((edge - fitted.height()) / 2),
    );
    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY)
        .write_image(square.as_raw(), edge, edge, ExtendedColorType::Rgb8)
        .ok()?;
    Some(jpeg)
}

// ---------------------------------------------------------------------
// Images
// ---------------------------------------------------------------------

/// One `mhii`: an image id, the track it belongs to, and its cover.
struct Image {
    id: u32,
    song_dbid: u64,
    cover: CoverId,
}

/// The catalogue's images and, per track, the id of the image it shows (0
/// for none). The `iTunesDB` and the `ArtworkDB` are written from the same
/// table, so their links agree.
pub(crate) struct Images {
    images: Vec<Image>,
    pub(crate) track_links: Vec<u32>,
}

impl Images {
    pub(crate) fn new(tracks: &[IpodTrack], profile: Profile) -> Result<Self, WriteError> {
        let mut images: Vec<Image> = Vec::new();
        let mut track_links = vec![0; tracks.len()];
        if profile.cover_formats.is_empty() {
            return Ok(Self {
                images,
                track_links,
            });
        }
        let mut by_cover: HashMap<u32, u32> = HashMap::new();
        for (track, link) in tracks.iter().zip(&mut track_links) {
            let Some(cover) = track.cover else {
                continue;
            };
            if profile.sparse_artwork
                && let Some(&id) = by_cover.get(&cover.index)
            {
                *link = id;
                continue;
            }
            let id = u32_len(images.len())?
                .checked_add(FIRST_IMAGE_ID)
                .ok_or(WriteError::TooLarge)?;
            images.push(Image {
                id,
                song_dbid: track.dbid,
                cover,
            });
            by_cover.insert(cover.index, id);
            *link = id;
        }
        Ok(Self {
            images,
            track_links,
        })
    }

    /// The highest image id, which libgpod stores as the catalogue's next
    /// id; 0 when there are none.
    fn last_id(&self) -> u32 {
        self.images.last().map_or(0, |image| image.id)
    }
}

// ---------------------------------------------------------------------
// .ithmb files
// ---------------------------------------------------------------------

/// Where a cover's pixels sit for one format: which file, at what offset.
fn slot(format: CoverFormat, cover: u32) -> (u32, u32) {
    let per_shard = covers_per_shard(format);
    (
        cover / per_shard + 1,
        (cover % per_shard) * format.image_bytes(),
    )
}

fn covers_per_shard(format: CoverFormat) -> u32 {
    MAX_SHARD_BYTES / format.image_bytes()
}

fn shard_name(format: CoverFormat, number: u32) -> String {
    format!("F{}_{number}.ithmb", format.id)
}

/// One `.ithmb` file: a run of covers in one format, back to back.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shard {
    format: CoverFormat,
    number: u32,
    covers: Range<u32>,
}

impl Shard {
    /// The file's name within [`ARTWORK_DIRECTORY`].
    pub fn file_name(&self) -> String {
        shard_name(self.format, self.number)
    }

    /// The file's bytes: each cover resized to the format and stored as
    /// RGB565 rows, the padding at the end of each row zeroed.
    pub fn render(&self, covers: &CoverSet) -> Result<Vec<u8>, RenderError> {
        let format = self.format;
        let mut bytes = Vec::with_capacity(self.covers.len() * format.image_bytes() as usize);
        for index in self.covers.clone() {
            let jpeg = covers
                .covers
                .get(index as usize)
                .ok_or(RenderError::UnknownCover)?;
            let image = image::load_from_memory_with_format(jpeg, ImageFormat::Jpeg)
                .map_err(|_| RenderError::Decode)?
                .resize_exact(
                    u32::from(format.width),
                    u32::from(format.height),
                    FilterType::Lanczos3,
                )
                .to_rgb8();
            let padding = usize::from(format.row_bytes - format.width * 2);
            for row in image.rows() {
                for pixel in row {
                    bytes.extend_from_slice(&rgb565(pixel.0).to_le_bytes());
                }
                bytes.extend(std::iter::repeat_n(0, padding));
            }
        }
        Ok(bytes)
    }
}

/// Why a `.ithmb` file could not be rendered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderError {
    /// The shard names a cover the set does not hold.
    UnknownCover,
    /// A prepared cover could not be decoded.
    Decode,
}

impl std::fmt::Display for RenderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownCover => f.write_str("an iPod cover file names an unknown cover"),
            Self::Decode => f.write_str("a prepared iPod cover could not be decoded"),
        }
    }
}

impl std::error::Error for RenderError {}

fn rgb565([red, green, blue]: [u8; 3]) -> u16 {
    (u16::from(red >> 3) << 11) | (u16::from(green >> 2) << 5) | u16::from(blue >> 3)
}

/// The `.ithmb` files for `covers` on a device with `profile`, format by
/// format; none when the device shows no covers.
pub fn shards(covers: &CoverSet, profile: Profile) -> Vec<Shard> {
    let count = covers.count();
    let mut shards = Vec::new();
    for &format in profile.cover_formats {
        let per_shard = covers_per_shard(format);
        let mut start = 0;
        while start < count {
            let end = count.min(start.saturating_add(per_shard));
            shards.push(Shard {
                format,
                number: start / per_shard + 1,
                covers: start..end,
            });
            start = end;
        }
    }
    shards
}

// ---------------------------------------------------------------------
// ArtworkDB
// ---------------------------------------------------------------------

// Header sizes, libgpod's.
const MHFD_LEN: u32 = 0x84;
const MHSD_LEN: u32 = 0x60;
const LIST_LEN: u32 = 0x5c;
const MHII_LEN: u32 = 0x98;
const MHOD_LEN: u32 = 0x18;
const MHNI_LEN: u32 = 0x4c;
const MHIF_LEN: u32 = 0x7c;
/// A string `mhod`'s fields end at `+0x24`, though its header length says
/// 0x18, as iTunes writes it.
const STRING_MHOD_FIELDS: usize = 0x24;
const THUMBNAIL: u16 = 2;
const FILE_NAME: u16 = 3;

/// The `ArtworkDB` for `tracks`, whose covers are in `covers`, on a device
/// with `profile`.
pub fn database(
    tracks: &[IpodTrack],
    covers: &CoverSet,
    profile: Profile,
) -> Result<Vec<u8>, WriteError> {
    if let Some(index) = tracks
        .iter()
        .position(|track| track.cover.is_some_and(|cover| !covers.contains(cover)))
    {
        return Err(WriteError::UnknownCover { track: index });
    }
    let images = Images::new(tracks, profile)?;
    let mut out = Out::default();
    let mut root = Header::new(b"mhfd", MHFD_LEN);
    root.u32(0x10, 2)
        .u32(0x14, 3)
        .u32(0x1c, images.last_id())
        .u8(0x30, 2);
    let root = out.open_record(&root.0);

    let dataset = open_dataset(&mut out, 1);
    out.extend(&list_header(b"mhli", images.images.len())?);
    for image in &images.images {
        write_image(&mut out, image, profile)?;
    }
    out.close_record(dataset)?;

    let dataset = open_dataset(&mut out, 2);
    out.extend(&list_header(b"mhla", 0)?);
    out.close_record(dataset)?;

    let dataset = open_dataset(&mut out, 3);
    out.extend(&list_header(b"mhlf", profile.cover_formats.len())?);
    for format in profile.cover_formats {
        let mut file = Header::new(b"mhif", MHIF_LEN);
        file.u32(0x08, MHIF_LEN)
            .u32(0x10, format.id)
            .u32(0x14, format.image_bytes());
        out.extend(&file.0);
    }
    out.close_record(dataset)?;

    out.close_record(root)?;
    Ok(out.bytes)
}

fn open_dataset(out: &mut Out, kind: u16) -> Open {
    let mut header = Header::new(b"mhsd", MHSD_LEN);
    header.u16(0x0c, kind);
    out.open_record(&header.0)
}

fn list_header(tag: &[u8; 4], count: usize) -> Result<Vec<u8>, WriteError> {
    let mut header = Header::new(tag, LIST_LEN);
    header.u32(0x08, u32_len(count)?);
    Ok(header.0)
}

fn write_image(out: &mut Out, image: &Image, profile: Profile) -> Result<(), WriteError> {
    let mut header = Header::new(b"mhii", MHII_LEN);
    header
        .u32(0x0c, u32_len(profile.cover_formats.len())?)
        .u32(0x10, image.id)
        .u64(0x14, image.song_dbid)
        // libgpod stores the source size plus the track's cover count.
        .u32(0x30, image.cover.source_bytes.saturating_add(1));
    let record = out.open_record(&header.0);
    for &format in profile.cover_formats {
        let (shard, offset) = slot(format, image.cover.index);
        let mut container = Header::new(b"mhod", MHOD_LEN);
        container.u16(0x0c, THUMBNAIL);
        let container = out.open_record(&container.0);
        let mut thumbnail = Header::new(b"mhni", MHNI_LEN);
        thumbnail
            .u32(0x0c, 1)
            .u32(0x10, format.id)
            .u32(0x14, offset)
            .u32(0x18, format.image_bytes())
            .u16(0x20, format.height)
            .u16(0x22, format.width);
        let thumbnail = out.open_record(&thumbnail.0);
        out.extend(&file_name_record(&format!(
            ":{}",
            shard_name(format, shard)
        ))?);
        out.close_record(thumbnail)?;
        out.close_record(container)?;
    }
    out.close_record(record)
}

/// A file-name `mhod`: UTF-16LE, padded to four bytes.
fn file_name_record(name: &str) -> Result<Vec<u8>, WriteError> {
    let text: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let padding = (4 - (STRING_MHOD_FIELDS + text.len()) % 4) % 4;
    let total = STRING_MHOD_FIELDS + text.len() + padding;
    let mut header = Header::new(b"mhod", u32_len(STRING_MHOD_FIELDS)?);
    header
        .u32(0x04, MHOD_LEN)
        .u32(0x08, u32_len(total)?)
        .u16(0x0c, FILE_NAME)
        .u8(0x0f, padding as u8)
        .u32(0x18, u32_len(text.len())?)
        // UTF-16.
        .u8(0x1c, 2);
    let mut bytes = header.0;
    bytes.extend_from_slice(&text);
    bytes.resize(total, 0);
    Ok(bytes)
}

#[cfg(test)]
mod tests;
