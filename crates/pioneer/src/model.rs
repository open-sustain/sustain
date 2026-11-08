// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Neutral input types for the Pioneer writers.
//!
//! Callers (the device-sync layer) translate Sustain's `Track` and
//! playlist models into these flat structs, so the format code never
//! reaches back into the library, the database, or the DSP pipeline.

use std::time::Duration;

use sustain_domain::{BeatGrid, Bpm, MusicalKey, WaveformSegments};

/// Pioneer's file-type discriminant, written into each track row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum PioneerFileType {
    Unknown = 0x00,
    Mp3 = 0x01,
    M4a = 0x04,
    Flac = 0x05,
    Wav = 0x0B,
    Aiff = 0x0C,
}

impl PioneerFileType {
    /// Classify from a file extension (case-insensitive, no dot).
    pub fn from_extension(extension: &str) -> Self {
        match extension.to_ascii_lowercase().as_str() {
            "mp3" => Self::Mp3,
            "m4a" | "mp4" | "aac" => Self::M4a,
            "flac" => Self::Flac,
            "wav" => Self::Wav,
            "aiff" | "aif" => Self::Aiff,
            _ => Self::Unknown,
        }
    }
}

/// One track to write into the PDB. Strings are the final values; paths
/// are relative to the drive root and start with a leading slash. An
/// empty artist, album, genre, or composer is written as "unset" (row id
/// 0), exactly as rekordbox does, never as a nameless entity row.
#[derive(Clone, Debug)]
pub struct PioneerTrack {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: Option<String>,
    /// Composer, stored (as rekordbox does) as a row of the artist table.
    pub composer: Option<String>,
    /// Free-text comment. Truncated to rekordbox's limit by the writer.
    pub comment: Option<String>,
    pub bpm: Option<Bpm>,
    pub key: Option<MusicalKey>,
    /// Track length in whole seconds.
    pub duration_secs: u32,
    /// Size of the audio file in bytes.
    pub file_size: u64,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<u32>,
    /// 0 (unrated) through 5.
    pub rating: u8,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: u32,
    pub bit_depth: u16,
    pub file_type: PioneerFileType,
    /// Artwork-table id linking this track to its cover thumbnails, or
    /// `0` for no artwork. Assigned by [`crate::artwork::ArtworkSet`].
    pub artwork_id: u32,
    /// `YYYY-MM-DD` the track entered the library, or `None` to leave the
    /// PDB's date-added string empty. Cosmetic (rekordbox's "date added"
    /// column); the format does not depend on it.
    pub date_added: Option<String>,
    /// On-drive audio path, e.g. `/Contents/Artist/Album/01 Title.mp3`.
    pub device_audio_path: String,
    /// On-drive analysis path (the `.DAT` file), stored in the PDB's
    /// `analyze_path`. The hardware ignores it and recomputes the path
    /// itself, but rekordbox writes it so we mirror that.
    pub device_anlz_path: String,
}

/// One playlist to write into the PDB. `entries` are indices into the
/// track slice handed to the writer, in playlist order.
#[derive(Clone, Debug)]
pub struct PioneerPlaylist {
    pub name: String,
    pub entries: Vec<usize>,
}

/// One row of the PDB artwork table: the id a track's `artwork_id`
/// references and the on-drive path to its (small) JPEG thumbnail. The
/// thumbnail files themselves are produced and written by
/// [`crate::artwork::ArtworkSet`].
#[derive(Clone, Debug)]
pub struct PioneerArtwork {
    pub id: u32,
    /// On-drive path of the 80×80 thumbnail, e.g.
    /// `/PIONEER/Artwork/00001/a1.jpg`.
    pub path: String,
}

/// Inputs the ANLZ serializer needs for one track. The waveform tiers
/// and the beat grid are Sustain's own, on Sustain's gapless timeline;
/// the serializer moves them onto the player's timeline
/// (`timeline_delay`) and repacks them into Pioneer's PQTZ and
/// PWAV/PWV2/PWV3/PWV4/PWV5 encodings without touching audio.
#[derive(Clone, Copy, Debug)]
pub struct AnlzInput<'a> {
    /// On-drive audio path, used for the PPTH section.
    pub device_audio_path: &'a str,
    /// The track's beats. `None` writes an empty grid: the player shows
    /// no beat grid rather than a wrong one.
    pub beat_grid: Option<BeatGrid>,
    /// How far the player's timeline runs behind Sustain's for this file
    /// ([`crate::timeline::device_delay`]).
    pub timeline_delay: Duration,
    pub duration_ms: u32,
    pub waveform_preview: &'a WaveformSegments,
    pub waveform_detail: &'a WaveformSegments,
}
