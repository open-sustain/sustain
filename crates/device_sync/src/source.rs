// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Live source-file observation and stable SHA-256 hashing for device export.

use std::{
    fs::{File, Metadata},
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::Path,
};

use sha2::{Digest, Sha256};
use sustain_domain::{SourceFileStat, SourceFingerprint, TrackContentHash};

pub fn source_file_stat(path: &Path) -> io::Result<SourceFileStat> {
    stat_from_metadata(&std::fs::metadata(path)?)
}

pub fn resolve_source_fingerprint(
    path: &Path,
    cached: Option<&SourceFingerprint>,
) -> io::Result<SourceFingerprint> {
    let stat = source_file_stat(path)?;
    if let Some(cached) = cached
        && cached.stat == stat
    {
        return Ok(cached.clone());
    }

    Ok(SourceFingerprint {
        stat,
        content_hash: hash_source_file(path, &stat)?,
    })
}

pub(crate) fn hash_source_file(
    path: &Path,
    expected: &SourceFileStat,
) -> io::Result<TrackContentHash> {
    let mut file = File::open(path)?;
    ensure_source_unchanged(path, &file, expected)?;

    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    ensure_source_unchanged(path, &file, expected)?;
    TrackContentHash::new(lower_hex(&hasher.finalize()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid SHA-256 digest"))
}

/// Verify a source file still matches the stat that was observed for it,
/// both through the already-opened descriptor and through a fresh lookup of
/// the path, so a file rewritten or swapped during export is caught. The
/// `device_mtp` transport reuses this to guard its streaming copies.
pub fn ensure_source_unchanged(
    path: &Path,
    opened_file: &File,
    expected: &SourceFileStat,
) -> io::Result<()> {
    let opened_stat = stat_from_metadata(&opened_file.metadata()?)?;
    let path_stat = source_file_stat(path)?;
    if &opened_stat == expected && &path_stat == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("source file changed while exporting: {}", path.display()),
        ))
    }
}

fn stat_from_metadata(metadata: &Metadata) -> io::Result<SourceFileStat> {
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source path is not a regular file",
        ));
    }
    Ok(SourceFileStat {
        device: metadata.dev(),
        inode: metadata.ino(),
        size_bytes: metadata.size(),
        modified_at_ns: timestamp_ns(metadata.mtime(), metadata.mtime_nsec())?,
        changed_at_ns: timestamp_ns(metadata.ctime(), metadata.ctime_nsec())?,
    })
}

fn timestamp_ns(seconds: i64, nanoseconds: i64) -> io::Result<i64> {
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanoseconds))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "source timestamp overflow"))
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn reuses_cached_hash_while_stat_matches_then_rehashes_a_same_size_change() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("track.mp3");
        std::fs::write(&path, b"version-one").expect("write v1");

        let first = resolve_source_fingerprint(&path, None).expect("hash v1");

        // Unchanged file: the cached fingerprint is reused verbatim — a stable
        // library must not re-hash every file on every sync.
        let reused = resolve_source_fingerprint(&path, Some(&first)).expect("reuse cache");
        assert_eq!(reused, first);

        // #100: a same-size byte replacement. "version-one" and "version-two"
        // are both 11 bytes, so a size-only token would miss it; the stat
        // tuple catches it via the bumped modification time.
        std::fs::write(&path, b"version-two").expect("write v2");
        force_distinct_mtime(&path, &first.stat);

        let second = resolve_source_fingerprint(&path, Some(&first)).expect("hash v2");
        assert_eq!(
            second.stat.size_bytes, first.stat.size_bytes,
            "the replacement is the same size"
        );
        assert_ne!(
            second.content_hash, first.content_hash,
            "the changed content is re-hashed"
        );
    }

    /// Set a modification time that cannot coincide with `previous`, so the
    /// re-hash assertion is deterministic regardless of the filesystem's
    /// timestamp granularity.
    fn force_distinct_mtime(path: &Path, previous: &SourceFileStat) {
        let bumped = UNIX_EPOCH
            + Duration::from_nanos(previous.modified_at_ns.unsigned_abs())
            + Duration::from_secs(10);
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for set_modified");
        file.set_modified(bumped).expect("set modification time");
    }
}
