// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Audio files written for tests, so no fixture has to be checked in.

use std::path::Path;

/// Write `interleaved` samples, `channels` to a frame, as a 32-bit IEEE
/// float WAV at `sample_rate`. Any value is written as given, a NaN or an
/// infinity included.
pub(crate) fn write_float_wav(path: &Path, sample_rate: u32, channels: u16, interleaved: &[f32]) {
    let data_len = u32::try_from(interleaved.len() * 4).expect("a test file fits a WAV");
    let block_align = channels * 4;
    let mut bytes = Vec::with_capacity(44 + data_len as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
    bytes.extend_from_slice(b"WAVE");
    bytes.extend_from_slice(b"fmt ");
    bytes.extend_from_slice(&16u32.to_le_bytes());
    bytes.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
    bytes.extend_from_slice(&channels.to_le_bytes());
    bytes.extend_from_slice(&sample_rate.to_le_bytes());
    bytes.extend_from_slice(&(sample_rate * u32::from(block_align)).to_le_bytes());
    bytes.extend_from_slice(&block_align.to_le_bytes());
    bytes.extend_from_slice(&32u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_len.to_le_bytes());
    for sample in interleaved {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    std::fs::write(path, bytes).expect("write wav fixture");
}

/// `secs` of a sine at `frequency` and `amplitude`, one sample per frame.
pub(crate) fn sine(sample_rate: u32, secs: f64, frequency: f64, amplitude: f64) -> Vec<f32> {
    (0..(secs * f64::from(sample_rate)) as usize)
        .map(|index| {
            let t = index as f64 / f64::from(sample_rate);
            (amplitude * (std::f64::consts::TAU * frequency * t).sin()) as f32
        })
        .collect()
}
