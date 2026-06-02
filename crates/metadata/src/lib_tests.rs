// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use lofty::{
    picture::{Picture, PictureType},
    prelude::Accessor,
    tag::{ItemKey, Tag, TagType},
};
use sustain_domain::{FieldChange, TrackMetadata};

use super::{
    AudioFormat, InitialTags, LibraryScanner, MetadataError, MetadataResult, MetadataService,
    Rating, ScanFilesystem, ScanFingerprint, StdScanFilesystem, apply_year_change,
    atomic_write_via_rename, audio_format_from_path, hash_file_content, valid_embedded_picture,
};
use sustain_domain::TrackRelativePath;

#[test]
fn detects_supported_audio_formats_case_insensitively() {
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.MP3")),
        Ok(AudioFormat::Mp3)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.ogg")),
        Ok(AudioFormat::Ogg)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.OPUS")),
        Ok(AudioFormat::Ogg)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.flac")),
        Ok(AudioFormat::Flac)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.m4a")),
        Ok(AudioFormat::Mp4)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.mp4")),
        Ok(AudioFormat::Mp4)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.WAV")),
        Ok(AudioFormat::Wav)
    );
}

#[test]
fn rejects_unsupported_audio_formats() {
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.aiff")),
        Err(MetadataError::UnsupportedAudioFormat)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/no-extension")),
        Err(MetadataError::UnsupportedAudioFormat)
    );
}

#[test]
fn year_update_replaces_the_higher_priority_recording_date() {
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.insert_text(ItemKey::RecordingDate, "1998-09-30".to_owned());

    apply_year_change(&mut tag, FieldChange::Set(2015));

    assert_eq!(tag.date().map(|date| date.year), Some(2015));
    assert_eq!(tag.get_string(ItemKey::RecordingDate), Some("2015-09-30"));
    assert_eq!(tag.get_string(ItemKey::Year), None);
}

#[test]
fn year_clear_removes_recording_date_and_year_items() {
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.insert_text(ItemKey::RecordingDate, "1998-09-30".to_owned());
    tag.insert_text(ItemKey::Year, "1998".to_owned());

    apply_year_change(&mut tag, FieldChange::Clear);

    assert_eq!(tag.get_string(ItemKey::RecordingDate), None);
    assert_eq!(tag.get_string(ItemKey::Year), None);
}

#[test]
fn year_update_outside_loftys_timestamp_range_keeps_a_textual_year() {
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.insert_text(ItemKey::RecordingDate, "1998-09-30".to_owned());

    apply_year_change(&mut tag, FieldChange::Set(10_000));

    assert_eq!(tag.get_string(ItemKey::RecordingDate), None);
    assert_eq!(tag.get_string(ItemKey::Year), Some("10000"));
}

#[test]
fn embedded_picture_selection_skips_invalid_front_cover() {
    let valid = vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let mut tag = Tag::new(TagType::Id3v2);
    tag.push_picture(
        Picture::unchecked(b"not an image".to_vec())
            .pic_type(PictureType::CoverFront)
            .build(),
    );
    tag.push_picture(Picture::unchecked(valid.clone()).build());

    assert_eq!(
        valid_embedded_picture(&tag).map(Picture::data),
        Some(valid.as_slice())
    );
}

#[test]
fn scanner_recurses_and_ignores_unsupported_files() {
    let root = unique_test_directory();
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("create nested test directory");
    fs::write(root.join("one.mp3"), b"not real audio").expect("write test file");
    fs::write(nested.join("two.flac"), b"not real audio").expect("write test file");
    fs::write(root.join("notes.txt"), b"ignore").expect("write test file");

    let metadata_service =
        FakeMetadataService::for_paths([root.join("one.mp3"), nested.join("two.flac")]);
    let scan = LibraryScanner::new(&metadata_service)
        .scan(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
        )
        .expect("scan test directory");

    let scanned_paths = scan
        .tracks
        .iter()
        .map(|track| track.relative_path.as_path().to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(
        scanned_paths,
        vec![PathBuf::from("nested/two.flac"), PathBuf::from("one.mp3")]
    );
    assert_eq!(scan.skipped_unsupported_files, 1);
    assert_eq!(scan.failures, Vec::new());
    assert!(!scan.cancelled);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_returns_partial_results_when_cancellation_is_observed() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    fs::write(root.join("a.mp3"), b"audio").expect("write a.mp3");
    fs::write(root.join("b.flac"), b"audio").expect("write b.flac");

    let metadata_service =
        FakeMetadataService::for_paths([root.join("a.mp3"), root.join("b.flac")]);
    // Pre-set the cancellation flag so the very first per-entry
    // check inside the scanner trips. The walk must abort before
    // visiting any audio file and the result must report
    // `cancelled = true` so callers know not to treat unwalked
    // tracks as missing.
    let cancellation = std::sync::atomic::AtomicBool::new(true);
    let scan = LibraryScanner::new(&metadata_service)
        .scan(&root, &cancellation, &BTreeMap::new())
        .expect("scan test directory");

    assert!(scan.cancelled);
    assert!(!scan.complete_for_missing_reconciliation);
    assert!(scan.tracks.is_empty());

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_marks_nested_directory_read_failure_incomplete() {
    let root = unique_test_directory();
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("create nested test directory");
    let filesystem = FaultInjectingScanFilesystem {
        unreadable_directory: Some(nested),
        ..FaultInjectingScanFilesystem::default()
    };

    let scan = LibraryScanner::new(&FakeMetadataService::default())
        .scan_with_filesystem(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
            &filesystem,
        )
        .expect("scan test directory");

    assert!(!scan.complete_for_missing_reconciliation);
    assert_eq!(scan.failures.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_records_directory_iterator_errors_instead_of_flattening_them() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let track_path = root.join("one.mp3");
    fs::write(&track_path, b"audio").expect("write test file");
    let filesystem = FaultInjectingScanFilesystem {
        entry_error_directory: Some(root.clone()),
        ..FaultInjectingScanFilesystem::default()
    };

    let scan = LibraryScanner::new(&FakeMetadataService::for_paths([track_path]))
        .scan_with_filesystem(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
            &filesystem,
        )
        .expect("scan test directory");

    assert_eq!(scan.tracks.len(), 1, "safe rows remain usable");
    assert!(!scan.complete_for_missing_reconciliation);
    assert_eq!(scan.failures.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_marks_stat_failures_incomplete() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let track_path = root.join("one.mp3");
    fs::write(&track_path, b"audio").expect("write test file");
    let filesystem = FaultInjectingScanFilesystem {
        unreadable_path: Some(track_path),
        ..FaultInjectingScanFilesystem::default()
    };

    let scan = LibraryScanner::new(&FakeMetadataService::default())
        .scan_with_filesystem(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
            &filesystem,
        )
        .expect("scan test directory");

    assert!(scan.tracks.is_empty());
    assert!(!scan.complete_for_missing_reconciliation);
    assert_eq!(scan.failures.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn atomic_write_keeps_open_readers_on_the_original_inode() {
    use std::io::Read;

    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("audio.bin");
    fs::write(&path, b"original-payload-bytes").expect("seed original file");

    // Open the file before the atomic write — this is the moment
    // that stands in for GStreamer holding an open fd on the
    // currently playing track.
    let mut pre_existing_reader = fs::File::open(&path).expect("open before replace");

    atomic_write_via_rename(&path, |temp_path| {
        fs::write(temp_path, b"replacement-payload").map_err(|_| MetadataError::WriteFailed)
    })
    .expect("atomic write succeeds");

    // The pre-existing reader must still see the original bytes.
    // If rename(2) were not preserving the prior inode for open
    // file descriptors, this would read either the new bytes or a
    // torn mixture — both would manifest as audio glitches in
    // GStreamer.
    let mut observed = Vec::new();
    pre_existing_reader
        .read_to_end(&mut observed)
        .expect("read pre-existing handle");
    assert_eq!(observed.as_slice(), b"original-payload-bytes");

    // A fresh open after the rename sees the replacement bytes.
    let post_swap = fs::read(&path).expect("read after replace");
    assert_eq!(post_swap.as_slice(), b"replacement-payload");

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn atomic_write_leaves_no_temp_file_when_modify_step_fails() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("audio.bin");
    fs::write(&path, b"original").expect("seed original file");

    let result =
        atomic_write_via_rename(&path, |_temp_path| Err::<(), _>(MetadataError::WriteFailed));
    assert_eq!(result, Err(MetadataError::WriteFailed));

    // The destination still holds the original content — failure
    // never replaces the user's file with partial bytes.
    let on_disk = fs::read(&path).expect("read after failure");
    assert_eq!(on_disk.as_slice(), b"original");

    // No `.sustain-*.tmp` debris lingers next to the audio file.
    let leftovers: Vec<_> = fs::read_dir(&root)
        .expect("list test directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("sustain-") && name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "expected no temp files, found: {leftovers:?}"
    );

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn hash_file_content_returns_sha256_hex() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("track.flac");
    fs::write(&path, b"abc").expect("write file");

    let hash = hash_file_content(&path).expect("hash file");

    assert_eq!(
        hash.as_str(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_skips_reparsing_unchanged_files_and_reports_them_present() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let unchanged = root.join("unchanged.mp3");
    let changed = root.join("changed.mp3");
    fs::write(&unchanged, b"audio").expect("write unchanged");
    fs::write(&changed, b"audio").expect("write changed");

    // Build the "already known" fingerprints from the real on-disk stat.
    // The unchanged file's fingerprint matches exactly; the changed file's
    // recorded size is stale, so its fingerprint will not match and it must
    // be re-parsed.
    let unchanged_meta = fs::symlink_metadata(&unchanged).expect("stat unchanged");
    let changed_meta = fs::symlink_metadata(&changed).expect("stat changed");
    let unchanged_relative = TrackRelativePath::new("unchanged.mp3").expect("relative");
    let changed_relative = TrackRelativePath::new("changed.mp3").expect("relative");
    let mut known = BTreeMap::new();
    known.insert(
        unchanged_relative.clone(),
        ScanFingerprint::new(
            unchanged_meta.len(),
            unchanged_meta.modified().expect("mtime"),
        )
        .expect("fingerprint"),
    );
    known.insert(
        changed_relative.clone(),
        ScanFingerprint::new(
            changed_meta.len() + 1,
            changed_meta.modified().expect("mtime"),
        )
        .expect("fingerprint"),
    );

    let metadata_service = RecordingMetadataService::default();
    let scan = LibraryScanner::new(&metadata_service)
        .scan(&root, &std::sync::atomic::AtomicBool::new(false), &known)
        .expect("scan test directory");

    // Only the changed file was opened/parsed; the unchanged one was not.
    assert_eq!(metadata_service.parsed_paths(), vec![changed.clone()]);
    // The unchanged file is reported present-but-skipped, never a parsed
    // track and never a candidate for "missing".
    assert_eq!(scan.unchanged, vec![unchanged_relative]);
    let parsed: Vec<_> = scan
        .tracks
        .iter()
        .map(|track| track.relative_path.clone())
        .collect();
    assert_eq!(parsed, vec![changed_relative]);
    // The re-parsed track records a fresh mtime so the next scan can skip it.
    assert!(scan.tracks[0].file_modified_at.is_some());

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_reparses_when_only_mtime_differs() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("song.mp3");
    fs::write(&path, b"audio").expect("write file");

    // Same size, but the recorded mtime is one second older than disk:
    // the fingerprint must miss and the file must be re-parsed.
    let meta = fs::symlink_metadata(&path).expect("stat");
    let stale_mtime = meta.modified().expect("mtime") - std::time::Duration::from_secs(1);
    let relative = TrackRelativePath::new("song.mp3").expect("relative");
    let mut known = BTreeMap::new();
    known.insert(
        relative.clone(),
        ScanFingerprint::new(meta.len(), stale_mtime).expect("fingerprint"),
    );

    let metadata_service = RecordingMetadataService::default();
    let scan = LibraryScanner::new(&metadata_service)
        .scan(&root, &std::sync::atomic::AtomicBool::new(false), &known)
        .expect("scan test directory");

    assert_eq!(metadata_service.parsed_paths(), vec![path]);
    assert!(scan.unchanged.is_empty());
    assert_eq!(scan.tracks.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[derive(Default)]
struct FakeMetadataService {
    tracks: BTreeMap<PathBuf, TrackMetadata>,
}

impl FakeMetadataService {
    fn for_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            tracks: paths
                .into_iter()
                .map(|path| {
                    (
                        path,
                        TrackMetadata {
                            title: Some("Test".to_owned()),
                            ..TrackMetadata::default()
                        },
                    )
                })
                .collect(),
        }
    }
}

impl MetadataService for FakeMetadataService {
    fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        let metadata = self
            .tracks
            .get(path)
            .cloned()
            .ok_or(MetadataError::ReadFailed)?;
        Ok(InitialTags {
            metadata,
            rating: Rating::new(4).expect("valid test rating"),
            has_embedded_artwork: false,
        })
    }

    fn write_metadata(&self, _path: &Path, _change: super::MetadataChange) -> MetadataResult<()> {
        Ok(())
    }

    fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
        Ok(())
    }

    fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        Ok(())
    }
}

/// A metadata service that records every path it is asked to parse, so a
/// test can assert which files the scanner actually opened (and, by
/// omission, which it skipped via the size+mtime fingerprint). Accepts any
/// path and returns a fixed, valid tag set.
#[derive(Default)]
struct RecordingMetadataService {
    parsed: std::sync::Mutex<Vec<PathBuf>>,
}

impl RecordingMetadataService {
    fn parsed_paths(&self) -> Vec<PathBuf> {
        self.parsed
            .lock()
            .expect("recording lock not poisoned")
            .clone()
    }
}

impl MetadataService for RecordingMetadataService {
    fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        self.parsed
            .lock()
            .expect("recording lock not poisoned")
            .push(path.to_path_buf());
        Ok(InitialTags {
            metadata: TrackMetadata {
                title: Some("Test".to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            has_embedded_artwork: false,
        })
    }

    fn write_metadata(&self, _path: &Path, _change: super::MetadataChange) -> MetadataResult<()> {
        Ok(())
    }

    fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
        Ok(())
    }

    fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FaultInjectingScanFilesystem {
    unreadable_directory: Option<PathBuf>,
    entry_error_directory: Option<PathBuf>,
    unreadable_path: Option<PathBuf>,
}

impl ScanFilesystem for FaultInjectingScanFilesystem {
    fn is_directory(&self, path: &Path) -> bool {
        StdScanFilesystem.is_directory(path)
    }

    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>> {
        if self.unreadable_directory.as_deref() == Some(path) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let entries = StdScanFilesystem.read_directory(path)?;
        if self.entry_error_directory.as_deref() == Some(path) {
            return Ok(Box::new(entries.chain(std::iter::once(Err(
                io::Error::from(io::ErrorKind::Other),
            )))));
        }
        Ok(entries)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        if self.unreadable_path.as_deref() == Some(path) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        StdScanFilesystem.symlink_metadata(path)
    }
}

fn unique_test_directory() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    // A wall-clock timestamp is not actually unique: two tests on
    // parallel harness threads can read the same tick (or the clock can
    // step backwards), landing in the same directory and racing each
    // other's `remove_dir_all`. Mirror the production temp-name scheme
    // (`temporary_sibling_name`) instead: a process id plus a monotonic
    // counter is collision-free within and across runs.
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sustain_metadata_test_{}_{id}", std::process::id()))
}
