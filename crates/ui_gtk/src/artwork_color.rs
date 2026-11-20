// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::BTreeMap;

use gtk::{gdk, gdk_pixbuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtworkPalette {
    background: RgbColor,
    foreground: RgbColor,
    secondary: RgbColor,
    accent: RgbColor,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ArtworkPaletteComponents {
    pub(crate) background: RgbColorComponents,
    pub(crate) foreground: RgbColorComponents,
    pub(crate) secondary: RgbColorComponents,
    pub(crate) accent: RgbColorComponents,
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
        let accent = pick_accent(&buckets, background);
        Some(Self {
            background,
            foreground,
            secondary,
            accent,
        })
    }

    pub(crate) fn background_css(self) -> String {
        self.background.css_hex()
    }

    pub(crate) fn background_rgba(self) -> gdk::RGBA {
        self.background.rgba()
    }

    /// The cover's most colourful colour (see [`pick_accent`]), lightened
    /// or darkened just enough to show on the window's chrome whatever the
    /// theme: the top bar is
    /// near-white in a light theme and near-black in a dark one, and a
    /// colour whose luminance sits between [`CHROME_MIN_LUMINANCE`] and
    /// [`CHROME_MAX_LUMINANCE`] keeps at least 2:1 against both. The hue
    /// is kept.
    pub(crate) fn chrome_rgba(self) -> gdk::RGBA {
        within_luminance(self.accent, CHROME_MIN_LUMINANCE, CHROME_MAX_LUMINANCE).rgba()
    }

    pub(crate) fn from_components(components: ArtworkPaletteComponents) -> Self {
        Self {
            background: RgbColor::from_components(components.background),
            foreground: RgbColor::from_components(components.foreground),
            secondary: RgbColor::from_components(components.secondary),
            accent: RgbColor::from_components(components.accent),
        }
    }

    pub(crate) fn components(self) -> ArtworkPaletteComponents {
        ArtworkPaletteComponents {
            background: self.background.components(),
            foreground: self.foreground.components(),
            secondary: self.secondary.components(),
            accent: self.accent.components(),
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
            accent: background,
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

/// Smallest share of the cover's sampled pixels a hue family must cover
/// to give the accent: a few stray pixels do not colour the chrome.
const ACCENT_MIN_SHARE: f64 = 0.01;
/// Width of the hue bins colourful buckets are gathered in, in degrees.
const ACCENT_HUE_BIN_DEGREES: f64 = 10.0;
const ACCENT_HUE_BINS: usize = 36;
/// Chroma (max minus min RGB channel) under which a colour reads as grey:
/// a cover with nothing more colourful keeps its dominant colour.
const ACCENT_MIN_CHROMA: u8 = 48;

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

/// The cover's strongest colour: colourful buckets (chroma of at least
/// [`ACCENT_MIN_CHROMA`]) are gathered by hue, since one shaded or
/// noisy motif spreads over many buckets that are each too small to
/// notice. A hue family is a hue bin with its two neighbours, so a red
/// either side of 0° stays one family. The family weighing most, each
/// pixel counted by its chroma, wins if it covers at least
/// [`ACCENT_MIN_SHARE`] of the cover, and its weightiest bucket is the
/// accent: a colour the cover holds, not a blend. A white cover with a
/// red motif shows the red. A cover with no colourful family keeps
/// `dominant`.
fn pick_accent(buckets: &[ColorBucket], dominant: RgbColor) -> RgbColor {
    let sampled: u64 = buckets.iter().map(|bucket| bucket.pixels).sum();
    let colourful: Vec<(usize, RgbColor, u64)> = buckets
        .iter()
        .filter_map(|bucket| Some((bucket.average()?, bucket.pixels)))
        .filter(|(color, _)| color.chroma() >= ACCENT_MIN_CHROMA)
        .map(|(color, pixels)| (color.hue_bin(), color, pixels))
        .collect();
    let mut bin_pixels = [0_u64; ACCENT_HUE_BINS];
    let mut bin_weight = [0_u64; ACCENT_HUE_BINS];
    for &(bin, color, pixels) in &colourful {
        bin_pixels[bin] += pixels;
        bin_weight[bin] += pixels * u64::from(color.chroma());
    }
    let family = |bin: usize| {
        [
            (bin + ACCENT_HUE_BINS - 1) % ACCENT_HUE_BINS,
            bin,
            (bin + 1) % ACCENT_HUE_BINS,
        ]
    };
    let family_sum = |totals: &[u64; ACCENT_HUE_BINS], bin: usize| -> u64 {
        family(bin).iter().map(|&member| totals[member]).sum()
    };
    let Some(centre) = (0..ACCENT_HUE_BINS)
        .filter(|&bin| family_sum(&bin_pixels, bin) as f64 >= sampled as f64 * ACCENT_MIN_SHARE)
        // On equal weight the lowest hue wins, so the pick is stable.
        .rev()
        .max_by_key(|&bin| family_sum(&bin_weight, bin))
    else {
        return dominant;
    };
    let members = family(centre);
    colourful
        .iter()
        .filter(|(bin, _, _)| members.contains(bin))
        // Buckets come by descending score: on equal weight the first wins.
        .rev()
        .max_by_key(|(_, color, pixels)| pixels * u64::from(color.chroma()))
        .map_or(dominant, |&(_, color, _)| color)
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

/// The darkest a cover colour drawn on the chrome may be: 2:1 against a
/// dark top bar (`#242424`).
const CHROME_MIN_LUMINANCE: f64 = 0.12;
/// The lightest a cover colour drawn on the chrome may be: 2:1 against a
/// light top bar (`#ebebeb`).
const CHROME_MAX_LUMINANCE: f64 = 0.38;

/// `color` mixed with white when its luminance is under `min`, or with
/// black when it is over `max`, by the least amount reaching the bound.
fn within_luminance(color: RgbColor, min: f64, max: f64) -> RgbColor {
    let luminance = relative_luminance(color);
    let too_dark = luminance < min;
    if !too_dark && luminance <= max {
        return color;
    }
    let toward = if too_dark {
        RgbColor::new(255, 255, 255)
    } else {
        RgbColor::new(0, 0, 0)
    };
    let short_of_bound = |mixed: RgbColor| {
        let mixed_luminance = relative_luminance(mixed);
        if too_dark {
            mixed_luminance < min
        } else {
            mixed_luminance > max
        }
    };
    let (mut short, mut enough) = (0.0, 1.0);
    for _ in 0..16 {
        let amount = (short + enough) / 2.0;
        if short_of_bound(color.mix(toward, amount)) {
            short = amount;
        } else {
            enough = amount;
        }
    }
    color.mix(toward, enough)
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

    /// How far the colour is from grey: its largest channel minus its
    /// smallest.
    fn chroma(self) -> u8 {
        self.red.max(self.green).max(self.blue) - self.red.min(self.green).min(self.blue)
    }

    /// The [`ACCENT_HUE_BIN_DEGREES`]-wide bin the colour's hue falls in.
    /// A grey's hue is undefined; it falls in bin 0.
    fn hue_bin(self) -> usize {
        let (red, green, blue) = (
            f64::from(self.red),
            f64::from(self.green),
            f64::from(self.blue),
        );
        let chroma = f64::from(self.chroma());
        if chroma == 0.0 {
            return 0;
        }
        let max = red.max(green).max(blue);
        let sector = if max == red {
            (green - blue) / chroma
        } else if max == green {
            (blue - red) / chroma + 2.0
        } else {
            (red - green) / chroma + 4.0
        };
        let degrees = (sector * 60.0).rem_euclid(360.0);
        (degrees / ACCENT_HUE_BIN_DEGREES) as usize % ACCENT_HUE_BINS
    }

    /// `self` moved `amount` (0 to 1) of the way to `other`.
    fn mix(self, other: Self, amount: f64) -> Self {
        let channel = |from: u8, to: u8| {
            (f64::from(from) + (f64::from(to) - f64::from(from)) * amount).round() as u8
        };
        Self::new(
            channel(self.red, other.red),
            channel(self.green, other.green),
            channel(self.blue, other.blue),
        )
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
        ArtworkPalette, CHROME_MAX_LUMINANCE, CHROME_MIN_LUMINANCE, ColorBucket, RgbColor,
        contrast_ratio, dominant_color, pick_accent, pick_secondary, readable_foreground,
        relative_luminance, sample_step, within_luminance,
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

    #[test]
    fn the_accent_is_the_most_colourful_noticeable_colour() {
        let grey = RgbColor::new(90, 90, 92);
        let buckets = vec![
            bucket(90, 90, 92, 800),
            bucket(150, 120, 110, 100),
            bucket(200, 40, 40, 60),
            // Too few pixels to count, however vivid.
            bucket(0, 255, 0, 5),
        ];

        assert_eq!(pick_accent(&buckets, grey), RgbColor::new(200, 40, 40));
    }

    #[test]
    fn a_motif_spread_over_many_small_buckets_is_the_accent_over_a_white_cover() {
        let white = RgbColor::new(245, 245, 245);
        // A shaded red motif: no bucket reaches 1% of the cover, its hue
        // family does, either side of 0°.
        let buckets = vec![
            bucket(245, 245, 245, 900),
            bucket(200, 30, 45, 4),
            bucket(220, 50, 40, 4),
            bucket(180, 20, 25, 4),
            bucket(160, 10, 20, 4),
        ];

        assert_eq!(pick_accent(&buckets, white), RgbColor::new(220, 50, 40));
    }

    #[test]
    fn the_heavier_hue_family_gives_the_accent() {
        let white = RgbColor::new(245, 245, 245);
        // One vivid blue bucket, outweighed by a broad orange family.
        let buckets = vec![
            bucket(245, 245, 245, 800),
            bucket(30, 60, 220, 30),
            bucket(230, 120, 30, 25),
            bucket(210, 110, 20, 25),
            bucket(240, 140, 50, 25),
        ];

        assert_eq!(pick_accent(&buckets, white), RgbColor::new(230, 120, 30));
    }

    #[test]
    fn hues_fall_in_their_ten_degree_bins() {
        assert_eq!(RgbColor::new(255, 0, 0).hue_bin(), 0);
        assert_eq!(RgbColor::new(255, 0, 20).hue_bin(), 35);
        assert_eq!(RgbColor::new(0, 255, 0).hue_bin(), 12);
        assert_eq!(RgbColor::new(0, 0, 255).hue_bin(), 24);
        assert_eq!(RgbColor::new(128, 128, 128).hue_bin(), 0);
    }

    #[test]
    fn a_grey_cover_keeps_its_dominant_colour_as_accent() {
        let grey = RgbColor::new(90, 90, 92);
        let buckets = vec![bucket(90, 90, 92, 800), bucket(140, 130, 120, 200)];

        assert_eq!(pick_accent(&buckets, grey), grey);
    }

    #[test]
    fn a_cover_colour_already_visible_on_the_chrome_is_kept() {
        let red = RgbColor::new(200, 40, 40);
        assert_eq!(chrome(red), red);
    }

    #[test]
    fn chrome_colours_show_on_light_and_dark_top_bars() {
        let dark_bar = RgbColor::new(0x24, 0x24, 0x24);
        let light_bar = RgbColor::new(0xeb, 0xeb, 0xeb);
        for cover in [
            RgbColor::new(0, 0, 0),
            RgbColor::new(0x30, 0x30, 0x30),
            RgbColor::new(10, 10, 120),
            RgbColor::new(255, 255, 255),
            RgbColor::new(250, 240, 120),
            RgbColor::new(120, 230, 230),
        ] {
            let shown = chrome(cover);
            assert!(
                contrast_ratio(shown, dark_bar) >= 2.0 && contrast_ratio(shown, light_bar) >= 2.0,
                "{cover:?} became {shown:?}"
            );
        }
    }

    #[test]
    fn a_dark_cover_colour_is_lightened_only_to_the_bound() {
        let shown = chrome(RgbColor::new(10, 10, 120));
        let luminance = relative_luminance(shown);
        assert!((CHROME_MIN_LUMINANCE..CHROME_MIN_LUMINANCE + 0.01).contains(&luminance));
        assert!(shown.blue > shown.red, "the hue is kept: {shown:?}");
    }

    fn chrome(color: RgbColor) -> RgbColor {
        within_luminance(color, CHROME_MIN_LUMINANCE, CHROME_MAX_LUMINANCE)
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
