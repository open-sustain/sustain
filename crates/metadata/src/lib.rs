// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use lofty::{
    config::{GlobalOptions, ParseOptions, apply_global_options},
    picture::{Picture, PictureType},
};
use sha2::{Digest, Sha256};
use sustain_artwork::{ArtworkPolicyError, MAX_ENCODED_ARTWORK_BYTES, validate_encoded_artwork};

pub use sustain_domain::{
    Bpm, FieldChange, KeyOrigin, MetadataChange, MusicalKey, Rating, TrackContentHash,
    TrackMetadata, TrackRelativePath,
};

mod atomic_file;
use atomic_file::{SourceVersion, atomic_write_via_rename};
mod hash;
mod mpeg_frame;
mod native;
mod read;
mod scan;
mod tag_write;
pub mod walk;

pub use hash::{copy_and_hash_reader_content, hash_file_content, hash_reader_content};
pub use scan::{
    KnownTracks, LibraryScanner, SUSTAIN_WORKSPACE_NAME_PREFIX, is_sustain_workspace_entry,
};

pub type MetadataResult<T> = Result<T, MetadataError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFormat {
    Mp3,
    Ogg,
    Flac,
    Mp4,
    Wav,
    Aiff,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataError {
    UnsupportedAudioFormat,
    ArtworkRejected(ArtworkPolicyError),
    ContainerFormatMismatch {
        expected: AudioFormat,
        detected: AudioFormat,
    },
    MalformedTag(MalformedTagError),
    WriteFailed,
    /// The file changed after Sustain read it, so a write prepared from
    /// what was read was not made: it would have undone the change.
    /// Reading the file again and redoing the write makes it.
    SourceChanged,
    /// The file opens with an ID3v2 tag carrying an extended header, which
    /// lofty's ID3v2 writer miscounts: a save would overwrite the start of
    /// the audio (#376). The write is refused and the file left as it is.
    Id3v2ExtendedHeader,
    ReadFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MalformedTagError {
    BadTimestamp,
    TextDecode,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataRepair {
    MalformedTag(MalformedTagError),
}

pub trait MetadataService: Send + Sync {
    /// Reads, in a single parse, the tag-derived values Sustain
    /// captures the first time a file enters the library: its
    /// editable metadata, star rating, and whether it carries an
    /// embedded cover.
    ///
    /// Both the library scan and the managed-library import call this
    /// to seed a brand-new track. Reading the three together is the
    /// point — answering each with its own file open would parse
    /// every track three times per scan. Per Sustain's persistence
    /// policy these are *initial* values only: once a track has a
    /// library row, SQLite is authoritative and the file's tags are
    /// never again consulted to override it.
    ///
    /// The returned title is backfilled from the filename stem when
    /// the tag carries none, so callers receive a display-ready
    /// [`TrackMetadata`].
    fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags>;

    fn read_initial_tags_with_diagnostics(&self, path: &Path) -> MetadataResult<InitialTagRead> {
        self.read_initial_tags(path).map(InitialTagRead::clean)
    }

    /// Reads the persisted tag values exactly as written, without the
    /// filename-title fallback used during import. The duplicate
    /// consolidation verifier needs this distinction: it verifies a staged
    /// rewrite before publishing that file into the library namespace.
    ///
    /// Test doubles predating staged verification may rely on the default;
    /// the production implementation overrides it with an exact read.
    fn read_persisted_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        self.read_initial_tags(path)
    }

    fn write_metadata(&self, path: &Path, change: MetadataChange) -> MetadataResult<()>;

    /// `change` as the file at `path` can hold it: writing `change` there
    /// leaves the file's tags reading as they did with this change
    /// applied. A value the file's tag format cannot hold (a fractional
    /// tempo in ID3, a disc number past 65535 in MP4) is carried as the
    /// nearest value it holds, or cleared when it holds none.
    ///
    /// Test doubles that store values exactly may rely on the default;
    /// the production implementation overrides it with the format's own
    /// carriage.
    fn carried_change(
        &self,
        _path: &Path,
        change: MetadataChange,
    ) -> MetadataResult<MetadataChange> {
        Ok(change)
    }

    fn write_rating(&self, path: &Path, rating: Rating) -> MetadataResult<()>;
    fn read_artwork(&self, path: &Path) -> MetadataResult<Option<Vec<u8>>>;
    fn write_artwork(&self, path: &Path, artwork: Option<Vec<u8>>) -> MetadataResult<()>;
    fn repair_malformed_tags(&self, _path: &Path, _repair: MetadataRepair) -> MetadataResult<bool> {
        Ok(false)
    }

    /// Repairs, in place, every malformation `read` diagnoses for `path`,
    /// re-reading after each repair. One malformation can hide another
    /// behind it (a bad date, then undecodable text), so each kind is
    /// repaired as it surfaces — once: a kind that survives its own repair
    /// stays in the returned read for a later attempt.
    ///
    /// The first failure ends the pass. Its read is the last one that
    /// succeeded, which no longer describes the file when a repair changed
    /// it and the following re-read failed; `file_changed` tells callers
    /// to re-stat or re-hash either way.
    fn repair_diagnosed_malformations(&self, path: &Path, read: InitialTagRead) -> RepairPass {
        let mut pass = RepairPass {
            read,
            file_changed: false,
            error: None,
        };
        let mut attempted = Vec::new();
        while let Some(repair) = pass.read.repair
            && !attempted.contains(&repair)
        {
            attempted.push(repair);
            match self.repair_malformed_tags(path, repair) {
                Ok(true) => pass.file_changed = true,
                Ok(false) => break,
                // Another tool changed the file since it was read: what it
                // holds now is read again, and repaired if it still needs
                // another repair.
                Err(MetadataError::SourceChanged) => pass.file_changed = true,
                Err(error) => {
                    // A failure after the atomic exchange (syncing the
                    // directory) leaves the rewritten file in place.
                    pass.file_changed = true;
                    pass.error = Some(error);
                    break;
                }
            }
            match self.read_initial_tags_with_diagnostics(path) {
                Ok(read) => pass.read = read,
                Err(error) => {
                    pass.error = Some(error);
                    break;
                }
            }
        }
        pass
    }
}

/// The outcome of [`MetadataService::repair_diagnosed_malformations`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepairPass {
    /// The last successful read; `read.repair` is the malformation still
    /// present, if any.
    pub read: InitialTagRead,
    /// True when a repair rewrote the file, or failed at a point where it
    /// may have.
    pub file_changed: bool,
    /// The repair or re-read failure that ended the pass early.
    pub error: Option<MetadataError>,
}

/// The tag-derived values captured the first time a file enters the
/// library — its editable metadata, star rating, and whether it
/// carries embedded artwork — read together by
/// [`MetadataService::read_initial_tags`] in a single parse.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitialTags {
    pub metadata: TrackMetadata,
    pub rating: Rating,
    /// True when the file's tag carried at least one embedded picture
    /// (any `PictureType`). Captured here so the online artwork
    /// retriever can filter candidates with a SQL predicate instead
    /// of re-probing every file on every cycle.
    pub has_embedded_artwork: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InitialTagRead {
    pub tags: InitialTags,
    pub repair: Option<MetadataRepair>,
}

impl InitialTagRead {
    pub fn clean(tags: InitialTags) -> Self {
        Self { tags, repair: None }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScannedTrack {
    pub relative_path: TrackRelativePath,
    pub metadata: TrackMetadata,
    pub rating: Rating,
    pub file_size_bytes: Option<u64>,
    /// True when the file's tag carried at least one embedded picture
    /// (any `PictureType`). Captured at scan time so the online
    /// artwork retriever can filter candidates with a SQL predicate
    /// instead of re-probing every file on every cycle.
    pub has_embedded_artwork: bool,
    /// The file's last-modified time read from the same `stat` that
    /// produced `file_size_bytes`. Persisted (truncated to seconds) so
    /// the next scan can fingerprint this file and skip re-parsing it
    /// when nothing changed (#71). `None` when the platform could not
    /// report an mtime, which simply forces a parse on the next scan.
    pub file_modified_at: Option<SystemTime>,
}

/// The size + last-modified fingerprint recorded for a track at its
/// previous scan. When a freshly stat'd file still matches the stored
/// fingerprint, none of the file-derived values a rescan consumes for an
/// already-imported track (audio-stream properties, file size, embedded-
/// artwork bit) can have changed, so the scanner skips re-parsing it
/// (#71).
///
/// The comparison is at one-second resolution because that is what the
/// library persists (`file_modified_at_unix`, matching the other
/// `*_at_unix` columns). [`ScanFingerprint::new`] is the single place that
/// truncation happens, so both the live file and the stored row run
/// through the identical conversion and compare apples to apples.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanFingerprint {
    pub file_size_bytes: u64,
    pub modified_at_unix: i64,
}

impl ScanFingerprint {
    /// Builds a fingerprint, truncating `modified_at` to whole Unix
    /// seconds the same way the store's `system_time_to_unix` does.
    /// Returns `None` for a pre-epoch mtime (which cannot be represented
    /// in the persisted column and so always re-parses).
    pub fn new(file_size_bytes: u64, modified_at: SystemTime) -> Option<Self> {
        let modified_at_unix = modified_at.duration_since(UNIX_EPOCH).ok()?.as_secs() as i64;
        Some(Self {
            file_size_bytes,
            modified_at_unix,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanFailure {
    pub path: PathBuf,
    pub error: MetadataError,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryScan {
    pub tracks: Vec<ScannedTrack>,
    /// Files found present on disk whose size + mtime fingerprint matched
    /// the stored one, so the scanner skipped re-parsing their tags
    /// (#71). Reconciliation keeps each such row exactly as the library
    /// already had it (only forcing availability back to Available) and
    /// counts it present, never missing.
    pub unchanged: Vec<TrackRelativePath>,
    pub skipped_unsupported_files: usize,
    pub failures: Vec<ScanFailure>,
    /// True only when every directory entry and supported file could be
    /// inspected. Callers may create new Missing markers only after a scan
    /// carrying this guarantee.
    pub complete_for_missing_reconciliation: bool,
    // True when the scanner stopped because the cancellation flag was
    // observed mid-walk. Callers must not interpret an unwalked
    // subtree as "tracks missing from disk" — partial scans only ever
    // produce additions/updates, never missing markers.
    pub cancelled: bool,
}

impl Default for LibraryScan {
    fn default() -> Self {
        Self {
            tracks: Vec::new(),
            unchanged: Vec::new(),
            skipped_unsupported_files: 0,
            failures: Vec::new(),
            complete_for_missing_reconciliation: true,
            cancelled: false,
        }
    }
}

impl LibraryScan {
    fn record_failure(&mut self, path: PathBuf, error: MetadataError) {
        self.complete_for_missing_reconciliation = false;
        self.failures.push(ScanFailure { path, error });
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LibraryScanError {
    LibraryPathUnavailable,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LoftyMetadataService;

impl MetadataService for LoftyMetadataService {
    fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        read::read_initial_tags(path).map(|read| read.tags)
    }

    fn read_initial_tags_with_diagnostics(&self, path: &Path) -> MetadataResult<InitialTagRead> {
        read::read_initial_tags(path)
    }

    fn read_persisted_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        read::read_persisted_tags(path)
    }

    fn write_metadata(&self, path: &Path, change: MetadataChange) -> MetadataResult<()> {
        audio_format_from_path(path)?;
        let parsed = native::parse(path, ParseOptions::new())?;
        let read = parsed.version();
        let mut tag = parsed.into_editable_tag()?;
        if tag.apply_change(&change) {
            tag.save(path, read)?;
        }
        Ok(())
    }

    fn carried_change(
        &self,
        path: &Path,
        change: MetadataChange,
    ) -> MetadataResult<MetadataChange> {
        audio_format_from_path(path)?;
        let parsed = native::parse(path, ParseOptions::new().read_properties(false))?;
        Ok(parsed.family().carriage().carried_change(change))
    }

    fn write_rating(&self, path: &Path, rating: Rating) -> MetadataResult<()> {
        audio_format_from_path(path)?;
        let parsed = native::parse(path, ParseOptions::new())?;
        let read = parsed.version();
        let mut tag = parsed.into_editable_tag()?;
        if tag.set_rating(rating) {
            tag.save(path, read)?;
        }
        Ok(())
    }

    fn read_artwork(&self, path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        read::read_cover(path)
    }

    fn write_artwork(&self, path: &Path, artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        if let Some(bytes) = artwork.as_deref() {
            validate_encoded_artwork(bytes).map_err(MetadataError::ArtworkRejected)?;
        }
        audio_format_from_path(path)?;
        let cover = artwork
            .map(|bytes| {
                let mut picture = Picture::from_reader(&mut io::Cursor::new(bytes))
                    .map_err(|_| MetadataError::WriteFailed)?;
                picture.set_pic_type(PictureType::CoverFront);
                Ok(picture)
            })
            .transpose()?;
        let parsed = native::parse(path, ParseOptions::new())?;
        let read = parsed.version();
        let mut tag = parsed.into_editable_tag()?;
        if tag.set_cover(cover)? {
            tag.save(path, read)?;
        }
        Ok(())
    }

    fn repair_malformed_tags(&self, path: &Path, repair: MetadataRepair) -> MetadataResult<bool> {
        tag_write::repair_malformed_tag_read_error(path, repair)
    }
}

/// Whether `name` is a supported audio name for the file whose own path is
/// `file` — the same path, unless `name` goes through a link. A link is a
/// name for a file only if it carries the file's own extension, so reading
/// a track through its name reads the file as the file's own name would,
/// and whatever is done to the track may be done to the file itself.
pub fn is_audio_name_for(name: &Path, file: &Path) -> bool {
    let extension = |path: &Path| {
        path.extension()
            .map(|extension| extension.to_ascii_lowercase())
    };
    audio_format_from_path(name).is_ok() && extension(name) == extension(file)
}

/// The file a track's path names: the path itself, or the file a link
/// there leads to, if that is a regular file the path is an audio name for
/// ([`is_audio_name_for`]). `None` if the path names no such file, which
/// is what a track whose link was turned to another kind of file is left
/// with; an error if the path could not be looked up.
pub fn resolve_track_file(path: &Path) -> io::Result<Option<PathBuf>> {
    let found = fs::canonicalize(path)
        .and_then(|own_path| fs::metadata(&own_path).map(|metadata| (own_path, metadata)));
    match found {
        Ok((own_path, metadata)) => {
            Ok((metadata.is_file() && is_audio_name_for(path, &own_path)).then_some(own_path))
        }
        Err(error) if names_nothing(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Whether looking up a path failed because it names nothing: there is no
/// entry by its name, one of the folders on its way is not a folder, or
/// its links lead round in a loop.
pub(crate) fn names_nothing(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) || rustix::io::Errno::from_io_error(error) == Some(rustix::io::Errno::LOOP)
}

pub fn audio_format_from_path(path: &Path) -> MetadataResult<AudioFormat> {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("mp3") => Ok(AudioFormat::Mp3),
        Some("ogg") | Some("oga") | Some("opus") => Ok(AudioFormat::Ogg),
        Some("flac") => Ok(AudioFormat::Flac),
        Some("m4a") | Some("m4b") | Some("mp4") => Ok(AudioFormat::Mp4),
        // WAV carries no native tag chunk, but lofty's primary tag for
        // WAV is ID3v2 (the same frames as MP3), so once a track is in
        // the library, edits — including POPM ratings, USLT lyrics, and
        // APIC artwork — mirror back through the identical write path.
        Some("wav") => Ok(AudioFormat::Wav),
        // AIFF has native text chunks, but lofty's primary tag for AIFF
        // is ID3v2 in an `ID3 ` chunk — the same tag WAV carries — so
        // edits follow the WAV path and existing text chunks stay
        // untouched. AIFF-C (`.aifc`) shares the container and the tag.
        Some("aif") | Some("aiff") | Some("aifc") => Ok(AudioFormat::Aiff),
        _ => Err(MetadataError::UnsupportedAudioFormat),
    }
}

/// Whether `error`, or an error it reports as its cause, is an `E`. lofty
/// reports a failure as the operation that failed (parsing or encoding a
/// file) and carries why in the error's source chain.
fn caused_by<E: std::error::Error + 'static>(error: &(dyn std::error::Error + 'static)) -> bool {
    std::iter::successors(Some(error), |error| error.source()).any(|error| error.is::<E>())
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
