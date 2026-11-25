// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Flat inputs for the catalogue writer.
//!
//! The caller translates its library into these plain values; the writer
//! never sees a library model, a path on the host, or a transport.

/// The audio container of a track, as the catalogue files it. The
/// catalogue tells formats apart by file extension; the iPod finds the
/// codec in the file itself when it plays it, so AAC and Apple Lossless —
/// both MPEG-4 audio — share one type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileType {
    Mp3,
    Mpeg4,
    Wav,
    Aiff,
}

impl FileType {
    /// The type of a file with this extension (no dot, any case), or `None`
    /// when classic iPods cannot play the format.
    pub fn from_extension(extension: &str) -> Option<Self> {
        match extension.to_ascii_lowercase().as_str() {
            "mp3" => Some(Self::Mp3),
            "m4a" => Some(Self::Mpeg4),
            "wav" => Some(Self::Wav),
            "aif" | "aiff" => Some(Self::Aiff),
            _ => None,
        }
    }

    /// The four-character marker the track record carries: the extension,
    /// upper-cased and space-padded, as libgpod derives it.
    pub(crate) const fn marker(self) -> [u8; 4] {
        match self {
            Self::Mp3 => *b"MP3 ",
            Self::Mpeg4 => *b"M4A ",
            Self::Wav => *b"WAV ",
            Self::Aiff => *b"AIFF",
        }
    }

    /// The human-readable kind, shown by iTunes-style tools.
    pub(crate) const fn kind(self) -> &'static str {
        match self {
            Self::Mp3 => "MPEG audio file",
            Self::Mpeg4 => "MPEG-4 audio file",
            Self::Wav => "WAV audio file",
            Self::Aiff => "AIFF audio file",
        }
    }
}

/// One track of the catalogue. Text fields that are `None` or blank are
/// left out of the record, as libgpod does.
#[derive(Clone, Debug)]
pub struct IpodTrack {
    /// The track's persistent 64-bit id. Nonzero, unique in the library,
    /// and stable across syncs.
    pub dbid: u64,
    /// The audio file, relative to the iPod's root, `/`-separated (e.g.
    /// `iPod_Control/Music/F07/S2K.mp3`).
    pub location: String,
    pub file_type: FileType,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub composer: Option<String>,
    pub comment: Option<String>,
    /// The "sort as" overrides; the browse menus order by these when set.
    pub sort_title: Option<String>,
    pub sort_artist: Option<String>,
    pub sort_album: Option<String>,
    pub sort_album_artist: Option<String>,
    pub sort_composer: Option<String>,
    pub compilation: bool,
    /// 0 (unrated) through 5 stars.
    pub rating_stars: u8,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
    pub disc_total: Option<u32>,
    pub year: Option<u32>,
    pub duration_ms: u32,
    pub size_bytes: u32,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate_hz: u16,
    /// When the track entered the library, as seconds since the Unix epoch.
    pub date_added: Option<i64>,
}

/// One playlist: a name, a persistent id, and its tracks as indices into
/// the library's track list, in order. A track may appear more than once.
#[derive(Clone, Debug)]
pub struct IpodPlaylist {
    /// Nonzero, unique among the library's playlists (the master playlist
    /// included), and stable across syncs.
    pub id: u64,
    pub name: String,
    pub track_indices: Vec<usize>,
}

/// A whole catalogue.
#[derive(Clone, Debug)]
pub struct IpodLibrary {
    /// The library's name, shown by the iPod as the name of its master
    /// playlist — the device's own name.
    pub name: String,
    /// The library's persistent id.
    pub library_id: u64,
    /// The master playlist's persistent id.
    pub master_playlist_id: u64,
    /// Every track, in the order the master playlist lists them.
    pub tracks: Vec<IpodTrack>,
    pub playlists: Vec<IpodPlaylist>,
}
