// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::{gdk, gdk_pixbuf, gio, glib};
use sustain_artwork::{ArtworkDimensions, validate_encoded_artwork};

use crate::artwork_color::{ArtworkPalette, ArtworkPaletteComponents, RgbColorComponents};

use super::disk_cache::CachedArtwork;

/// Maximum side length of the smaller cached texture. Sized to cover
/// the Albums grid tile (132px) and the now-playing tile (72px) without
/// either having to upscale. Bigger consumers (album-detail panel,
/// lyrics/artwork overlay) use the detail texture below.
const TILE_TEXTURE_MAX_SIDE: i32 = 132;

/// Maximum side length of the larger cached texture. Sized to cover
/// the album-detail panel (3× the grid tile). The cache stores PNG
/// payloads at this size; views downscale further at paint time.
const DETAIL_TEXTURE_MAX_SIDE: i32 = TILE_TEXTURE_MAX_SIDE * 3;

/// Decoded artwork shared between tile rendering (needs only the
/// texture) and detail-panel rendering (also needs the palette to tint
/// the panel background/text). Both are computed once per file and
/// cached.
#[derive(Clone, Default)]
pub(crate) struct DecodedArtwork {
    pub(crate) tile_texture: Option<gdk::Texture>,
    pub(crate) detail_texture: Option<gdk::Texture>,
    pub(crate) palette: Option<ArtworkPalette>,
    pub(crate) dimensions: Option<ArtworkDimensions>,
    pub(crate) encoded_bytes_len: Option<usize>,
}

/// Which cached texture size a request wants.
///
/// The two sizes have very different memory profiles, so they live in
/// separate bounded caches (`MAX_CACHED_TILE_ARTWORKS` and
/// `MAX_CACHED_DETAIL_ARTWORKS`) and a worker only uploads the texture for
/// the size that was actually asked for. The on-disk cache always holds both
/// PNG payloads, so the *other* size, when later requested, is produced from
/// disk without re-reading the audio file.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ArtworkVariant {
    /// Small grid / now-playing cover ([`TILE_TEXTURE_MAX_SIDE`]).
    Tile,
    /// Large panel / overlay cover ([`DETAIL_TEXTURE_MAX_SIDE`]).
    Detail,
}

#[derive(Default)]
pub(super) struct DecodedArtworkRecord {
    pub(super) artwork: DecodedArtwork,
    pub(super) cache_entry: CachedArtwork,
}

pub(super) fn decode_artwork(
    bytes: Option<Vec<u8>>,
    variant: ArtworkVariant,
) -> DecodedArtworkRecord {
    let Some(bytes) = bytes else {
        return DecodedArtworkRecord::default();
    };
    let encoded_bytes_len = bytes.len();
    let dimensions = match validate_encoded_artwork(&bytes) {
        Ok(dimensions) => dimensions,
        Err(_) => return DecodedArtworkRecord::default(),
    };
    let Some((decode_width, decode_height)) =
        scaled_dimensions(dimensions, DETAIL_TEXTURE_MAX_SIDE)
    else {
        return DecodedArtworkRecord::default();
    };
    let Some(pixbuf) = pixbuf_from_bytes_at_scale(bytes, decode_width, decode_height) else {
        return DecodedArtworkRecord::default();
    };

    // Both sizes are always scaled and PNG-encoded for the on-disk cache, so
    // the *other* size can later be served from disk without re-reading the
    // audio file. Only the requested size is uploaded to a GPU texture.
    let tile_pixbuf = scaled_pixbuf(&pixbuf, TILE_TEXTURE_MAX_SIDE);
    let detail_pixbuf = scaled_pixbuf(&pixbuf, DETAIL_TEXTURE_MAX_SIDE);
    let palette = ArtworkPalette::from_pixbuf(&pixbuf);
    let cache_entry = CachedArtwork {
        dimensions: Some(dimensions),
        encoded_bytes_len: Some(encoded_bytes_len),
        tile_png: tile_pixbuf.as_ref().and_then(pixbuf_png_bytes),
        detail_png: detail_pixbuf.as_ref().and_then(pixbuf_png_bytes),
        palette: palette.map(ArtworkPalette::components),
    };
    let artwork = artwork_for_variant(
        variant,
        tile_pixbuf.as_ref(),
        detail_pixbuf.as_ref(),
        palette,
        Some(dimensions),
        Some(encoded_bytes_len),
    );

    DecodedArtworkRecord {
        artwork,
        cache_entry,
    }
}

/// Assemble the in-memory [`DecodedArtwork`] for a single requested size.
///
/// Only the requested variant's `gdk::Texture` is uploaded; the other stays
/// `None`. This is the crux of the tile/detail split — a grid tile request
/// never materialises the ~9×-larger detail texture, so scrolling the Albums
/// grid cannot accumulate detail textures in the small device-local VRAM
/// heap. The palette and dimensions are size-independent and travel with
/// either variant.
fn artwork_for_variant(
    variant: ArtworkVariant,
    tile_pixbuf: Option<&gdk_pixbuf::Pixbuf>,
    detail_pixbuf: Option<&gdk_pixbuf::Pixbuf>,
    palette: Option<ArtworkPalette>,
    dimensions: Option<ArtworkDimensions>,
    encoded_bytes_len: Option<usize>,
) -> DecodedArtwork {
    let (tile_texture, detail_texture) = match variant {
        ArtworkVariant::Tile => (tile_pixbuf.map(gdk::Texture::for_pixbuf), None),
        ArtworkVariant::Detail => (None, detail_pixbuf.map(gdk::Texture::for_pixbuf)),
    };
    DecodedArtwork {
        tile_texture,
        detail_texture,
        palette,
        dimensions,
        encoded_bytes_len,
    }
}

/// Scale the source so the shorter side equals `max_side` (or the source's
/// own shorter side, whichever is smaller), then center-crop to a square.
///
/// The resulting pixbuf is always square. That matters because `GtkPicture`
/// with `ContentFit::Contain` does HEIGHT_FOR_WIDTH measurement: a 132×131
/// texture in a 132-wide cover would request natural height ≠ 132, which
/// propagates through the cover Box's measure and shifts the labels' Y
/// position downstream. Forcing the texture square pins natural height to
/// width across every album, regardless of source aspect ratio, and matches
/// the iTunes-style square-thumbnail look.
pub(super) fn scaled_pixbuf(
    pixbuf: &gdk_pixbuf::Pixbuf,
    max_side: i32,
) -> Option<gdk_pixbuf::Pixbuf> {
    let width = pixbuf.width();
    let height = pixbuf.height();
    if width <= 0 || height <= 0 || max_side <= 0 {
        return None;
    }

    let shorter_side = width.min(height);
    let scale = (f64::from(max_side) / f64::from(shorter_side)).min(1.0);
    let scaled_width = (f64::from(width) * scale).round().max(1.0) as i32;
    let scaled_height = (f64::from(height) * scale).round().max(1.0) as i32;

    let scaled = if scaled_width == width && scaled_height == height {
        pixbuf.clone()
    } else {
        pixbuf.scale_simple(
            scaled_width,
            scaled_height,
            gdk_pixbuf::InterpType::Bilinear,
        )?
    };

    let side = scaled.width().min(scaled.height());
    if scaled.width() == side && scaled.height() == side {
        return Some(scaled);
    }

    let x_offset = (scaled.width() - side) / 2;
    let y_offset = (scaled.height() - side) / 2;
    let cropped = gdk_pixbuf::Pixbuf::new(
        scaled.colorspace(),
        scaled.has_alpha(),
        scaled.bits_per_sample(),
        side,
        side,
    )?;
    scaled.copy_area(x_offset, y_offset, side, side, &cropped, 0, 0);
    Some(cropped)
}

pub(super) fn pixbuf_png_bytes(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<u8>> {
    pixbuf.save_to_bufferv("png", &[]).ok()
}

pub(super) fn texture_from_png(bytes: &[u8]) -> Option<gdk::Texture> {
    let dimensions = validate_encoded_artwork(bytes).ok()?;
    let width = i32::try_from(dimensions.width).ok()?;
    let height = i32::try_from(dimensions.height).ok()?;
    let pixbuf = pixbuf_from_bytes_at_scale(bytes.to_vec(), width, height)?;
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

fn scaled_dimensions(dimensions: ArtworkDimensions, max_side: i32) -> Option<(i32, i32)> {
    let width = i32::try_from(dimensions.width).ok()?;
    let height = i32::try_from(dimensions.height).ok()?;
    let shorter_side = width.min(height);
    if shorter_side <= 0 || max_side <= 0 {
        return None;
    }
    let scale = (f64::from(max_side) / f64::from(shorter_side)).min(1.0);
    Some((
        (f64::from(width) * scale).round().max(1.0) as i32,
        (f64::from(height) * scale).round().max(1.0) as i32,
    ))
}

fn pixbuf_from_bytes_at_scale(
    bytes: Vec<u8>,
    width: i32,
    height: i32,
) -> Option<gdk_pixbuf::Pixbuf> {
    let bytes = glib::Bytes::from_owned(bytes);
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream,
        width,
        height,
        false,
        None::<&gio::Cancellable>,
    )
    .ok()
}

pub(super) fn palette_components_from_cache_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Option<ArtworkPaletteComponents>> {
    let Some(background) = rgb_from_cache_columns(row, 5)? else {
        return Ok(None);
    };
    let Some(foreground) = rgb_from_cache_columns(row, 8)? else {
        return Ok(None);
    };
    let Some(secondary) = rgb_from_cache_columns(row, 11)? else {
        return Ok(None);
    };
    Ok(Some(ArtworkPaletteComponents {
        background,
        foreground,
        secondary,
    }))
}

pub(super) fn rgb_from_cache_columns(
    row: &rusqlite::Row<'_>,
    first_column: usize,
) -> rusqlite::Result<Option<RgbColorComponents>> {
    let red: Option<i64> = row.get(first_column)?;
    let green: Option<i64> = row.get(first_column + 1)?;
    let blue: Option<i64> = row.get(first_column + 2)?;
    let (Some(red), Some(green), Some(blue)) = (red, green, blue) else {
        return Ok(None);
    };
    let (Ok(red), Ok(green), Ok(blue)) =
        (u8::try_from(red), u8::try_from(green), u8::try_from(blue))
    else {
        return Ok(None);
    };
    Ok(Some(RgbColorComponents { red, green, blue }))
}

#[cfg(test)]
mod tests {
    use super::texture_from_png;

    #[test]
    fn corrupt_cached_png_degrades_to_placeholder() {
        assert!(texture_from_png(b"not a cached PNG").is_none());
    }
}
