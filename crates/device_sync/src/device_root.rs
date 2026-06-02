// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Root-confined filesystem access for removable media.
//!
//! A connected device is untrusted storage. Every mutation is anchored to one
//! opened mount-root directory descriptor and every descendant component is
//! opened with `O_NOFOLLOW`. This prevents a crafted drive from redirecting a
//! joined pathname through symlinks into the host filesystem.

use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{self, Read, Write},
    mem::MaybeUninit,
    os::fd::OwnedFd,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::{
    fs::{
        AtFlags, FileType, Mode, OFlags, RawDir, fstat, fstatvfs, fsync, mkdirat, open, openat,
        renameat, statat, unlinkat,
    },
    io::{Errno, dup},
};
use sustain_domain::{DeviceRelativePath, SourceFileStat};

use crate::{model::DeviceCapacity, source::ensure_source_unchanged};

const DIRECTORY_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::XUSR)
    .union(Mode::RGRP)
    .union(Mode::XGRP)
    .union(Mode::ROTH)
    .union(Mode::XOTH);
const FILE_MODE: Mode = Mode::RUSR
    .union(Mode::WUSR)
    .union(Mode::RGRP)
    .union(Mode::ROTH);
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
/// A removable device is untrusted storage. Recursive removal and stale-file
/// cleanup stop after this much directory work instead of walking an
/// arbitrarily large crafted tree.
#[cfg(not(test))]
const MAX_DIRECTORY_WORK_ENTRIES: usize = 20_000;
// Keep the adversarial breadth regression quick while exercising the same
// production branch.
#[cfg(test)]
const MAX_DIRECTORY_WORK_ENTRIES: usize = 128;
const MAX_DIRECTORY_DEPTH: usize = 128;

/// One opened device mount root. All descendant operations are relative to
/// `fd`; no caller receives an ambient joined path for mutation.
pub(crate) struct DeviceRoot {
    fd: OwnedFd,
}

impl DeviceRoot {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let fd = open(path, DIRECTORY_FLAGS, Mode::empty()).map_err(io::Error::from)?;
        Ok(Self { fd })
    }

    pub(crate) fn ensure_dir_all(&self, path: &DeviceRelativePath) -> io::Result<()> {
        self.open_dir(path, true).map(drop)
    }

    pub(crate) fn capacity(&self) -> io::Result<DeviceCapacity> {
        let stat = fstatvfs(&self.fd).map_err(io::Error::from)?;
        Ok(DeviceCapacity {
            total_bytes: stat.f_blocks.saturating_mul(stat.f_frsize),
            available_bytes: stat.f_bavail.saturating_mul(stat.f_frsize),
        })
    }

    pub(crate) fn is_regular_file(&self, path: &DeviceRelativePath) -> io::Result<bool> {
        self.regular_file_len(path).map(|len| len.is_some())
    }

    pub(crate) fn regular_file_len(&self, path: &DeviceRelativePath) -> io::Result<Option<u64>> {
        let (parent, file_name) = split_parent(path)?;
        let parent = match self.open_dir(&parent, false) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        match statat(&parent, file_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {
                u64::try_from(stat.st_size).map(Some).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("device file has a negative size: {path}"),
                    )
                })
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device path is not a regular file: {path}"),
            )),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn read_to_string(
        &self,
        path: &DeviceRelativePath,
        limit: u64,
    ) -> io::Result<String> {
        let (parent, file_name) = split_parent(path)?;
        let parent = self.open_dir(&parent, false)?;
        let fd = openat(
            &parent,
            file_name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let stat = fstat(&fd).map_err(io::Error::from)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device path is not a regular file: {path}"),
            ));
        }
        let mut bytes = Vec::new();
        File::from(fd).take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device file exceeds {limit} bytes: {path}"),
            ));
        }
        String::from_utf8(bytes).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device file is not UTF-8: {path}"),
            )
        })
    }

    pub(crate) fn write_file(&self, path: &DeviceRelativePath, bytes: &[u8]) -> io::Result<()> {
        self.publish_file(path, Some(bytes.len() as u64), |file| file.write_all(bytes))
    }

    pub(crate) fn copy_file(
        &self,
        source_path: &Path,
        path: &DeviceRelativePath,
        expected: &SourceFileStat,
    ) -> io::Result<()> {
        let mut source = File::open(source_path)?;
        ensure_source_unchanged(source_path, &source, expected)?;
        self.publish_file(path, Some(expected.size_bytes), |target| {
            io::copy(&mut source, target)?;
            ensure_source_unchanged(source_path, &source, expected)
        })
    }

    pub(crate) fn remove_file_if_exists(&self, path: &DeviceRelativePath) -> io::Result<bool> {
        let (parent, file_name) = split_parent(path)?;
        let parent = match self.open_dir(&parent, false) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match statat(&parent, file_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing to unlink non-regular device path: {path}"),
                ));
            }
            Err(Errno::NOENT) => return Ok(false),
            Err(error) => return Err(error.into()),
        }
        unlinkat(&parent, file_name, AtFlags::empty()).map_err(io::Error::from)?;
        sync_directory(&parent)?;
        Ok(true)
    }

    pub(crate) fn remove_tree_if_exists(
        &self,
        path: &DeviceRelativePath,
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<bool> {
        if path.is_root() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to remove the device mount root",
            ));
        }
        let (parent, file_name) = split_parent(path)?;
        let parent = match self.open_dir(&parent, false) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        let directory = match openat(&parent, file_name, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(directory) => directory,
            Err(Errno::NOENT) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        clear_directory(&directory, &mut DirectoryWorkBudget::new(), 0, cancel)?;
        unlinkat(&parent, file_name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        sync_directory(&parent)?;
        Ok(true)
    }

    pub(crate) fn cleanup_stale_temporary_files(
        &self,
        path: &DeviceRelativePath,
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        match self.open_dir(path, false) {
            // Publishing already cleans each target parent before writing.
            // This startup pass is intentionally scoped to `path` itself:
            // recursively exploring a Pioneer drive root would hand an
            // untrusted device control over sync latency.
            Ok(directory) => cleanup_temporary_files(&directory, cancel),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn open_dir(&self, path: &DeviceRelativePath, create: bool) -> io::Result<OwnedFd> {
        let mut directory = dup(&self.fd).map_err(io::Error::from)?;
        for component in path.components() {
            directory = match openat(&directory, component, DIRECTORY_FLAGS, Mode::empty()) {
                Ok(next) => next,
                Err(Errno::NOENT) if create => {
                    match mkdirat(&directory, component, DIRECTORY_MODE) {
                        Ok(()) => sync_directory(&directory)?,
                        Err(Errno::EXIST) => {}
                        Err(error) => return Err(error.into()),
                    }
                    openat(&directory, component, DIRECTORY_FLAGS, Mode::empty())
                        .map_err(io::Error::from)?
                }
                Err(error) => return Err(error.into()),
            };
        }
        Ok(directory)
    }

    fn publish_file(
        &self,
        path: &DeviceRelativePath,
        expected_len: Option<u64>,
        write_body: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        self.publish_file_with(path, expected_len, write_body, sync_directory)
    }

    fn publish_file_with(
        &self,
        path: &DeviceRelativePath,
        expected_len: Option<u64>,
        write_body: impl FnOnce(&mut File) -> io::Result<()>,
        sync_parent: impl FnOnce(&OwnedFd) -> io::Result<()>,
    ) -> io::Result<()> {
        let (parent_path, file_name) = split_parent(path)?;
        let parent = self.open_dir(&parent_path, true)?;
        reject_non_regular_destination(&parent, file_name, path)?;
        cleanup_temporary_files(&parent, &|| false)?;

        let (temporary_name, mut temporary) = create_temporary_file(&parent)?;
        let write_result = write_body(&mut temporary)
            .and_then(|()| temporary.flush())
            .and_then(|()| validate_file_len(&temporary, expected_len))
            .and_then(|()| temporary.sync_all());
        if let Err(error) = write_result {
            let _ = unlinkat(&parent, &temporary_name, AtFlags::empty());
            return Err(error);
        }
        drop(temporary);

        if let Err(error) = renameat(&parent, &temporary_name, &parent, file_name) {
            let _ = unlinkat(&parent, &temporary_name, AtFlags::empty());
            return Err(error.into());
        }
        sync_parent(&parent)
    }
}

fn split_parent(path: &DeviceRelativePath) -> io::Result<(DeviceRelativePath, &str)> {
    let mut components: Vec<_> = path.components().collect();
    let file_name = components.pop().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "device mount root is not a file path",
        )
    })?;
    let parent = DeviceRelativePath::new(components.join("/"))
        .expect("validated device path components remain normalized");
    Ok((parent, file_name))
}

fn reject_non_regular_destination(
    parent: &OwnedFd,
    file_name: &str,
    path: &DeviceRelativePath,
) -> io::Result<()> {
    match statat(parent, file_name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => Ok(()),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("refusing to replace non-regular device path: {path}"),
        )),
        Err(Errno::NOENT) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn create_temporary_file(parent: &OwnedFd) -> io::Result<(CString, File)> {
    static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);
    const MAX_ATTEMPTS: usize = 100;

    for _ in 0..MAX_ATTEMPTS {
        let id = NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
        let name = CString::new(format!(".sustain-write-{}-{id}.tmp", std::process::id()))
            .expect("generated temporary filename contains no NUL");
        match openat(
            parent,
            &name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            FILE_MODE,
        ) {
            Ok(fd) => return Ok((name, File::from(fd))),
            Err(Errno::EXIST) => continue,
            Err(error) => return Err(error.into()),
        }
    }

    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate an exclusive device staging file",
    ))
}

fn validate_file_len(file: &File, expected_len: Option<u64>) -> io::Result<()> {
    let Some(expected_len) = expected_len else {
        return Ok(());
    };
    let actual_len = file.metadata()?.len();
    if actual_len != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("staged file length mismatch: expected {expected_len}, wrote {actual_len}"),
        ));
    }
    Ok(())
}

fn sync_directory(directory: &OwnedFd) -> io::Result<()> {
    fsync(directory).map_err(io::Error::from)
}

fn cleanup_temporary_files(directory: &OwnedFd, cancel: &dyn Fn() -> bool) -> io::Result<()> {
    let mut budget = DirectoryWorkBudget::new();
    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut entries = RawDir::new(directory, &mut buffer);
    let mut removed = false;
    while let Some(entry) = entries.next() {
        ensure_cleanup_not_cancelled(cancel)?;
        let name = entry.map_err(io::Error::from)?.file_name().to_owned();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        budget.consume_entry()?;
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile if is_owned_temporary_name(&name) => {
                let fd = openat(
                    directory,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io::Error::from)?;
                if !File::from(fd).metadata()?.is_file() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "refusing to remove non-regular staging entry: {}",
                            cstr_display(&name)
                        ),
                    ));
                }
                unlinkat(directory, &name, AtFlags::empty()).map_err(io::Error::from)?;
                removed = true;
            }
            file_type if is_owned_temporary_name(&name) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refusing to remove non-regular staging entry ({file_type:?}): {}",
                        cstr_display(&name)
                    ),
                ));
            }
            _ => {}
        }
    }
    if removed {
        sync_directory(directory)?;
    }
    Ok(())
}

fn is_owned_temporary_name(name: &CStr) -> bool {
    let bytes = name.to_bytes();
    let Some(stem) = bytes
        .strip_prefix(b".sustain-write-")
        .and_then(|bytes| bytes.strip_suffix(b".tmp"))
    else {
        return false;
    };
    let mut parts = stem.split(|byte| *byte == b'-');
    let Some(pid) = parts.next() else {
        return false;
    };
    let Some(id) = parts.next() else {
        return false;
    };
    parts.next().is_none()
        && !pid.is_empty()
        && pid.iter().all(u8::is_ascii_digit)
        && !id.is_empty()
        && id.iter().all(u8::is_ascii_digit)
}

fn clear_directory(
    directory: &OwnedFd,
    budget: &mut DirectoryWorkBudget,
    depth: usize,
    cancel: &dyn Fn() -> bool,
) -> io::Result<()> {
    budget.enter_directory(depth)?;
    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut entries = RawDir::new(directory, &mut buffer);
    let mut removed = false;
    while let Some(entry) = entries.next() {
        ensure_cleanup_not_cancelled(cancel)?;
        let name = entry.map_err(io::Error::from)?.file_name().to_owned();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        budget.consume_entry()?;
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {
                unlinkat(directory, &name, AtFlags::empty()).map_err(io::Error::from)?;
                removed = true;
            }
            FileType::Directory => {
                let child = openat(directory, &name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(io::Error::from)?;
                clear_directory(&child, budget, depth + 1, cancel)?;
                unlinkat(directory, &name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
                removed = true;
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "refusing to remove non-regular device entry: {}",
                        cstr_display(&name)
                    ),
                ));
            }
        }
    }
    if removed {
        sync_directory(directory)?;
    }
    Ok(())
}

fn ensure_cleanup_not_cancelled(cancel: &dyn Fn() -> bool) -> io::Result<()> {
    if cancel() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "device cleanup was cancelled",
        ));
    }
    Ok(())
}

struct DirectoryWorkBudget {
    remaining_entries: usize,
}

impl DirectoryWorkBudget {
    fn new() -> Self {
        Self {
            remaining_entries: MAX_DIRECTORY_WORK_ENTRIES,
        }
    }

    fn enter_directory(&self, depth: usize) -> io::Result<()> {
        if depth > MAX_DIRECTORY_DEPTH {
            return Err(cleanup_budget_exceeded());
        }
        Ok(())
    }

    fn consume_entry(&mut self) -> io::Result<()> {
        self.remaining_entries = self
            .remaining_entries
            .checked_sub(1)
            .ok_or_else(cleanup_budget_exceeded)?;
        Ok(())
    }
}

fn cleanup_budget_exceeded() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "device cleanup exceeded its bounded directory-work budget",
    )
}

fn cstr_display(name: &CStr) -> String {
    String::from_utf8_lossy(name.to_bytes()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn staging_files(path: &Path) -> Vec<String> {
        std::fs::read_dir(path)
            .expect("read directory")
            .map(|entry| {
                entry
                    .expect("read directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(".sustain-write-") && name.ends_with(".tmp"))
            .collect()
    }

    #[test]
    fn capacity_is_probed_from_the_opened_root() {
        let device = tempfile::tempdir().expect("device dir");
        let root = DeviceRoot::open(device.path()).expect("root");

        let capacity = root.capacity().expect("capacity");

        assert!(capacity.total_bytes > 0);
        assert!(capacity.available_bytes <= capacity.total_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_removal_refuses_symlink_entries_without_touching_host_target() {
        use std::os::unix::fs::symlink;

        let device = tempfile::tempdir().expect("device dir");
        let host = tempfile::NamedTempFile::new().expect("host file");
        std::fs::write(host.path(), "host-data").expect("seed host file");
        let tree = device.path().join("PIONEER/Artwork/00001");
        std::fs::create_dir_all(&tree).expect("tree");
        symlink(host.path(), tree.join("host-link")).expect("host symlink");

        let root = DeviceRoot::open(device.path()).expect("root");
        let relative = DeviceRelativePath::new("PIONEER/Artwork/00001").expect("safe path");
        assert!(root.remove_tree_if_exists(&relative, &|| false).is_err());
        assert_eq!(
            std::fs::read_to_string(host.path()).expect("read host file"),
            "host-data"
        );
        assert!(tree.join("host-link").exists());
    }

    #[cfg(unix)]
    #[test]
    fn bounded_marker_read_rejects_fifo_without_blocking() {
        use rustix::fs::{CWD, Mode, mkfifoat};

        let device = tempfile::tempdir().expect("device dir");
        mkfifoat(CWD, device.path().join("marker"), Mode::RUSR | Mode::WUSR).expect("create FIFO");

        let root = DeviceRoot::open(device.path()).expect("root");
        let marker = DeviceRelativePath::new("marker").expect("safe path");
        assert!(root.read_to_string(&marker, 4096).is_err());
    }

    #[test]
    fn recursive_removal_rejects_trees_beyond_the_depth_budget() {
        let device = tempfile::tempdir().expect("device dir");
        let mut deepest = device.path().join("tree");
        std::fs::create_dir(&deepest).expect("create tree root");
        for index in 0..=MAX_DIRECTORY_DEPTH {
            deepest = deepest.join(format!("level-{index}"));
            std::fs::create_dir(&deepest).expect("create nested directory");
        }

        let root = DeviceRoot::open(device.path()).expect("root");
        let relative = DeviceRelativePath::new("tree").expect("safe path");
        assert!(root.remove_tree_if_exists(&relative, &|| false).is_err());
        assert!(device.path().join("tree").exists());
    }

    #[test]
    fn recursive_removal_rejects_trees_beyond_the_entry_budget() {
        let device = tempfile::tempdir().expect("device dir");
        let tree = device.path().join("tree");
        std::fs::create_dir(&tree).expect("create tree root");
        for index in 0..=MAX_DIRECTORY_WORK_ENTRIES {
            std::fs::write(tree.join(format!("entry-{index}")), b"x").expect("write entry");
        }

        let root = DeviceRoot::open(device.path()).expect("root");
        let relative = DeviceRelativePath::new("tree").expect("safe path");
        assert!(root.remove_tree_if_exists(&relative, &|| false).is_err());
        assert!(device.path().join("tree").exists());
    }

    #[test]
    fn recursive_removal_observes_cancellation_before_deleting_entries() {
        let device = tempfile::tempdir().expect("device dir");
        let tree = device.path().join("tree");
        std::fs::create_dir(&tree).expect("create tree root");
        std::fs::write(tree.join("entry"), b"x").expect("write entry");

        let root = DeviceRoot::open(device.path()).expect("root");
        let relative = DeviceRelativePath::new("tree").expect("safe path");
        assert!(root.remove_tree_if_exists(&relative, &|| true).is_err());
        assert!(tree.join("entry").exists());
    }

    #[test]
    fn copy_file_rejects_a_source_that_changed_since_it_was_observed() {
        // #100: the engine stats a source, then asks the device root to copy
        // it. If the bytes change in between, the publish guard must refuse
        // and leave no destination behind. This exercises the same
        // `ensure_source_unchanged` check that also runs after the copy body
        // to catch a mutation racing the read.
        let device = tempfile::tempdir().expect("device dir");
        let source = tempfile::NamedTempFile::new().expect("source file");
        std::fs::write(source.path(), b"observed-bytes").expect("seed source");
        let observed = crate::source::source_file_stat(source.path()).expect("stat source");

        std::fs::write(source.path(), b"different-bytes-entirely").expect("mutate source");

        let root = DeviceRoot::open(device.path()).expect("root");
        let dest = DeviceRelativePath::new("Music/track.mp3").expect("safe path");
        assert!(root.copy_file(source.path(), &dest, &observed).is_err());
        assert!(
            !device.path().join("Music/track.mp3").exists(),
            "no partial destination is published when the source changed"
        );
    }

    #[test]
    fn publish_cleans_only_owned_stale_temporary_files() {
        let device = tempfile::tempdir().expect("device dir");
        std::fs::write(
            device.path().join(".sustain-write-123-456.tmp"),
            b"interrupted staging bytes",
        )
        .expect("seed stale temporary file");
        std::fs::write(
            device.path().join(".sustain-write-not-ours.tmp"),
            b"user bytes",
        )
        .expect("seed similarly named user file");

        let root = DeviceRoot::open(device.path()).expect("root");
        let dest = DeviceRelativePath::new("marker").expect("safe path");
        root.write_file(&dest, b"published").expect("publish");

        assert!(!device.path().join(".sustain-write-123-456.tmp").exists());
        assert!(device.path().join(".sustain-write-not-ours.tmp").exists());
        assert_eq!(
            std::fs::read(device.path().join("marker")).expect("read published file"),
            b"published"
        );
    }

    #[test]
    fn failed_copy_preserves_the_previous_complete_destination() {
        let device = tempfile::tempdir().expect("device dir");
        let source = tempfile::NamedTempFile::new().expect("source file");
        std::fs::write(source.path(), b"observed-bytes").expect("seed source");
        let observed = crate::source::source_file_stat(source.path()).expect("stat source");

        let root = DeviceRoot::open(device.path()).expect("root");
        let dest = DeviceRelativePath::new("Music/track.mp3").expect("safe path");
        root.write_file(&dest, b"old-complete-bytes")
            .expect("seed destination");

        std::fs::write(source.path(), b"different-bytes").expect("mutate source");
        assert!(root.copy_file(source.path(), &dest, &observed).is_err());
        assert_eq!(
            std::fs::read(device.path().join("Music/track.mp3")).expect("read destination"),
            b"old-complete-bytes"
        );
    }

    #[test]
    fn failed_staging_write_leaves_the_previous_destination_untouched() {
        let device = tempfile::tempdir().expect("device dir");
        let root = DeviceRoot::open(device.path()).expect("root");
        let dest = DeviceRelativePath::new("track.mp3").expect("safe path");
        root.write_file(&dest, b"old-complete-bytes")
            .expect("seed destination");

        let result = root.publish_file(&dest, None, |_temporary| {
            Err(io::Error::other("injected staged-write failure"))
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(device.path().join("track.mp3")).expect("read destination"),
            b"old-complete-bytes"
        );
        assert!(staging_files(device.path()).is_empty());
    }

    #[test]
    fn rename_failure_removes_the_staging_file() {
        let device = tempfile::tempdir().expect("device dir");
        let root = DeviceRoot::open(device.path()).expect("root");
        let dest = DeviceRelativePath::new("track.mp3").expect("safe path");

        let result = root.publish_file(&dest, None, |temporary| {
            temporary.write_all(b"new bytes")?;
            std::fs::create_dir(device.path().join("track.mp3"))?;
            Ok(())
        });
        assert!(result.is_err());
        assert!(device.path().join("track.mp3").is_dir());
        assert!(staging_files(device.path()).is_empty());
    }

    #[test]
    fn directory_sync_failure_is_reported_after_atomic_publish() {
        let device = tempfile::tempdir().expect("device dir");
        let root = DeviceRoot::open(device.path()).expect("root");
        let dest = DeviceRelativePath::new("track.mp3").expect("safe path");

        let result = root.publish_file_with(
            &dest,
            Some(10),
            |temporary| temporary.write_all(b"new bytes!"),
            |_parent| Err(io::Error::other("injected directory fsync failure")),
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(device.path().join("track.mp3")).expect("read destination"),
            b"new bytes!"
        );
        assert!(staging_files(device.path()).is_empty());
    }
}
