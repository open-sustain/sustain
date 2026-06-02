// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Crash recovery for YouTube audio replacement publication.

use std::{
    fs,
    io::{Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rustix::fs::{CWD, RenameFlags, renameat_with};
use sustain_domain::{TrackContentHash, TrackId, TrackRelativePath};
use sustain_library_store::LibraryStore;
use sustain_metadata::hash_file_content;

use crate::{
    ApplicationRuntimeError, ApplicationRuntimeResult,
    managed_library::file_ops::{
        FileIdentity, open_regular_file, regular_file_identity, remove_file_and_sync_parent,
        remove_regular_file_matching_capability, sync_directory,
    },
};

const JOURNAL_FILE_NAME: &str = ".sustain-youtube-replacement-journal";
const JOURNAL_HEADER: &str = "# sustain youtube audio replacement journal v1";
const MAX_JOURNAL_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct YoutubeReplacementJournalEntry {
    pub(crate) track_id: TrackId,
    pub(crate) original_identity: FileIdentity,
    pub(crate) original_relative_path: TrackRelativePath,
    pub(crate) replacement_content_hash: TrackContentHash,
    pub(crate) replacement_size_bytes: u64,
    pub(crate) replacement_relative_path: TrackRelativePath,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct YoutubeReplacementRecoveryOutcome {
    pub(crate) original_retained: bool,
}

pub(crate) fn write_youtube_replacement_journal(
    library_root: &Path,
    entry: &YoutubeReplacementJournalEntry,
) -> ApplicationRuntimeResult<()> {
    if regular_file_identity(&entry.original_relative_path.resolve(library_root))
        != Ok(entry.original_identity)
        || !path_entry_is_missing(&entry.replacement_relative_path.resolve(library_root))?
    {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }

    let journal_path = journal_path(library_root);
    if !path_entry_is_missing(&journal_path)? {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    let temporary_path = temporary_journal_path(library_root);
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
            .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        writeln!(file, "{JOURNAL_HEADER}")
            .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        writeln!(
            file,
            "replace\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            entry.track_id.get(),
            entry.original_identity.device,
            entry.original_identity.inode,
            entry.replacement_content_hash.as_str(),
            entry.replacement_size_bytes,
            encode_relative_path(&entry.original_relative_path),
            encode_relative_path(&entry.replacement_relative_path),
        )
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        file.flush()
            .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        file.sync_all()
            .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        renameat_with(
            CWD,
            &temporary_path,
            CWD,
            &journal_path,
            RenameFlags::NOREPLACE,
        )
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        sync_directory(library_root)
            .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)
    })();
    if result.is_err() {
        let _ = remove_file_and_sync_parent(&temporary_path);
    }
    result
}

pub(crate) fn remove_youtube_replacement_journal_if_present(
    library_root: &Path,
) -> ApplicationRuntimeResult<()> {
    let path = journal_path(library_root);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => {
            let journal = open_regular_file(&path)
                .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
            remove_regular_file_matching_capability(&path, &journal)
                .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)
        }
        Err(_) => Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed),
    }
}

pub(crate) fn recover_youtube_replacement_journal(
    library_root: &Path,
    store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<YoutubeReplacementRecoveryOutcome> {
    recover_youtube_replacement_journal_with(library_root, store, &mut |handoff| {
        trash::delete(handoff).map_err(|_| ())
    })
}

fn recover_youtube_replacement_journal_with(
    library_root: &Path,
    store: &dyn LibraryStore,
    _trash_backend: &mut dyn FnMut(&Path) -> Result<(), ()>,
) -> ApplicationRuntimeResult<YoutubeReplacementRecoveryOutcome> {
    let path = journal_path(library_root);
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(YoutubeReplacementRecoveryOutcome::default());
        }
        Ok(_) => {}
        Err(_) => return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed),
    }
    let entry = read_youtube_replacement_journal(library_root)?;
    let track = store
        .track(entry.track_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    let original_path = entry.original_relative_path.resolve(library_root);
    let replacement_path = entry.replacement_relative_path.resolve(library_root);
    let original = inspect_path(&original_path, entry.original_identity)?;
    let replacement = inspect_replacement_path(
        &replacement_path,
        &entry.replacement_content_hash,
        entry.replacement_size_bytes,
    )?;

    let original_is_authoritative = track.location.relative_path == entry.original_relative_path
        && !track.location.is_missing();
    let replacement_is_authoritative = track.location.relative_path
        == entry.replacement_relative_path
        && !track.location.is_missing();

    let original_retained = if original_is_authoritative {
        if !matches!(original, JournalPathState::Expected(_)) {
            return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
        }
        store
            .flush_durable()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        match replacement {
            JournalPathState::Missing => {}
            JournalPathState::Expected(identity) => {
                remove_matching_file(&replacement_path, identity)?;
            }
            JournalPathState::Unexpected => {
                return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
            }
        }
        false
    } else if replacement_is_authoritative {
        if !matches!(replacement, JournalPathState::Expected(_)) {
            return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
        }
        store
            .flush_durable()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        match original {
            JournalPathState::Missing => false,
            // Recovery has no live descriptor from the original publication
            // attempt. Retain any surviving old pathname rather than deleting
            // a file based on a historical `(device, inode)` pair.
            JournalPathState::Expected(_) => true,
            JournalPathState::Unexpected => true,
        }
    } else {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    };

    remove_youtube_replacement_journal_if_present(library_root)?;
    Ok(YoutubeReplacementRecoveryOutcome { original_retained })
}

fn remove_matching_file(path: &Path, identity: FileIdentity) -> ApplicationRuntimeResult<()> {
    let source = open_regular_file(path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    if source.identity() != identity {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    remove_regular_file_matching_capability(path, &source)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JournalPathState {
    Missing,
    Expected(FileIdentity),
    Unexpected,
}

fn inspect_path(path: &Path, expected: FileIdentity) -> ApplicationRuntimeResult<JournalPathState> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_file()
                && metadata.dev() == expected.device
                && metadata.ino() == expected.inode =>
        {
            Ok(JournalPathState::Expected(expected))
        }
        Ok(_) => Ok(JournalPathState::Unexpected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(JournalPathState::Missing),
        Err(_) => Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed),
    }
}

fn inspect_replacement_path(
    path: &Path,
    expected_content_hash: &TrackContentHash,
    expected_size_bytes: u64,
) -> ApplicationRuntimeResult<JournalPathState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() && metadata.len() == expected_size_bytes => {
            let identity = FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            };
            if hash_file_content(path) == Ok(expected_content_hash.clone())
                && regular_file_identity(path) == Ok(identity)
            {
                Ok(JournalPathState::Expected(identity))
            } else {
                Ok(JournalPathState::Unexpected)
            }
        }
        Ok(_) => Ok(JournalPathState::Unexpected),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(JournalPathState::Missing),
        Err(_) => Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed),
    }
}

fn read_youtube_replacement_journal(
    library_root: &Path,
) -> ApplicationRuntimeResult<YoutubeReplacementJournalEntry> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(journal_path(library_root))
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    if !file
        .metadata()
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
    {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    let mut contents = String::new();
    file.take(MAX_JOURNAL_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    if contents.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    let mut lines = contents.lines();
    if lines.next() != Some(JOURNAL_HEADER) {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    let mut parts = lines
        .next()
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?
        .split('\t');
    let entry = YoutubeReplacementJournalEntry {
        track_id: match parts.next() {
            Some("replace") => parts
                .next()
                .and_then(|value| value.parse::<i64>().ok())
                .and_then(TrackId::new)
                .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?,
            _ => return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed),
        },
        original_identity: FileIdentity {
            device: parse_u64(parts.next())?,
            inode: parse_u64(parts.next())?,
        },
        replacement_content_hash: parts
            .next()
            .and_then(TrackContentHash::new)
            .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?,
        replacement_size_bytes: parse_u64(parts.next())?,
        original_relative_path: parts
            .next()
            .and_then(decode_relative_path)
            .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?,
        replacement_relative_path: parts
            .next()
            .and_then(decode_relative_path)
            .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?,
    };
    if parts.next().is_some() || lines.any(|line| !line.trim().is_empty()) {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    Ok(entry)
}

fn path_entry_is_missing(path: &Path) -> ApplicationRuntimeResult<bool> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Ok(_) => Ok(false),
        Err(_) => Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed),
    }
}

fn parse_u64(value: Option<&str>) -> ApplicationRuntimeResult<u64> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
}

fn journal_path(library_root: &Path) -> PathBuf {
    library_root.join(JOURNAL_FILE_NAME)
}

fn temporary_journal_path(library_root: &Path) -> PathBuf {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    library_root.join(format!(
        ".sustain-youtube-replacement-journal-{}-{unique}.tmp",
        std::process::id()
    ))
}

fn encode_relative_path(relative_path: &TrackRelativePath) -> String {
    relative_path
        .as_path()
        .as_os_str()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_relative_path(value: &str) -> Option<TrackRelativePath> {
    if value.len() % 2 != 0 {
        return None;
    }
    let bytes = value
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| Some((hex_value(chunk[0])? << 4) | hex_value(chunk[1])?))
        .collect::<Option<Vec<_>>>()?;
    TrackRelativePath::new(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{CWD, Mode, mkfifoat};
    use std::os::unix::fs::symlink;
    use sustain_domain::{
        PlayStatistics, Rating, Track, TrackAudioProperties, TrackLocation, TrackMetadata,
    };
    use sustain_library_store::InMemoryLibraryStore;

    #[test]
    fn recovery_rejects_symlink_journal() {
        let root = tempfile::tempdir().expect("library root");
        let external = tempfile::NamedTempFile::new().expect("external file");
        symlink(external.path(), journal_path(root.path())).expect("create journal symlink");

        assert_eq!(
            recover_youtube_replacement_journal(root.path(), &InMemoryLibraryStore::new()),
            Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
        );
    }

    #[test]
    fn recovery_rejects_fifo_journal_without_blocking() {
        let root = tempfile::tempdir().expect("library root");
        mkfifoat(CWD, journal_path(root.path()), Mode::RUSR | Mode::WUSR)
            .expect("create journal FIFO");

        assert_eq!(
            recover_youtube_replacement_journal(root.path(), &InMemoryLibraryStore::new()),
            Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
        );
    }

    #[test]
    fn recovery_rolls_back_published_copy_when_sqlite_still_names_original() {
        let fixture = Fixture::new();
        let mut trash_called = false;

        let outcome = recover_youtube_replacement_journal_with(
            fixture.root.path(),
            &fixture.store,
            &mut |_| {
                trash_called = true;
                Err(())
            },
        )
        .expect("recover");

        assert_eq!(outcome, YoutubeReplacementRecoveryOutcome::default());
        assert!(!trash_called);
        assert!(fixture.original_path.exists());
        assert!(!fixture.replacement_path.exists());
        assert!(!journal_path(fixture.root.path()).exists());
    }

    #[test]
    fn recovery_clears_intent_when_copy_was_not_published() {
        let fixture = Fixture::intent_only();
        let mut trash_called = false;

        let outcome = recover_youtube_replacement_journal_with(
            fixture.root.path(),
            &fixture.store,
            &mut |_| {
                trash_called = true;
                Err(())
            },
        )
        .expect("recover");

        assert_eq!(outcome, YoutubeReplacementRecoveryOutcome::default());
        assert!(!trash_called);
        assert!(fixture.original_path.exists());
        assert!(!fixture.replacement_path.exists());
        assert!(!journal_path(fixture.root.path()).exists());
    }

    #[test]
    fn recovery_retains_old_source_after_sqlite_rebind_without_a_live_capability() {
        let fixture = Fixture::new();
        fixture.rebind_to_replacement();
        let mut trash_called = false;

        let outcome = recover_youtube_replacement_journal_with(
            fixture.root.path(),
            &fixture.store,
            &mut |_| {
                trash_called = true;
                Ok(())
            },
        )
        .expect("recover");

        assert!(outcome.original_retained);
        assert!(!trash_called);
        assert!(fixture.original_path.exists());
        assert!(fixture.replacement_path.exists());
        assert!(!journal_path(fixture.root.path()).exists());
    }

    #[test]
    fn recovery_retains_unexpected_old_path_after_sqlite_rebind() {
        let fixture = Fixture::new();
        fixture.rebind_to_replacement();
        fs::remove_file(&fixture.original_path).expect("remove original");
        fs::write(&fixture.original_path, b"unrelated bytes").expect("replace original path");
        let mut trash_called = false;

        let outcome = recover_youtube_replacement_journal_with(
            fixture.root.path(),
            &fixture.store,
            &mut |_| {
                trash_called = true;
                Err(())
            },
        )
        .expect("recover");

        assert!(outcome.original_retained);
        assert!(!trash_called);
        assert_eq!(
            fs::read(&fixture.original_path).expect("read unrelated file"),
            b"unrelated bytes"
        );
        assert!(!journal_path(fixture.root.path()).exists());
    }

    #[test]
    fn recovery_retains_journal_when_replacement_identity_is_uncertain() {
        let fixture = Fixture::new();
        fixture.rebind_to_replacement();
        fs::remove_file(&fixture.replacement_path).expect("remove replacement");
        fs::write(&fixture.replacement_path, b"unrelated bytes!!").expect("replace destination");

        assert_eq!(
            recover_youtube_replacement_journal_with(
                fixture.root.path(),
                &fixture.store,
                &mut |_| Ok(()),
            ),
            Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
        );
        assert!(journal_path(fixture.root.path()).exists());
    }

    struct Fixture {
        root: tempfile::TempDir,
        store: InMemoryLibraryStore,
        original_path: PathBuf,
        replacement_path: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let fixture = Self::intent_only();
            fs::write(&fixture.replacement_path, b"replacement audio")
                .expect("publish replacement");
            fixture
        }

        fn intent_only() -> Self {
            let root = tempfile::tempdir().expect("library root");
            let original_path = root.path().join("old.mp3");
            let replacement_path = root.path().join("new.opus");
            fs::write(&original_path, b"old audio").expect("write original");
            fs::write(&replacement_path, b"replacement audio").expect("write replacement");
            let replacement_content_hash =
                hash_file_content(&replacement_path).expect("replacement content hash");
            fs::remove_file(&replacement_path).expect("reserve unpublished replacement path");
            let store = InMemoryLibraryStore::new();
            let track_id = TrackId::new(1).expect("track id");
            store
                .save_track(Track {
                    id: track_id,
                    location: TrackLocation::available(relative("old.mp3")),
                    metadata: TrackMetadata::default(),
                    rating: Rating::unrated(),
                    statistics: PlayStatistics::default(),
                    file_size_bytes: Some(9),
                    has_embedded_artwork: Some(false),
                    file_modified_at: None,
                })
                .expect("save track");
            write_youtube_replacement_journal(
                root.path(),
                &YoutubeReplacementJournalEntry {
                    track_id,
                    original_identity: regular_file_identity(&original_path)
                        .expect("original identity"),
                    original_relative_path: relative("old.mp3"),
                    replacement_content_hash,
                    replacement_size_bytes: b"replacement audio".len() as u64,
                    replacement_relative_path: relative("new.opus"),
                },
            )
            .expect("write journal");
            Self {
                root,
                store,
                original_path,
                replacement_path,
            }
        }

        fn rebind_to_replacement(&self) {
            self.store
                .replace_track_audio(
                    TrackId::new(1).expect("track id"),
                    &TrackLocation::available(relative("new.opus")),
                    TrackAudioProperties::default(),
                    b"replacement audio".len() as u64,
                    false,
                )
                .expect("replace track audio");
        }
    }

    fn relative(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(path).expect("relative path")
    }
}
