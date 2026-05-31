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
        AtFlags, FileType, Mode, OFlags, RawDir, mkdirat, open, openat, renameat, statat, unlinkat,
    },
    io::{Errno, dup},
};
use sustain_domain::DeviceRelativePath;

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

    pub(crate) fn is_regular_file(&self, path: &DeviceRelativePath) -> io::Result<bool> {
        let (parent, file_name) = split_parent(path)?;
        let parent = match self.open_dir(&parent, false) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        match statat(&parent, file_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device path is not a regular file: {path}"),
            )),
            Err(Errno::NOENT) => Ok(false),
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
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
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
        self.publish_file(path, |file| file.write_all(bytes))
    }

    pub(crate) fn copy_file(&self, source: &Path, path: &DeviceRelativePath) -> io::Result<()> {
        let mut source = File::open(source)?;
        self.publish_file(path, |target| io::copy(&mut source, target).map(|_| ()))
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
        Ok(true)
    }

    pub(crate) fn remove_tree_if_exists(&self, path: &DeviceRelativePath) -> io::Result<bool> {
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
        clear_directory(&directory)?;
        unlinkat(&parent, file_name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
        Ok(true)
    }

    fn open_dir(&self, path: &DeviceRelativePath, create: bool) -> io::Result<OwnedFd> {
        let mut directory = dup(&self.fd).map_err(io::Error::from)?;
        for component in path.components() {
            directory = match openat(&directory, component, DIRECTORY_FLAGS, Mode::empty()) {
                Ok(next) => next,
                Err(Errno::NOENT) if create => {
                    match mkdirat(&directory, component, DIRECTORY_MODE) {
                        Ok(()) | Err(Errno::EXIST) => {}
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
        write_body: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        let (parent_path, file_name) = split_parent(path)?;
        let parent = self.open_dir(&parent_path, true)?;
        reject_non_regular_destination(&parent, file_name, path)?;

        let (temporary_name, mut temporary) = create_temporary_file(&parent)?;
        let write_result = write_body(&mut temporary)
            .and_then(|()| temporary.flush())
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
        Ok(())
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

fn clear_directory(directory: &OwnedFd) -> io::Result<()> {
    let mut buffer = [MaybeUninit::<u8>::uninit(); 8192];
    let mut entries = RawDir::new(directory, &mut buffer);
    while let Some(entry) = entries.next() {
        let name = entry.map_err(io::Error::from)?.file_name().to_owned();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let stat = statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(io::Error::from)?;
        match FileType::from_raw_mode(stat.st_mode) {
            FileType::RegularFile => {
                unlinkat(directory, &name, AtFlags::empty()).map_err(io::Error::from)?;
            }
            FileType::Directory => {
                let child = openat(directory, &name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(io::Error::from)?;
                clear_directory(&child)?;
                unlinkat(directory, &name, AtFlags::REMOVEDIR).map_err(io::Error::from)?;
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
    Ok(())
}

fn cstr_display(name: &CStr) -> String {
    String::from_utf8_lossy(name.to_bytes()).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(root.remove_tree_if_exists(&relative).is_err());
        assert_eq!(
            std::fs::read_to_string(host.path()).expect("read host file"),
            "host-data"
        );
        assert!(tree.join("host-link").exists());
    }
}
