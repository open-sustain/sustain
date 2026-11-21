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
/// the panel background/text). Each request uploads only its requested
/// texture size; shared metadata is cached with either size.
#[derive(Clone, Default)]
pub(crate) struct DecodedArtwork {
    pub(crate) tile_texture: Option<gdk::Texture>,
    pub(crate) detail_texture: Option<gdk::Texture>,
    pub(crate) palette: Option<ArtworkPalette>,
    pub(crate) dimensions: Option<ArtworkDimensions>,
    pub(crate) encoded_bytes_len: Option<usize>,
}

impl DecodedArtwork {
    /// Whether the source has a cover, drawable or not: its dimensions are
    /// known even when no texture could be made of it.
    pub(crate) const fn has_cover(&self) -> bool {
        self.dimensions.is_some()
    }
}

/// Which cached texture size a request wants.
///
/// The two sizes have very different memory profiles, so they live in
/// separate bounded caches (`MAX_CACHED_TILE_ARTWORKS` and
/// `MAX_CACHED_DETAIL_ARTWORKS`) and a worker only uploads the texture for
/// the size that was actually asked for. The on-disk cache stores tile and
/// detail payloads independently in the same row, so a later request for the
/// other size is a hit only if that variant was already produced.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum ArtworkVariant {
    /// Small grid / now-playing cover ([`TILE_TEXTURE_MAX_SIDE`]).
    Tile,
    /// Large panel / overlay cover ([`DETAIL_TEXTURE_MAX_SIDE`]).
    Detail,
}

impl ArtworkVariant {
    /// The side of the variant's square texture, at most.
    const fn max_side(self) -> i32 {
        match self {
            Self::Tile => TILE_TEXTURE_MAX_SIDE,
            Self::Detail => DETAIL_TEXTURE_MAX_SIDE,
        }
    }
}

#[derive(Default)]
pub(super) struct DecodedArtworkRecord {
    pub(super) artwork: DecodedArtwork,
    pub(super) cache_entry: CachedArtwork,
}

/// What loading a source's artwork came to.
pub(crate) enum ArtworkLoad {
    /// The cover, or the confirmed absence of one: a texture-less
    /// [`DecodedArtwork`]. Both may be cached.
    Settled(DecodedArtwork),
    /// The artwork could not be read or decoded, which may not last (a
    /// file mid-replacement, the image loader unavailable). Its waiters
    /// get the placeholder, but nothing is cached, so the next request
    /// tries again.
    Failed,
}

impl ArtworkLoad {
    /// What to show for this outcome now.
    pub(crate) fn shown(&self) -> DecodedArtwork {
        match self {
            Self::Settled(decoded) => decoded.clone(),
            Self::Failed => DecodedArtwork::default(),
        }
    }
}

/// A source's new artwork, decoded in both sizes: what
/// [`ArtworkLoader::prime`](super::ArtworkLoader::prime) installs.
///
/// Decoding is the expensive part, so it is done first, off the GTK
/// thread; priming then changes the loader's state at once, leaving no
/// moment in which the source's old artwork is gone and its new one not
/// yet there.
pub(crate) struct PrimedArtwork {
    pub(super) tile: ArtworkLoad,
    pub(super) detail: ArtworkLoad,
}

impl PrimedArtwork {
    /// Decode `bytes`, a source's new encoded cover or `None` when it no
    /// longer has one. The image is decoded once, at the detail size, and
    /// the tile scaled from that. Too slow for the GTK thread: run it on a
    /// worker.
    ///
    /// A cover the image loader cannot draw still is the source's cover:
    /// it is installed without a texture, shown as the placeholder, so the
    /// file is not read again for the cover it is about to replace. Only
    /// bytes that are no image at all fail.
    pub(crate) fn decode(bytes: Option<&[u8]>) -> Self {
        match bytes {
            Some(bytes) => Self::decode_cover(bytes, None).0,
            None => Self::settled(&DecodedArtwork::default()),
        }
    }

    /// [`Self::decode`] a new cover, with a preview of the whole image,
    /// uncropped, fitting a `side`-pixel square: what an editor shows before
    /// the cover is accepted. Still one decode.
    pub(crate) fn decode_with_preview(bytes: &[u8], side: i32) -> (Self, Option<gdk::Texture>) {
        Self::decode_cover(bytes, Some(side))
    }

    fn decode_cover(bytes: &[u8], preview_side: Option<i32>) -> (Self, Option<gdk::Texture>) {
        let max_side = preview_side.map_or(DETAIL_TEXTURE_MAX_SIDE, |side| {
            side.max(DETAIL_TEXTURE_MAX_SIDE)
        });
        if let Some(decoded) = decode_scaled(bytes.to_vec(), max_side) {
            let preview = preview_side
                .and_then(|side| fitted_pixbuf(&decoded.pixbuf, side))
                .map(|pixbuf| gdk::Texture::for_pixbuf(&pixbuf));
            let primed = Self {
                tile: ArtworkLoad::Settled(decoded.at(ArtworkVariant::Tile).artwork()),
                detail: ArtworkLoad::Settled(decoded.at(ArtworkVariant::Detail).artwork()),
            };
            return (primed, preview);
        }
        let primed = match validate_encoded_artwork(bytes) {
            Ok(dimensions) => Self::settled(&DecodedArtwork {
                dimensions: Some(dimensions),
                encoded_bytes_len: Some(bytes.len()),
                ..DecodedArtwork::default()
            }),
            Err(_) => Self {
                tile: ArtworkLoad::Failed,
                detail: ArtworkLoad::Failed,
            },
        };
        (primed, None)
    }

    fn settled(decoded: &DecodedArtwork) -> Self {
        Self {
            tile: ArtworkLoad::Settled(decoded.clone()),
            detail: ArtworkLoad::Settled(decoded.clone()),
        }
    }
}

/// Decode `bytes`, a source's embedded artwork or `None` for none, at
/// `variant`'s size, with the cache entry that stores it: the variant's
/// texture encoded as PNG. `None` when there are bytes but they do not
/// decode.
pub(super) fn decode_artwork_record(
    bytes: Option<Vec<u8>>,
    variant: ArtworkVariant,
) -> Option<DecodedArtworkRecord> {
    let Some(bytes) = bytes else {
        return Some(DecodedArtworkRecord::default());
    };
    let decoded = decode_variant(bytes, variant)?;
    let png = decoded.texture_pixbuf.as_ref().and_then(pixbuf_png_bytes);
    let (tile_png, detail_png) = match variant {
        ArtworkVariant::Tile => (png, None),
        ArtworkVariant::Detail => (None, png),
    };
    Some(DecodedArtworkRecord {
        artwork: decoded.artwork(),
        cache_entry: CachedArtwork {
            dimensions: Some(decoded.dimensions),
            encoded_bytes_len: Some(decoded.encoded_bytes_len),
            tile_png,
            detail_png,
            palette: decoded.palette.map(ArtworkPalette::components),
        },
    })
}

/// Encoded artwork decoded at one variant's size.
struct VariantDecode {
    variant: ArtworkVariant,
    /// The variant's square texture, `None` if it could not be scaled.
    texture_pixbuf: Option<gdk_pixbuf::Pixbuf>,
    palette: Option<ArtworkPalette>,
    dimensions: ArtworkDimensions,
    encoded_bytes_len: usize,
}

/// Decode `bytes` at `variant`'s size only: tile scrolling must not pay for
/// the 396px detail texture; a later detail request decodes that variant if
/// it is actually needed.
fn decode_variant(bytes: Vec<u8>, variant: ArtworkVariant) -> Option<VariantDecode> {
    decode_scaled(bytes, variant.max_side()).map(|decoded| decoded.at(variant))
}

/// Encoded artwork decoded at a scale, uncropped.
struct ScaledDecode {
    pixbuf: gdk_pixbuf::Pixbuf,
    palette: Option<ArtworkPalette>,
    dimensions: ArtworkDimensions,
    encoded_bytes_len: usize,
}

/// Decode `bytes` scaled so its shorter side is `max_side`, or its own if
/// smaller: enough for any variant no larger than that.
fn decode_scaled(bytes: Vec<u8>, max_side: i32) -> Option<ScaledDecode> {
    let encoded_bytes_len = bytes.len();
    let dimensions = validate_encoded_artwork(&bytes).ok()?;
    let (decode_width, decode_height) = scaled_dimensions(dimensions, max_side)?;
    let pixbuf = pixbuf_from_bytes_at_scale(bytes, decode_width, decode_height)?;
    Some(ScaledDecode {
        palette: ArtworkPalette::from_pixbuf(&pixbuf),
        pixbuf,
        dimensions,
        encoded_bytes_len,
    })
}

impl ScaledDecode {
    /// The decode at `variant`'s size, which must not exceed the decode's.
    fn at(&self, variant: ArtworkVariant) -> VariantDecode {
        VariantDecode {
            variant,
            texture_pixbuf: scaled_pixbuf(&self.pixbuf, variant.max_side()),
            palette: self.palette,
            dimensions: self.dimensions,
            encoded_bytes_len: self.encoded_bytes_len,
        }
    }
}

impl VariantDecode {
    /// The in-memory [`DecodedArtwork`] for the decoded size.
    ///
    /// Only that variant's `gdk::Texture` is uploaded; the other stays
    /// `None`. This is the crux of the tile/detail split — a grid tile
    /// request never materialises the ~9×-larger detail texture, so
    /// scrolling the Albums grid cannot accumulate detail textures in the
    /// small device-local VRAM heap. The palette and dimensions are
    /// size-independent and travel with either variant.
    fn artwork(&self) -> DecodedArtwork {
        let texture = self.texture_pixbuf.as_ref().map(gdk::Texture::for_pixbuf);
        let (tile_texture, detail_texture) = match self.variant {
            ArtworkVariant::Tile => (texture, None),
            ArtworkVariant::Detail => (None, texture),
        };
        DecodedArtwork {
            tile_texture,
            detail_texture,
            palette: self.palette,
            dimensions: Some(self.dimensions),
            encoded_bytes_len: Some(self.encoded_bytes_len),
        }
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

/// `pixbuf` scaled down to fit a `side`-pixel square, whole.
fn fitted_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf, side: i32) -> Option<gdk_pixbuf::Pixbuf> {
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let longer_side = width.max(height);
    if longer_side <= 0 || side <= 0 {
        return None;
    }
    if longer_side <= side {
        return Some(pixbuf.clone());
    }
    let scale = f64::from(side) / f64::from(longer_side);
    pixbuf.scale_simple(
        (f64::from(width) * scale).round().max(1.0) as i32,
        (f64::from(height) * scale).round().max(1.0) as i32,
        gdk_pixbuf::InterpType::Bilinear,
    )
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
    let Some(accent) = rgb_from_cache_columns(row, 14)? else {
        return Ok(None);
    };
    Ok(Some(ArtworkPaletteComponents {
        background,
        foreground,
        secondary,
        accent,
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
    use gtk::gdk_pixbuf;
    use gtk::prelude::*;

    use super::{PrimedArtwork, texture_from_png};

    #[test]
    fn an_editor_previews_the_whole_image_while_the_covers_stay_square() {
        let pixbuf = gdk_pixbuf::Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 800, 400)
            .expect("a pixbuf");
        pixbuf.fill(0x3366_99ff);
        let bytes = pixbuf.save_to_bufferv("png", &[]).expect("png encodes");

        let (primed, preview) = PrimedArtwork::decode_with_preview(&bytes, 320);

        let preview = preview.expect("a preview");
        assert_eq!((preview.width(), preview.height()), (320, 160));
        let tile = primed.tile.shown().tile_texture.expect("a tile");
        let detail = primed
            .detail
            .shown()
            .detail_texture
            .expect("a detail cover");
        assert_eq!((tile.width(), tile.height()), (132, 132));
        assert_eq!((detail.width(), detail.height()), (396, 396));
    }

    #[test]
    fn corrupt_cached_png_degrades_to_placeholder() {
        assert!(texture_from_png(b"not a cached PNG").is_none());
    }
}
