// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Cover-art rendering for the Pioneer export.
//!
//! The XDJ/CDJ browse and now-playing screens read album art from JPEG
//! thumbnails on the drive, addressed by the `export.pdb` artwork table.
//! Each unique cover is stored as two square renditions under a single
//! bucket directory:
//!
//! ```text
//! /PIONEER/Artwork/00001/a{id}.jpg     80×80  (browse list)
//! /PIONEER/Artwork/00001/a{id}_m.jpg   240×240 (full-screen)
//! ```
//!
//! [`ArtworkSet`] accumulates covers across a track set, de-duplicating
//! identical images by content hash so an album's shared art is decoded,
//! resized, and stored once. It hands the caller the PDB id↔path rows
//! ([`crate::model::PioneerArtwork`]) and rendered JPEG byte payloads. The
//! device-sync crate owns filesystem publication so removable-media writes
//! stay root-confined.
//!
//! Artwork ids are assigned from scratch on every export (1, 2, 3… in
//! first-seen order), so the caller clears the bucket before writing — a
//! previous, differently-numbered run leaves no orphan thumbnails behind.

use std::collections::HashMap;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::{DynamicImage, ExtendedColorType, ImageEncoder};
use sha2::{Digest, Sha256};

use crate::model::PioneerArtwork;

/// Browse-list thumbnail edge, in pixels.
const SMALL_SIZE: u32 = 80;
/// Full-screen thumbnail edge, in pixels.
const LARGE_SIZE: u32 = 240;
/// JPEG quality for both renditions. The reference exporter intended 90
/// (its code drifted to the encoder default); 90 is the deliberate
/// choice here — a few KB per cover for a visibly cleaner thumbnail.
const JPEG_QUALITY: u8 = 90;
/// The single bucket directory every Pioneer thumbnail lives under,
/// relative to the drive root. Rekordbox uses a numbered-bucket scheme;
/// one bucket is sufficient and matches the validated reference.
pub const ARTWORK_BUCKET: &str = "PIONEER/Artwork/00001";

/// A cover failed to render. Callers treat this as "this track has no
/// artwork" (a non-fatal degradation) rather than aborting the export.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtworkError {
    /// The embedded image bytes could not be decoded (unsupported or
    /// corrupt format).
    Decode,
    /// The resized image could not be re-encoded as JPEG.
    Encode,
}

impl std::fmt::Display for ArtworkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Decode => f.write_str("could not decode embedded cover art"),
            Self::Encode => f.write_str("could not encode cover-art thumbnail"),
        }
    }
}

impl std::error::Error for ArtworkError {}

/// One processed cover, retained until the files are written.
struct Processed {
    id: u32,
    path: String,
    small: Vec<u8>,
    large: Vec<u8>,
}

/// Accumulates the unique covers for one export, de-duplicating by
/// content hash and assigning each a 1-based id used by the PDB.
#[derive(Default)]
pub struct ArtworkSet {
    /// Maps a cover's SHA-256 to its already-assigned id.
    by_hash: HashMap<[u8; 32], u32>,
    /// Unique covers, in id order (`processed[i].id == i + 1`).
    processed: Vec<Processed>,
}

impl ArtworkSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a track's embedded cover bytes, returning the artwork id
    /// to store in its PDB row. Identical covers (byte-for-byte) reuse
    /// the existing id, so an album's shared art is rendered once.
    /// Returns [`ArtworkError`] when the bytes are not a decodable image.
    pub fn add(&mut self, cover: &[u8]) -> Result<u32, ArtworkError> {
        let hash: [u8; 32] = Sha256::digest(cover).into();
        if let Some(&id) = self.by_hash.get(&hash) {
            return Ok(id);
        }
        let image = image::load_from_memory(cover).map_err(|_| ArtworkError::Decode)?;
        let small = encode_thumbnail(&image, SMALL_SIZE)?;
        let large = encode_thumbnail(&image, LARGE_SIZE)?;
        let id = self.processed.len() as u32 + 1;
        self.processed.push(Processed {
            id,
            path: format!("/{ARTWORK_BUCKET}/a{id}.jpg"),
            small,
            large,
        });
        self.by_hash.insert(hash, id);
        Ok(id)
    }

    /// The id↔path rows for the PDB artwork table, in id order.
    pub fn rows(&self) -> Vec<PioneerArtwork> {
        self.processed
            .iter()
            .map(|p| PioneerArtwork {
                id: p.id,
                path: p.path.clone(),
            })
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.processed.is_empty()
    }

    pub fn len(&self) -> usize {
        self.processed.len()
    }

    /// Rendered JPEG files for root-confined publication by the sync engine.
    pub fn files(&self) -> impl Iterator<Item = (String, &[u8])> {
        self.processed.iter().flat_map(|art| {
            [
                (format!("a{}.jpg", art.id), art.small.as_slice()),
                (format!("a{}_m.jpg", art.id), art.large.as_slice()),
            ]
        })
    }
}

/// Resize a decoded cover to a square `size`×`size` thumbnail and encode
/// it as JPEG. Covers are normally square already; matching the
/// reference, the resize is exact (any non-square source is squished
/// rather than letterboxed) so the hardware's fixed thumbnail slot fills
/// edge to edge.
fn encode_thumbnail(image: &DynamicImage, size: u32) -> Result<Vec<u8>, ArtworkError> {
    let scaled = image
        .resize_exact(size, size, FilterType::Lanczos3)
        .to_rgb8();
    let mut buffer = Vec::new();
    JpegEncoder::new_with_quality(&mut buffer, JPEG_QUALITY)
        .write_image(scaled.as_raw(), size, size, ExtendedColorType::Rgb8)
        .map_err(|_| ArtworkError::Encode)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal solid-colour PNG, to exercise decode → resize → encode
    /// without bundling a binary fixture.
    fn solid_png(r: u8, g: u8, b: u8) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(8, 8, image::Rgb([r, g, b]));
        let mut buffer = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("encode test png");
        buffer.into_inner()
    }

    #[test]
    fn assigns_sequential_ids_and_deduplicates() {
        let mut set = ArtworkSet::new();
        let red = solid_png(255, 0, 0);
        let blue = solid_png(0, 0, 255);

        assert_eq!(set.add(&red).expect("add red"), 1);
        assert_eq!(set.add(&blue).expect("add blue"), 2);
        // The same cover reuses its id and adds no new entry.
        assert_eq!(set.add(&red).expect("re-add red"), 1);
        assert_eq!(set.len(), 2);

        let rows = set.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, 1);
        assert_eq!(rows[0].path, "/PIONEER/Artwork/00001/a1.jpg");
        assert_eq!(rows[1].path, "/PIONEER/Artwork/00001/a2.jpg");
    }

    #[test]
    fn rejects_undecodable_bytes() {
        let mut set = ArtworkSet::new();
        assert_eq!(set.add(b"this is not an image"), Err(ArtworkError::Decode));
        assert!(set.is_empty());
    }

    #[test]
    fn exposes_both_rendered_renditions() {
        let mut set = ArtworkSet::new();
        set.add(&solid_png(0, 255, 0)).expect("add green");
        let files: Vec<_> = set.files().map(|(name, _)| name).collect();
        assert_eq!(files, ["a1.jpg", "a1_m.jpg"]);
    }
}
