// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Filesystem primitives for the managed library: verified copies and
//! hard-link moves that never overwrite an existing destination, plus the
//! small `stat`-based helpers the import, consolidation, and journal-recovery
//! paths share.

use std::{
    fs::{self, File},
    io,
    io::{BufReader, BufWriter, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use rustix::{
    fd::OwnedFd,
    fs::{AtFlags, CWD, Mode, OFlags, RenameFlags, linkat, openat, renameat_with, unlinkat},
    io::Errno,
};
use sustain_domain::TrackContentHash;
use sustain_metadata::{copy_and_hash_reader_content, hash_reader_content};

#[derive(Clone, Debug)]
pub(crate) struct VerifiedFileCopy {
    pub(crate) destination_path: PathBuf,
    pub(crate) bytes_copied: u64,
    capability: RegularFileCapability,
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedFileStaging {
    temporary_path: PathBuf,
    pub(crate) bytes_copied: u64,
    pub(crate) content_hash: TrackContentHash,
    capability: RegularFileCapability,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct RegularFileCapability {
    file: Arc<File>,
    identity: FileIdentity,
}

impl RegularFileCapability {
    pub(crate) fn identity(&self) -> FileIdentity {
        self.identity
    }

    pub(crate) fn try_clone_file(&self) -> io::Result<File> {
        self.file.try_clone()
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct EmptyDirectoryPruneOutcome {
    pub(crate) failed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EmptyDirectoryPruneError {
    RootUnavailable,
    SourceOutsideManagedRoot,
    InspectDirectoryFailed,
    RemoveDirectoryFailed,
    SyncParentDirectoryFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum VerifiedFileCopyError {
    SourceUnavailable,
    SourceIsNotFile,
    DestinationHasNoParent,
    DestinationExists,
    CreateDestinationDirectoryFailed,
    CreateTemporaryFileFailed,
    CopyFailed,
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    HashMismatch {
        expected: TrackContentHash,
        actual: TrackContentHash,
    },
    SyncCopiedFileFailed,
    FinalizeFailed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileMoveError {
    SourceUnavailable,
    SourceIsNotFile,
    DestinationHasNoParent,
    DestinationExists,
    CreateDestinationDirectoryFailed,
    SourceChanged,
    LinkFailed,
    SyncDestinationDirectoryFailed,
    RemoveSourceFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FileTrashError {
    SourceUnavailable,
    SourceIsNotFile,
    SourceChanged,
    StageFailed,
    SyncParentDirectoryFailed,
    TrashBackendFailed,
    RestoreFailed,
}

pub(crate) fn copy_file_verified(
    source_path: &Path,
    destination_path: &Path,
    expected_hash: &TrackContentHash,
) -> Result<VerifiedFileCopy, VerifiedFileCopyError> {
    if destination_path.exists() {
        return Err(VerifiedFileCopyError::DestinationExists);
    }

    let destination_parent = destination_path
        .parent()
        .ok_or(VerifiedFileCopyError::DestinationHasNoParent)?;
    ensure_directory_all(destination_parent)
        .map_err(|_| VerifiedFileCopyError::CreateDestinationDirectoryFailed)?;
    let staging = copy_file_to_staging_verified(source_path, destination_parent)?;
    if &staging.content_hash != expected_hash {
        let actual = staging.content_hash.clone();
        remove_staged_file(staging);
        return Err(VerifiedFileCopyError::HashMismatch {
            expected: expected_hash.clone(),
            actual,
        });
    }
    publish_staged_file(staging, destination_path)
}

pub(crate) fn copy_file_to_staging_verified(
    source_path: &Path,
    staging_directory: &Path,
) -> Result<VerifiedFileStaging, VerifiedFileCopyError> {
    let source = open_regular_file(source_path).map_err(map_source_copy_error)?;
    let source_metadata = source
        .file
        .metadata()
        .map_err(|_| VerifiedFileCopyError::SourceUnavailable)?;
    ensure_directory_all(staging_directory)
        .map_err(|_| VerifiedFileCopyError::CreateDestinationDirectoryFailed)?;
    let temporary_path = create_temporary_copy_path(staging_directory)?;
    let temporary = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(|_| VerifiedFileCopyError::CreateTemporaryFileFailed)?;
    let capability = regular_file_capability_from_file(temporary)
        .map_err(|_| VerifiedFileCopyError::CreateTemporaryFileFailed)?;
    let result = copy_file_to_staging_verified_inner(
        &source,
        &capability,
        source_metadata.len(),
        source_metadata.permissions(),
    );
    match result {
        Ok((bytes_copied, content_hash)) => Ok(VerifiedFileStaging {
            temporary_path,
            bytes_copied,
            content_hash,
            capability,
        }),
        Err(error) => {
            let _ = remove_regular_file_matching_capability(&temporary_path, &capability);
            Err(error)
        }
    }
}

fn copy_file_to_staging_verified_inner(
    source: &RegularFileCapability,
    temporary: &RegularFileCapability,
    expected_size: u64,
    source_permissions: fs::Permissions,
) -> Result<(u64, TrackContentHash), VerifiedFileCopyError> {
    let mut reader = BufReader::new(
        source
            .file
            .try_clone()
            .map_err(|_| VerifiedFileCopyError::CopyFailed)?,
    );
    let mut writer = BufWriter::new(
        temporary
            .file
            .try_clone()
            .map_err(|_| VerifiedFileCopyError::CopyFailed)?,
    );
    let (bytes_copied, content_hash) = copy_and_hash_reader_content(&mut reader, &mut writer)
        .map_err(|_| VerifiedFileCopyError::CopyFailed)?;
    writer
        .flush()
        .map_err(|_| VerifiedFileCopyError::CopyFailed)?;
    if bytes_copied != expected_size {
        return Err(VerifiedFileCopyError::SizeMismatch {
            expected: expected_size,
            actual: bytes_copied,
        });
    }

    temporary
        .file
        .set_permissions(source_permissions)
        .map_err(|_| VerifiedFileCopyError::CopyFailed)?;
    temporary
        .file
        .sync_all()
        .map_err(|_| VerifiedFileCopyError::SyncCopiedFileFailed)?;
    let mut staged_reader = temporary
        .file
        .try_clone()
        .map_err(|_| VerifiedFileCopyError::CopyFailed)?;
    staged_reader
        .seek(SeekFrom::Start(0))
        .map_err(|_| VerifiedFileCopyError::CopyFailed)?;
    let actual_hash =
        hash_reader_content(&mut staged_reader).map_err(|_| VerifiedFileCopyError::CopyFailed)?;
    if actual_hash != content_hash {
        return Err(VerifiedFileCopyError::HashMismatch {
            expected: content_hash,
            actual: actual_hash,
        });
    }
    Ok((bytes_copied, actual_hash))
}

pub(crate) fn publish_staged_file(
    staging: VerifiedFileStaging,
    destination_path: &Path,
) -> Result<VerifiedFileCopy, VerifiedFileCopyError> {
    if destination_path.exists() {
        remove_staged_file(staging);
        return Err(VerifiedFileCopyError::DestinationExists);
    }
    let destination_parent = destination_path
        .parent()
        .ok_or(VerifiedFileCopyError::DestinationHasNoParent)?;
    ensure_directory_all(destination_parent)
        .map_err(|_| VerifiedFileCopyError::CreateDestinationDirectoryFailed)?;

    // `rename` replaces existing files on Unix. A hard link created in the same
    // directory gives us no-overwrite finalization: it fails if the destination
    // appeared while we were copying. The inode is synced before publication;
    // syncing the directory after the link and again after removing the temp
    // name makes the final namespace durable before SQLite can reference it.
    link_file_capability(&staging.capability, destination_path).map_err(|_| {
        remove_staged_file(staging.clone());
        VerifiedFileCopyError::FinalizeFailed
    })?;
    if sync_parent_directory(destination_path).is_err() {
        let _ = remove_regular_file_matching_capability(destination_path, &staging.capability);
        remove_staged_file(staging);
        return Err(VerifiedFileCopyError::FinalizeFailed);
    }
    if remove_regular_file_matching_capability(&staging.temporary_path, &staging.capability)
        .is_err()
    {
        let _ = remove_regular_file_matching_capability(destination_path, &staging.capability);
        return Err(VerifiedFileCopyError::FinalizeFailed);
    }

    Ok(VerifiedFileCopy {
        destination_path: destination_path.to_path_buf(),
        bytes_copied: staging.bytes_copied,
        capability: staging.capability,
    })
}

pub(crate) fn remove_staged_file(staging: VerifiedFileStaging) {
    let _ = remove_regular_file_matching_capability(&staging.temporary_path, &staging.capability);
}

fn create_temporary_copy_path(parent: &Path) -> Result<PathBuf, VerifiedFileCopyError> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();

    for attempt in 0..100u32 {
        let temporary_name = format!(
            ".sustain-copy-{}-{unique}-{attempt}.tmp",
            std::process::id()
        );
        let temporary_path = parent.join(temporary_name);
        if !temporary_path.exists() {
            return Ok(temporary_path);
        }
    }

    Err(VerifiedFileCopyError::CreateTemporaryFileFailed)
}

pub(super) fn move_file_without_copy_or_overwrite(
    source_path: &Path,
    destination_path: &Path,
) -> Result<(), FileMoveError> {
    let source = open_regular_file(source_path)?;
    move_file_without_copy_or_overwrite_matching_capability(source_path, destination_path, &source)
}

pub(super) fn move_file_without_copy_or_overwrite_matching_capability(
    source_path: &Path,
    destination_path: &Path,
    source: &RegularFileCapability,
) -> Result<(), FileMoveError> {
    if !path_refers_to_capability(source_path, source)? {
        return Err(FileMoveError::SourceChanged);
    }
    if destination_path.exists() {
        return Err(FileMoveError::DestinationExists);
    }

    let destination_parent = destination_path
        .parent()
        .ok_or(FileMoveError::DestinationHasNoParent)?;
    ensure_directory_all(destination_parent)
        .map_err(|_| FileMoveError::CreateDestinationDirectoryFailed)?;
    if destination_path.exists() {
        return Err(FileMoveError::DestinationExists);
    }

    link_file_capability(source, destination_path).map_err(|_| FileMoveError::LinkFailed)?;
    if !path_refers_to_capability(destination_path, source).unwrap_or(false) {
        let _ = remove_regular_file_matching_capability(destination_path, source);
        return Err(FileMoveError::SourceChanged);
    }
    if sync_parent_directory(destination_path).is_err() {
        let _ = remove_regular_file_matching_capability(destination_path, source);
        return Err(FileMoveError::SyncDestinationDirectoryFailed);
    }
    if remove_regular_file_matching_capability(source_path, source).is_err() {
        let _ = remove_regular_file_matching_capability(destination_path, source);
        return Err(FileMoveError::RemoveSourceFailed);
    }

    Ok(())
}

pub(crate) fn open_regular_file(path: &Path) -> Result<RegularFileCapability, FileMoveError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| FileMoveError::SourceUnavailable)?;
    regular_file_capability_from_file(file)
}

pub(crate) fn regular_file_capability_from_file(
    file: File,
) -> Result<RegularFileCapability, FileMoveError> {
    let metadata = file
        .metadata()
        .map_err(|_| FileMoveError::SourceUnavailable)?;
    if !metadata.file_type().is_file() {
        return Err(FileMoveError::SourceIsNotFile);
    }
    Ok(RegularFileCapability {
        identity: FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
        file: Arc::new(file),
    })
}

fn map_source_copy_error(error: FileMoveError) -> VerifiedFileCopyError {
    match error {
        FileMoveError::SourceIsNotFile => VerifiedFileCopyError::SourceIsNotFile,
        _ => VerifiedFileCopyError::SourceUnavailable,
    }
}

pub(crate) fn path_refers_to_capability(
    path: &Path,
    capability: &RegularFileCapability,
) -> Result<bool, FileMoveError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        } == capability.identity),
        Ok(_) => Err(FileMoveError::SourceIsNotFile),
        Err(_) => Err(FileMoveError::SourceUnavailable),
    }
}

pub(crate) fn link_file_capability(
    capability: &RegularFileCapability,
    destination_path: &Path,
) -> io::Result<()> {
    let source = proc_self_fd_path(capability.file.as_raw_fd());
    linkat(CWD, &source, CWD, destination_path, AtFlags::SYMLINK_FOLLOW).map_err(io::Error::from)
}

fn proc_self_fd_path(fd: RawFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{fd}"))
}

pub(crate) fn regular_file_identity(path: &Path) -> Result<FileIdentity, FileMoveError> {
    let metadata = fs::symlink_metadata(path).map_err(|_| FileMoveError::SourceUnavailable)?;
    if !metadata.file_type().is_file() {
        return Err(FileMoveError::SourceIsNotFile);
    }
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(crate) fn open_regular_file_if_present(
    path: &Path,
) -> Result<Option<RegularFileCapability>, FileTrashError> {
    match open_regular_file(path) {
        Ok(source) => Ok(Some(source)),
        Err(FileMoveError::SourceUnavailable)
            if fs::symlink_metadata(path)
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound) =>
        {
            Ok(None)
        }
        Err(FileMoveError::SourceIsNotFile) => Err(FileTrashError::SourceIsNotFile),
        Err(_) => Err(FileTrashError::SourceUnavailable),
    }
}

/// Move the open regular file to an unpredictable sibling name before handing
/// a pathname to a trash backend. The retained descriptor pins the inode, so a
/// replacement pathname cannot pass verification through immediate inode
/// reuse. If the source changes before the rename, the unrelated entry is
/// restored and retained.
pub(crate) fn trash_regular_file_matching_capability(
    path: &Path,
    source: &RegularFileCapability,
    trash_backend: impl FnOnce(&Path) -> Result<(), ()>,
) -> Result<(), FileTrashError> {
    let handoff_path = temporary_trash_handoff_path(path)?;
    renameat_with(CWD, path, CWD, &handoff_path, RenameFlags::NOREPLACE)
        .map_err(|_| FileTrashError::StageFailed)?;
    if sync_parent_directory(path).is_err() {
        rollback_trash_handoff(&handoff_path, path)?;
        return Err(FileTrashError::SyncParentDirectoryFailed);
    }

    if !path_refers_to_capability(&handoff_path, source).unwrap_or(false) {
        rollback_trash_handoff(&handoff_path, path)?;
        return Err(FileTrashError::SourceChanged);
    }

    if trash_backend(&handoff_path).is_err() {
        rollback_trash_handoff(&handoff_path, path)?;
        return Err(FileTrashError::TrashBackendFailed);
    }
    Ok(())
}

pub(crate) fn remove_regular_file_matching_capability(
    path: &Path,
    source: &RegularFileCapability,
) -> Result<(), FileTrashError> {
    trash_regular_file_matching_capability(path, source, |handoff| {
        fs::remove_file(handoff).map_err(|_| ())
    })
}

fn temporary_trash_handoff_path(path: &Path) -> Result<PathBuf, FileTrashError> {
    static NEXT_HANDOFF_ID: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().ok_or(FileTrashError::StageFailed)?;
    let file_name = path.file_name().ok_or(FileTrashError::StageFailed)?;
    for _ in 0..100 {
        let id = NEXT_HANDOFF_ID.fetch_add(1, Ordering::Relaxed);
        let mut handoff_name = file_name.to_os_string();
        handoff_name.push(format!(".sustain-trash-{}-{id}.tmp", std::process::id()));
        let candidate = parent.join(handoff_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(FileTrashError::StageFailed)
}

fn rollback_trash_handoff(handoff_path: &Path, source_path: &Path) -> Result<(), FileTrashError> {
    renameat_with(CWD, handoff_path, CWD, source_path, RenameFlags::NOREPLACE)
        .map_err(|_| FileTrashError::RestoreFailed)?;
    sync_parent_directory(source_path).map_err(|_| FileTrashError::RestoreFailed)
}

pub(super) fn rollback_file_move(
    source_path: &Path,
    destination_path: &Path,
) -> Result<(), FileMoveError> {
    match (
        path_is_regular_file(source_path),
        path_is_regular_file(destination_path),
    ) {
        (true, true) if paths_refer_to_same_file(source_path, destination_path) => {
            let source = open_regular_file(source_path)?;
            remove_regular_file_matching_capability(destination_path, &source)
                .map_err(|_| FileMoveError::RemoveSourceFailed)
        }
        (true, false) => Ok(()),
        (false, true) => move_file_without_copy_or_overwrite(destination_path, source_path),
        _ => Err(FileMoveError::SourceUnavailable),
    }
}

pub(super) fn path_is_regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_file())
        .unwrap_or(false)
}

pub(super) fn paths_refer_to_same_file(left: &Path, right: &Path) -> bool {
    match (fs::metadata(left), fs::metadata(right)) {
        (Ok(left), Ok(right)) => left.dev() == right.dev() && left.ino() == right.ino(),
        _ => false,
    }
}

pub(crate) fn remove_copied_files(copies: &[VerifiedFileCopy]) -> Result<(), ()> {
    let mut failed = false;
    for copy in copies.iter().rev() {
        if remove_regular_file_matching_capability(&copy.destination_path, &copy.capability)
            .is_err()
        {
            failed = true;
        }
    }
    (!failed).then_some(()).ok_or(())
}

pub(crate) fn remove_file_and_sync_parent(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent_directory(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn prune_empty_ancestor_directories_for_sources(
    library_root: &Path,
    source_paths: &[PathBuf],
) -> EmptyDirectoryPruneOutcome {
    let mut outcome = EmptyDirectoryPruneOutcome::default();
    for source_path in source_paths {
        match prune_empty_ancestor_directories(library_root, source_path) {
            Ok(_) => {}
            Err(error) => {
                eprintln!(
                    "Sustain: could not prune empty managed-library folders above {}: {error:?}",
                    source_path.display()
                );
                outcome.failed = true;
            }
        }
    }
    outcome
}

fn prune_empty_ancestor_directories(
    library_root: &Path,
    source_path: &Path,
) -> Result<usize, EmptyDirectoryPruneError> {
    if source_path.strip_prefix(library_root).is_err() {
        return Err(EmptyDirectoryPruneError::SourceOutsideManagedRoot);
    }
    let root = File::open(library_root).map_err(|_| EmptyDirectoryPruneError::RootUnavailable)?;
    if !root
        .metadata()
        .map_err(|_| EmptyDirectoryPruneError::RootUnavailable)?
        .is_dir()
    {
        return Err(EmptyDirectoryPruneError::RootUnavailable);
    }
    let mut removed_directories = 0;
    let mut directory = source_path.parent();

    while let Some(candidate) = directory {
        if candidate == library_root {
            break;
        }
        if candidate.strip_prefix(library_root).is_err() {
            return Err(EmptyDirectoryPruneError::SourceOutsideManagedRoot);
        }
        let relative_candidate = candidate
            .strip_prefix(library_root)
            .map_err(|_| EmptyDirectoryPruneError::SourceOutsideManagedRoot)?;
        match remove_empty_descendant_directory(&root, relative_candidate)? {
            EmptyDirectoryRemoveOutcome::Removed => {
                removed_directories += 1;
            }
            EmptyDirectoryRemoveOutcome::Missing => {}
            EmptyDirectoryRemoveOutcome::Stop => break,
        }

        directory = candidate.parent();
    }

    Ok(removed_directories)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EmptyDirectoryRemoveOutcome {
    Removed,
    Missing,
    Stop,
}

fn remove_empty_descendant_directory(
    root: &File,
    relative_directory: &Path,
) -> Result<EmptyDirectoryRemoveOutcome, EmptyDirectoryPruneError> {
    let mut components = relative_directory.components().collect::<Vec<_>>();
    let Some(Component::Normal(directory_name)) = components.pop() else {
        return Err(EmptyDirectoryPruneError::SourceOutsideManagedRoot);
    };
    let parent = match open_descendant_directory(root, &components) {
        Ok(parent) => parent,
        Err(DescendantDirectoryOpenError::Missing) => {
            return Ok(EmptyDirectoryRemoveOutcome::Missing);
        }
        Err(DescendantDirectoryOpenError::Unsafe) => {
            return Ok(EmptyDirectoryRemoveOutcome::Stop);
        }
        Err(DescendantDirectoryOpenError::Failed) => {
            return Err(EmptyDirectoryPruneError::InspectDirectoryFailed);
        }
    };

    match unlinkat(&parent, directory_name, AtFlags::REMOVEDIR) {
        Ok(()) => {
            File::from(parent)
                .sync_all()
                .map_err(|_| EmptyDirectoryPruneError::SyncParentDirectoryFailed)?;
            Ok(EmptyDirectoryRemoveOutcome::Removed)
        }
        Err(Errno::NOENT) => Ok(EmptyDirectoryRemoveOutcome::Missing),
        Err(Errno::LOOP | Errno::NOTDIR | Errno::NOTEMPTY) => Ok(EmptyDirectoryRemoveOutcome::Stop),
        Err(_) => Err(EmptyDirectoryPruneError::RemoveDirectoryFailed),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DescendantDirectoryOpenError {
    Missing,
    Unsafe,
    Failed,
}

fn open_descendant_directory(
    root: &File,
    components: &[Component<'_>],
) -> Result<OwnedFd, DescendantDirectoryOpenError> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory =
        openat(root, ".", flags, Mode::empty()).map_err(map_descendant_directory_open_error)?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(DescendantDirectoryOpenError::Unsafe);
        };
        directory = openat(&directory, *name, flags, Mode::empty())
            .map_err(map_descendant_directory_open_error)?;
    }
    Ok(directory)
}

fn map_descendant_directory_open_error(error: Errno) -> DescendantDirectoryOpenError {
    match error {
        Errno::NOENT => DescendantDirectoryOpenError::Missing,
        Errno::LOOP | Errno::NOTDIR => DescendantDirectoryOpenError::Unsafe,
        _ => DescendantDirectoryOpenError::Failed,
    }
}

pub(crate) fn ensure_directory_all(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "managed-library destination component is not a directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "managed-library destination directory has no parent",
        )
    })?;
    ensure_directory_all(parent)?;
    match fs::create_dir(path) {
        Ok(()) => sync_directory(parent),
        Err(error)
            if error.kind() == io::ErrorKind::AlreadyExists
                && fs::symlink_metadata(path)
                    .is_ok_and(|metadata| metadata.file_type().is_dir()) =>
        {
            Ok(())
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn sync_parent_directory(path: &Path) -> io::Result<()> {
    path.parent()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "managed-library path has no parent directory",
            )
        })
        .and_then(sync_directory)
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path).and_then(|directory| directory.sync_all())
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};

    use sustain_metadata::hash_file_content;

    use super::{
        EmptyDirectoryPruneError, FileMoveError, FileTrashError, VerifiedFileCopyError,
        copy_file_verified, open_regular_file, prune_empty_ancestor_directories,
        trash_regular_file_matching_capability,
    };

    #[test]
    fn verified_copy_copies_and_verifies_file_before_finalizing() {
        let root = unique_test_directory();
        let source = root.join("source.flac");
        let destination = root.join("Artist").join("Album").join("01 Song.flac");
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(&source, b"audio bytes").expect("write source");
        let hash = hash_file_content(&source).expect("hash source");

        let copy = copy_file_verified(&source, &destination, &hash).expect("copy succeeds");

        assert_eq!(copy.destination_path, destination);
        assert_eq!(copy.bytes_copied, 11);
        assert_eq!(
            fs::read(&copy.destination_path).expect("read dest"),
            b"audio bytes"
        );
        assert_no_temporary_files(&root);

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn verified_copy_refuses_to_overwrite_existing_destination() {
        let root = unique_test_directory();
        let source = root.join("source.flac");
        let destination = root.join("dest.flac");
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(&source, b"new bytes").expect("write source");
        fs::write(&destination, b"existing bytes").expect("write destination");
        let hash = hash_file_content(&source).expect("hash source");

        let result = copy_file_verified(&source, &destination, &hash);

        assert!(matches!(
            result,
            Err(VerifiedFileCopyError::DestinationExists)
        ));
        assert_eq!(
            fs::read(&destination).expect("read destination"),
            b"existing bytes"
        );
        assert_no_temporary_files(&root);

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn verified_copy_removes_temporary_file_on_hash_mismatch() {
        let root = unique_test_directory();
        let source = root.join("source.flac");
        let destination = root.join("dest.flac");
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(&source, b"new bytes").expect("write source");
        let wrong_hash = sustain_domain::TrackContentHash::new(
            "0000000000000000000000000000000000000000000000000000000000000000",
        )
        .expect("valid hash");

        let result = copy_file_verified(&source, &destination, &wrong_hash);

        assert!(matches!(
            result,
            Err(VerifiedFileCopyError::HashMismatch { .. })
        ));
        assert!(!destination.exists());
        assert_no_temporary_files(&root);

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn managed_move_uses_metadata_operations_and_refuses_overwrite() {
        let root = unique_test_directory();
        let source = root.join("source.flac");
        let destination = root.join("Artist").join("Album").join("01 Song.flac");
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(&source, b"audio bytes").expect("write source");
        let source_metadata = fs::metadata(&source).expect("source metadata");

        super::move_file_without_copy_or_overwrite(&source, &destination).expect("move succeeds");

        assert!(!source.exists());
        let destination_metadata = fs::metadata(&destination).expect("destination metadata");
        assert_eq!(source_metadata.dev(), destination_metadata.dev());
        assert_eq!(source_metadata.ino(), destination_metadata.ino());

        let second_source = root.join("second.flac");
        fs::write(&second_source, b"other bytes").expect("write second source");
        assert_eq!(
            super::move_file_without_copy_or_overwrite(&second_source, &destination),
            Err(FileMoveError::DestinationExists)
        );
        assert!(second_source.exists());

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn managed_move_refuses_source_inode_replacement_after_planning() {
        let root = unique_test_directory();
        let source = root.join("source.flac");
        let destination = root.join("Artist").join("Album").join("01 Song.flac");
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(&source, b"original bytes").expect("write source");
        let source_capability = open_regular_file(&source).expect("open source");
        fs::remove_file(&source).expect("remove original source");
        fs::write(&source, b"replacement bytes").expect("write replacement source");

        assert_eq!(
            super::move_file_without_copy_or_overwrite_matching_capability(
                &source,
                &destination,
                &source_capability,
            ),
            Err(FileMoveError::SourceChanged)
        );
        assert_eq!(
            fs::read(&source).expect("read replacement"),
            b"replacement bytes"
        );
        assert!(!destination.exists());

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn trash_handoff_refuses_to_delete_a_replaced_source_path() {
        let root = unique_test_directory();
        let source = root.join("song.flac");
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(&source, b"original bytes").expect("write source");
        let source_capability = open_regular_file(&source).expect("open source");
        fs::remove_file(&source).expect("remove original");
        fs::write(&source, b"unrelated replacement").expect("write replacement");
        let mut backend_called = false;

        let result = trash_regular_file_matching_capability(&source, &source_capability, |_| {
            backend_called = true;
            Ok(())
        });

        assert_eq!(result, Err(FileTrashError::SourceChanged));
        assert!(!backend_called);
        assert_eq!(
            fs::read(&source).expect("read replacement"),
            b"unrelated replacement"
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn trash_handoff_restores_source_when_backend_fails() {
        let root = unique_test_directory();
        let source = root.join("song.flac");
        fs::create_dir_all(&root).expect("create test directory");
        fs::write(&source, b"audio bytes").expect("write source");
        let source_capability = open_regular_file(&source).expect("open source");

        let result =
            trash_regular_file_matching_capability(&source, &source_capability, |_| Err(()));

        assert_eq!(result, Err(FileTrashError::TrashBackendFailed));
        assert_eq!(
            fs::read(&source).expect("read restored source"),
            b"audio bytes"
        );
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn empty_directory_prune_removes_empty_ancestors_but_not_library_root() {
        let root = unique_test_directory();
        let source = root.join("Loose").join("Album").join("song.flac");
        fs::create_dir_all(source.parent().expect("source parent")).expect("create source parent");

        assert_eq!(prune_empty_ancestor_directories(&root, &source), Ok(2));
        assert!(root.exists());
        assert!(!root.join("Loose").exists());

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn empty_directory_prune_stops_at_sidecar_file() {
        let root = unique_test_directory();
        let album = root.join("Loose").join("Album");
        let source = album.join("song.flac");
        fs::create_dir_all(&album).expect("create album");
        fs::write(album.join(".keep"), b"sidecar").expect("write sidecar");

        assert_eq!(prune_empty_ancestor_directories(&root, &source), Ok(0));
        assert!(album.exists());

        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn empty_directory_prune_refuses_paths_outside_library_root() {
        let root = unique_test_directory();
        let outside = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(&outside).expect("create outside");

        assert_eq!(
            prune_empty_ancestor_directories(&root, &outside.join("song.flac")),
            Err(EmptyDirectoryPruneError::SourceOutsideManagedRoot)
        );
        assert!(outside.exists());

        fs::remove_dir_all(root).expect("remove test root");
        fs::remove_dir_all(outside).expect("remove test outside");
    }

    #[test]
    fn empty_directory_prune_never_follows_symlink_out_of_library_root() {
        let root = unique_test_directory();
        let outside = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(outside.join("Album")).expect("create outside album");
        std::os::unix::fs::symlink(&outside, root.join("linked")).expect("create symlink");

        assert_eq!(
            prune_empty_ancestor_directories(&root, &root.join("linked/Album/song.flac")),
            Ok(0)
        );
        assert!(outside.join("Album").exists());
        assert!(root.join("linked").exists());

        fs::remove_dir_all(root).expect("remove test root");
        fs::remove_dir_all(outside).expect("remove test outside");
    }

    #[test]
    fn managed_directory_creation_refuses_symlink_components() {
        let root = unique_test_directory();
        let outside = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        fs::create_dir_all(&outside).expect("create outside");
        std::os::unix::fs::symlink(&outside, root.join("linked")).expect("create symlink");

        assert!(super::ensure_directory_all(&root.join("linked/Album")).is_err());
        assert!(!outside.join("Album").exists());

        fs::remove_dir_all(root).expect("remove test root");
        fs::remove_dir_all(outside).expect("remove test outside");
    }

    fn assert_no_temporary_files(root: &std::path::Path) {
        let entries = fs::read_dir(root).expect("read test directory");
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            assert!(
                !name.contains(".sustain-copy-"),
                "temporary file left behind: {name}"
            );
        }
    }

    fn unique_test_directory() -> PathBuf {
        let unique_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sustain_managed_copy_test_{unique_suffix}"))
    }
}
