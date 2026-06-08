// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Content hashing for library tracks.
//!
//! A track's identity in the library is its SHA-256 content hash, computed
//! here from the raw file bytes. The streaming variants let the import
//! pipeline hash a file while copying it into the managed library in a single
//! pass, so a large file is read only once.

use super::*;

pub fn hash_file_content(path: &Path) -> MetadataResult<TrackContentHash> {
    let mut file = fs::File::open(path).map_err(|_| MetadataError::ReadFailed)?;
    hash_reader_content(&mut file)
}

pub fn hash_reader_content(reader: &mut impl Read) -> MetadataResult<TrackContentHash> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|_| MetadataError::ReadFailed)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    TrackContentHash::new(lower_hex(&hasher.finalize())).ok_or(MetadataError::ReadFailed)
}

pub fn copy_and_hash_reader_content(
    reader: &mut impl Read,
    writer: &mut impl io::Write,
) -> MetadataResult<(u64, TrackContentHash)> {
    let mut hasher = Sha256::new();
    let mut bytes_copied = 0;
    let mut buffer = vec![0; 64 * 1024];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|_| MetadataError::ReadFailed)?;
        if bytes_read == 0 {
            break;
        }
        writer
            .write_all(&buffer[..bytes_read])
            .map_err(|_| MetadataError::ReadFailed)?;
        hasher.update(&buffer[..bytes_read]);
        bytes_copied += bytes_read as u64;
    }

    let content_hash =
        TrackContentHash::new(lower_hex(&hasher.finalize())).ok_or(MetadataError::ReadFailed)?;
    Ok((bytes_copied, content_hash))
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}
