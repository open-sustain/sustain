// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! What the iTunesDB and the ArtworkDB share: records that start with a
//! four-byte tag, a header length at `+4` and a total length at `+8`, all
//! little-endian, and the error either writer can fail with.

/// Why a catalogue — the `iTunesDB` or the `ArtworkDB` — could not be
/// written.
#[derive(Debug, Eq, PartialEq)]
pub enum WriteError {
    /// The catalogue would exceed the 4 GiB its 32-bit lengths can address.
    TooLarge,
    /// A track's persistent id is zero or not unique.
    InvalidTrackId { dbid: u64 },
    /// A playlist's persistent id is zero or not unique.
    InvalidPlaylistId { id: u64 },
    /// A playlist names a track the library does not hold.
    UnknownTrack { playlist: String, index: usize },
    /// The device needs a hash58 signature and no GUID was given.
    MissingFirewireGuid,
    /// A track's cover lies outside the cover set the artwork is written
    /// from.
    UnknownCover { track: usize },
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge => write!(f, "the iPod catalogue would exceed 4 GiB"),
            Self::InvalidTrackId { dbid } => {
                write!(f, "track id {dbid:#x} is zero or used twice")
            }
            Self::InvalidPlaylistId { id } => {
                write!(f, "playlist id {id:#x} is zero or used twice")
            }
            Self::UnknownTrack { playlist, index } => {
                write!(
                    f,
                    "playlist {playlist:?} names track {index}, which is not in the library"
                )
            }
            Self::UnknownCover { track } => {
                write!(f, "track {track}'s cover is not in the iPod's cover set")
            }
            Self::MissingFirewireGuid => {
                write!(
                    f,
                    "the iPod's FireWire GUID is needed to sign its catalogue"
                )
            }
        }
    }
}

impl std::error::Error for WriteError {}

/// The output buffer. A record's total length is known only once its
/// children are written: [`Out::open_record`] remembers where it starts and
/// [`Out::close_record`] fills the length in.
#[derive(Default)]
pub(crate) struct Out {
    pub(crate) bytes: Vec<u8>,
}

/// Where an open record starts.
#[derive(Clone, Copy)]
pub(crate) struct Open(usize);

impl Out {
    pub(crate) fn open_record(&mut self, header: &[u8]) -> Open {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(header);
        Open(start)
    }

    /// Fill in the total length, at `+8`, of the record opened at `open`.
    pub(crate) fn close_record(&mut self, open: Open) -> Result<(), WriteError> {
        let length = u32_len(self.bytes.len() - open.0)?;
        self.bytes[open.0 + 8..open.0 + 12].copy_from_slice(&length.to_le_bytes());
        Ok(())
    }

    /// Set a 32-bit field of the record opened at `open`.
    pub(crate) fn set_u32(&mut self, open: Open, offset: usize, value: u32) {
        self.bytes[open.0 + offset..open.0 + offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    pub(crate) fn extend(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }
}

pub(crate) fn u32_len(length: usize) -> Result<u32, WriteError> {
    u32::try_from(length).map_err(|_| WriteError::TooLarge)
}

/// A record header of `length` bytes with its tag and header length set.
pub(crate) struct Header(pub(crate) Vec<u8>);

impl Header {
    pub(crate) fn new(tag: &[u8; 4], length: u32) -> Self {
        let mut bytes = vec![0u8; length as usize];
        bytes[..4].copy_from_slice(tag);
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        Self(bytes)
    }

    pub(crate) fn u8(&mut self, offset: usize, value: u8) -> &mut Self {
        self.0[offset] = value;
        self
    }

    pub(crate) fn u16(&mut self, offset: usize, value: u16) -> &mut Self {
        self.0[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        self
    }

    pub(crate) fn u32(&mut self, offset: usize, value: u32) -> &mut Self {
        self.0[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        self
    }

    pub(crate) fn u64(&mut self, offset: usize, value: u64) -> &mut Self {
        self.0[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        self
    }

    pub(crate) fn bytes(&mut self, offset: usize, value: &[u8]) -> &mut Self {
        self.0[offset..offset + value.len()].copy_from_slice(value);
        self
    }
}
