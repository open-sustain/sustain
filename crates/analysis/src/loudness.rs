// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Loudness as ITU-R BS.1770-4 and EBU R128 measure it (#344).
//!
//! The decoder feeds every decoded frame, channel by channel, to a
//! [`LoudnessMeter`]: each channel goes through the two-stage K-weighting
//! filter (a high-frequency shelf, then a low-frequency high-pass), and
//! its power is weighted by where the channel sits — 1.0 at the front,
//! 1.41 at the sides and rear, none for the LFE — and summed across
//! channels. Channels are never mixed down first, so content in opposite
//! polarity across channels does not cancel out.
//!
//! The meter keeps only that weighted power, summed over consecutive
//! 100 ms [`Segment`]s: every measure is a run of whole segments.
//!
//! * **Integrated loudness** gates 400 ms blocks overlapping by 75 %
//!   (four segments, one segment apart): blocks below −70 LUFS are
//!   dropped, then blocks more than 10 LU below the loudness of those
//!   left; the loudness of the remaining blocks' mean power is the
//!   track's.
//! * **Short-term loudness** is ungated, over 3 s windows (30 segments)
//!   one segment apart. The last window ends at the end of the stream.
//! * **Loudness range** (EBU Tech 3342) drops short-term values below
//!   −70 LUFS, then those more than 20 LU below the loudness of the mean
//!   power of those left, and spans the 10th to the 95th percentile of
//!   the rest.

use symphonia::core::audio::{ChannelLabel, Channels, Position};

/// Segments per second: every block and window is a run of them.
const SEGMENTS_PER_SECOND: usize = 10;
/// A gating block, in segments: 400 ms.
const BLOCK_SEGMENTS: usize = 4;
/// A short-term window, in segments: 3 s.
const SHORT_TERM_SEGMENTS: usize = 30;

/// The absolute gate, in LUFS, of both integrated loudness and loudness
/// range.
const ABSOLUTE_GATE_LUFS: f64 = -70.0;
/// The relative gate of integrated loudness, in LU below the loudness of
/// the blocks above the absolute gate.
const INTEGRATED_RELATIVE_GATE_LU: f64 = 10.0;
/// The relative gate of loudness range, in LU below the loudness of the
/// short-term values above the absolute gate.
const RANGE_RELATIVE_GATE_LU: f64 = 20.0;
/// The percentiles loudness range spans.
const RANGE_LOW_PERCENTILE: f64 = 0.10;
const RANGE_HIGH_PERCENTILE: f64 = 0.95;

/// The weight of a channel at the sides or rear (+1.5 dB).
const SURROUND_WEIGHT: f64 = 1.41;

/// The weighted K-filtered power of one 100 ms span of the stream.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Segment {
    /// Σ over the span's frames of Σ over channels of weight · y².
    energy: f64,
    /// Frames the span holds; fewer than its length only for the last.
    frames: usize,
}

/// One biquad section, transposed direct form II, in `f64`: the
/// high-pass sits at 38 Hz, where single precision loses the response at
/// high sample rates.
#[derive(Clone, Copy, Debug)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    state: [f64; 2],
}

impl Biquad {
    fn new(b: [f64; 3], a: [f64; 2]) -> Self {
        Self {
            b,
            a,
            state: [0.0; 2],
        }
    }

    #[inline]
    fn run(&mut self, input: f64) -> f64 {
        let output = self.b[0] * input + self.state[0];
        self.state[0] = self.b[1] * input - self.a[0] * output + self.state[1];
        self.state[1] = self.b[2] * input - self.a[1] * output;
        output
    }
}

/// The K-weighting filter of BS.1770 for one channel, its two stages
/// derived for the sample rate from the analogue prototypes the
/// recommendation's 48 kHz coefficients come from. A stage whose corner
/// frequency is not below Nyquist cannot be mapped to the rate (its
/// filter would be unstable) and is left out: the shelf below a 3.4 kHz
/// rate, the high-pass below 77 Hz.
#[derive(Clone, Copy, Debug)]
struct KWeighting {
    shelf: Option<Biquad>,
    high_pass: Option<Biquad>,
}

impl KWeighting {
    fn new(sample_rate: u32) -> Self {
        let rate = f64::from(sample_rate);

        // Stage 1: the high-frequency shelf modelling the head.
        let (f0, gain_db, q) = (
            1_681.974_450_955_533,
            3.999_843_853_973_347,
            0.707_175_236_955_419_6,
        );
        let shelf = (f0 < rate / 2.0).then(|| {
            let k = (std::f64::consts::PI * f0 / rate).tan();
            let vh = 10_f64.powf(gain_db / 20.0);
            let vb = vh.powf(0.499_666_774_154_541_6);
            let a0 = 1.0 + k / q + k * k;
            Biquad::new(
                [
                    (vh + vb * k / q + k * k) / a0,
                    2.0 * (k * k - vh) / a0,
                    (vh - vb * k / q + k * k) / a0,
                ],
                [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
            )
        });

        // Stage 2: the low-frequency high-pass (the RLB curve).
        let (f0, q) = (38.135_470_876_024_44, 0.500_327_037_323_877_3);
        let high_pass = (f0 < rate / 2.0).then(|| {
            let k = (std::f64::consts::PI * f0 / rate).tan();
            let a0 = 1.0 + k / q + k * k;
            Biquad::new(
                [1.0, -2.0, 1.0],
                [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
            )
        });

        Self { shelf, high_pass }
    }

    #[inline]
    fn run(&mut self, input: f64) -> f64 {
        let shelved = self.shelf.as_mut().map_or(input, |shelf| shelf.run(input));
        self.high_pass
            .as_mut()
            .map_or(shelved, |high_pass| high_pass.run(shelved))
    }
}

/// The weight BS.1770-4 gives a channel at `position` in a layout that
/// does or does not also hold side channels. It weighs a channel by its
/// azimuth: 1.41 between 60° and 120° off the front, 1.0 nearer the front
/// and further behind, none for the LFE. A 5.1 layout's surround pair, at
/// ±110°, is carried as side channels or as rear ones depending on the
/// format (WAV names them rear); behind side channels, as in 7.1, the
/// rear pair sits at ±135° to ±150°.
fn position_weight(position: Position, has_sides: bool) -> f64 {
    let sides = Position::SIDE_LEFT | Position::SIDE_RIGHT;
    let rears = Position::REAR_LEFT | Position::REAR_RIGHT;
    if position.intersects(Position::LFE1 | Position::LFE2) {
        0.0
    } else if position.intersects(sides) || (position.intersects(rears) && !has_sides) {
        SURROUND_WEIGHT
    } else {
        1.0
    }
}

/// The weight of each of `count` channels laid out as `channels`, in the
/// order their samples are interleaved. A channel whose position is not
/// known counts as a front one.
fn channel_weights(channels: &Channels, count: usize) -> Vec<f64> {
    let has_sides =
        |positions: Position| positions.intersects(Position::SIDE_LEFT | Position::SIDE_RIGHT);
    let weights: Vec<f64> = match channels {
        Channels::Positioned(positions) => (0..u64::BITS)
            .map(|bit| Position::from_bits_retain(1 << bit))
            .filter(|position| positions.contains(*position))
            .map(|position| position_weight(position, has_sides(*positions)))
            .collect(),
        Channels::Custom(labels) => {
            let positions = labels
                .iter()
                .filter_map(|label| match label {
                    ChannelLabel::Positioned(position) => Some(*position),
                    _ => None,
                })
                .fold(Position::empty(), |all, position| all | position);
            labels
                .iter()
                .map(|label| match label {
                    ChannelLabel::Positioned(position) => {
                        position_weight(*position, has_sides(positions))
                    }
                    _ => 1.0,
                })
                .collect()
        }
        _ => Vec::new(),
    };
    if weights.len() == count {
        weights
    } else {
        vec![1.0; count]
    }
}

/// Streams decoded frames into 100 ms [`Segment`]s of weighted
/// K-filtered power.
pub(crate) struct LoudnessMeter {
    sample_rate: u32,
    channels: Channels,
    weights: Vec<f64>,
    filters: Vec<KWeighting>,
    segments: Vec<Segment>,
    current: Segment,
    /// Frames the current segment holds when whole.
    current_len: usize,
}

impl LoudnessMeter {
    pub(crate) fn new(sample_rate: u32) -> Self {
        let mut meter = Self {
            sample_rate,
            channels: Channels::None,
            weights: Vec::new(),
            filters: Vec::new(),
            segments: Vec::new(),
            current: Segment::default(),
            current_len: 0,
        };
        meter.current_len = meter.segment_len(0);
        meter
    }

    /// Frames segment `index` holds when whole. Boundaries fall on whole
    /// frames nearest below each tenth of a second, so the timeline stays
    /// exact at rates a tenth of which is not a whole number of frames.
    fn segment_len(&self, index: usize) -> usize {
        let rate = self.sample_rate as usize;
        ((index + 1) * rate / SEGMENTS_PER_SECOND - index * rate / SEGMENTS_PER_SECOND).max(1)
    }

    /// Lay the frames that follow out as `channels`, `count` of them. A
    /// change of layout starts the filters afresh.
    pub(crate) fn set_layout(&mut self, channels: &Channels, count: usize) {
        if self.filters.len() != count || self.channels != *channels {
            self.channels = channels.clone();
            self.weights = channel_weights(channels, count);
            self.filters = vec![KWeighting::new(self.sample_rate); count];
        }
    }

    /// Measure the next `frame`, its samples interleaved in the layout
    /// last set.
    #[inline]
    pub(crate) fn push_frame(&mut self, frame: &[f32]) {
        let mut energy = 0.0;
        for ((sample, filter), weight) in frame.iter().zip(&mut self.filters).zip(&self.weights) {
            let filtered = filter.run(f64::from(*sample));
            energy += weight * filtered * filtered;
        }
        self.current.energy += energy;
        self.current.frames += 1;
        if self.current.frames == self.current_len {
            self.segments.push(self.current);
            self.current = Segment::default();
            self.current_len = self.segment_len(self.segments.len());
        }
    }

    /// The measured stream, its last partial segment included.
    pub(crate) fn finish(mut self) -> Loudness {
        if self.current.frames > 0 {
            self.segments.push(self.current);
        }
        Loudness {
            segments: self.segments,
            last_is_whole: self.current.frames == 0,
        }
    }
}

/// The loudness of a measured stream (see the module documentation).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Loudness {
    segments: Vec<Segment>,
    /// Whether the last segment holds a whole 100 ms.
    last_is_whole: bool,
}

/// The loudness, in LUFS, of a mean weighted power.
fn lufs(power: f64) -> f64 {
    -0.691 + 10.0 * power.log10()
}

/// The mean power of a run of segments.
fn mean_power(run: &[Segment]) -> f64 {
    let (energy, frames) = run.iter().fold((0.0, 0), |(energy, frames), segment| {
        (energy + segment.energy, frames + segment.frames)
    });
    if frames == 0 {
        0.0
    } else {
        energy / frames as f64
    }
}

/// The mean of `powers`, in LUFS; −∞ for none.
fn lufs_of_mean(powers: impl Iterator<Item = f64>) -> f64 {
    let (sum, count) = powers.fold((0.0, 0_usize), |(sum, count), power| {
        (sum + power, count + 1)
    });
    if count == 0 {
        f64::NEG_INFINITY
    } else {
        lufs(sum / count as f64)
    }
}

impl Loudness {
    /// Gated integrated loudness, in LUFS; `None` when no 400 ms block
    /// rises above the absolute gate.
    pub(crate) fn integrated(&self) -> Option<f32> {
        // Only whole segments make a 400 ms block.
        let whole = if self.last_is_whole {
            &self.segments[..]
        } else {
            &self.segments[..self.segments.len().saturating_sub(1)]
        };
        let blocks: Vec<f64> = whole
            .windows(BLOCK_SEGMENTS)
            .map(mean_power)
            .filter(|power| lufs(*power) > ABSOLUTE_GATE_LUFS)
            .collect();
        let relative_gate = lufs_of_mean(blocks.iter().copied()) - INTEGRATED_RELATIVE_GATE_LU;
        let integrated = lufs_of_mean(
            blocks
                .iter()
                .copied()
                .filter(|power| lufs(*power) > relative_gate),
        );
        integrated.is_finite().then_some(integrated as f32)
    }

    /// Ungated short-term loudness, in LUFS, of every 3 s window one
    /// segment apart, the last ending at the end of the stream; a stream
    /// shorter than a window is one window. Silence reads −∞.
    pub(crate) fn short_term(&self) -> Vec<f64> {
        if self.segments.is_empty() {
            return Vec::new();
        }
        let window = SHORT_TERM_SEGMENTS.min(self.segments.len());
        self.segments
            .windows(window)
            .map(|run| lufs(mean_power(run)))
            .collect()
    }
}

/// The loudness range, in LU, of `short_term` values (EBU Tech 3342).
pub(crate) fn loudness_range(short_term: &[f64]) -> f32 {
    let above_absolute: Vec<f64> = short_term
        .iter()
        .copied()
        .filter(|value| *value >= ABSOLUTE_GATE_LUFS)
        .collect();
    let relative_gate = lufs_of_mean(
        above_absolute
            .iter()
            .map(|value| 10_f64.powf((value + 0.691) / 10.0)),
    ) - RANGE_RELATIVE_GATE_LU;
    let mut gated: Vec<f64> = above_absolute
        .into_iter()
        .filter(|value| *value >= relative_gate)
        .collect();
    if gated.len() < 2 {
        return 0.0;
    }
    gated.sort_by(f64::total_cmp);
    (percentile(&gated, RANGE_HIGH_PERCENTILE) - percentile(&gated, RANGE_LOW_PERCENTILE)).max(0.0)
        as f32
}

/// Nearest-rank percentile of a sorted, non-empty slice. `q` in `[0, 1]`.
fn percentile(sorted: &[f64], q: f64) -> f64 {
    let index = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::{Loudness, LoudnessMeter, loudness_range};
    use symphonia::core::audio::{Channels, Position, layouts};

    /// A 1 kHz sine whose level, in dBFS (peak), is `level(t)` at `t`
    /// seconds, `secs` long, the same in each of `channels`.
    fn measure(
        sample_rate: u32,
        channels: &Channels,
        secs: f64,
        level: impl Fn(f64) -> f64,
        frequency: f64,
    ) -> Loudness {
        let count = channels.count();
        let mut meter = LoudnessMeter::new(sample_rate);
        meter.set_layout(channels, count);
        let mut frame = vec![0.0_f32; count];
        for index in 0..(secs * f64::from(sample_rate)) as usize {
            let t = index as f64 / f64::from(sample_rate);
            let value =
                10_f64.powf(level(t) / 20.0) * (std::f64::consts::TAU * frequency * t).sin();
            frame.fill(value as f32);
            meter.push_frame(&frame);
        }
        meter.finish()
    }

    fn stereo() -> Channels {
        layouts::CHANNEL_LAYOUT_STEREO
    }

    fn assert_near(measured: f32, expected: f32, tolerance: f32, what: &str) {
        assert!(
            (measured - expected).abs() <= tolerance,
            "{what}: measured {measured}, expected {expected} ± {tolerance}"
        );
    }

    #[test]
    fn a_stereo_tone_at_minus_23_dbfs_reads_minus_23_lufs_at_every_rate() {
        // EBU Tech 3341, case 1: 20 s of a 1 kHz stereo tone in phase.
        for rate in [32_000, 44_100, 48_000, 96_000, 192_000] {
            let loudness = measure(rate, &stereo(), 20.0, |_| -23.0, 1_000.0);
            assert_near(
                loudness.integrated().expect("loud enough"),
                -23.0,
                0.05,
                &format!("{rate} Hz"),
            );
        }
    }

    #[test]
    fn tones_read_as_ffmpeg_reads_them() {
        // FFmpeg's `ebur128` on a mono sine of amplitude 0.25 (−12.04 dBFS).
        let mono = Channels::Positioned(Position::FRONT_CENTER);
        for (frequency, expected) in [(1_000.0, -15.0), (100.0, -16.9)] {
            for rate in [44_100, 48_000, 96_000] {
                let loudness = measure(rate, &mono, 10.0, |_| -12.041_2, frequency);
                assert_near(
                    loudness.integrated().expect("loud enough"),
                    expected,
                    0.1,
                    &format!("{frequency} Hz at {rate} Hz"),
                );
            }
        }
    }

    #[test]
    fn quiet_passages_are_gated_out_of_integrated_loudness() {
        // EBU Tech 3341, cases 3 and 4: 60 s at −23 dBFS between 10 s
        // quieter passages the relative gate drops, and silent ones the
        // absolute gate does.
        let case_3 = measure(
            48_000,
            &stereo(),
            80.0,
            |t| {
                if (10.0..70.0).contains(&t) {
                    -23.0
                } else {
                    -36.0
                }
            },
            1_000.0,
        );
        assert_near(
            case_3.integrated().expect("loud enough"),
            -23.0,
            0.1,
            "case 3",
        );
        let case_4 = measure(
            48_000,
            &stereo(),
            100.0,
            |t| match t {
                t if t < 10.0 => -72.0,
                t if t < 20.0 => -36.0,
                t if t < 80.0 => -23.0,
                t if t < 90.0 => -36.0,
                _ => -72.0,
            },
            1_000.0,
        );
        assert_near(
            case_4.integrated().expect("loud enough"),
            -23.0,
            0.1,
            "case 4",
        );
    }

    #[test]
    fn loudness_range_spans_the_loud_and_the_quiet_passages() {
        // EBU Tech 3342, cases 1 to 3: 20 s at one level, then 20 s at
        // another.
        for (first, second, expected) in [
            (-20.0, -30.0, 10.0),
            (-20.0, -15.0, 5.0),
            (-40.0, -20.0, 20.0),
        ] {
            let loudness = measure(
                48_000,
                &stereo(),
                40.0,
                |t| if t < 20.0 { first } else { second },
                1_000.0,
            );
            assert_near(
                loudness_range(&loudness.short_term()),
                expected,
                1.0,
                &format!("{first} then {second} dBFS"),
            );
        }
    }

    #[test]
    fn the_range_gate_is_relative_to_the_mean_not_the_loudest_window() {
        // The mean of −20 and −41.5 dBFS halves reads about −23 LUFS, so
        // the gate falls near −43 and keeps the quiet half (FFmpeg: 21.5
        // LU); a gate 20 LU below the loudest window, at −40, would drop it.
        let loudness = measure(
            48_000,
            &stereo(),
            40.0,
            |t| if t < 20.0 { -20.0 } else { -41.5 },
            1_000.0,
        );

        assert_near(loudness_range(&loudness.short_term()), 21.5, 0.5, "range");
    }

    #[test]
    fn gating_blocks_overlap_by_three_quarters() {
        // A 400 ms burst 200 ms in: blocks 100 ms apart catch it whole in
        // one and in part in five others (FFmpeg: −22.0 LUFS); disjoint
        // blocks would split it in halves and read −23.0.
        let loudness = measure(
            48_000,
            &stereo(),
            2.0,
            |t| {
                if (0.2..0.6).contains(&t) {
                    -20.0
                } else {
                    -200.0
                }
            },
            1_000.0,
        );

        assert_near(
            loudness.integrated().expect("loud enough"),
            -22.04,
            0.1,
            "burst",
        );
    }

    #[test]
    fn channels_in_opposite_polarity_do_not_cancel() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.set_layout(&stereo(), 2);
        for index in 0..48_000 * 5 {
            let value = 0.1 * (std::f32::consts::TAU * 1_000.0 * index as f32 / 48_000.0).sin();
            meter.push_frame(&[value, -value]);
        }
        let opposite = meter.finish().integrated().expect("loud enough");
        let alike = measure(48_000, &stereo(), 5.0, |_| -20.0, 1_000.0)
            .integrated()
            .expect("loud enough");

        assert_near(opposite, alike, 0.01, "opposite polarity");
    }

    #[test]
    fn surround_channels_weigh_more_and_the_lfe_not_at_all() {
        let front = measure(
            48_000,
            &Channels::Positioned(Position::FRONT_LEFT),
            5.0,
            |_| -20.0,
            1_000.0,
        )
        .integrated()
        .expect("loud enough");
        let side = measure(
            48_000,
            &Channels::Positioned(Position::SIDE_LEFT),
            5.0,
            |_| -20.0,
            1_000.0,
        )
        .integrated()
        .expect("loud enough");
        let rear = measure(
            48_000,
            &Channels::Positioned(Position::REAR_RIGHT),
            5.0,
            |_| -20.0,
            1_000.0,
        )
        .integrated()
        .expect("loud enough");
        let lfe = measure(
            48_000,
            &Channels::Positioned(Position::LFE1),
            5.0,
            |_| -20.0,
            100.0,
        )
        .integrated();

        assert_near(side - front, 1.49, 0.01, "side");
        assert_near(rear - front, 1.49, 0.01, "rear");
        assert_eq!(lfe, None);
    }

    #[test]
    fn rear_channels_behind_side_ones_weigh_as_the_front() {
        let seven_one = Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::LFE1
            | Position::REAR_LEFT
            | Position::REAR_RIGHT
            | Position::SIDE_LEFT
            | Position::SIDE_RIGHT;
        let five_one = Position::FRONT_LEFT
            | Position::FRONT_RIGHT
            | Position::FRONT_CENTER
            | Position::LFE1
            | Position::REAR_LEFT
            | Position::REAR_RIGHT;

        // Canonical order: FL FR FC LFE RL RR SL SR.
        assert_eq!(
            super::channel_weights(&Channels::Positioned(seven_one), 8),
            [1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.41, 1.41]
        );
        // Without side channels, the rear pair is 5.1's surround pair.
        assert_eq!(
            super::channel_weights(&Channels::Positioned(five_one), 6),
            [1.0, 1.0, 1.0, 0.0, 1.41, 1.41]
        );
    }

    #[test]
    fn a_rate_too_low_for_a_filter_stage_still_measures() {
        // At 2 kHz the shelf's 1.68 kHz corner lies past Nyquist: left
        // out, it reads about 1 LU high, but it reads rather than
        // overflowing.
        let mono = Channels::Positioned(Position::FRONT_CENTER);
        let low = measure(2_000, &mono, 5.0, |_| -12.0, 300.0)
            .integrated()
            .expect("measured");
        let reference = measure(48_000, &mono, 5.0, |_| -12.0, 300.0)
            .integrated()
            .expect("measured");

        assert_near(low, reference, 1.5, "300 Hz at 2 kHz");
    }

    #[test]
    fn the_range_gates_keep_a_value_at_the_gate() {
        // −70 LUFS sits on the absolute gate.
        assert_near(loudness_range(&[-70.0, -60.0]), 10.0, 1e-4, "absolute gate");
        // −40 LUFS sits on the relative gate, 20 LU below the pair's mean
        // power.
        assert_near(
            loudness_range(&[-40.0, -17.011_469_235_902_933]),
            22.988_53,
            1e-3,
            "relative gate",
        );
    }

    #[test]
    fn the_last_short_term_window_ends_at_the_end_of_the_stream() {
        // Silence, then a tone in the stream's last 50 ms only.
        let loudness = measure(
            48_000,
            &stereo(),
            10.05,
            |t| if t < 10.0 { -200.0 } else { -20.0 },
            1_000.0,
        );

        let loudest = loudness
            .short_term()
            .into_iter()
            .fold(f64::NEG_INFINITY, f64::max);
        assert!(loudest > -70.0, "the last window reads the tone: {loudest}");
    }

    #[test]
    fn a_stream_shorter_than_a_window_is_one_window() {
        let loudness = measure(48_000, &stereo(), 1.0, |_| -20.0, 1_000.0);

        let short_term = loudness.short_term();
        assert_eq!(short_term.len(), 1);
        assert_near(short_term[0] as f32, -20.0, 0.1, "the whole stream");
    }

    #[test]
    fn silence_has_no_loudness() {
        let loudness = measure(48_000, &stereo(), 2.0, |_| -200.0, 1_000.0);

        assert_eq!(loudness.integrated(), None);
        assert_eq!(loudness_range(&loudness.short_term()), 0.0);
    }

    #[test]
    fn digital_silence_has_no_loudness() {
        let mut meter = LoudnessMeter::new(48_000);
        meter.set_layout(&stereo(), 2);
        for _ in 0..2 * 48_000 {
            meter.push_frame(&[0.0, 0.0]);
        }
        let loudness = meter.finish();

        assert_eq!(loudness.integrated(), None);
        let short_term = loudness.short_term();
        assert!(!short_term.is_empty());
        assert!(
            short_term.iter().all(|value| *value == f64::NEG_INFINITY),
            "every window reads as silence: {short_term:?}"
        );
        assert_eq!(loudness_range(&short_term), 0.0);
    }
}
