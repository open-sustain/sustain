// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Symphonia-based audio decoder. Two entry points: a full decode for
//! whole-track work (the waveform on normal-length tracks) and a
//! *windowed* decode that seeks to an offset and collects a bounded
//! span — used to pull a centered analysis window without decoding the
//! material on either side of it. Both produce interleaved-collapsed
//! mono `f32` samples plus the original sample rate.
//!
//! The windowed path seeks rather than decode-and-discards: reaching
//! the centre of a two-hour file by discarding the first ~56 minutes
//! would defeat the entire point of windowing. When the container
//! cannot seek (or reports no seek support), it falls back to decoding
//! from the start and dropping the lead-in — bounded in memory (the
//! skipped samples are never retained), only costly in CPU, and only on
//! the handful of formats that cannot seek. All formats Sustain imports
//! (FLAC, MP3, M4A/AAC, Ogg/Vorbis, WAV) seek.
//!
//! The decoder is loaded by symphonia's default registry; only the
//! codecs enabled in `Cargo.toml`'s `symphonia` features are actually
//! linked. Unsupported codecs surface as `AnalysisError::DecoderError`.

use std::path::Path;

use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::AnalysisError;

/// One contiguous block of mono `f32` samples plus the rate they were
/// captured at. Multi-channel sources are averaged down to mono so
/// downstream DSP only ever sees one stream.
pub(crate) struct DecodedAudio {
    pub(crate) samples: Vec<f32>,
    pub(crate) sample_rate: u32,
}

/// Which span of the track to decode.
#[derive(Clone, Copy)]
enum Region {
    /// The whole track, start to finish.
    Full,
    /// `len_secs` of audio starting `start_secs` into the track. The
    /// decoder seeks to `start_secs`; collection stops after `len_secs`
    /// of mono samples (or at end-of-stream, whichever comes first).
    Window { start_secs: f64, len_secs: f64 },
}

/// Decode the entire audio file to mono `f32`. Used by the waveform
/// pass on normal-length tracks (and acoustics, which reuses the same
/// decode) so the result reflects the whole track.
pub(crate) fn decode_full(path: &Path) -> Result<DecodedAudio, AnalysisError> {
    decode_region(path, Region::Full)
}

/// Decode `len_secs` of audio starting `start_secs` into the track,
/// seeking to the offset rather than decoding the lead-in. Used to pull
/// a centered analysis window (BPM/key, and acoustics on long tracks)
/// without paying for the material outside it.
pub(crate) fn decode_window(
    path: &Path,
    start_secs: f64,
    len_secs: f64,
) -> Result<DecodedAudio, AnalysisError> {
    decode_region(
        path,
        Region::Window {
            start_secs: start_secs.max(0.0),
            len_secs: len_secs.max(0.0),
        },
    )
}

fn decode_region(path: &Path, region: Region) -> Result<DecodedAudio, AnalysisError> {
    let path_label = path.display().to_string();

    let file = std::fs::File::open(path).map_err(|source| AnalysisError::OpenFailed {
        path: path_label.clone(),
        source,
    })?;
    let media_source = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(extension) = path.extension().and_then(|ext| ext.to_str()) {
        hint.with_extension(extension);
    }

    // 0.6's probe returns the `FormatReader` directly (no `probed.format`
    // wrapper) and takes the format/metadata options by value.
    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            media_source,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|err| AnalysisError::DecoderError {
            path: path_label.clone(),
            message: format!("probe failed: {err}"),
        })?;

    // 0.6 moved non-codec track info out of codec parameters and made
    // `Track::codec_params` optional (`None` for unknown/non-audio
    // tracks), so the old `CODEC_TYPE_NULL` scan becomes "the default
    // audio track, if it carries audio codec parameters". The Copy values
    // and the decoder are pulled out in this block so the immutable borrow
    // of `format` ends before the seek / decode loop borrows it mutably.
    let (track_id, sample_rate, mut decoder) = {
        let track =
            format
                .default_track(TrackType::Audio)
                .ok_or_else(|| AnalysisError::NoAudioTrack {
                    path: path_label.clone(),
                })?;
        let track_id = track.id;
        let codec_params = track
            .codec_params
            .as_ref()
            .and_then(|params| params.audio())
            .ok_or_else(|| AnalysisError::NoAudioTrack {
                path: path_label.clone(),
            })?;
        let sample_rate = codec_params
            .sample_rate
            .ok_or_else(|| AnalysisError::DecoderError {
                path: path_label.clone(),
                message: "no sample rate reported by decoder".to_string(),
            })?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(codec_params, &AudioDecoderOptions::default())
            .map_err(|err| AnalysisError::DecoderError {
                path: path_label.clone(),
                message: format!("decoder factory failed: {err}"),
            })?;
        (track_id, sample_rate, decoder)
    };

    // Plan the collection bounds for this region. `skip_samples` is the
    // mono-sample lead-in to discard when we could not seek; `max_samples`
    // caps how many mono samples we keep.
    let (skip_samples, max_samples) = match region {
        Region::Full => (0_usize, None),
        Region::Window {
            start_secs,
            len_secs,
        } => {
            let len = (len_secs * sample_rate as f64) as usize;
            let max = Some(len.max(1));
            if start_secs <= 0.0 {
                (0, max)
            } else if seek_to(&mut *format, &mut *decoder, track_id, start_secs) {
                // Seek landed; decode forward from there with no lead-in
                // to drop.
                (0, max)
            } else {
                // Seek unsupported: decode from the top and drop the
                // lead-in as mono samples stream in.
                ((start_secs * sample_rate as f64) as usize, max)
            }
        }
    };

    let samples = collect_samples(
        &mut *format,
        &mut *decoder,
        track_id,
        skip_samples,
        max_samples,
        &path_label,
    )?;

    Ok(DecodedAudio {
        samples,
        sample_rate,
    })
}

/// Best-effort accurate seek to `start_secs`. Returns `true` when the
/// container seeked, `false` when seeking is unsupported (the caller
/// falls back to decode-and-discard). A successful seek is followed by
/// a decoder reset, as symphonia requires after a discontinuity.
fn seek_to(
    format: &mut dyn FormatReader,
    decoder: &mut dyn AudioDecoder,
    track_id: u32,
    start_secs: f64,
) -> bool {
    // 0.6's `Time` is a seconds/nanoseconds pair built from a float total;
    // a non-finite offset has no representation, so treat that as "cannot
    // seek" and let the caller fall back to decode-and-discard.
    let Some(time) = Time::try_from_secs_f64(start_secs) else {
        return false;
    };
    let seeked = format
        .seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(track_id),
            },
        )
        .is_ok();
    if seeked {
        decoder.reset();
    }
    seeked
}

/// Drive the decode loop, collapsing every packet of `track_id` to mono,
/// dropping the first `skip_samples` mono samples, and stopping once
/// `max_samples` have been retained (or at end-of-stream).
fn collect_samples(
    format: &mut dyn FormatReader,
    decoder: &mut dyn AudioDecoder,
    track_id: u32,
    mut skip_samples: usize,
    max_samples: Option<usize>,
    path_label: &str,
) -> Result<Vec<f32>, AnalysisError> {
    let mut samples: Vec<f32> = Vec::new();
    // Reused across packets so the interleaved copy does not reallocate
    // every iteration.
    let mut interleaved: Vec<f32> = Vec::new();

    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            // 0.6 signals end-of-media as `Ok(None)`; an IO `UnexpectedEof`
            // is now a genuine error rather than the terminator.
            Ok(None) => break,
            Err(err) => {
                return Err(AnalysisError::DecoderError {
                    path: path_label.to_string(),
                    message: format!("read packet: {err}"),
                });
            }
        };

        if packet.track_id != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(buffer) => buffer,
            // A bad packet should not abort the whole decode; symphonia
            // recommends skipping and continuing. Real-world files
            // occasionally have a corrupted frame in the middle, and the
            // first packet after a seek can also surface as a recoverable
            // decode error.
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => {
                return Err(AnalysisError::DecoderError {
                    path: path_label.to_string(),
                    message: format!("decode packet: {err}"),
                });
            }
        };

        // 0.6 removed `SampleBuffer`; copy the decoded audio into an
        // interleaved `f32` scratch buffer via the audio-buffer API.
        // `num_planes` is the channel count for interleaved audio.
        let channel_count = decoded.num_planes();
        interleaved.resize(decoded.samples_interleaved(), 0.0);
        decoded.copy_to_slice_interleaved(&mut interleaved);

        // Collapse to mono on the fly, honoring the lead-in skip and the
        // collection cap so a windowed decode never materializes more
        // than the window it was asked for.
        if channel_count <= 1 {
            for &sample in &interleaved {
                if skip_samples > 0 {
                    skip_samples -= 1;
                    continue;
                }
                samples.push(sample);
                if max_samples.is_some_and(|cap| samples.len() >= cap) {
                    return Ok(samples);
                }
            }
        } else {
            // Average across channels for mono. Sum-then-divide on f32
            // is fine; if any one channel saturates we accept the
            // attenuation.
            let inverse = 1.0_f32 / channel_count as f32;
            for frame in interleaved.chunks_exact(channel_count) {
                if skip_samples > 0 {
                    skip_samples -= 1;
                    continue;
                }
                samples.push(frame.iter().sum::<f32>() * inverse);
                if max_samples.is_some_and(|cap| samples.len() >= cap) {
                    return Ok(samples);
                }
            }
        }
    }

    Ok(samples)
}

#[cfg(test)]
mod tests {
    use super::{decode_full, decode_window};
    use tempfile::TempDir;

    /// Write a mono 16-bit PCM WAV whose samples are a linear ramp from
    /// −1 to +1, so each sample's value uniquely encodes its frame index
    /// (`value ≈ index / (frames − 1) · 2 − 1`). That lets a decode test
    /// recover *where* in the track a returned sample came from and so
    /// assert that a seek landed at the requested offset.
    fn write_ramp_wav(path: &std::path::Path, sample_rate: u32, frames: u32) {
        let data_len = frames * 2; // 16-bit mono
        let mut bytes = Vec::with_capacity(44 + data_len as usize);
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(b"fmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes()); // PCM fmt chunk size
        bytes.extend_from_slice(&1u16.to_le_bytes()); // PCM
        bytes.extend_from_slice(&1u16.to_le_bytes()); // mono
        bytes.extend_from_slice(&sample_rate.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
        bytes.extend_from_slice(&2u16.to_le_bytes()); // block align
        bytes.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for i in 0..frames {
            let normalized = (i as f64 / (frames - 1) as f64) * 2.0 - 1.0;
            let sample = (normalized * 32767.0) as i16;
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        std::fs::write(path, bytes).expect("write wav fixture");
    }

    /// Recover a sample's original frame index from its ramp value.
    fn recovered_index(value: f32, frames: u32) -> f64 {
        ((value as f64 + 1.0) / 2.0) * (frames - 1) as f64
    }

    #[test]
    fn decode_full_reads_the_whole_track() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("ramp.wav");
        let sample_rate = 8_000;
        let frames = sample_rate * 4; // 4 s
        write_ramp_wav(&path, sample_rate, frames);

        let audio = decode_full(&path).expect("decode full");
        assert_eq!(audio.sample_rate, sample_rate);
        assert_eq!(audio.samples.len(), frames as usize);
    }

    #[test]
    fn decode_window_seeks_to_the_requested_offset() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("ramp.wav");
        let sample_rate = 8_000;
        let frames = sample_rate * 4; // 4 s
        write_ramp_wav(&path, sample_rate, frames);

        // Pull 1 s starting 2 s in — the centre of the file.
        let audio = decode_window(&path, 2.0, 1.0).expect("decode window");
        assert_eq!(audio.sample_rate, sample_rate);
        // Roughly one second of samples (seek lands on a frame boundary).
        let collected = audio.samples.len() as i64;
        assert!(
            (collected - sample_rate as i64).abs() < 256,
            "expected ~{sample_rate} samples, got {collected}"
        );
        // The first returned sample must come from ~frame 16_000 (2 s in),
        // proving the seek positioned the read rather than starting at 0.
        // A container seek lands on the packet boundary containing the
        // timestamp (WAV packs 1024 frames per packet), so allow a packet
        // of slack — that 0.13 s is negligible against a 120 s window.
        let first_index = recovered_index(audio.samples[0], frames);
        assert!(
            (first_index - (2.0 * sample_rate as f64)).abs() < 2_048.0,
            "seek landed at frame {first_index}, expected ~{}",
            2.0 * sample_rate as f64
        );
    }

    #[test]
    fn decode_window_from_zero_starts_at_the_top() {
        let dir = TempDir::new().expect("temp dir");
        let path = dir.path().join("ramp.wav");
        let sample_rate = 8_000;
        let frames = sample_rate * 4;
        write_ramp_wav(&path, sample_rate, frames);

        // start_secs == 0 takes the no-seek fast path; the first sample is
        // frame 0 and we collect the requested span.
        let audio = decode_window(&path, 0.0, 1.0).expect("decode window");
        assert!(recovered_index(audio.samples[0], frames) < 8.0);
        let collected = audio.samples.len() as i64;
        assert!((collected - sample_rate as i64).abs() < 256);
    }
}
