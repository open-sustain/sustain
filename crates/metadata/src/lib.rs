// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use lofty::{
    config::{GlobalOptions, WriteOptions, apply_global_options},
    file::TaggedFile,
    picture::{Picture, PictureType},
    prelude::{Accessor, AudioFile, TaggedFileExt},
    tag::{
        ItemKey, Tag,
        items::popularimeter::{Popularimeter, StarRating},
    },
};
use sha2::{Digest, Sha256};
use sustain_artwork::{ArtworkPolicyError, MAX_ENCODED_ARTWORK_BYTES, validate_encoded_artwork};

pub use sustain_domain::{
    FieldChange, MetadataChange, Rating, TrackContentHash, TrackMetadata, TrackRelativePath,
};

mod atomic_file;
use atomic_file::atomic_write_via_rename;

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
    WriteFailed,
    ReadFailed,
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

pub struct LibraryScanner<'a, S: ?Sized> {
    metadata_service: &'a S,
}

trait ScanFilesystem {
    fn is_directory(&self, path: &Path) -> bool;
    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>>;
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
}

#[derive(Clone, Copy, Debug, Default)]
struct StdScanFilesystem;

impl ScanFilesystem for StdScanFilesystem {
    fn is_directory(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>> {
        Ok(Box::new(
            fs::read_dir(path)?.map(|entry| entry.map(|entry| entry.path())),
        ))
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }
}

impl<'a, S> LibraryScanner<'a, S>
where
    S: MetadataService + ?Sized,
{
    pub const fn new(metadata_service: &'a S) -> Self {
        Self { metadata_service }
    }

    /// Walks `library_path`, parsing each supported file's tags. Files
    /// whose `(size, mtime)` fingerprint already matches an entry in
    /// `known_fingerprints` are reported as `unchanged` and not re-parsed
    /// (#71); pass an empty map to parse everything (a cold scan).
    pub fn scan(
        &self,
        library_path: &Path,
        cancellation: &AtomicBool,
        known_fingerprints: &BTreeMap<TrackRelativePath, ScanFingerprint>,
    ) -> Result<LibraryScan, LibraryScanError> {
        self.scan_with_filesystem(
            library_path,
            cancellation,
            known_fingerprints,
            &StdScanFilesystem,
        )
    }

    fn scan_with_filesystem(
        &self,
        library_path: &Path,
        cancellation: &AtomicBool,
        known_fingerprints: &BTreeMap<TrackRelativePath, ScanFingerprint>,
        filesystem: &impl ScanFilesystem,
    ) -> Result<LibraryScan, LibraryScanError> {
        if !filesystem.is_directory(library_path) {
            return Err(LibraryScanError::LibraryPathUnavailable);
        }

        let mut scan = LibraryScan::default();
        self.scan_directory(
            library_path,
            library_path,
            &mut scan,
            cancellation,
            known_fingerprints,
            filesystem,
        );
        scan.cancelled = scan.cancelled || cancellation.load(Ordering::SeqCst);
        scan.complete_for_missing_reconciliation &= !scan.cancelled;
        scan.tracks
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        scan.unchanged.sort();
        Ok(scan)
    }

    fn scan_directory(
        &self,
        library_path: &Path,
        directory: &Path,
        scan: &mut LibraryScan,
        cancellation: &AtomicBool,
        known_fingerprints: &BTreeMap<TrackRelativePath, ScanFingerprint>,
        filesystem: &impl ScanFilesystem,
    ) {
        let entries = match filesystem.read_directory(directory) {
            Ok(entries) => entries,
            Err(_) => {
                scan.record_failure(directory.to_path_buf(), MetadataError::ReadFailed);
                return;
            }
        };

        for entry in entries {
            if cancellation.load(Ordering::SeqCst) {
                scan.cancelled = true;
                return;
            }
            let path = match entry {
                Ok(path) => path,
                Err(_) => {
                    scan.record_failure(directory.to_path_buf(), MetadataError::ReadFailed);
                    continue;
                }
            };
            let metadata = match filesystem.symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    scan.record_failure(path, MetadataError::ReadFailed);
                    continue;
                }
            };
            if metadata.file_type().is_dir() {
                self.scan_directory(
                    library_path,
                    &path,
                    scan,
                    cancellation,
                    known_fingerprints,
                    filesystem,
                );
                if scan.cancelled {
                    return;
                }
            } else if metadata.file_type().is_file() {
                // Read the mtime from the stat we already have — no extra
                // syscall — so an unchanged file can be fingerprinted and
                // skipped below (#71).
                self.scan_file(
                    library_path,
                    path,
                    metadata.len(),
                    metadata.modified().ok(),
                    known_fingerprints,
                    scan,
                );
            }
        }
    }

    fn scan_file(
        &self,
        library_path: &Path,
        path: PathBuf,
        file_size_bytes: u64,
        modified_at: Option<SystemTime>,
        known_fingerprints: &BTreeMap<TrackRelativePath, ScanFingerprint>,
        scan: &mut LibraryScan,
    ) {
        if audio_format_from_path(&path).is_err() {
            scan.skipped_unsupported_files += 1;
            return;
        }

        let relative_path = match path
            .strip_prefix(library_path)
            .ok()
            .and_then(|path| TrackRelativePath::new(path.to_path_buf()))
        {
            Some(relative_path) => relative_path,
            None => {
                scan.record_failure(path, MetadataError::ReadFailed);
                return;
            }
        };

        // Skip the tag parse entirely when this file is already known with
        // an identical size + mtime fingerprint: its audio-stream
        // properties, size, and embedded-artwork bit cannot have changed,
        // and SQLite already owns everything else for an imported track
        // (#71). A missing mtime (None) cannot produce a fingerprint, so
        // such a file is always parsed and re-records its mtime.
        let fingerprint =
            modified_at.and_then(|modified_at| ScanFingerprint::new(file_size_bytes, modified_at));
        if let Some(fingerprint) = fingerprint {
            if known_fingerprints.get(&relative_path) == Some(&fingerprint) {
                scan.unchanged.push(relative_path);
                return;
            }
        }

        let InitialTags {
            metadata,
            rating,
            has_embedded_artwork,
        } = match self.metadata_service.read_initial_tags(&path) {
            Ok(tags) => tags,
            Err(error) => {
                scan.record_failure(path, error);
                return;
            }
        };
        scan.tracks.push(ScannedTrack {
            relative_path,
            metadata,
            rating,
            file_size_bytes: Some(file_size_bytes),
            has_embedded_artwork,
            file_modified_at: modified_at,
        });
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct LoftyMetadataService;

impl MetadataService for LoftyMetadataService {
    fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        read_tags(path, true)
    }

    fn read_persisted_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        read_tags(path, false)
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
        apply_number_change(tag, ItemKey::TrackNumber, change.track_number);
        apply_number_change(tag, ItemKey::TrackTotal, change.track_total);
        apply_number_change(tag, ItemKey::DiscNumber, change.disc_number);
        apply_number_change(tag, ItemKey::DiscTotal, change.disc_total);
        apply_year_change(tag, change.year);
        apply_bool_change(tag, ItemKey::FlagCompilation, change.compilation);
        apply_number_change(tag, ItemKey::Bpm, change.bpm);
        apply_text_change(tag, ItemKey::InitialKey, change.key);
        apply_text_change(tag, ItemKey::Comment, change.comments);
        apply_text_change(tag, ItemKey::Lyrics, change.lyrics);

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
        let preserved_counter = tag
            .ratings()
            .next()
            .map(|popularimeter| popularimeter.play_counter)
            .unwrap_or(0);

        if rating == Rating::unrated() {
            // The high-level Popularimeter API has no representation
            // for "POPM with rating=0", so transitioning a rated track
            // to unrated removes the frame entirely. In the rare case
            // where another tool stored a play counter there, it is
            // lost. Sustain does not use the counter for its own
            // accounting, so this only affects external readers.
            let _removed = tag.take(ItemKey::Popularimeter).count();
        } else {
            tag.insert_text(
                ItemKey::Popularimeter,
                popularimeter_from_rating(rating, preserved_counter).to_string(),
            );
        }

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

        atomic_save_to_path(&tagged_file, path, WriteOptions::default())
    }
}

fn read_tags(path: &Path, backfill_title_from_filename: bool) -> MetadataResult<InitialTags> {
    audio_format_from_path(path)?;
    let tagged_file = read_tagged_file(path)?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());
    let properties = tagged_file.properties();

    let mut metadata = TrackMetadata {
        title: tag.and_then(|tag| tag.title().map(|value| value.into_owned())),
        artist: tag.and_then(|tag| tag.artist().map(|value| value.into_owned())),
        album: tag.and_then(|tag| tag.album().map(|value| value.into_owned())),
        album_artist: tag
            .and_then(|tag| tag.get_string(ItemKey::AlbumArtist))
            .map(ToOwned::to_owned),
        composer: tag
            .and_then(|tag| tag.get_string(ItemKey::Composer))
            .map(ToOwned::to_owned),
        grouping: tag
            .and_then(|tag| tag.get_string(ItemKey::ContentGroup))
            .map(ToOwned::to_owned),
        genre: tag.and_then(|tag| tag.genre().map(|value| value.into_owned())),
        track_number: tag.and_then(Accessor::track),
        track_total: tag.and_then(Accessor::track_total),
        disc_number: tag.and_then(Accessor::disk),
        disc_total: tag.and_then(Accessor::disk_total),
        year: tag.and_then(|tag| tag.date().map(|date| i32::from(date.year))),
        compilation: tag
            .and_then(|tag| tag.get_string(ItemKey::FlagCompilation))
            .and_then(parse_flag),
        bpm: tag
            .and_then(|tag| tag.get_string(ItemKey::Bpm))
            .and_then(|value| value.trim().parse::<u32>().ok()),
        key: tag
            .and_then(|tag| tag.get_string(ItemKey::InitialKey))
            .map(ToOwned::to_owned),
        comments: tag.and_then(|tag| tag.comment().map(|value| value.into_owned())),
        lyrics: tag
            .and_then(|tag| tag.get_string(ItemKey::Lyrics))
            .map(ToOwned::to_owned),
        // Tag-derived "sort as" names (issue #13). Read once at import
        // alongside the display fields; only used for ordering, never
        // written back.
        title_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::TrackTitleSortOrder))
            .map(ToOwned::to_owned),
        artist_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::TrackArtistSortOrder))
            .map(ToOwned::to_owned),
        album_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::AlbumTitleSortOrder))
            .map(ToOwned::to_owned),
        album_artist_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::AlbumArtistSortOrder))
            .map(ToOwned::to_owned),
        composer_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::ComposerSortOrder))
            .map(ToOwned::to_owned),
        duration: Some(properties.duration()),
        bitrate_kbps: properties.audio_bitrate().or(properties.overall_bitrate()),
        sample_rate_hz: properties.sample_rate(),
        channels: properties.channels(),
    };
    if backfill_title_from_filename {
        metadata.ensure_title_from_filename(path);
    }

    let rating = tag
        .and_then(|tag| tag.ratings().next())
        .and_then(|rating| Rating::new(star_rating_value(rating.rating())))
        .unwrap_or_else(Rating::unrated);
    let has_embedded_artwork = tag.and_then(valid_embedded_picture).is_some();

    Ok(InitialTags {
        metadata,
        rating,
        has_embedded_artwork,
    })
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

pub fn hash_file_content(path: &Path) -> MetadataResult<TrackContentHash> {
    let mut file = fs::File::open(path).map_err(|_| MetadataError::ReadFailed)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];

    loop {
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|_| MetadataError::ReadFailed)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    TrackContentHash::new(lower_hex(&hasher.finalize())).ok_or(MetadataError::ReadFailed)
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

fn read_tagged_file(path: &Path) -> MetadataResult<TaggedFile> {
    // Lofty's allocation guard is thread-local. Reapply Sustain's policy at
    // every metadata entry point so worker threads and future Lofty defaults
    // cannot drift away from the application's encoded-artwork cap.
    apply_global_options(GlobalOptions::new().allocation_limit(MAX_ENCODED_ARTWORK_BYTES));
    lofty::read_from_path(path).map_err(|_| MetadataError::ReadFailed)
}

fn valid_embedded_picture(tag: &Tag) -> Option<&Picture> {
    tag.get_picture_type(PictureType::CoverFront)
        .filter(|picture| validate_encoded_artwork(picture.data()).is_ok())
        .or_else(|| {
            tag.pictures()
                .iter()
                .find(|picture| validate_encoded_artwork(picture.data()).is_ok())
        })
}

fn ensure_primary_tag(tagged_file: &mut TaggedFile) {
    if tagged_file.primary_tag().is_some() {
        return;
    }

    tagged_file.insert_tag(Tag::new(tagged_file.primary_tag_type()));
}

// Persists `tagged_file` over `path` via atomic replace-by-rename: the
// new bytes land in an exclusive sibling temp file, retain the source
// filesystem metadata, get fsync'd to disk, atomically replace the
// pathname, and are made durable by syncing the parent directory. The key
// property this buys us is that GStreamer (or any other reader holding an open
// file descriptor on `path`) keeps seeing the *original* inode's bytes
// until it closes the descriptor — Linux/POSIX `rename` only swaps the
// directory entry, the prior inode is kept alive by outstanding fds.
// That eliminates the audio glitch caused by lofty's default in-place
// rewrite happening underneath an active playback read.
fn atomic_save_to_path(
    tagged_file: &lofty::file::TaggedFile,
    path: &Path,
    options: WriteOptions,
) -> MetadataResult<()> {
    atomic_write_via_rename(path, |temp_path| {
        tagged_file
            .save_to_path(temp_path, options)
            .map_err(|_| MetadataError::WriteFailed)
    })
}

fn apply_text_change(tag: &mut Tag, item_key: ItemKey, change: FieldChange<String>) {
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(value) => {
            tag.insert_text(item_key, value);
        }
        FieldChange::Clear => {
            let _removed = tag.take(item_key).count();
        }
    }
}

fn apply_number_change<T>(tag: &mut Tag, item_key: ItemKey, change: FieldChange<T>)
where
    T: ToString,
{
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(value) => {
            tag.insert_text(item_key, value.to_string());
        }
        FieldChange::Clear => {
            let _removed = tag.take(item_key).count();
        }
    }
}

fn apply_year_change(tag: &mut Tag, change: FieldChange<i32>) {
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(year) => {
            if let Some(year) = u16::try_from(year).ok().filter(|year| *year <= 9999) {
                let mut date = tag.date().unwrap_or_default();
                date.year = year;
                tag.set_date(date);
            } else {
                // Lofty's timestamp representation is unsigned. Retain the
                // prior textual behavior for values outside that range while
                // still removing any higher-priority recording date.
                tag.remove_date();
                tag.insert_text(ItemKey::Year, year.to_string());
            }
        }
        FieldChange::Clear => tag.remove_date(),
    }
}

fn apply_bool_change(tag: &mut Tag, item_key: ItemKey, change: FieldChange<bool>) {
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(value) => {
            tag.insert_text(item_key, if value { "1" } else { "0" }.to_owned());
        }
        FieldChange::Clear => {
            let _removed = tag.take(item_key).count();
        }
    }
}

fn parse_flag(value: &str) -> Option<bool> {
    match value.trim() {
        "1" | "true" | "TRUE" | "True" | "yes" => Some(true),
        "0" | "false" | "FALSE" | "False" | "no" => Some(false),
        _ => None,
    }
}

fn star_rating_value(rating: StarRating) -> u8 {
    match rating {
        StarRating::One => 1,
        StarRating::Two => 2,
        StarRating::Three => 3,
        StarRating::Four => 4,
        StarRating::Five => 5,
    }
}

fn popularimeter_from_rating(rating: Rating, play_counter: u64) -> Popularimeter<'static> {
    match rating.stars() {
        1 => Popularimeter::musicbee(StarRating::One, play_counter),
        2 => Popularimeter::musicbee(StarRating::Two, play_counter),
        3 => Popularimeter::musicbee(StarRating::Three, play_counter),
        4 => Popularimeter::musicbee(StarRating::Four, play_counter),
        5 => Popularimeter::musicbee(StarRating::Five, play_counter),
        _ => unreachable!("unrated ratings are removed before conversion"),
    }
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
