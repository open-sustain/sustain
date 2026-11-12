// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Constant-tempo beat tracking: the tempo, where its beats fall, and
//! which of them start a bar.
//!
//! Everything reads onset envelopes — how sharply the spectrum gains
//! energy, frame to frame — in three bands:
//!
//! * the **kick band** (`KICK_BAND_HZ`), where the beat of dance music
//!   lives and the hi-hats and off-beat stabs that mislead a broadband
//!   tracker do not;
//! * the **mid band** (`MID_BAND_HZ`), where the snare's crack and the
//!   voice's syllables land — on the beat, in the songs whose kick plays
//!   around it;
//! * the **broadband** envelope, whose sharper attacks place a beat more
//!   precisely once its tempo is known.
//!
//! The steps:
//!
//! 1. **Tempo.** The caller's estimate (the tempogram's) settles which
//!    octave is the beat — a comb is easily drawn to two or four beats.
//!    It can also lock onto a figure longer than the beat: three beats
//!    heard as two (a dotted, 3-3-2 bass line), or four as three. The
//!    faster levels that would undo such a lock are checked on the kick
//!    envelope, which plays the beat, and one replaces the estimate only
//!    if the kick is clearly stronger on its beats (`METRICAL_MARGIN`).
//!    Near the level settled on, a
//!    comb over the kick envelope takes over: for
//!    each candidate tempo the envelope is folded onto one beat period,
//!    and the tempo whose fold piles the most onset energy onto one phase
//!    wins. The winner is then refined by summing the envelope at every
//!    beat of the track across a narrow tempo span — over a whole track
//!    the sum is sensitive to a hundredth of a beat per minute.
//! 2. **Beat phase.** At the tempo rounded to the hundredth — the
//!    precision a grid is stored at, so the beats laid out here are the
//!    beats a player reproduces — the broadband comb finds the phase. A
//!    comb cannot tell the beat from the off-beat when both are loud
//!    (rolling bass lines, off-beat hats), so the phase and the phase
//!    half a beat away are compared on the kick and the mid envelopes,
//!    each read as its strongest onsets within an eighth of a beat: they
//!    play on the beat, though not always at the broadband attack's exact
//!    instant. Each band's contrast — how many times stronger it is at
//!    one phase than at the other — multiplies into the verdict, so a
//!    dance track's four-on-the-floor kick carries it, and a song's
//!    snare and voice outvote a kick syncopated onto the off-beats.
//! 3. **Bar phase.** Music changes at bar lines — chords, bass notes, new
//!    parts entering. The spectrum of each beat is compared with the
//!    beats around it, and a learned model picks the beat position, of
//!    the four, the changes say starts the bar (`downbeat`).

use std::time::Duration;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use sustain_domain::{BEATS_PER_BAR, BeatGrid, Bpm};

use crate::downbeat::BarPositionFeatures;

/// Analysis frame at 44.1/48 kHz (~46 ms), doubled per octave of sample
/// rate above that so the frame spans the same time at any rate.
const BASE_FRAME_LEN: usize = 2048;
const BASE_SAMPLE_RATE: u32 = 44_100;
/// Frames advance by an eighth of a frame (~5.8 ms at 44.1 kHz).
const HOP_DIVISOR: usize = 8;
/// Log-spaced bands the spectrum is summed into.
pub(crate) const BAND_COUNT: usize = 24;
const LOWEST_BAND_HZ: f64 = 30.0;
const HIGHEST_BAND_HZ: f64 = 16_000.0;
/// The bands centered in this range (Hz) make up the kick band.
const KICK_BAND_HZ: (f64, f64) = (0.0, 250.0);
/// The bands centered in this range (Hz) make up the mid band.
const MID_BAND_HZ: (f64, f64) = (400.0, 1_500.0);
/// Gain applied before log compression of band energies: it sets where
/// the compression knee sits relative to full-scale audio.
const BAND_ENERGY_GAIN: f32 = 1_000.0;
/// Each envelope has its local mean, over this span, subtracted, so
/// sustained energy changes (a build-up, a breakdown) do not count as
/// onsets.
const DETREND_SECS: f64 = 0.5;

/// How far from the estimate the tempo is searched: the estimate is
/// within ±2 BPM of the true tempo on nearly every track, and 2 % of a
/// dance tempo is more than that.
const ESTIMATE_SPAN: f64 = 0.02;
/// Faster metrical levels the estimate is checked against, as ratios to
/// it: the beat when the estimate locked onto three of them, or four.
/// Only faster levels: a kick that skips beats (syncopated, as in
/// breakbeat, tek or dembow) can make a slower grid land on it more
/// squarely than the beat's does, but it cannot fill every pulse of a
/// faster one. Octaves are the estimate's and the range's to settle.
const METRICAL_ALTERNATIVES: [f64; 2] = [3.0 / 2.0, 4.0 / 3.0];
/// How much stronger, per beat, the kick must be at an alternative level
/// than at the estimate's for the alternative to replace it. The
/// estimate has heard the whole spectrum; the kick only overrides it
/// where it clearly disagrees. On 1,559 development tracks the levels
/// that won were at least 1.24 times stronger and every other at most
/// 1.03 times; this margin sits between the two.
const METRICAL_MARGIN: f64 = 1.12;
/// Coarse tempo candidates are spaced by this ratio (0.05 %, about
/// 0.06 BPM at 128 BPM).
const COARSE_TEMPO_STEP: f64 = 1.0005;
/// Phase bins the coarse comb folds each beat period into.
const COARSE_PHASE_BINS: usize = 64;
/// The refined tempo is searched within this fraction of the coarse one.
const REFINE_TEMPO_SPAN: f64 = 0.004;
const REFINE_TEMPO_STEPS: usize = 161;
const REFINE_PHASE_STEPS: usize = 64;
/// The final tempo is picked among the hundredths this close to the
/// refined one.
const FINAL_TEMPO_SPAN_HUNDREDTHS: u32 = 3;
/// Phase candidates across one beat at the final tempo (~1.2 ms apart at
/// 128 BPM).
const PHASE_STEPS: usize = 400;
/// How far the onset envelope's peak runs ahead of the onset that causes
/// it. Log-compressed energy rises as soon as an attack enters the
/// leading edge of the analysis frame, well before the frame is centered
/// on it; the lead is constant for a given frame shape and was measured
/// on synthetic onsets (kick, click, noise burst) — see the
/// `grid_lands_on_*` tests.
const ONSET_LEAD_SECS: f64 = 0.016;
/// How far, in beats, from a candidate phase the kick and mid envelopes
/// are read when telling the beat from the off-beat. A band's envelope
/// does not peak where the broadband one does — a sub swelling after
/// its click, a long frame on a low note — so it is read around the
/// candidate, out to halfway to the quarter-beat between the beat and
/// the off-beat: a sixteenth-note figure there belongs to neither.
const PHASE_VOTE_REACH: f64 = 0.125;
/// A grid needs at least this many beats to measure anything.
const MIN_BEATS: usize = 16;

/// Onset envelopes and band energies of one stretch of audio.
pub(crate) struct Onsets {
    /// Envelope frames per second.
    frame_rate: f64,
    /// Time of frame 0's center.
    origin_secs: f64,
    /// Length of the analysed audio.
    duration_secs: f64,
    kick: Vec<f32>,
    mid: Vec<f32>,
    broadband: Vec<f32>,
    /// Log-compressed energy per band, per frame.
    bands: Vec<[f32; BAND_COUNT]>,
}

impl Onsets {
    /// Envelopes of mono `samples`. `None` when the audio is shorter than
    /// one analysis frame.
    pub(crate) fn of(samples: &[f32], sample_rate: u32) -> Option<Self> {
        let scale = (sample_rate / BASE_SAMPLE_RATE).max(1).next_power_of_two() as usize;
        let frame_len = BASE_FRAME_LEN * scale;
        let hop = frame_len / HOP_DIVISOR;
        if samples.len() < frame_len {
            return None;
        }
        let bands = band_energies(samples, sample_rate, frame_len, hop, scale);
        let edges = band_edges(sample_rate);
        let frame_rate = f64::from(sample_rate) / hop as f64;
        let detrend_len = (frame_rate * DETREND_SECS).round() as usize;
        Some(Self {
            frame_rate,
            origin_secs: (frame_len / 2) as f64 / f64::from(sample_rate),
            duration_secs: samples.len() as f64 / f64::from(sample_rate),
            kick: envelope(&bands, bands_centered_in(&edges, KICK_BAND_HZ), detrend_len),
            mid: envelope(&bands, bands_centered_in(&edges, MID_BAND_HZ), detrend_len),
            broadband: envelope(&bands, 0..BAND_COUNT, detrend_len),
            bands,
        })
    }

    /// The metrical level of the beat, as a tempo estimate within
    /// `[min_bpm, max_bpm]`: `estimate`, unless the kick is
    /// [`METRICAL_MARGIN`] times stronger per beat at one of the
    /// [`METRICAL_ALTERNATIVES`] that the range holds.
    pub(crate) fn beat_level(&self, estimate: f64, min_bpm: f64, max_bpm: f64) -> f64 {
        let Some(estimate_strength) = self.kick_per_beat(estimate, min_bpm, max_bpm) else {
            return estimate;
        };
        METRICAL_ALTERNATIVES
            .iter()
            .map(|ratio| estimate * ratio)
            .filter(|&level| level <= max_bpm)
            .filter_map(|level| {
                let strength = self.kick_per_beat(level, min_bpm, max_bpm)?;
                (strength > estimate_strength * METRICAL_MARGIN).then_some((strength, level))
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map_or(estimate, |(_, level)| level)
    }

    /// The mean kick-envelope energy per beat at the tempo near `estimate`
    /// and its best phase: how squarely that level's beats land on kicks.
    /// A mean over the beats weighs every level alike, where the coarse
    /// fold's peak, read from bins a fixed fraction of the period wide,
    /// favours slower levels.
    fn kick_per_beat(&self, estimate: f64, min_bpm: f64, max_bpm: f64) -> Option<f64> {
        let coarse = self.coarse_tempo_near(estimate, min_bpm, max_bpm)?;
        let (sum, tempo) = self.refined_tempo(coarse);
        Some(sum / (self.duration_secs * tempo / 60.0))
    }

    /// The precise tempo near `estimate` (beats per minute), kept within
    /// `[min_bpm, max_bpm]`. The estimate settles which metrical level
    /// is the beat; this finds the tempo at that level to the hundredth.
    /// `None` when there is too little audio to hold [`MIN_BEATS`] beats
    /// or no onsets at all.
    pub(crate) fn tempo_near(&self, estimate: f64, min_bpm: f64, max_bpm: f64) -> Option<Bpm> {
        let coarse = self.coarse_tempo_near(estimate, min_bpm, max_bpm)?;
        let (_, refined) = self.refined_tempo(coarse);
        let refined = Bpm::from_f64(refined)?.hundredths();
        // The kick envelope is smooth enough to find the tempo's
        // neighbourhood but too blunt to pick its hundredth; the
        // broadband attacks pick it, among the hundredths around.
        let span = FINAL_TEMPO_SPAN_HUNDREDTHS;
        (refined.saturating_sub(span)..=refined + span)
            .filter_map(Bpm::from_hundredths)
            .filter(|tempo| (min_bpm..=max_bpm).contains(&tempo.as_f64()))
            .map(|tempo| {
                let period = 60.0 / tempo.as_f64();
                let phase = self.best_phase(&self.broadband, period, PHASE_STEPS);
                (self.comb(&self.broadband, period, phase), tempo)
            })
            .max_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, tempo)| tempo)
    }

    /// The beat grid at `tempo`. `None` when the audio is too short to
    /// hold [`MIN_BEATS`] beats.
    pub(crate) fn beat_grid(&self, tempo: Bpm) -> Option<BeatGrid> {
        self.beat_grid_and_bars(tempo).map(|(grid, _)| grid)
    }

    /// The beat grid at `tempo`, and the evidence its downbeat was chosen
    /// from, position 0 being the grid's first beat. The evidence is
    /// `None` when there are too few beats to read bars from; the first
    /// beat then starts a bar. `None` when the audio is too short to hold
    /// [`MIN_BEATS`] beats.
    pub(crate) fn beat_grid_and_bars(
        &self,
        tempo: Bpm,
    ) -> Option<(BeatGrid, Option<BarPositionFeatures>)> {
        let period = 60.0 / tempo.as_f64();
        if self.duration_secs / period < MIN_BEATS as f64 {
            return None;
        }
        let comb_peak = self.best_phase(&self.broadband, period, PHASE_STEPS);
        let off_beat = (comb_peak + period / 2.0) % period;
        // The product of each band's onsets at one phase against the
        // product at the other: whether the contrasts, multiplied, favour
        // the off-beat. A band silent at both phases has no say.
        let (mut at_peak, mut at_off_beat) = (1.0, 1.0);
        for envelope in [&self.kick, &self.mid] {
            let on = self.strongest_near(envelope, comb_peak, period);
            let off = self.strongest_near(envelope, off_beat, period);
            if on + off > 0.0 {
                at_peak *= on;
                at_off_beat *= off;
            }
        }
        let peak = if at_off_beat > at_peak {
            off_beat
        } else {
            comb_peak
        };
        let mut first_beat = (peak + ONSET_LEAD_SECS).rem_euclid(period);
        // Rounding to the grid's precision can carry the phase onto the
        // interval itself: that beat is the timeline's first, a
        // microsecond after its start.
        if BeatGrid::new(tempo, Duration::from_secs_f64(first_beat), 1).is_err() {
            first_beat = 0.0;
        }
        let bars = self.bar_position_features(first_beat, period);
        let downbeat = bars.as_ref().map_or(0, BarPositionFeatures::downbeat);
        let bar = usize::from(BEATS_PER_BAR);
        let first_beat_number = ((bar - downbeat) % bar) as u8 + 1;
        let grid = BeatGrid::new(
            tempo,
            Duration::from_secs_f64(first_beat),
            first_beat_number,
        )
        .ok()?;
        Some((grid, bars))
    }

    /// The strongest comb over `envelope` at `period` within
    /// [`PHASE_VOTE_REACH`] of a beat at `phase`.
    fn strongest_near(&self, envelope: &[f32], phase: f64, period: f64) -> f64 {
        let step = period / PHASE_STEPS as f64;
        let reach = (PHASE_VOTE_REACH * PHASE_STEPS as f64).round() as usize;
        (0..=2 * reach)
            .map(|index| {
                let offset = (index as f64 - reach as f64) * step;
                self.comb(envelope, period, (phase + offset).rem_euclid(period))
            })
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// [`Self::coarse_tempo`] within [`ESTIMATE_SPAN`] of `estimate`,
    /// kept within `[min_bpm, max_bpm]`. `None` when there is too little
    /// audio to hold [`MIN_BEATS`] beats there, or no onsets at all.
    fn coarse_tempo_near(&self, estimate: f64, min_bpm: f64, max_bpm: f64) -> Option<f64> {
        let low = (estimate * (1.0 - ESTIMATE_SPAN)).max(min_bpm);
        let high = (estimate * (1.0 + ESTIMATE_SPAN)).min(max_bpm);
        if !(low > 0.0 && high >= low) {
            return None;
        }
        if self.duration_secs * low / 60.0 < MIN_BEATS as f64 {
            return None;
        }
        self.coarse_tempo(low, high)
    }

    /// The candidate tempo whose period, folded over the kick envelope,
    /// stacks the largest share of onset energy onto one phase.
    fn coarse_tempo(&self, min_bpm: f64, max_bpm: f64) -> Option<f64> {
        let total: f64 = self.kick.iter().map(|&value| f64::from(value)).sum();
        if total <= 0.0 {
            return None;
        }
        let steps = ((max_bpm / min_bpm).ln() / COARSE_TEMPO_STEP.ln()).ceil() as usize + 1;
        let mut best = (f64::NEG_INFINITY, min_bpm);
        let mut histogram = [0.0_f64; COARSE_PHASE_BINS];
        for step in 0..steps {
            let tempo = (min_bpm * COARSE_TEMPO_STEP.powi(step as i32)).min(max_bpm);
            histogram.fill(0.0);
            let beats_per_frame = tempo / 60.0 / self.frame_rate;
            for (frame, &value) in self.kick.iter().enumerate() {
                let phase = (frame as f64 * beats_per_frame).fract();
                let bin = ((phase * COARSE_PHASE_BINS as f64) as usize).min(COARSE_PHASE_BINS - 1);
                histogram[bin] += f64::from(value);
            }
            let peak = (0..COARSE_PHASE_BINS)
                .map(|bin| {
                    let before = histogram[(bin + COARSE_PHASE_BINS - 1) % COARSE_PHASE_BINS];
                    let after = histogram[(bin + 1) % COARSE_PHASE_BINS];
                    histogram[bin] + 0.5 * (before + after)
                })
                .fold(f64::NEG_INFINITY, f64::max);
            if peak > best.0 {
                best = (peak, tempo);
            }
        }
        Some(best.1)
    }

    /// The tempo near `coarse` whose beats, at their best phase, sum the
    /// most kick-envelope energy across the whole track, and that sum.
    fn refined_tempo(&self, coarse: f64) -> (f64, f64) {
        let mut best = (f64::NEG_INFINITY, coarse);
        for step in 0..REFINE_TEMPO_STEPS {
            let fraction = step as f64 / (REFINE_TEMPO_STEPS - 1) as f64;
            let tempo = coarse * (1.0 - REFINE_TEMPO_SPAN + 2.0 * REFINE_TEMPO_SPAN * fraction);
            let period = 60.0 / tempo;
            let phase = self.best_phase(&self.kick, period, REFINE_PHASE_STEPS);
            let score = self.comb(&self.kick, period, phase);
            if score > best.0 {
                best = (score, tempo);
            }
        }
        best
    }

    /// The phase in `[0, period)`, among `steps` evenly spaced ones,
    /// whose beats sum the most `envelope` energy.
    fn best_phase(&self, envelope: &[f32], period: f64, steps: usize) -> f64 {
        let mut best = (f64::NEG_INFINITY, 0.0);
        for step in 0..steps {
            let phase = period * step as f64 / steps as f64;
            let score = self.comb(envelope, period, phase);
            if score > best.0 {
                best = (score, phase);
            }
        }
        best.1
    }

    /// Sum of `envelope`, interpolated, at every beat `phase + k·period`
    /// inside the audio.
    fn comb(&self, envelope: &[f32], period: f64, phase: f64) -> f64 {
        let last = envelope.len().saturating_sub(1) as f64;
        let mut sum = 0.0;
        let mut time = phase;
        loop {
            let position = (time - self.origin_secs) * self.frame_rate;
            if position >= last {
                return sum;
            }
            if position >= 0.0 {
                let index = position as usize;
                let weight = position - index as f64;
                sum += f64::from(envelope[index]) * (1.0 - weight)
                    + f64::from(envelope[index + 1]) * weight;
            }
            time += period;
        }
    }

    /// The evidence for a bar starting on each of the four beats from
    /// `first_beat`: the mean band spectrum and the broadband onset of
    /// every whole beat in the audio, pooled by bar position.
    fn bar_position_features(&self, first_beat: f64, period: f64) -> Option<BarPositionFeatures> {
        let frame_at = |time: f64| -> usize {
            (((time - self.origin_secs) * self.frame_rate)
                .round()
                .max(0.0) as usize)
                .min(self.bands.len() - 1)
        };
        let whole_beats = ((self.duration_secs - first_beat) / period)
            .floor()
            .max(0.0) as usize;
        let beat_starts: Vec<f64> = (0..whole_beats)
            .map(|index| first_beat + index as f64 * period)
            .collect();
        let beat_spectra: Vec<[f32; BAND_COUNT]> = beat_starts
            .iter()
            .map(|&start| {
                let (from, to) = (frame_at(start), frame_at(start + period));
                let frames = &self.bands[from..to.max(from + 1)];
                let mut mean = [0.0_f32; BAND_COUNT];
                for frame in frames {
                    for (sum, value) in mean.iter_mut().zip(frame) {
                        *sum += value;
                    }
                }
                mean.map(|sum| sum / frames.len() as f32)
            })
            .collect();
        // The envelope peaks [`ONSET_LEAD_SECS`] ahead of the beat it
        // marks; it is read within [`PHASE_VOTE_REACH`] of that peak.
        let reach = PHASE_VOTE_REACH * period;
        let beat_onsets: Vec<f32> = beat_starts
            .iter()
            .map(|&start| {
                let peak = start - ONSET_LEAD_SECS;
                let (from, to) = (frame_at(peak - reach), frame_at(peak + reach));
                self.broadband[from..=to]
                    .iter()
                    .copied()
                    .fold(0.0, f32::max)
            })
            .collect();
        BarPositionFeatures::of(&beat_spectra, &beat_onsets)
    }
}

/// Band edges, log-spaced from `LOWEST_BAND_HZ` to `HIGHEST_BAND_HZ`
/// (or just below Nyquist at low sample rates).
fn band_edges(sample_rate: u32) -> [f64; BAND_COUNT + 1] {
    let top = HIGHEST_BAND_HZ.min(f64::from(sample_rate) / 2.0 * 0.95);
    let ratio = (top / LOWEST_BAND_HZ).powf(1.0 / BAND_COUNT as f64);
    let mut edges = [0.0; BAND_COUNT + 1];
    for (index, edge) in edges.iter_mut().enumerate() {
        *edge = LOWEST_BAND_HZ * ratio.powi(index as i32);
    }
    edges
}

/// The bands whose center — the geometric mean of their edges, the
/// middle of a log-spaced band — lies within `(low, high)` Hz.
fn bands_centered_in(
    edges: &[f64; BAND_COUNT + 1],
    (low, high): (f64, f64),
) -> std::ops::Range<usize> {
    let centers = edges.windows(2).map(|edge| (edge[0] * edge[1]).sqrt());
    let first = centers.clone().take_while(|&center| center < low).count();
    let end = centers.take_while(|&center| center <= high).count();
    first..end.max(first)
}

/// Log-compressed band energies of every frame. Frames are transformed
/// two at a time — one as the real part, the other as the imaginary part
/// of a single complex FFT — and separated by conjugate symmetry.
fn band_energies(
    samples: &[f32],
    sample_rate: u32,
    frame_len: usize,
    hop: usize,
    scale: usize,
) -> Vec<[f32; BAND_COUNT]> {
    let frame_count = (samples.len() - frame_len) / hop + 1;
    let edges = band_edges(sample_rate);
    let bin_hz = f64::from(sample_rate) / frame_len as f64;
    let bin_band: Vec<Option<usize>> = (0..=frame_len / 2)
        .map(|bin| {
            let freq = bin as f64 * bin_hz;
            edges
                .windows(2)
                .position(|edge| freq >= edge[0] && freq < edge[1])
        })
        .collect();
    // Symmetric Hann window, as most onset-detection literature uses.
    let window: Vec<f32> = (0..frame_len)
        .map(|n| {
            let phase = std::f64::consts::TAU * n as f64 / (frame_len - 1) as f64;
            (0.5 - 0.5 * phase.cos()) as f32
        })
        .collect();
    // A longer frame gathers proportionally more energy; scale it back so
    // the compression knee sits at the same loudness at every rate.
    let gain = BAND_ENERGY_GAIN / (scale * scale) as f32;
    let fft = FftPlanner::<f32>::new().plan_fft_forward(frame_len);
    let mut buffer = vec![Complex::new(0.0, 0.0); frame_len];
    let mut scratch = vec![Complex::new(0.0, 0.0); fft.get_inplace_scratch_len()];
    let mut energies = vec![[0.0_f32; BAND_COUNT]; frame_count];
    for pair_start in (0..frame_count).step_by(2) {
        let second = (pair_start + 1 < frame_count).then_some(pair_start + 1);
        let first_frame = &samples[pair_start * hop..pair_start * hop + frame_len];
        for (index, slot) in buffer.iter_mut().enumerate() {
            let im = second.map_or(0.0, |frame| samples[frame * hop + index]);
            *slot = Complex::new(first_frame[index] * window[index], im * window[index]);
        }
        fft.process_with_scratch(&mut buffer, &mut scratch);
        let mut first_bands = [0.0_f32; BAND_COUNT];
        let mut second_bands = [0.0_f32; BAND_COUNT];
        for (bin, band) in bin_band.iter().enumerate() {
            let Some(band) = *band else { continue };
            let z = buffer[bin];
            let mirror = buffer[(frame_len - bin) % frame_len].conj();
            let first = (z + mirror) * 0.5;
            let second = (z - mirror) * 0.5;
            first_bands[band] += first.norm_sqr();
            second_bands[band] += second.norm_sqr();
        }
        energies[pair_start] = first_bands.map(|energy| (gain * energy).ln_1p());
        if let Some(frame) = second {
            energies[frame] = second_bands.map(|energy| (gain * energy).ln_1p());
        }
    }
    energies
}

/// Onset envelope of `bands`: the summed rise in band energy frame to
/// frame, with its local mean (over `detrend_len` frames) removed and
/// scaled to a mean of one.
fn envelope(
    energies: &[[f32; BAND_COUNT]],
    bands: std::ops::Range<usize>,
    detrend_len: usize,
) -> Vec<f32> {
    let mut flux = Vec::with_capacity(energies.len());
    flux.push(0.0_f32);
    for pair in energies.windows(2) {
        flux.push(
            bands
                .clone()
                .map(|band| (pair[1][band] - pair[0][band]).max(0.0))
                .sum(),
        );
    }
    let detrend_len = detrend_len.max(1);
    let mut prefix = Vec::with_capacity(flux.len() + 1);
    prefix.push(0.0_f64);
    for &value in &flux {
        prefix.push(prefix[prefix.len() - 1] + f64::from(value));
    }
    let half = detrend_len / 2;
    let mut detrended: Vec<f32> = (0..flux.len())
        .map(|index| {
            let from = index.saturating_sub(half);
            let to = (index + detrend_len - half).min(flux.len());
            let local_mean = (prefix[to] - prefix[from]) / detrend_len as f64;
            (f64::from(flux[index]) - local_mean).max(0.0) as f32
        })
        .collect();
    let mean = detrended.iter().map(|&value| f64::from(value)).sum::<f64>() / flux.len() as f64;
    if mean > 0.0 {
        for value in &mut detrended {
            *value = (f64::from(*value) / mean) as f32;
        }
    }
    detrended
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sustain_domain::Bpm;

    use super::{BAND_COUNT, KICK_BAND_HZ, MID_BAND_HZ, Onsets, band_edges, bands_centered_in};

    const SAMPLE_RATE: u32 = 44_100;

    /// `secs` of audio with `hit` started at every beat of `bpm`, the
    /// first at `first_secs`. Every `accent_every`-th beat, from the
    /// first, gets a held chord tone — a bar's worth of harmony that
    /// changes at the bar line.
    fn render(
        bpm: f64,
        first_secs: f64,
        secs: f64,
        hit: &[f32],
        accent_every: Option<usize>,
    ) -> Vec<f32> {
        let mut samples = vec![0.0_f32; (secs * f64::from(SAMPLE_RATE)) as usize];
        let period = 60.0 / bpm;
        let beat_times = (0_usize..)
            .map(|beat| (beat, first_secs + beat as f64 * period))
            .take_while(|&(_, time)| time < secs);
        for (beat, time) in beat_times {
            let start = (time * f64::from(SAMPLE_RATE)).round() as usize;
            for (offset, value) in hit.iter().enumerate() {
                if let Some(sample) = samples.get_mut(start + offset) {
                    *sample += value;
                }
            }
            if let Some(every) = accent_every
                && beat % every == 0
            {
                let freq = [220.0, 330.0, 262.0, 392.0][(beat / every) % 4];
                let len = (period * every as f64 * f64::from(SAMPLE_RATE)) as usize;
                for offset in 0..len {
                    if let Some(sample) = samples.get_mut(start + offset) {
                        let t = offset as f64 / f64::from(SAMPLE_RATE);
                        *sample += (0.2 * (std::f64::consts::TAU * freq * t).sin()) as f32;
                    }
                }
            }
        }
        samples
    }

    /// A synthetic kick: a pitch drop from 150 to 50 Hz, decaying.
    fn kick() -> Vec<f32> {
        let len = SAMPLE_RATE as usize / 4;
        let mut phase = 0.0_f64;
        (0..len)
            .map(|n| {
                let t = n as f64 / f64::from(SAMPLE_RATE);
                let freq = 50.0 + 100.0 * (-t / 0.03).exp();
                phase += std::f64::consts::TAU * freq / f64::from(SAMPLE_RATE);
                (phase.sin() * (-t / 0.12).exp() * 0.8) as f32
            })
            .collect()
    }

    /// A noise burst (a hi-hat): deterministic pseudo-random noise.
    fn hat() -> Vec<f32> {
        let mut state = 0x2545_f491_u32;
        (0..SAMPLE_RATE as usize / 20)
            .map(|n| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                let noise = state as f32 / u32::MAX as f32 * 2.0 - 1.0;
                noise * (-(n as f32) / (0.01 * SAMPLE_RATE as f32)).exp() * 0.3
            })
            .collect()
    }

    fn phase_error_ms(grid_first: Duration, onset_secs: f64, bpm: f64) -> f64 {
        let period = 60.0 / bpm;
        let error = (grid_first.as_secs_f64() - onset_secs) / period;
        (error - error.round()) * period * 1_000.0
    }

    #[test]
    fn tempo_is_found_to_the_hundredth_over_a_whole_track() {
        for bpm in [118.86, 124.0, 128.0, 140.5] {
            let samples = render(bpm, 0.3, 120.0, &kick(), None);
            let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
            let tempo = onsets.tempo_near(bpm.round(), 76.0, 155.0).expect("tempo");
            assert_eq!(tempo, Bpm::from_f64(bpm).expect("tempo"), "{bpm}");
        }
    }

    #[test]
    fn a_figure_longer_than_the_beat_gives_way_to_the_kick() {
        // A kick on every beat under a loud stab every one and a half
        // beats (a dotted figure) or every four thirds; the estimate
        // locked onto the stabs, the kick band hears the beat.
        for (bpm, figure_beats) in [(125.0, 1.5), (128.0, 4.0 / 3.0)] {
            let kicks = render(bpm, 0.2, 90.0, &kick(), None);
            let figure_bpm = bpm / figure_beats;
            let stab = tone(330.0, 0.9, 0.002, 0.2, 0.3);
            let stabs = render(figure_bpm, 0.2, 90.0, &stab, None);
            let samples: Vec<f32> = kicks.iter().zip(&stabs).map(|(k, s)| k + s).collect();
            let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
            let level = onsets.beat_level(figure_bpm, 76.0, 155.0);
            assert!((level - bpm).abs() < 1.0, "{bpm}: {level}");
        }
    }

    #[test]
    fn the_beat_stays_when_a_faster_level_misses_the_kicks() {
        // 100 BPM with off-beat hats: 133 and 150 BPM are in range but
        // land on a kick only every third or fourth pulse.
        let period = 60.0 / 100.0;
        let kicks = render(100.0, 0.25, 90.0, &kick(), None);
        let hats = render(100.0, 0.25 + period / 2.0, 90.0, &hat(), None);
        let samples: Vec<f32> = kicks.iter().zip(&hats).map(|(k, h)| k + h).collect();
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        assert_eq!(onsets.beat_level(100.0, 76.0, 155.0), 100.0);
    }

    #[test]
    fn grid_lands_on_kicks() {
        let samples = render(125.0, 0.2, 60.0, &kick(), None);
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        let tempo = Bpm::from_hundredths(12_500).expect("tempo");
        let grid = onsets.beat_grid(tempo).expect("grid");
        let error = phase_error_ms(grid.first_beat(), 0.2, 125.0);
        assert!(error.abs() < 2.0, "off by {error} ms");
    }

    #[test]
    fn grid_lands_on_the_kick_not_the_off_beat_hat() {
        // A kick on every beat and a louder hat on every off-beat: the
        // broadband comb alone could lock onto either.
        let period = 60.0 / 128.0;
        let kicks = render(128.0, 0.25, 60.0, &kick(), None);
        let hats = render(128.0, 0.25 + period / 2.0, 60.0, &hat(), None);
        let samples: Vec<f32> = kicks.iter().zip(&hats).map(|(k, h)| k + 2.0 * h).collect();
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        let grid = onsets
            .beat_grid(Bpm::from_hundredths(12_800).expect("tempo"))
            .expect("grid");
        let error = phase_error_ms(grid.first_beat(), 0.25, 128.0);
        assert!(error.abs() < 3.0, "off by {error} ms");
    }

    /// `secs` of a sine at `freq` Hz, rising linearly over `attack_secs`
    /// and decaying with time constant `decay_secs`.
    fn tone(freq: f64, amplitude: f64, attack_secs: f64, decay_secs: f64, secs: f64) -> Vec<f32> {
        (0..(secs * f64::from(SAMPLE_RATE)) as usize)
            .map(|n| {
                let t = n as f64 / f64::from(SAMPLE_RATE);
                let envelope = (t / attack_secs).min(1.0) * (-t / decay_secs).exp();
                (amplitude * envelope * (std::f64::consts::TAU * freq * t).sin()) as f32
            })
            .collect()
    }

    #[test]
    fn grid_lands_on_a_slow_sub_kick_not_the_off_beat_bass() {
        // A layered kick — a click, then a sub whose body swells over
        // 35 ms — and a punchier, quieter bass note on every off-beat.
        // The click places the broadband peak ahead of the sub's rise,
        // so the kick band must be read around each candidate phase,
        // not at it, or the off-beat bass outweighs a kick still
        // swelling.
        let bpm = 128.0;
        let period = 60.0 / bpm;
        let layered: Vec<f32> = hat()
            .iter()
            .chain(std::iter::repeat(&0.0))
            .zip(tone(55.0, 0.9, 0.035, 0.15, 0.3))
            .map(|(click, sub)| click + sub)
            .collect();
        let kicks = render(bpm, 0.25, 60.0, &layered, None);
        let bass = render(
            bpm,
            0.25 + period / 2.0,
            60.0,
            &tone(80.0, 0.5, 0.002, 0.08, 0.2),
            None,
        );
        let samples: Vec<f32> = kicks.iter().zip(&bass).map(|(k, b)| k + b).collect();
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        let grid = onsets
            .beat_grid(Bpm::from_hundredths(12_800).expect("tempo"))
            .expect("grid");
        let error = phase_error_ms(grid.first_beat(), 0.25, bpm);
        assert!(error.abs() < 10.0, "off by {error} ms");
    }

    /// A snare: a noise burst over a short 900 Hz crack.
    fn snare() -> Vec<f32> {
        hat()
            .iter()
            .chain(std::iter::repeat(&0.0))
            .zip(tone(900.0, 0.4, 0.001, 0.04, 0.15))
            .map(|(noise, crack)| noise + crack)
            .collect()
    }

    #[test]
    fn grid_lands_on_the_backbeat_not_a_syncopated_kick() {
        // A song's groove: the kick on beat 1 and on the off-beats after
        // 2 and 3, the snare on 2 and 4. Off the beat the kick plays
        // twice a bar to once on it; the snare decides.
        let bpm = 96.0;
        let period = 60.0 / bpm;
        let first = 0.25;
        let hit =
            |hit: &[f32], beat: f64| render(bpm / 4.0, first + beat * period, 60.0, hit, None);
        let layers = [
            hit(&kick(), 0.0),
            hit(&kick(), 1.5),
            hit(&kick(), 2.5),
            hit(&snare(), 1.0),
            hit(&snare(), 3.0),
        ];
        let samples: Vec<f32> = (0..layers[0].len())
            .map(|n| layers.iter().map(|layer| layer[n]).sum())
            .collect();
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        let grid = onsets
            .beat_grid(Bpm::from_hundredths(9_600).expect("tempo"))
            .expect("grid");
        let error = phase_error_ms(grid.first_beat(), first, bpm);
        assert!(error.abs() < 10.0, "off by {error} ms");
    }

    #[test]
    fn bands_are_picked_by_their_center() {
        let edges = band_edges(SAMPLE_RATE);
        for (low, high) in [KICK_BAND_HZ, MID_BAND_HZ] {
            let picked = bands_centered_in(&edges, (low, high));
            assert!(!picked.is_empty());
            for band in 0..BAND_COUNT {
                let center = (edges[band] * edges[band + 1]).sqrt();
                assert_eq!(picked.contains(&band), (low..=high).contains(&center));
            }
        }
    }

    #[test]
    fn bars_start_where_the_harmony_changes() {
        // Kicks on every beat from 0.3 s; the chord changes every fourth
        // beat starting with the second one, so beat 2 is a downbeat and
        // the first beat is the bar's last.
        let bpm = 126.0;
        let period = 60.0 / bpm;
        let kicks = render(bpm, 0.3, 90.0, &kick(), None);
        let chords = render(bpm, 0.3 + period, 90.0, &[], Some(4));
        let samples: Vec<f32> = kicks.iter().zip(&chords).map(|(k, c)| k + c).collect();
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        let grid = onsets
            .beat_grid(Bpm::from_hundredths(12_600).expect("tempo"))
            .expect("grid");
        assert_eq!(grid.first_beat_number(), 4);
    }

    #[test]
    fn too_little_audio_has_no_tempo_or_grid() {
        let samples = render(128.0, 0.1, 5.0, &kick(), None);
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        assert_eq!(onsets.tempo_near(128.0, 76.0, 155.0), None);
        assert_eq!(
            onsets.beat_grid(Bpm::from_hundredths(12_800).expect("tempo")),
            None
        );
        assert!(Onsets::of(&[0.0; 100], SAMPLE_RATE).is_none());
    }

    #[test]
    fn silence_has_no_tempo() {
        let samples = vec![0.0_f32; SAMPLE_RATE as usize * 30];
        let onsets = Onsets::of(&samples, SAMPLE_RATE).expect("onsets");
        assert_eq!(onsets.tempo_near(128.0, 76.0, 155.0), None);
    }
}
