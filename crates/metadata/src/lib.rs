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
    config::{GlobalOptions, WriteOptions, apply_global_options},
    error::ErrorKind,
    file::TaggedFile,
    id3::v2::{Frame, Id3v2Tag, PopularimeterFrame},
    picture::{Picture, PictureType},
    prelude::{Accessor, AudioFile, TaggedFileExt},
    tag::{
        ItemKey, Tag, TagExt, TagItem, TagType,
        items::{
            Lang, UNKNOWN_LANGUAGE,
            popularimeter::{Popularimeter, StarRating},
        },
    },
};
use sha2::{Digest, Sha256};
use sustain_artwork::{ArtworkPolicyError, MAX_ENCODED_ARTWORK_BYTES, validate_encoded_artwork};

pub use sustain_domain::{
    FieldChange, MetadataChange, Rating, TrackContentHash, TrackMetadata, TrackRelativePath,
    validate_bpm,
};

mod atomic_file;
use atomic_file::atomic_write_via_rename;
mod hash;
mod mpeg_frame;
mod read;
mod scan;
mod tag_write;

pub use hash::{copy_and_hash_reader_content, hash_file_content, hash_reader_content};
pub use scan::LibraryScanner;

use read::{read_tagged_file, read_tags, valid_embedded_picture};
use tag_write::{
    PreservedPopularimeter, apply_bool_change, apply_number_change, apply_text_change,
    apply_year_change, atomic_save_id3v2_to_path, atomic_save_to_path, clear_rating,
    ensure_primary_tag, id3v2_tag_clearing_rating_preserving_counter, popularimeter_from_rating,
    repair_invalid_id3v2_languages,
};

pub type MetadataResult<T> = Result<T, MetadataError>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFormat {
    Mp3,
    Ogg,
    Flac,
    Mp4,
    Wav,
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
    fn write_rating(&self, path: &Path, rating: Rating) -> MetadataResult<()>;
    fn read_artwork(&self, path: &Path) -> MetadataResult<Option<Vec<u8>>>;
    fn write_artwork(&self, path: &Path, artwork: Option<Vec<u8>>) -> MetadataResult<()>;
    fn repair_malformed_tags(&self, _path: &Path, _repair: MetadataRepair) -> MetadataResult<bool> {
        Ok(false)
    }
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
        read_tags(path, true).map(|read| read.tags)
    }

    fn read_initial_tags_with_diagnostics(&self, path: &Path) -> MetadataResult<InitialTagRead> {
        read_tags(path, true)
    }

    fn read_persisted_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        read_tags(path, false).map(|read| read.tags)
    }

    fn write_metadata(&self, path: &Path, change: MetadataChange) -> MetadataResult<()> {
        audio_format_from_path(path)?;
        let mut tagged_file = read_tagged_file(path)?;
        ensure_primary_tag(&mut tagged_file);
        let tag = tagged_file
            .primary_tag_mut()
            .ok_or(MetadataError::WriteFailed)?;

        apply_text_change(tag, ItemKey::TrackTitle, change.title);
        apply_text_change(tag, ItemKey::TrackArtist, change.artist);
        apply_text_change(tag, ItemKey::AlbumTitle, change.album);
        apply_text_change(tag, ItemKey::AlbumArtist, change.album_artist);
        apply_text_change(tag, ItemKey::Composer, change.composer);
        apply_text_change(tag, ItemKey::ContentGroup, change.grouping);
        apply_text_change(tag, ItemKey::Genre, change.genre);
        apply_text_change(tag, ItemKey::TrackTitleSortOrder, change.title_sort);
        apply_text_change(tag, ItemKey::TrackArtistSortOrder, change.artist_sort);
        apply_text_change(tag, ItemKey::AlbumTitleSortOrder, change.album_sort);
        apply_text_change(tag, ItemKey::AlbumArtistSortOrder, change.album_artist_sort);
        // Lofty 0.24 has ID3v2/MP4 mappings for composer sort, but no Vorbis
        // `COMPOSERSORT` mapping. Inserting the generic key into a Vorbis tag is
        // silently discarded on merge, so keep SQLite authoritative rather than
        // pretending we mirrored a field the backend cannot round-trip.
        if tag.tag_type() != TagType::VorbisComments {
            apply_text_change(tag, ItemKey::ComposerSortOrder, change.composer_sort);
        }
        apply_number_change(tag, ItemKey::TrackNumber, change.track_number);
        apply_number_change(tag, ItemKey::TrackTotal, change.track_total);
        apply_number_change(tag, ItemKey::DiscNumber, change.disc_number);
        apply_number_change(tag, ItemKey::DiscTotal, change.disc_total);
        apply_year_change(tag, change.year);
        apply_bool_change(tag, ItemKey::FlagCompilation, change.compilation);
        let bpm_key = bpm_item_key(tag.tag_type());
        apply_number_change(tag, bpm_key, change.bpm);
        apply_text_change(tag, ItemKey::InitialKey, change.key);
        apply_text_change(tag, ItemKey::Comment, change.comments);
        let lyrics_key = lyrics_item_key(tag.tag_type());
        apply_text_change(tag, lyrics_key, change.lyrics);

        repair_invalid_id3v2_languages(tag);
        atomic_save_to_path(&tagged_file, path, WriteOptions::default())
    }

    fn write_rating(&self, path: &Path, rating: Rating) -> MetadataResult<()> {
        audio_format_from_path(path)?;
        let mut tagged_file = read_tagged_file(path)?;
        ensure_primary_tag(&mut tagged_file);
        let tag = tagged_file
            .primary_tag_mut()
            .ok_or(MetadataError::WriteFailed)?;

        // Preserve any existing POPM play counter when overwriting the
        // frame with a new rating. Sustain itself never reads or writes
        // this counter — listening statistics live in SQLite, per the
        // persistence policy in AGENTS.md — but other applications
        // (MusicBee, Foobar2000, …) store play counts in the POPM
        // counter field, and silently zeroing them out on every rating
        // edit would clobber data that doesn't belong to us.
        let preserved_popularimeter = tag.ratings().next().map(|popularimeter| {
            PreservedPopularimeter::from_parts(
                popularimeter.email().unwrap_or("").to_owned(),
                popularimeter.play_counter,
            )
        });
        let preserved_counter = preserved_popularimeter
            .as_ref()
            .map(|popularimeter| popularimeter.play_counter)
            .unwrap_or(0);

        if rating == Rating::unrated() {
            if let Some(id3v2) =
                id3v2_tag_clearing_rating_preserving_counter(tag, preserved_popularimeter)
            {
                return atomic_save_id3v2_to_path(&id3v2, path, WriteOptions::default());
            }
            clear_rating(tag);
        } else {
            tag.insert_text(
                ItemKey::Popularimeter,
                popularimeter_from_rating(rating, preserved_counter).to_string(),
            );
        }

        repair_invalid_id3v2_languages(tag);
        atomic_save_to_path(&tagged_file, path, WriteOptions::default())
    }

    fn read_artwork(&self, path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        audio_format_from_path(path)?;
        let tagged_file = read_tagged_file(path)?;
        let Some(tag) = tagged_file
            .primary_tag()
            .or_else(|| tagged_file.first_tag())
        else {
            return Ok(None);
        };

        Ok(valid_embedded_picture(tag).map(|picture| picture.data().to_vec()))
    }

    fn write_artwork(&self, path: &Path, artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        if let Some(bytes) = artwork.as_deref() {
            validate_encoded_artwork(bytes).map_err(MetadataError::ArtworkRejected)?;
        }
        audio_format_from_path(path)?;
        let mut tagged_file = read_tagged_file(path)?;
        ensure_primary_tag(&mut tagged_file);
        let tag = tagged_file
            .primary_tag_mut()
            .ok_or(MetadataError::WriteFailed)?;

        // Drop every existing CoverFront picture before writing the new one
        // (or leaving the slot empty). Walk in reverse so the indices stay
        // valid as we remove entries.
        let cover_indices: Vec<usize> = tag
            .pictures()
            .iter()
            .enumerate()
            .filter(|(_, picture)| picture.pic_type() == PictureType::CoverFront)
            .map(|(index, _)| index)
            .collect();
        for index in cover_indices.into_iter().rev() {
            let _removed = tag.remove_picture(index);
        }

        if let Some(bytes) = artwork {
            let mut cursor = std::io::Cursor::new(bytes);
            let mut picture =
                Picture::from_reader(&mut cursor).map_err(|_| MetadataError::WriteFailed)?;
            picture.set_pic_type(PictureType::CoverFront);
            tag.push_picture(picture);
        }

        repair_invalid_id3v2_languages(tag);
        atomic_save_to_path(&tagged_file, path, WriteOptions::default())
    }

    fn repair_malformed_tags(&self, path: &Path, repair: MetadataRepair) -> MetadataResult<bool> {
        tag_write::repair_malformed_tag_read_error(path, repair)
    }
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
        _ => Err(MetadataError::UnsupportedAudioFormat),
    }
}

/// The tag item that carries beats-per-minute in `tag_type`.
///
/// Lofty models BPM as two distinct keys: the decimal [`ItemKey::Bpm`]
/// and the integer [`ItemKey::IntegerBpm`]. They map to different
/// container fields, and not every format defines both. ID3v2 only has
/// the integer `TBPM` frame and MP4 the integer `tmpo` atom — neither
/// maps [`ItemKey::Bpm`], so writing it there silently drops the value.
/// Vorbis comments conversely only define a decimal `BPM` field. Sustain
/// stores BPM as an integer, so prefer the integer key wherever the
/// format maps it and fall back to the decimal key otherwise. Used for
/// both reading and writing so the value round-trips on every format.
fn bpm_item_key(tag_type: TagType) -> ItemKey {
    if ItemKey::IntegerBpm.map_key(tag_type).is_some() {
        ItemKey::IntegerBpm
    } else {
        ItemKey::Bpm
    }
}

/// The tag item that carries unsynchronized lyrics in `tag_type`.
///
/// ID3v2 exposes lyrics only through the `USLT` frame
/// ([`ItemKey::UnsyncLyrics`]) and deliberately leaves [`ItemKey::Lyrics`]
/// unmapped, so writing the plain key to an MP3 silently drops the text.
/// MP4 (`©lyr`) and Vorbis (`LYRICS`) accept [`ItemKey::Lyrics`]. Prefer
/// the plain key where the format maps it and fall back to the
/// unsynchronized key otherwise, symmetrically for read and write.
fn lyrics_item_key(tag_type: TagType) -> ItemKey {
    if ItemKey::Lyrics.map_key(tag_type).is_some() {
        ItemKey::Lyrics
    } else {
        ItemKey::UnsyncLyrics
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
