// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Whether two tracks hold the same recording, judged from their detail
//! waveforms (#302, #303).
//!
//! Matching tags and similar loudness say two tracks are *about* the same
//! song; only the time-dependent shape of the audio says they are the same
//! recording. Two live performances, two pianists, a radio edit and its
//! extended mix all share a title, and all differ here.
//!
//! The comparison works on the cached detail waveform (four bytes per
//! 1/150 s: peak amplitude and three band energies, each normalised to the
//! track's own peak, so gain differences vanish). It
//!
//! 1. trims silence from both ends, so encoder delay and padding are
//!    ignored;
//! 2. finds the offset that best lines the two programmes up;
//! 3. measures ten-second windows along that offset, letting each window
//!    move a little, at the waveform's full time resolution;
//! 4. fits one straight timeline through the windows that agree, which
//!    absorbs a small clock difference between two masters without ever
//!    warping different sections into agreement;
//! 5. re-measures every window on that timeline and counts the audible
//!    material either track has that the other lacks.
//!
//! The verdict is deliberately three-valued. [`RecordingIdentity::Same`]
//! needs every window to agree and almost nothing unmatched;
//! [`RecordingIdentity::Different`] needs either no alignment at all (a
//! different performance or mix) or a clean alignment plus a long stretch
//! only one track has (an edit). Everything in between — a changed section,
//! a short extra intro, an alignment that does not settle — is
//! [`RecordingIdentity::NotEstablished`]: missing evidence is never
//! presented as identity, nor as difference.
//!
//! The thresholds were calibrated on a real 10,000-track library: 304 pairs
//! a full-resolution audit had merged, 85 pairs it had found distinct, and
//! re-encoded, padded, resampled, clock-shifted, cut and partly replaced
//! copies of 72 tracks. No distinct pair and no modified copy was judged
//! the same; no genuine copy was judged different.

use crate::{DETAIL_SEGMENTS_PER_SECOND, WaveformSegments};

/// Detail segments averaged into one envelope bin: the comparison runs on a
/// 10 Hz envelope, aligned at the full 150 Hz resolution.
const SEGMENTS_PER_BIN: usize = 15;
/// Peak amplitude (of 255) below which a segment counts as silence, about
/// 30 dB under the track's peak.
const SILENCE_AMPLITUDE: u8 = 8;
/// Length of one compared window, in bins (ten seconds).
const WINDOW_BINS: usize = 100;
/// How far one window may move from the overall offset while it searches
/// for its own best match, in bins (three seconds).
const WINDOW_SEARCH_BINS: usize = 30;
/// Bins averaged per step of the first, coarse overall-offset search
/// (one second).
const OFFSET_SEARCH_DECIMATION: usize = 10;
/// Extra offset range the overall alignment searches beyond the two
/// programmes' length difference, in bins (thirty seconds).
const ALIGNMENT_SLACK_BINS: usize = 300;
/// Shortest programme the comparison judges, in bins (thirty seconds):
/// below that there are too few windows to tell repetition from identity.
const MIN_PROGRAMME_BINS: usize = 300;
/// Largest clock-speed difference still treated as one recording.
const MAX_CLOCK_DEVIATION: f64 = 0.01;
/// A window agrees with the timeline when its correlation reaches this.
const TIMELINE_WINDOW_CORRELATION: f64 = 0.9;
/// A window found *some* match when its freely searched correlation
/// reaches this. Fewer than half such windows means no alignment exists.
const MATCHED_WINDOW_CORRELATION: f64 = 0.8;
const MIN_MATCHED_WINDOW_SHARE: f64 = 0.5;
/// Share of windows that must sit on the timeline before unmatched
/// material is read as an edit rather than a failed alignment.
const MIN_TIMELINE_WINDOW_SHARE: f64 = 0.8;
/// [`RecordingIdentity::Same`] needs every window at least this close…
const SAME_WORST_WINDOW_CORRELATION: f64 = 0.90;
/// …the median window at least this close…
const SAME_MEDIAN_WINDOW_CORRELATION: f64 = 0.98;
/// …and at most this much audible material on either side unmatched.
const SAME_MAX_UNMATCHED_SECONDS: f64 = 3.0;
/// Aligned tracks are different edits when one has this much more.
const EDIT_MIN_UNMATCHED_SECONDS: f64 = 20.0;
/// A channel whose window varies less than this carries no shape.
const FLAT_CHANNEL_DEVIATION: f64 = 1e-6;

/// Whether two tracks hold the same recording.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordingIdentity {
    /// The same recording: aligned end to end, every window agreeing.
    Same,
    /// Clearly different audio: another performance, mix or edit.
    Different,
    /// The evidence does not decide either way, or is missing.
    NotEstablished,
}

/// A track's detail waveform prepared for comparison: the audible
/// programme between its first and last non-silent segment.
#[derive(Clone, Debug)]
pub struct RecordingProfile {
    /// Running per-channel sums over the programme's segments, one entry
    /// more than the segment count, so any span's envelope bin is one
    /// subtraction whatever its offset.
    prefix_sums: Vec<[u32; 4]>,
}

impl RecordingProfile {
    /// Prepare `detail` for comparison. `None` when the waveform is not at
    /// the detail resolution or its audible programme is too short to judge.
    pub fn from_detail(detail: &WaveformSegments) -> Option<Self> {
        let expected_ms = 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32;
        if (detail.segment_duration_ms - expected_ms).abs() > 0.01 {
            return None;
        }
        let segments = &detail.segments;
        let first = segments
            .iter()
            .position(|segment| segment.amplitude >= SILENCE_AMPLITUDE)?;
        let last = segments
            .iter()
            .rposition(|segment| segment.amplitude >= SILENCE_AMPLITUDE)?;
        let programme = &segments[first..=last];
        if programme.len() < MIN_PROGRAMME_BINS * SEGMENTS_PER_BIN {
            return None;
        }
        let mut prefix_sums = Vec::with_capacity(programme.len() + 1);
        let mut running = [0_u32; 4];
        prefix_sums.push(running);
        for segment in programme {
            let values = [
                segment.amplitude,
                segment.low_band,
                segment.mid_band,
                segment.high_band,
            ];
            for (sum, value) in running.iter_mut().zip(values) {
                *sum += u32::from(value);
            }
            prefix_sums.push(running);
        }
        Some(Self { prefix_sums })
    }

    fn segment_count(&self) -> usize {
        self.prefix_sums.len() - 1
    }

    /// The whole programme's envelope, one bin per [`SEGMENTS_PER_BIN`]
    /// segments (a trailing partial bin is dropped).
    fn envelope(&self) -> Envelope {
        let bins = self.segment_count() / SEGMENTS_PER_BIN;
        let mut channels = [
            Vec::with_capacity(bins),
            Vec::with_capacity(bins),
            Vec::with_capacity(bins),
            Vec::with_capacity(bins),
        ];
        for bin in 0..bins {
            let from = self.prefix_sums[bin * SEGMENTS_PER_BIN];
            let to = self.prefix_sums[(bin + 1) * SEGMENTS_PER_BIN];
            for (channel, (to, from)) in channels.iter_mut().zip(to.iter().zip(from)) {
                channel.push(f64::from(to - from) / SEGMENTS_PER_BIN as f64);
            }
        }
        Envelope { channels }
    }
}

/// Four channels of envelope bins.
struct Envelope {
    channels: [Vec<f64>; 4],
}

impl Envelope {
    fn len(&self) -> usize {
        self.channels[0].len()
    }

    /// Each run of `factor` bins averaged into one (a trailing partial run
    /// is dropped).
    fn decimated(&self, factor: usize) -> Envelope {
        Envelope {
            channels: self.channels.each_ref().map(|channel| {
                channel
                    .chunks_exact(factor)
                    .map(|run| run.iter().sum::<f64>() / factor as f64)
                    .collect()
            }),
        }
    }

    /// Each channel shifted to zero mean and unit deviation (flat channels
    /// become all zeros).
    fn standardized(&self) -> [Vec<f64>; 4] {
        self.channels.each_ref().map(|channel| {
            let (mean, deviation) = mean_and_deviation(channel);
            if deviation < FLAT_CHANNEL_DEVIATION {
                vec![0.0; channel.len()]
            } else {
                channel
                    .iter()
                    .map(|value| (value - mean) / deviation)
                    .collect()
            }
        })
    }
}

/// One window of the longer programme, standardized once so it can be
/// correlated against many candidate positions without recomputation.
struct StandardWindow {
    /// Zero-mean, unit-deviation values per channel.
    values: [[f64; WINDOW_BINS]; 4],
    /// Channels that vary; flat channels carry no shape and are skipped.
    varies: [bool; 4],
}

impl StandardWindow {
    /// The window of `envelope` starting at bin `start`, or `None` when it
    /// is silent throughout and so says nothing about identity.
    fn new(envelope: &Envelope, start: usize) -> Option<Self> {
        let audible = envelope.channels[0][start..start + WINDOW_BINS]
            .iter()
            .any(|value| *value >= f64::from(SILENCE_AMPLITUDE));
        if !audible {
            return None;
        }
        let mut values = [[0.0; WINDOW_BINS]; 4];
        let mut varies = [false; 4];
        for (channel, (values, varies)) in envelope
            .channels
            .iter()
            .zip(values.iter_mut().zip(varies.iter_mut()))
        {
            let window = &channel[start..start + WINDOW_BINS];
            let (mean, deviation) = mean_and_deviation(window);
            *varies = deviation >= FLAT_CHANNEL_DEVIATION;
            if *varies {
                for (standard, value) in values.iter_mut().zip(window) {
                    *standard = (value - mean) / deviation;
                }
            }
        }
        Some(Self { values, varies })
    }

    /// Mean Pearson correlation, over the channels that vary on both
    /// sides, with the shorter programme's window starting at segment
    /// `start`; `None` past its end, zero when no channel varies.
    fn correlation(&self, short: &RecordingProfile, start: usize) -> Option<f64> {
        let end = start + WINDOW_BINS * SEGMENTS_PER_BIN;
        if end > short.segment_count() {
            return None;
        }
        let mut bins = [[0.0; WINDOW_BINS]; 4];
        for (channel, row) in bins.iter_mut().enumerate() {
            for (bin, value) in row.iter_mut().enumerate() {
                let from = short.prefix_sums[start + bin * SEGMENTS_PER_BIN];
                let to = short.prefix_sums[start + (bin + 1) * SEGMENTS_PER_BIN];
                *value = f64::from(to[channel] - from[channel]);
            }
        }
        let mut total = 0.0;
        let mut channels = 0;
        for ((standards, varies), row) in self.values.iter().zip(self.varies).zip(&bins) {
            if !varies {
                continue;
            }
            let (mean, deviation) = mean_and_deviation(row);
            if deviation < FLAT_CHANNEL_DEVIATION {
                continue;
            }
            let covariance = standards
                .iter()
                .zip(row)
                .map(|(standard, value)| standard * (value - mean))
                .sum::<f64>();
            total += covariance / (WINDOW_BINS as f64 * deviation);
            channels += 1;
        }
        Some(if channels == 0 {
            0.0
        } else {
            total / f64::from(channels)
        })
    }
}

/// One window of the longer programme and where it matched in the other.
#[derive(Clone, Copy)]
struct WindowMatch {
    /// First bin of the window in the longer programme.
    start_bin: usize,
    /// Offset (longer minus shorter position), in segments.
    offset: f64,
    correlation: f64,
}

impl WindowMatch {
    /// Centre of the window, in segments of the longer programme.
    fn centre(&self) -> f64 {
        ((self.start_bin * SEGMENTS_PER_BIN) as f64) + (WINDOW_BINS * SEGMENTS_PER_BIN) as f64 / 2.0
    }
}

/// Judge whether `left` and `right` hold the same recording.
pub fn compare_recordings(left: &RecordingProfile, right: &RecordingProfile) -> RecordingIdentity {
    // Windows walk the longer programme, so every window of the shorter
    // one's material has a counterpart to be found.
    let (long, short) = if left.segment_count() >= right.segment_count() {
        (left, right)
    } else {
        (right, left)
    };
    let long_envelope = long.envelope();
    let short_envelope = short.envelope();

    let overall_offset =
        overall_offset_bins(&long_envelope, &short_envelope) * SEGMENTS_PER_BIN as isize;
    let windows = (0..long_envelope.len().saturating_sub(WINDOW_BINS - 1))
        .step_by(WINDOW_BINS)
        .filter_map(|start| {
            StandardWindow::new(&long_envelope, start).map(|window| (start, window))
        })
        .collect::<Vec<_>>();
    let matches = windows
        .iter()
        .filter_map(|(start, window)| window_match(*start, window, short, overall_offset))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return RecordingIdentity::NotEstablished;
    }

    let matched_share = share(&matches, |window| {
        window.correlation >= MATCHED_WINDOW_CORRELATION
    });
    if matched_share < MIN_MATCHED_WINDOW_SHARE {
        return RecordingIdentity::Different;
    }

    let timeline = Timeline::fit(&matches, overall_offset as f64);
    let on_timeline_share = share(&matches, |window| {
        window.correlation >= TIMELINE_WINDOW_CORRELATION
            && (window.offset - timeline.offset_at(window.centre())).abs()
                <= SEGMENTS_PER_BIN as f64
    });
    let clock_matches = (timeline.clock_ratio() - 1.0).abs() <= MAX_CLOCK_DEVIATION;
    let unmatched_seconds = timeline.unmatched_seconds(long.segment_count(), short.segment_count());

    if clock_matches
        && on_timeline_share >= MIN_TIMELINE_WINDOW_SHARE
        && unmatched_seconds >= EDIT_MIN_UNMATCHED_SECONDS
    {
        return RecordingIdentity::Different;
    }

    let mut timeline_correlations = windows
        .iter()
        .filter(|(start, _)| matches.iter().any(|window| window.start_bin == *start))
        .filter_map(|(start, window)| {
            let centre =
                (start * SEGMENTS_PER_BIN) as f64 + (WINDOW_BINS * SEGMENTS_PER_BIN) as f64 / 2.0;
            let offset = timeline.offset_at(centre).round() as isize;
            // Two segments either way absorb the rounding of the timeline.
            best_offset(
                window,
                short,
                start * SEGMENTS_PER_BIN,
                offset - 2..=offset + 2,
            )
            .map(|(correlation, _)| correlation)
        })
        .collect::<Vec<_>>();
    if timeline_correlations.is_empty() {
        return RecordingIdentity::NotEstablished;
    }
    timeline_correlations.sort_by(f64::total_cmp);
    let worst = timeline_correlations[0];
    let median = timeline_correlations[timeline_correlations.len() / 2];
    if clock_matches
        && worst >= SAME_WORST_WINDOW_CORRELATION
        && median >= SAME_MEDIAN_WINDOW_CORRELATION
        && unmatched_seconds <= SAME_MAX_UNMATCHED_SECONDS
    {
        RecordingIdentity::Same
    } else {
        RecordingIdentity::NotEstablished
    }
}

/// The offset, in bins, at which the two whole envelopes correlate best,
/// among offsets that overlap at least half of the shorter one. Searched
/// at one-second resolution first, then refined bin by bin, so a long
/// length difference does not make the search quadratic in bins.
fn overall_offset_bins(long: &Envelope, short: &Envelope) -> isize {
    let factor = OFFSET_SEARCH_DECIMATION as isize;
    let span = (long.len() as isize - short.len() as isize).abs() + ALIGNMENT_SLACK_BINS as isize;
    let coarse = best_overall_offset(
        &long.decimated(OFFSET_SEARCH_DECIMATION),
        &short.decimated(OFFSET_SEARCH_DECIMATION),
        -span / factor..=span / factor,
    )
    .unwrap_or(0);
    best_overall_offset(
        long,
        short,
        coarse * factor - factor..=coarse * factor + factor,
    )
    .unwrap_or(coarse * factor)
}

fn best_overall_offset(
    long: &Envelope,
    short: &Envelope,
    offsets: std::ops::RangeInclusive<isize>,
) -> Option<isize> {
    let long_standard = long.standardized();
    let short_standard = short.standardized();
    let long_len = long.len() as isize;
    let short_len = short.len() as isize;
    let min_overlap = (long_len.min(short_len) / 2).max(1);
    let mut best: Option<(f64, isize)> = None;
    for offset in offsets {
        let start = offset.max(0);
        let end = long_len.min(short_len + offset);
        if end - start < min_overlap {
            continue;
        }
        let mut sum = 0.0;
        for (long_channel, short_channel) in long_standard.iter().zip(&short_standard) {
            let long_part = &long_channel[start as usize..end as usize];
            let short_part = &short_channel[(start - offset) as usize..(end - offset) as usize];
            sum += long_part
                .iter()
                .zip(short_part)
                .map(|(left, right)| left * right)
                .sum::<f64>();
        }
        let score = sum / (4 * (end - start)) as f64;
        if best.is_none_or(|(best_score, _)| score > best_score) {
            best = Some((score, offset));
        }
    }
    best.map(|(_, offset)| offset)
}

/// Where the window starting at bin `start` best matches in the shorter
/// programme, within [`WINDOW_SEARCH_BINS`] of the overall offset: first
/// bin by bin, then segment by segment around the best bin.
fn window_match(
    start: usize,
    window: &StandardWindow,
    short: &RecordingProfile,
    overall_offset: isize,
) -> Option<WindowMatch> {
    let bin = SEGMENTS_PER_BIN as isize;
    let search = (WINDOW_SEARCH_BINS * SEGMENTS_PER_BIN) as isize;
    let start_segment = start * SEGMENTS_PER_BIN;
    let (_, coarse_offset) = best_offset(
        window,
        short,
        start_segment,
        (overall_offset - search..=overall_offset + search).step_by(SEGMENTS_PER_BIN),
    )?;
    let (correlation, offset) = best_offset(
        window,
        short,
        start_segment,
        coarse_offset - bin + 1..coarse_offset + bin,
    )?;
    Some(WindowMatch {
        start_bin: start,
        offset: offset as f64,
        correlation,
    })
}

/// The best-correlating offset among `offsets` for `window`, which starts
/// at `start_segment` of the longer programme.
fn best_offset(
    window: &StandardWindow,
    short: &RecordingProfile,
    start_segment: usize,
    offsets: impl Iterator<Item = isize>,
) -> Option<(f64, isize)> {
    let mut best: Option<(f64, isize)> = None;
    for offset in offsets {
        let Ok(short_start) = usize::try_from(start_segment as isize - offset) else {
            continue;
        };
        let Some(correlation) = window.correlation(short, short_start) else {
            continue;
        };
        if best.is_none_or(|(best_correlation, _)| correlation > best_correlation) {
            best = Some((correlation, offset));
        }
    }
    best
}

/// One straight timeline mapping the longer programme onto the shorter:
/// `offset(t) = intercept + slope · t`, in segments.
struct Timeline {
    intercept: f64,
    slope: f64,
}

impl Timeline {
    /// Fit through the windows that matched well (Theil–Sen: the median of
    /// pairwise slopes, robust to the windows a repeated phrase misplaced);
    /// a flat timeline at `fallback_offset` when fewer than two did.
    fn fit(matches: &[WindowMatch], fallback_offset: f64) -> Self {
        let good = matches
            .iter()
            .filter(|window| window.correlation >= TIMELINE_WINDOW_CORRELATION)
            .collect::<Vec<_>>();
        if good.len() < 2 {
            return Self {
                intercept: fallback_offset,
                slope: 0.0,
            };
        }
        let mut slopes = Vec::with_capacity(good.len() * (good.len() - 1) / 2);
        for (index, first) in good.iter().enumerate() {
            for second in &good[index + 1..] {
                slopes.push((second.offset - first.offset) / (second.centre() - first.centre()));
            }
        }
        let slope = median(&mut slopes);
        let mut intercepts = good
            .iter()
            .map(|window| window.offset - slope * window.centre())
            .collect::<Vec<_>>();
        Self {
            intercept: median(&mut intercepts),
            slope,
        }
    }

    fn offset_at(&self, segment: f64) -> f64 {
        self.intercept + self.slope * segment
    }

    /// Shorter-programme time per longer-programme time.
    fn clock_ratio(&self) -> f64 {
        1.0 - self.slope
    }

    /// Audible material, in seconds, that one programme has beyond the
    /// other's ends under this timeline (the larger of the two sides).
    fn unmatched_seconds(&self, long_segments: usize, short_segments: usize) -> f64 {
        let ratio = self.clock_ratio();
        if ratio <= 0.0 {
            return f64::INFINITY;
        }
        let long_segments = long_segments as f64;
        // Where the shorter programme's first and last segments land on the
        // longer one.
        let short_start = self.intercept / ratio;
        let short_end = (short_segments as f64 + self.intercept) / ratio;
        let long_unmatched = short_start.max(0.0) + (long_segments - short_end).max(0.0);
        let short_unmatched =
            ((-short_start).max(0.0) + (short_end - long_segments).max(0.0)) * ratio;
        long_unmatched.max(short_unmatched) / f64::from(DETAIL_SEGMENTS_PER_SECOND)
    }
}

fn mean_and_deviation(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    let count = values.len() as f64;
    let mean = values.iter().sum::<f64>() / count;
    let variance = values
        .iter()
        .map(|value| (value - mean) * (value - mean))
        .sum::<f64>()
        / count;
    (mean, variance.sqrt())
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn share(matches: &[WindowMatch], predicate: impl Fn(&WindowMatch) -> bool) -> f64 {
    matches.iter().filter(|window| predicate(window)).count() as f64 / matches.len() as f64
}

#[cfg(test)]
#[path = "recording_identity_tests.rs"]
mod tests;
