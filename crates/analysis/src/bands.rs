// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Three-band IIR splitter used to derive per-segment low/mid/high
//! energies for the waveform. Built from three independent biquad
//! filters (low-pass, band-pass, high-pass) rather than a Linkwitz-
//! Riley crossover, because the waveform's purpose is *visual* band
//! energy display — overlap between adjacent bands at the crossover
//! points is acceptable, and a crossover would cost us a second
//! cascaded biquad per band without changing the visual result.
//!
//! The splitter is single-sample and stateful: the waveform module
//! drives one decoded sample at a time and the splitter returns the
//! three filtered values in lockstep. Memory cost is constant — six
//! values of filter state plus three filter coefficient sets per track.
//! The filters run in `f64`, so no finite sample, however far beyond
//! full scale, overflows them.

use biquad::{Biquad, Coefficients, DirectForm1, Q_BUTTERWORTH_F64, ToHertz, Type as FilterType};

/// Crossover between low and mid bands, in Hz. Below this the signal
/// is mostly sub-bass / kick-drum body; matches the lower edge of the
/// "mid" band on most spectrum analyzers and the rendering convention
/// used by Serato/Rekordbox-style colored waveforms.
const LOW_MID_CROSSOVER_HZ: f64 = 250.0;
/// Crossover between mid and high bands, in Hz. Above this the signal
/// is dominated by cymbals, sibilance, and air; below this is vocals,
/// snares, and most harmonic content.
const MID_HIGH_CROSSOVER_HZ: f64 = 4_000.0;
/// Center frequency for the band-pass biquad covering the mid band.
/// Geometric mean of the two crossovers (≈ 1 kHz) so the response is
/// symmetric in log-frequency space.
const MID_CENTER_HZ: f64 = 1_000.0;

/// One band of the splitter at a given sample rate.
enum Band {
    /// The band's filter.
    Filtered(DirectForm1<f64>),
    /// A low-pass whose cutoff is at or above Nyquist: every frequency
    /// the stream carries lies below it, so the band is the stream.
    Whole,
    /// A band whose frequency is at or above Nyquist otherwise: the stream
    /// carries none of it, so it reads zero.
    Empty,
}

impl Band {
    /// The Butterworth-Q biquad of `kind` at `frequency` for `sample_rate`.
    fn new(kind: FilterType<f64>, sample_rate: u32, frequency: f64) -> Self {
        let rate = f64::from(sample_rate);
        if frequency >= rate / 2.0 {
            return if matches!(kind, FilterType::LowPass) {
                Self::Whole
            } else {
                Self::Empty
            };
        }
        Coefficients::<f64>::from_params(kind, rate.hz(), frequency.hz(), Q_BUTTERWORTH_F64)
            .map_or(Self::Empty, |coefficients| {
                Self::Filtered(DirectForm1::<f64>::new(coefficients))
            })
    }

    #[inline]
    fn run(&mut self, sample: f64) -> f64 {
        match self {
            Self::Filtered(filter) => filter.run(sample),
            Self::Whole => sample,
            Self::Empty => 0.0,
        }
    }
}

/// The three band filters. Each band stands on its own: one the sample
/// rate cannot carry reads zero, and the others are kept (#344).
pub(crate) struct ThreeBandSplitter {
    low: Band,
    mid: Band,
    high: Band,
}

impl ThreeBandSplitter {
    /// Build a splitter for audio captured at `sample_rate` Hz.
    pub(crate) fn new(sample_rate: u32) -> Self {
        Self {
            low: Band::new(FilterType::LowPass, sample_rate, LOW_MID_CROSSOVER_HZ),
            mid: Band::new(FilterType::BandPass, sample_rate, MID_CENTER_HZ),
            high: Band::new(FilterType::HighPass, sample_rate, MID_HIGH_CROSSOVER_HZ),
        }
    }

    /// Push one mono sample through the splitter; returns
    /// `(low, mid, high)` filtered values for that sample.
    #[inline]
    pub(crate) fn process(&mut self, sample: f32) -> (f64, f64, f64) {
        let sample = f64::from(sample);
        (
            self.low.run(sample),
            self.mid.run(sample),
            self.high.run(sample),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::ThreeBandSplitter;
    use std::f32::consts::TAU;

    const SAMPLE_RATE: u32 = 44_100;

    /// Generate a steady sine at `freq` Hz for `duration_secs` seconds
    /// at amplitude 1.0. Deterministic — no audio files in repo.
    fn sine(freq: f32, duration_secs: f32) -> Vec<f32> {
        let count = (duration_secs * SAMPLE_RATE as f32) as usize;
        (0..count)
            .map(|i| (TAU * freq * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    /// RMS of a slice, ignoring the first half (to give the IIR
    /// filters time to reach steady state).
    fn settled_rms(samples: &[f64]) -> f64 {
        let start = samples.len() / 2;
        let tail = &samples[start..];
        let sum_sq: f64 = tail.iter().map(|x| x.powi(2)).sum();
        (sum_sq / tail.len() as f64).sqrt()
    }

    #[test]
    fn a_rate_below_the_low_crossover_keeps_the_whole_stream_in_the_low_band() {
        // At 500 Hz every frequency the stream carries is under 250 Hz.
        let rate = 500;
        let mut splitter = ThreeBandSplitter::new(rate);
        for i in 0..rate {
            let sample = (TAU * 100.0 * i as f32 / rate as f32).sin();
            assert_eq!(splitter.process(sample), (f64::from(sample), 0.0, 0.0));
        }
    }

    #[test]
    fn a_sample_far_beyond_full_scale_does_not_overflow_the_filters() {
        let mut splitter = ThreeBandSplitter::new(48_000);
        for sample in [0.5, -f32::MAX, f32::MAX, f32::MAX, -f32::MAX, 0.0] {
            let (low, mid, high) = splitter.process(sample);
            assert!(
                low.is_finite() && mid.is_finite() && high.is_finite(),
                "{sample}: ({low}, {mid}, {high})"
            );
        }
    }

    #[test]
    fn a_band_the_rate_cannot_carry_reads_zero_and_the_others_are_kept() {
        // At 8 kHz the 4 kHz high-pass sits at Nyquist.
        let rate = 8_000;
        let mut splitter = ThreeBandSplitter::new(rate);
        let tone = |freq: f32| -> Vec<f32> {
            (0..rate as usize * 2)
                .map(|i| (TAU * freq * i as f32 / rate as f32).sin())
                .collect()
        };
        let (mut low, mut mid, mut high) = (Vec::new(), Vec::new(), Vec::new());
        for sample in tone(80.0).into_iter().chain(tone(1_000.0)) {
            let (l, m, h) = splitter.process(sample);
            low.push(l);
            mid.push(m);
            high.push(h);
        }
        let half = low.len() / 2;

        assert!(
            settled_rms(&low[..half]) > 0.5,
            "the low band still reads the low tone"
        );
        assert!(
            settled_rms(&mid[half..]) > 0.3,
            "the mid band still reads the mid tone"
        );
        assert!(
            high.iter().all(|value| *value == 0.0),
            "the high band reads zero"
        );
    }

    #[test]
    fn low_tone_concentrates_in_low_band() {
        let mut splitter = ThreeBandSplitter::new(SAMPLE_RATE);
        let input = sine(80.0, 2.0);
        let mut low = Vec::with_capacity(input.len());
        let mut high = Vec::with_capacity(input.len());
        for &sample in &input {
            let (l, _m, h) = splitter.process(sample);
            low.push(l);
            high.push(h);
        }
        let low_rms = settled_rms(&low);
        let high_rms = settled_rms(&high);
        assert!(
            low_rms > 0.5,
            "80 Hz should pass the low-band low-pass; got {low_rms}"
        );
        assert!(
            high_rms < 0.05,
            "80 Hz should be heavily attenuated in the high band; got {high_rms}"
        );
    }

    #[test]
    fn high_tone_concentrates_in_high_band() {
        let mut splitter = ThreeBandSplitter::new(SAMPLE_RATE);
        let input = sine(8_000.0, 2.0);
        let mut low = Vec::with_capacity(input.len());
        let mut high = Vec::with_capacity(input.len());
        for &sample in &input {
            let (l, _m, h) = splitter.process(sample);
            low.push(l);
            high.push(h);
        }
        let low_rms = settled_rms(&low);
        let high_rms = settled_rms(&high);
        assert!(
            high_rms > 0.5,
            "8 kHz should pass the high-band high-pass; got {high_rms}"
        );
        assert!(
            low_rms < 0.05,
            "8 kHz should be heavily attenuated in the low band; got {low_rms}"
        );
    }
}
