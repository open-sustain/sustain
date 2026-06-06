// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Resource policy for untrusted cover artwork.
//!
//! Artwork arrives from tags, local picker selections, remote providers, and
//! the derived GTK cache. Every consumer validates through this crate before
//! decoding so compressed images cannot expand into unbounded memory use.

#![forbid(unsafe_code)]

use std::{
    fmt,
    fs::File,
    io::{self, Cursor, Read},
    path::Path,
};

use image::{
    DynamicImage, ImageReader, Limits, RgbImage, codecs::jpeg::JpegEncoder, imageops::FilterType,
};

pub const MAX_ENCODED_ARTWORK_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ARTWORK_WIDTH: u64 = 4096;
pub const MAX_ARTWORK_HEIGHT: u64 = 4096;
pub const MAX_DECODED_ARTWORK_PIXELS: u64 = 16_777_216;
const MAX_DECODED_ARTWORK_BYTES: u64 = MAX_DECODED_ARTWORK_PIXELS * 8;

/// Longest-edge ceiling for artwork that Sustain re-encodes before it
/// enters the library. The largest on-screen consumer is the 396 px
/// detail view (≈792 px at 2× HiDPI), so 1024 px leaves comfortable
/// headroom while keeping the embedded payload small.
pub const NORMALIZE_MAX_EDGE: u32 = 1024;
/// Soft size budget for normalized artwork. Re-encoding steps quality
/// down toward a quality floor to try to land under this; a pathological
/// high-entropy cover may still finish above it.
pub const NORMALIZE_TARGET_BYTES: usize = 250 * 1024;
/// Initial JPEG quality used when re-encoding oversized artwork.
const NORMALIZE_JPEG_QUALITY: u8 = 90;
/// Lowest JPEG quality the size-budget step-down will fall back to.
const NORMALIZE_MIN_JPEG_QUALITY: u8 = 70;
/// Quality decrement applied each time an encode overshoots the budget.
const NORMALIZE_JPEG_QUALITY_STEP: u8 = 5;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtworkDimensions {
    pub width: u32,
    pub height: u32,
    pub pixels: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtworkPolicyError {
    EncodedPayloadTooLarge,
    UnsupportedOrCorruptImage,
    ZeroDimension,
    WidthTooLarge,
    HeightTooLarge,
    DecodedPixelCountOverflow,
    DecodedPixelCountTooLarge,
}

impl fmt::Display for ArtworkPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EncodedPayloadTooLarge => {
                f.write_str("the image exceeds the 16 MiB encoded-size limit")
            }
            Self::UnsupportedOrCorruptImage => {
                f.write_str("the image format is unsupported or the file is corrupt")
            }
            Self::ZeroDimension => f.write_str("the image has an invalid zero-sized dimension"),
            Self::WidthTooLarge => f.write_str("the image is wider than 4096 pixels"),
            Self::HeightTooLarge => f.write_str("the image is taller than 4096 pixels"),
            Self::DecodedPixelCountOverflow => {
                f.write_str("the image dimensions overflow the decoded-size calculation")
            }
            Self::DecodedPixelCountTooLarge => {
                f.write_str("the decoded image exceeds the 16,777,216-pixel limit")
            }
        }
    }
}

impl std::error::Error for ArtworkPolicyError {}

#[derive(Debug)]
pub enum ArtworkReadError {
    Io(io::Error),
    Policy(ArtworkPolicyError),
}

impl fmt::Display for ArtworkReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => f.write_str("the image file could not be read"),
            Self::Policy(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ArtworkReadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Policy(error) => Some(error),
        }
    }
}

impl From<io::Error> for ArtworkReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ArtworkPolicyError> for ArtworkReadError {
    fn from(error: ArtworkPolicyError) -> Self {
        Self::Policy(error)
    }
}

pub fn validate_encoded_artwork(bytes: &[u8]) -> Result<ArtworkDimensions, ArtworkPolicyError> {
    if bytes.len() > MAX_ENCODED_ARTWORK_BYTES {
        return Err(ArtworkPolicyError::EncodedPayloadTooLarge);
    }
    let mut reader = artwork_reader(bytes)?;
    reader.limits(decoder_limits());
    let (width, height) = reader
        .into_dimensions()
        .map_err(|_| ArtworkPolicyError::UnsupportedOrCorruptImage)?;
    validate_dimensions(u64::from(width), u64::from(height))
}

pub fn validate_dimensions(
    width: u64,
    height: u64,
) -> Result<ArtworkDimensions, ArtworkPolicyError> {
    let pixels = width
        .checked_mul(height)
        .ok_or(ArtworkPolicyError::DecodedPixelCountOverflow)?;
    if width == 0 || height == 0 {
        return Err(ArtworkPolicyError::ZeroDimension);
    }
    if width > MAX_ARTWORK_WIDTH {
        return Err(ArtworkPolicyError::WidthTooLarge);
    }
    if height > MAX_ARTWORK_HEIGHT {
        return Err(ArtworkPolicyError::HeightTooLarge);
    }
    if pixels > MAX_DECODED_ARTWORK_PIXELS {
        return Err(ArtworkPolicyError::DecodedPixelCountTooLarge);
    }
    Ok(ArtworkDimensions {
        width: u32::try_from(width).map_err(|_| ArtworkPolicyError::WidthTooLarge)?,
        height: u32::try_from(height).map_err(|_| ArtworkPolicyError::HeightTooLarge)?,
        pixels,
    })
}

pub fn read_artwork_file(path: &Path) -> Result<Vec<u8>, ArtworkReadError> {
    let file = File::open(path)?;
    read_artwork(file)
}

pub fn read_artwork(reader: impl Read) -> Result<Vec<u8>, ArtworkReadError> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_ENCODED_ARTWORK_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    validate_encoded_artwork(&bytes)?;
    Ok(bytes)
}

/// Decode one representative static frame under the same resource policy.
///
/// `DynamicImage` is deliberately static: animated formats contribute their
/// representative frame only, which is sufficient for cover thumbnails.
pub fn decode_static_artwork(bytes: &[u8]) -> Result<DynamicImage, ArtworkPolicyError> {
    validate_encoded_artwork(bytes)?;
    let mut reader = artwork_reader(bytes)?;
    reader.limits(decoder_limits());
    reader
        .decode()
        .map_err(|_| ArtworkPolicyError::UnsupportedOrCorruptImage)
}

/// Tame an oversized cover before it is written into the library.
///
/// Remote providers happily serve multi-megabyte, multi-thousand-pixel
/// covers; embedding one verbatim into every track's tags wastes disk
/// and memory for detail the UI never renders above ~792 px. This caps
/// the longest edge at [`NORMALIZE_MAX_EDGE`] and re-encodes to JPEG,
/// stepping quality down toward a quality floor only when the result
/// overshoots [`NORMALIZE_TARGET_BYTES`].
///
/// Artwork already within both the dimension ceiling and the size
/// budget is returned untouched, so a small, clean embedded cover is
/// never needlessly recompressed (nor a lossless PNG silently degraded
/// to JPEG). The bytes are decoded under the same resource policy as
/// [`decode_static_artwork`], so a corrupt payload surfaces as an
/// [`ArtworkPolicyError`] rather than reaching the tag writer.
pub fn normalize_artwork(bytes: &[u8]) -> Result<Vec<u8>, ArtworkPolicyError> {
    let image = decode_static_artwork(bytes)?;
    let longest_edge = image.width().max(image.height());

    if longest_edge <= NORMALIZE_MAX_EDGE && bytes.len() <= NORMALIZE_TARGET_BYTES {
        return Ok(bytes.to_vec());
    }

    let scaled = if longest_edge > NORMALIZE_MAX_EDGE {
        image.resize(NORMALIZE_MAX_EDGE, NORMALIZE_MAX_EDGE, FilterType::Lanczos3)
    } else {
        image
    };

    // JPEG carries no alpha channel; flatten to RGB. Cover art is
    // opaque in practice, but a stray alpha plane must not corrupt the
    // encode.
    let rgb = scaled.to_rgb8();

    let mut quality = NORMALIZE_JPEG_QUALITY;
    loop {
        let encoded = encode_jpeg(&rgb, quality)?;
        if encoded.len() <= NORMALIZE_TARGET_BYTES || quality <= NORMALIZE_MIN_JPEG_QUALITY {
            return Ok(encoded);
        }
        quality -= NORMALIZE_JPEG_QUALITY_STEP;
    }
}

fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>, ArtworkPolicyError> {
    let mut buffer = Vec::new();
    JpegEncoder::new_with_quality(&mut buffer, quality)
        .encode_image(image)
        .map_err(|_| ArtworkPolicyError::UnsupportedOrCorruptImage)?;
    Ok(buffer)
}

fn artwork_reader(bytes: &[u8]) -> Result<ImageReader<Cursor<&[u8]>>, ArtworkPolicyError> {
    ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ArtworkPolicyError::UnsupportedOrCorruptImage)
}

fn decoder_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_ARTWORK_WIDTH as u32);
    limits.max_image_height = Some(MAX_ARTWORK_HEIGHT as u32);
    limits.max_alloc = Some(MAX_DECODED_ARTWORK_BYTES);
    limits
}

#[cfg(test)]
mod tests {
    use std::{io::Cursor, thread};

    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};

    use super::*;

    fn solid_png(width: u32, height: u32) -> Vec<u8> {
        let image = RgbImage::from_pixel(width, height, Rgb([10, 20, 30]));
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut output, ImageFormat::Png)
            .expect("encode PNG");
        output.into_inner()
    }

    fn noisy_rgb(width: u32, height: u32) -> RgbImage {
        // Deterministic high-entropy fill (xorshift32) so JPEG cannot
        // trivially compress it away — exercises the size-budget path.
        let mut state: u32 = 0x1234_5678;
        RgbImage::from_fn(width, height, |_, _| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            Rgb([
                (state & 0xff) as u8,
                ((state >> 8) & 0xff) as u8,
                ((state >> 16) & 0xff) as u8,
            ])
        })
    }

    fn noisy_png(width: u32, height: u32) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(noisy_rgb(width, height))
            .write_to(&mut output, ImageFormat::Png)
            .expect("encode PNG");
        output.into_inner()
    }

    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = Vec::from(*b"IHDR");
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        png.extend_from_slice(&(13_u32).to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        png
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    #[test]
    fn rejects_payload_over_encoded_limit_before_parsing() {
        assert_eq!(
            validate_encoded_artwork(&vec![0; MAX_ENCODED_ARTWORK_BYTES + 1]),
            Err(ArtworkPolicyError::EncodedPayloadTooLarge)
        );
    }

    #[test]
    fn rejects_tiny_payload_declaring_huge_dimensions() {
        assert_eq!(
            validate_encoded_artwork(&png_header(MAX_ARTWORK_WIDTH as u32 + 1, 1)),
            Err(ArtworkPolicyError::UnsupportedOrCorruptImage)
        );
    }

    #[test]
    fn rejects_dimension_arithmetic_overflow() {
        assert_eq!(
            validate_dimensions(u64::MAX, 2),
            Err(ArtworkPolicyError::DecodedPixelCountOverflow)
        );
    }

    #[test]
    fn accepts_large_but_allowed_dimensions() {
        assert_eq!(
            validate_dimensions(MAX_ARTWORK_WIDTH, MAX_ARTWORK_HEIGHT),
            Ok(ArtworkDimensions {
                width: 4096,
                height: 4096,
                pixels: MAX_DECODED_ARTWORK_PIXELS,
            })
        );
    }

    #[test]
    fn bounded_reader_rejects_oversized_input() {
        assert!(matches!(
            read_artwork(Cursor::new(vec![0; MAX_ENCODED_ARTWORK_BYTES + 32])),
            Err(ArtworkReadError::Policy(
                ArtworkPolicyError::EncodedPayloadTooLarge
            ))
        ));
    }

    #[test]
    fn static_decoder_accepts_valid_png() {
        let decoded = decode_static_artwork(&solid_png(8, 8)).expect("decode PNG");
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
    }

    #[test]
    fn normalize_passes_through_small_artwork_unchanged() {
        // Well within both the dimension ceiling and the size budget:
        // the original bytes must be returned verbatim, not re-encoded.
        let source = solid_png(300, 300);
        assert!(source.len() <= NORMALIZE_TARGET_BYTES);
        assert_eq!(normalize_artwork(&source).expect("normalize"), source);
    }

    #[test]
    fn normalize_caps_longest_edge_preserving_aspect() {
        let source = solid_png(2000, 1000);
        let normalized = normalize_artwork(&source).expect("normalize");
        let decoded = decode_static_artwork(&normalized).expect("decode normalized");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (NORMALIZE_MAX_EDGE, 512)
        );
    }

    #[test]
    fn normalize_shrinks_large_cover_and_caps_dimensions() {
        let source = noisy_png(1500, 1500);
        let normalized = normalize_artwork(&source).expect("normalize");
        assert!(normalized.len() < source.len());
        let decoded = decode_static_artwork(&normalized).expect("decode normalized");
        assert_eq!(decoded.width().max(decoded.height()), NORMALIZE_MAX_EDGE);
    }

    #[test]
    fn normalize_meets_size_budget_for_compressible_cover() {
        let source = solid_png(3000, 3000);
        let normalized = normalize_artwork(&source).expect("normalize");
        assert!(normalized.len() <= NORMALIZE_TARGET_BYTES);
        let decoded = decode_static_artwork(&normalized).expect("decode normalized");
        assert_eq!(decoded.width().max(decoded.height()), NORMALIZE_MAX_EDGE);
    }

    #[test]
    fn lower_jpeg_quality_yields_smaller_payload() {
        // The step-down loop relies on quality being a monotone size
        // lever; verify it directly on a high-entropy image.
        let rgb = noisy_rgb(256, 256);
        let high = encode_jpeg(&rgb, NORMALIZE_JPEG_QUALITY).expect("encode high quality");
        let low = encode_jpeg(&rgb, NORMALIZE_MIN_JPEG_QUALITY).expect("encode low quality");
        assert!(low.len() < high.len());
    }

    #[test]
    fn concurrent_validation_rejects_corrupt_payloads() {
        let handles: Vec<_> = (0..8)
            .map(|_| thread::spawn(|| validate_encoded_artwork(b"not an image")))
            .collect();
        for handle in handles {
            assert_eq!(
                handle.join().expect("join validator"),
                Err(ArtworkPolicyError::UnsupportedOrCorruptImage)
            );
        }
    }
}
