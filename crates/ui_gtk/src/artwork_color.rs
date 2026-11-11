// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::BTreeMap;

use gtk::{gdk, gdk_pixbuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtworkPalette {
    background: RgbColor,
    foreground: RgbColor,
    secondary: RgbColor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtworkPaletteComponents {
    pub(crate) background: RgbColorComponents,
    pub(crate) foreground: RgbColorComponents,
    pub(crate) secondary: RgbColorComponents,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RgbColorComponents {
    pub(crate) red: u8,
    pub(crate) green: u8,
    pub(crate) blue: u8,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct QuantizedColor(u16);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ColorBucket {
    red_total: u64,
    green_total: u64,
    blue_total: u64,
    score: u64,
    pixels: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RgbColor {
    red: u8,
    green: u8,
    blue: u8,
}

impl ArtworkPalette {
    pub(crate) fn from_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Self> {
        let buckets = scored_buckets_from_pixbuf(pixbuf)?;
        let background = buckets.first()?.average()?;
        let foreground = readable_foreground(background);
        let secondary = pick_secondary(&buckets, background, foreground);
        Some(Self {
            background,
            foreground,
            secondary,
        })
    }

    pub(crate) fn background_css(self) -> String {
        self.background.css_hex()
    }

    pub(crate) fn background_rgba(self) -> gdk::RGBA {
        self.background.rgba()
    }

    pub(crate) fn from_components(components: ArtworkPaletteComponents) -> Self {
        Self {
            background: RgbColor::from_components(components.background),
            foreground: RgbColor::from_components(components.foreground),
            secondary: RgbColor::from_components(components.secondary),
        }
    }

    pub(crate) fn components(self) -> ArtworkPaletteComponents {
        ArtworkPaletteComponents {
            background: self.background.components(),
            foreground: self.foreground.components(),
            secondary: self.secondary.components(),
        }
    }

    pub(crate) fn foreground_css(self) -> String {
        self.foreground.css_hex()
    }

    /// A second artwork-derived colour, picked so it stands off from the
    /// dominant background and stays at least mildly readable on top of
    /// it. Used for the artist name, track number, and duration so those
    /// labels inherit some of the cover's palette instead of being a
    /// uniform alpha-faded white/black. The track-playing speaker icon
    /// must stay on the strict-contrast `foreground` so it remains
    /// instantly readable regardless of artwork.
    pub(crate) fn secondary_css(self) -> String {
        self.secondary.css_hex()
    }

    #[cfg(test)]
    fn from_background(background: RgbColor) -> Self {
        let foreground = readable_foreground(background);
        let secondary = blend_toward(foreground, background, SECONDARY_FALLBACK_BLEND);
        Self {
            background,
            foreground,
            secondary,
        }
    }
}

/// Minimum RGB euclidean distance a candidate secondary colour must
/// keep from the dominant background so the two read as distinct.
const SECONDARY_MIN_DISTANCE: f64 = 60.0;
/// Minimum WCAG-style contrast ratio a candidate secondary colour must
/// achieve against the dominant background. Lower than the AA body-text
/// threshold (4.5) because this colour is used for muted accents, not
/// primary copy.
const SECONDARY_MIN_CONTRAST: f64 = 2.2;
/// When no artwork-derived secondary survives the distance + contrast
/// filters, blend the strict-contrast foreground this far toward the
/// background to soften it. Keeps the muted text on-palette without
/// pretending the artwork had a chromatic accent it didn't have.
const SECONDARY_FALLBACK_BLEND: f64 = 0.35;

fn scored_buckets_from_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<ColorBucket>> {
    let width = usize::try_from(pixbuf.width()).ok()?;
    let height = usize::try_from(pixbuf.height()).ok()?;
    let channels = usize::try_from(pixbuf.n_channels()).ok()?;
    let rowstride = usize::try_from(pixbuf.rowstride()).ok()?;
    if width == 0 || height == 0 || channels < 3 {
        return None;
    }

    let bytes = pixbuf.read_pixel_bytes();
    let pixels = bytes.as_ref();
    let sample_step = sample_step(width, height);
    let mut colors = Vec::new();

    for y in (0..height).step_by(sample_step) {
        let row_offset = y.checked_mul(rowstride)?;
        for x in (0..width).step_by(sample_step) {
            let offset = row_offset.checked_add(x.checked_mul(channels)?)?;
            if offset + channels > pixels.len() {
                continue;
            }

            let alpha = if channels >= 4 {
                pixels[offset + 3]
            } else {
                255
            };
            colors.push((
                pixels[offset],
                pixels[offset + 1],
                pixels[offset + 2],
                alpha,
            ));
        }
    }

    Some(scored_buckets(colors))
}

fn scored_buckets<I>(colors: I) -> Vec<ColorBucket>
where
    I: IntoIterator<Item = (u8, u8, u8, u8)>,
{
    let mut buckets = BTreeMap::<QuantizedColor, ColorBucket>::new();

    for (red, green, blue, alpha) in colors {
        if alpha < 32 {
            continue;
        }

        let weight = color_weight(red, green, blue, alpha);
        let bucket = buckets
            .entry(QuantizedColor::from_rgb(red, green, blue))
            .or_default();
        bucket.red_total += u64::from(red);
        bucket.green_total += u64::from(green);
        bucket.blue_total += u64::from(blue);
        bucket.score += u64::from(weight);
        bucket.pixels += 1;
    }

    let mut sorted: Vec<ColorBucket> = buckets.into_values().collect();
    sorted.sort_by_key(|bucket| std::cmp::Reverse(bucket.score));
    sorted
}

#[cfg(test)]
fn dominant_color<I>(colors: I) -> Option<RgbColor>
where
    I: IntoIterator<Item = (u8, u8, u8, u8)>,
{
    scored_buckets(colors)
        .into_iter()
        .next()
        .and_then(|bucket| bucket.average())
}

/// Walk the artwork's scored colour buckets in descending score order
/// (skipping the dominant one) and return the first that stands far
/// enough from BOTH the dominant background AND the strict-contrast
/// foreground (so it actually feels like a third colour, not a
/// rebrand of the white/black contrast colour) AND keeps an
/// acceptable contrast against the background. When the artwork has
/// no such colour (monochrome covers, simple white-on-black or
/// black-on-white covers, tightly-clustered palettes), fall back to
/// nudging the strict-contrast foreground toward the background so
/// the muted accent still feels palette-aware instead of pure
/// white/black at reduced alpha.
fn pick_secondary(buckets: &[ColorBucket], background: RgbColor, foreground: RgbColor) -> RgbColor {
    for bucket in buckets.iter().skip(1) {
        let Some(candidate) = bucket.average() else {
            continue;
        };
        if rgb_distance(candidate, background) < SECONDARY_MIN_DISTANCE {
            continue;
        }
        if rgb_distance(candidate, foreground) < SECONDARY_MIN_DISTANCE {
            continue;
        }
        if contrast_ratio(candidate, background) < SECONDARY_MIN_CONTRAST {
            continue;
        }
        return candidate;
    }

    blend_toward(foreground, background, SECONDARY_FALLBACK_BLEND)
}

fn rgb_distance(a: RgbColor, b: RgbColor) -> f64 {
    let dr = f64::from(a.red) - f64::from(b.red);
    let dg = f64::from(a.green) - f64::from(b.green);
    let db = f64::from(a.blue) - f64::from(b.blue);
    (dr * dr + dg * dg + db * db).sqrt()
}

fn blend_toward(from: RgbColor, toward: RgbColor, weight: f64) -> RgbColor {
    RgbColor::new(
        lerp_u8(from.red, toward.red, weight),
        lerp_u8(from.green, toward.green, weight),
        lerp_u8(from.blue, toward.blue, weight),
    )
}

fn lerp_u8(from: u8, toward: u8, weight: f64) -> u8 {
    let blended = f64::from(from) * (1.0 - weight) + f64::from(toward) * weight;
    blended.clamp(0.0, 255.0).round() as u8
}

fn sample_step(width: usize, height: usize) -> usize {
    let pixels = width.saturating_mul(height);
    let target_samples = 40_000;
    if pixels <= target_samples {
        return 1;
    }

    ((pixels as f64 / target_samples as f64).sqrt().ceil() as usize).max(1)
}

fn color_weight(red: u8, green: u8, blue: u8, alpha: u8) -> u16 {
    let max = red.max(green).max(blue);
    let min = red.min(green).min(blue);
    let chroma = u16::from(max - min);
    let opacity = u16::from(alpha);

    (16 + chroma).saturating_mul(opacity) / 255
}

fn readable_foreground(background: RgbColor) -> RgbColor {
    let white = RgbColor::new(255, 255, 255);
    let black = RgbColor::new(0, 0, 0);
    if contrast_ratio(background, white) >= contrast_ratio(background, black) {
        white
    } else {
        black
    }
}

fn contrast_ratio(left: RgbColor, right: RgbColor) -> f64 {
    let left_luminance = relative_luminance(left);
    let right_luminance = relative_luminance(right);
    let lighter = left_luminance.max(right_luminance);
    let darker = left_luminance.min(right_luminance);

    (lighter + 0.05) / (darker + 0.05)
}

fn relative_luminance(color: RgbColor) -> f64 {
    0.2126 * linear_component(color.red)
        + 0.7152 * linear_component(color.green)
        + 0.0722 * linear_component(color.blue)
}

fn linear_component(value: u8) -> f64 {
    let normalized = f64::from(value) / 255.0;
    if normalized <= 0.04045 {
        normalized / 12.92
    } else {
        ((normalized + 0.055) / 1.055).powf(2.4)
    }
}

impl QuantizedColor {
    fn from_rgb(red: u8, green: u8, blue: u8) -> Self {
        Self((u16::from(red >> 4) << 8) | (u16::from(green >> 4) << 4) | u16::from(blue >> 4))
    }
}

impl ColorBucket {
    fn average(self) -> Option<RgbColor> {
        (self.pixels > 0).then(|| {
            RgbColor::new(
                (self.red_total / self.pixels) as u8,
                (self.green_total / self.pixels) as u8,
                (self.blue_total / self.pixels) as u8,
            )
        })
    }
}

impl RgbColor {
    const fn new(red: u8, green: u8, blue: u8) -> Self {
        Self { red, green, blue }
    }

    const fn from_components(components: RgbColorComponents) -> Self {
        Self::new(components.red, components.green, components.blue)
    }

    const fn components(self) -> RgbColorComponents {
        RgbColorComponents {
            red: self.red,
            green: self.green,
            blue: self.blue,
        }
    }

    fn css_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.red, self.green, self.blue)
    }

    fn rgba(self) -> gdk::RGBA {
        let channel = |value: u8| f32::from(value) / 255.0;
        gdk::RGBA::new(
            channel(self.red),
            channel(self.green),
            channel(self.blue),
            1.0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ArtworkPalette, ColorBucket, RgbColor, dominant_color, pick_secondary, readable_foreground,
        sample_step,
    };

    #[test]
    fn dominant_color_prefers_the_largest_bucket() {
        let colors = [(20, 40, 200, 255), (21, 41, 201, 255), (220, 20, 20, 255)];

        assert_eq!(dominant_color(colors), Some(RgbColor::new(20, 40, 200)));
    }

    #[test]
    fn transparent_pixels_do_not_drive_the_palette() {
        let colors = [(255, 0, 0, 0), (0, 80, 0, 255), (0, 82, 0, 255)];

        assert_eq!(dominant_color(colors), Some(RgbColor::new(0, 81, 0)));
    }

    #[test]
    fn foreground_uses_the_higher_contrast_neutral() {
        assert_eq!(
            readable_foreground(RgbColor::new(10, 10, 10)),
            RgbColor::new(255, 255, 255)
        );
        assert_eq!(
            readable_foreground(RgbColor::new(240, 240, 240)),
            RgbColor::new(0, 0, 0)
        );
    }

    #[test]
    fn palette_formats_css_colors() {
        let palette = ArtworkPalette::from_background(RgbColor::new(12, 34, 56));

        assert_eq!(palette.background_css(), "#0c2238");
        assert_eq!(palette.foreground_css(), "#ffffff");
        // Fallback secondary on a chromatic-less construction = white
        // blended 35% toward the dark blue background.
        assert_eq!(palette.secondary_css(), "#aab2b9");
    }

    #[test]
    fn sample_step_caps_large_images() {
        assert_eq!(sample_step(100, 100), 1);
        assert!(sample_step(4000, 4000) > 1);
    }

    #[test]
    fn pick_secondary_returns_the_first_contrasting_artwork_color() {
        let buckets = vec![
            bucket(20, 40, 200, 100),
            bucket(25, 45, 195, 80),
            bucket(220, 80, 30, 50),
        ];
        let background = RgbColor::new(20, 40, 200);
        let foreground = readable_foreground(background);

        let secondary = pick_secondary(&buckets, background, foreground);

        assert_eq!(secondary, RgbColor::new(220, 80, 30));
    }

    #[test]
    fn pick_secondary_skips_buckets_near_the_strict_contrast_foreground() {
        // Black-dominant cover with a high-scoring near-white bucket
        // (logo, text overlay) and a smaller chromatic bucket. Without
        // the foreground-distance filter, the near-white bucket would
        // win and the secondary would equal the foreground — i.e. the
        // muted text would just be the title colour again.
        let buckets = vec![
            bucket(10, 10, 10, 200),
            bucket(248, 248, 248, 120),
            bucket(180, 60, 60, 40),
        ];
        let background = RgbColor::new(10, 10, 10);
        let foreground = readable_foreground(background);

        let secondary = pick_secondary(&buckets, background, foreground);

        assert_eq!(secondary, RgbColor::new(180, 60, 60));
    }

    #[test]
    fn pick_secondary_falls_back_when_artwork_is_monochromatic() {
        let buckets = vec![bucket(20, 40, 200, 100), bucket(22, 42, 198, 80)];
        let background = RgbColor::new(20, 40, 200);
        let foreground = readable_foreground(background);

        let secondary = pick_secondary(&buckets, background, foreground);

        // White (foreground) blended 35% toward the dark blue background.
        assert_eq!(secondary, RgbColor::new(173, 180, 236));
    }

    fn bucket(red: u8, green: u8, blue: u8, pixels: u64) -> ColorBucket {
        ColorBucket {
            red_total: u64::from(red) * pixels,
            green_total: u64::from(green) * pixels,
            blue_total: u64::from(blue) * pixels,
            score: pixels * 10,
            pixels,
        }
    }
}
