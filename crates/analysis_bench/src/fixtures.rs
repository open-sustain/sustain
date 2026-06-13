// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Deterministic synthetic audio fixtures.
//!
//! Each [`Synthetic`] variant renders to a 16-bit PCM WAV with pure
//! integer/float math, so the bytes are identical on every run and every
//! machine — that reproducibility is what makes the synthetic tier
//! committable (we commit the *recipe*, never the audio) and what backs
//! the decoder/seek/mono/waveform determinism guarantee. Several variants
//! also carry an exact, by-construction ground truth (a click train's BPM,
//! a triad's key) so they double as a coarse accuracy smoke-check.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Default fixture length in seconds. Comfortably above
/// `sustain_analysis`'s one-second analysis floor and long enough for a
/// stable tempogram/chroma estimate without bloating test time.
fn default_secs() -> f64 {
    8.0
}

/// Default sample rate. 44.1 kHz is the analysis pipeline's reference
/// rate and the rate the band-split crossovers are reasoned about at.
fn default_sample_rate() -> u32 {
    44_100
}

/// Default channel count. Mono unless a fixture explicitly exercises the
/// multi-channel collapse path.
fn default_channels() -> u16 {
    1
}

/// Default tone amplitude (linear, pre-quantization). Leaves headroom so
/// the 16-bit conversion never clips.
fn default_amplitude() -> f64 {
    0.5
}

/// A deterministically generated audio fixture. Declared inline in a
/// manifest as `synthetic = { kind = "...", ... }`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Synthetic {
    /// Digital silence. Exercises the "no measurable loudness" and
    /// empty-waveform paths.
    Silence {
        #[serde(default = "default_secs")]
        secs: f64,
        #[serde(default = "default_sample_rate")]
        sample_rate: u32,
        #[serde(default = "default_channels")]
        channels: u16,
    },
    /// A steady sine tone. Exercises loudness measurement and, on a
    /// multi-channel rendering, the mono collapse.
    Tone {
        freq_hz: f64,
        #[serde(default = "default_secs")]
        secs: f64,
        #[serde(default = "default_amplitude")]
        amplitude: f64,
        #[serde(default = "default_sample_rate")]
        sample_rate: u32,
        #[serde(default = "default_channels")]
        channels: u16,
    },
    /// A mono linear ramp from −1 to +1: each sample's value encodes its
    /// frame index, so a decode can prove *where* in the track a sample
    /// came from. The seek-positioning analogue of the decoder's own unit
    /// fixture, surfaced here for end-to-end runs.
    Ramp {
        #[serde(default = "default_secs")]
        secs: f64,
        #[serde(default = "default_sample_rate")]
        sample_rate: u32,
    },
    /// Impulses spaced one beat apart over a silent bed — a metronome at
    /// `bpm`. The strongest possible periodic novelty signal, so the
    /// detected tempo should land on `bpm` (or an octave of it). Ground
    /// truth: `bpm`.
    ClickTrain {
        bpm: f64,
        #[serde(default = "default_secs")]
        secs: f64,
        #[serde(default = "default_sample_rate")]
        sample_rate: u32,
        #[serde(default = "default_channels")]
        channels: u16,
    },
    /// A sustained major or minor triad rooted at pitch class `root_pc`
    /// (0 = C … 11 = B). Three summed sine partials at the chord tones;
    /// the chroma/key detector should resolve it to that key. Ground
    /// truth: the key named by `root_pc` + `minor`.
    Triad {
        root_pc: u8,
        #[serde(default)]
        minor: bool,
        #[serde(default = "default_secs")]
        secs: f64,
        #[serde(default = "default_sample_rate")]
        sample_rate: u32,
    },
}

/// Frequency of pitch class `pc` (0 = C) in the octave starting at C4
/// (≈261.63 Hz), in hertz.
fn pitch_class_freq(pc: u8) -> f64 {
    const C4: f64 = 261.625_565_3;
    C4 * 2.0_f64.powf(f64::from(pc % 12) / 12.0)
}

impl Synthetic {
    /// Sample rate of the rendered audio.
    pub fn sample_rate(&self) -> u32 {
        match *self {
            Synthetic::Silence { sample_rate, .. }
            | Synthetic::Tone { sample_rate, .. }
            | Synthetic::Ramp { sample_rate, .. }
            | Synthetic::ClickTrain { sample_rate, .. }
            | Synthetic::Triad { sample_rate, .. } => sample_rate,
        }
    }

    /// Rendered length in seconds (used as the analyzer's duration hint).
    pub fn secs(&self) -> f64 {
        match *self {
            Synthetic::Silence { secs, .. }
            | Synthetic::Tone { secs, .. }
            | Synthetic::Ramp { secs, .. }
            | Synthetic::ClickTrain { secs, .. }
            | Synthetic::Triad { secs, .. } => secs,
        }
    }

    /// Channel count of the rendered audio.
    fn channels(&self) -> u16 {
        match *self {
            Synthetic::Silence { channels, .. }
            | Synthetic::Tone { channels, .. }
            | Synthetic::ClickTrain { channels, .. } => channels.max(1),
            Synthetic::Ramp { .. } | Synthetic::Triad { .. } => 1,
        }
    }

    /// Render the mono signal (one value per frame). Multi-channel
    /// fixtures replicate this across channels at write time.
    fn mono_frames(&self) -> Vec<f32> {
        let sample_rate = f64::from(self.sample_rate());
        let frame_count = (self.secs() * sample_rate).round().max(0.0) as usize;
        match *self {
            Synthetic::Silence { .. } => vec![0.0; frame_count],
            Synthetic::Tone {
                freq_hz, amplitude, ..
            } => (0..frame_count)
                .map(|i| {
                    let phase = std::f64::consts::TAU * freq_hz * i as f64 / sample_rate;
                    (amplitude * phase.sin()) as f32
                })
                .collect(),
            Synthetic::Ramp { .. } => (0..frame_count)
                .map(|i| {
                    if frame_count <= 1 {
                        0.0
                    } else {
                        ((i as f64 / (frame_count - 1) as f64) * 2.0 - 1.0) as f32
                    }
                })
                .collect(),
            Synthetic::ClickTrain { bpm, .. } => {
                let mut frames = vec![0.0_f32; frame_count];
                if bpm > 0.0 {
                    let beat_frames = (sample_rate * 60.0 / bpm).round().max(1.0) as usize;
                    // A short exponentially-decaying click (~5 ms) at each
                    // beat boundary. Deterministic and rich in onsets.
                    let click_len = ((sample_rate * 0.005) as usize).max(1);
                    let mut beat = 0;
                    while beat * beat_frames < frame_count {
                        let start = beat * beat_frames;
                        for k in 0..click_len {
                            let idx = start + k;
                            if idx >= frame_count {
                                break;
                            }
                            let decay = 1.0 - (k as f64 / click_len as f64);
                            frames[idx] = (0.9 * decay) as f32;
                        }
                        beat += 1;
                    }
                }
                frames
            }
            Synthetic::Triad { root_pc, minor, .. } => {
                let third = if minor { 3 } else { 4 };
                let intervals = [0_u8, third, 7];
                let freqs: Vec<f64> = intervals
                    .iter()
                    .map(|semi| pitch_class_freq(root_pc.wrapping_add(*semi)))
                    .collect();
                let scale = 0.5 / freqs.len() as f64;
                (0..frame_count)
                    .map(|i| {
                        let t = i as f64 / sample_rate;
                        let sum: f64 = freqs
                            .iter()
                            .map(|f| (std::f64::consts::TAU * f * t).sin())
                            .sum();
                        (scale * sum) as f32
                    })
                    .collect()
            }
        }
    }

    /// Render to a 16-bit PCM WAV at `path`. Multi-channel fixtures write
    /// the same mono signal to every channel.
    pub fn write_wav(&self, path: &Path) -> io::Result<()> {
        let mono = self.mono_frames();
        let channels = self.channels();
        let bytes = encode_wav(self.sample_rate(), channels, &mono);
        std::fs::write(path, bytes)
    }
}

/// Encode `mono` (one value per frame) as a little-endian 16-bit PCM WAV,
/// replicating each frame across `channels` channels.
fn encode_wav(sample_rate: u32, channels: u16, mono: &[f32]) -> Vec<u8> {
    let channels = channels.max(1);
    let block_align = channels * 2; // 16-bit
    let byte_rate = sample_rate * u32::from(block_align);
    let data_len = (mono.len() * usize::from(channels) * 2) as u32;

    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
    bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for &value in mono {
        let quantized = (f64::from(value).clamp(-1.0, 1.0) * 32_767.0).round() as i16;
        for _ in 0..channels {
            bytes.extend_from_slice(&quantized.to_le_bytes());
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::{Synthetic, encode_wav};

    #[test]
    fn render_is_byte_identical_across_runs() {
        let fixture = Synthetic::ClickTrain {
            bpm: 120.0,
            secs: 2.0,
            sample_rate: 44_100,
            channels: 1,
        };
        let a = encode_wav(fixture.sample_rate(), 1, &fixture.mono_frames());
        let b = encode_wav(fixture.sample_rate(), 1, &fixture.mono_frames());
        assert_eq!(a, b, "synthetic render must be deterministic");
    }

    #[test]
    fn click_train_places_one_click_per_beat() {
        // 120 BPM over 2 s = 4 beats → 4 clicks; each click starts on a
        // beat boundary with a positive leading sample.
        let fixture = Synthetic::ClickTrain {
            bpm: 120.0,
            secs: 2.0,
            sample_rate: 44_100,
            channels: 1,
        };
        let frames = fixture.mono_frames();
        let beat_frames = (44_100.0 * 60.0 / 120.0) as usize; // 22_050
        for beat in 0..4 {
            assert!(
                frames[beat * beat_frames] > 0.5,
                "expected a click at beat {beat}"
            );
        }
    }

    #[test]
    fn wav_header_reports_the_channel_count() {
        let mono = vec![0.0_f32; 10];
        let stereo = encode_wav(44_100, 2, &mono);
        // Channel count lives at byte offset 22 in the fmt chunk.
        assert_eq!(u16::from_le_bytes([stereo[22], stereo[23]]), 2);
        // data chunk length = frames * channels * 2 bytes.
        let data_len = u32::from_le_bytes([stereo[40], stereo[41], stereo[42], stereo[43]]);
        assert_eq!(data_len, 10 * 2 * 2);
    }
}
