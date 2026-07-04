// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A [`DeviceTransport`] backed by gio/GVfs for Android phones over MTP.
//!
//! Every operation resolves a [`DeviceRelativePath`] beneath the storage
//! root (`gio::File::for_uri(volume_uri).child(storage)`) and hands it to
//! the desktop's `gvfsd-mtp` backend through `gio::File`. MTP storage
//! cannot rename (`access::can-rename` is false on these volumes), so a
//! file is published by replacing it in place; a write interrupted partway
//! leaves a short file that the next sync's size/fingerprint diff detects
//! and recopies. All gio objects are created and used on the calling
//! thread (the sync worker) and never cross threads.

use std::io;
use std::path::Path;

use gio::prelude::*;
use gio::{Cancellable, File, FileCreateFlags, FileQueryInfoFlags, FileType};
use sustain_device_sync::{DeviceCapacity, DeviceTransport, MtpTarget, ensure_source_unchanged};
use sustain_domain::{DeviceRelativePath, SourceFileStat};

/// A removable device is untrusted storage. Recursive removal stops after
/// this much directory work rather than walking an arbitrarily large tree.
const MAX_DIRECTORY_WORK_ENTRIES: usize = 20_000;
const MAX_DIRECTORY_DEPTH: usize = 128;
/// Streaming copy chunk size.
const COPY_CHUNK_BYTES: usize = 256 * 1024;

/// An opened MTP storage root. All paths resolve beneath `root`.
pub struct MtpTransport {
    root: File,
}

impl MtpTransport {
    /// Open the storage root addressed by `target`. Constructing the gio
    /// file handles never performs I/O, so this cannot fail; errors surface
    /// later, when an operation actually talks to the device.
    pub fn open(target: &MtpTarget) -> Self {
        let root = File::for_uri(&target.volume_uri).child(&target.storage);
        Self { root }
    }

    /// Resolve a device-relative path to a gio file beneath the root.
    fn resolve(&self, path: &DeviceRelativePath) -> File {
        let mut file = self.root.clone();
        for component in path.components() {
            file = file.child(component);
        }
        file
    }

    fn stream_copy(
        source: &mut std::fs::File,
        stream: &gio::FileOutputStream,
        source_path: &Path,
        expected: &SourceFileStat,
    ) -> io::Result<()> {
        use std::io::Read;
        let mut buffer = vec![0u8; COPY_CHUNK_BYTES];
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            let (_written, partial) = stream
                .write_all(&buffer[..read], Cancellable::NONE)
                .map_err(map_glib_error)?;
            if let Some(error) = partial {
                return Err(map_glib_error(error));
            }
        }
        ensure_source_unchanged(source_path, source, expected)
    }
}

impl DeviceTransport for MtpTransport {
    fn capacity(&self) -> io::Result<DeviceCapacity> {
        let info = self
            .root
            .query_filesystem_info("filesystem::size,filesystem::free", Cancellable::NONE)
            .map_err(map_glib_error)?;
        Ok(DeviceCapacity {
            total_bytes: info.attribute_uint64("filesystem::size"),
            available_bytes: info.attribute_uint64("filesystem::free"),
        })
    }

    fn ensure_dir_all(&self, path: &DeviceRelativePath) -> io::Result<()> {
        if path.is_root() {
            return Ok(());
        }
        let dir = self.resolve(path);
        match dir.make_directory_with_parents(Cancellable::NONE) {
            Ok(()) => Ok(()),
            Err(error) if is_exists(&error) => Ok(()),
            Err(error) => Err(map_glib_error(error)),
        }
    }

    fn is_regular_file(&self, path: &DeviceRelativePath) -> io::Result<bool> {
        self.regular_file_len(path).map(|len| len.is_some())
    }

    fn regular_file_len(&self, path: &DeviceRelativePath) -> io::Result<Option<u64>> {
        let file = self.resolve(path);
        match file.query_info(
            "standard::type,standard::size",
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            Ok(info) if info.file_type() == FileType::Regular => {
                Ok(Some(info.size().max(0) as u64))
            }
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device path is not a regular file: {path}"),
            )),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(map_glib_error(error)),
        }
    }

    fn read_to_string(&self, path: &DeviceRelativePath, limit: u64) -> io::Result<String> {
        let file = self.resolve(path);
        let info = file
            .query_info(
                "standard::type,standard::size",
                FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                Cancellable::NONE,
            )
            .map_err(map_glib_error)?;
        if info.file_type() != FileType::Regular {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device path is not a regular file: {path}"),
            ));
        }
        if info.size().max(0) as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device file exceeds {limit} bytes: {path}"),
            ));
        }
        let (bytes, _etag) = file
            .load_contents(Cancellable::NONE)
            .map_err(map_glib_error)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device file is not UTF-8: {path}"),
            )
        })
    }

    fn write_file(&self, path: &DeviceRelativePath, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = parent_of(path) {
            self.ensure_dir_all(&parent)?;
        }
        let file = self.resolve(path);
        let stream = file
            .replace(
                None,
                false,
                FileCreateFlags::REPLACE_DESTINATION,
                Cancellable::NONE,
            )
            .map_err(map_glib_error)?;
        let write = stream
            .write_all(bytes, Cancellable::NONE)
            .map_err(map_glib_error)
            .and_then(|(_written, partial)| match partial {
                Some(error) => Err(map_glib_error(error)),
                None => Ok(()),
            });
        let close = stream.close(Cancellable::NONE).map_err(map_glib_error);
        match write.and(close) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = file.delete(Cancellable::NONE);
                Err(error)
            }
        }
    }

    fn copy_file(
        &self,
        source_path: &Path,
        path: &DeviceRelativePath,
        expected: &SourceFileStat,
    ) -> io::Result<()> {
        let mut source = std::fs::File::open(source_path)?;
        ensure_source_unchanged(source_path, &source, expected)?;
        if let Some(parent) = parent_of(path) {
            self.ensure_dir_all(&parent)?;
        }
        let file = self.resolve(path);
        let stream = file
            .replace(
                None,
                false,
                FileCreateFlags::REPLACE_DESTINATION,
                Cancellable::NONE,
            )
            .map_err(map_glib_error)?;
        let copy = Self::stream_copy(&mut source, &stream, source_path, expected);
        let close = stream.close(Cancellable::NONE).map_err(map_glib_error);
        match copy.and(close) {
            Ok(()) => Ok(()),
            Err(error) => {
                // No atomic rename on MTP: drop the partial so it cannot be
                // mistaken for a complete copy.
                let _ = file.delete(Cancellable::NONE);
                Err(error)
            }
        }
    }

    fn can_continue_after_copy_error(&self, _error: &io::Error) -> bool {
        true
    }

    fn remove_file_if_exists(&self, path: &DeviceRelativePath) -> io::Result<bool> {
        let file = self.resolve(path);
        match file.query_info(
            "standard::type",
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            Ok(info) if info.file_type() == FileType::Regular => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing to unlink non-regular device path: {path}"),
                ));
            }
            Err(error) if is_not_found(&error) => return Ok(false),
            Err(error) => return Err(map_glib_error(error)),
        }
        match file.delete(Cancellable::NONE) {
            Ok(()) => Ok(true),
            Err(error) if is_not_found(&error) => Ok(false),
            Err(error) => Err(map_glib_error(error)),
        }
    }

    fn remove_tree_if_exists(
        &self,
        path: &DeviceRelativePath,
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<bool> {
        if path.is_root() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to remove the device storage root",
            ));
        }
        let dir = self.resolve(path);
        match dir.query_info(
            "standard::type",
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            Ok(info) if info.file_type() == FileType::Directory => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing to remove non-directory device path: {path}"),
                ));
            }
            Err(error) if is_not_found(&error) => return Ok(false),
            Err(error) => return Err(map_glib_error(error)),
        }
        let mut budget = DirectoryWorkBudget::new();
        clear_directory(&dir, &mut budget, 0, cancel)?;
        dir.delete(Cancellable::NONE).map_err(map_glib_error)?;
        Ok(true)
    }

    fn cleanup_stale_temporary_files(
        &self,
        _path: &DeviceRelativePath,
        _cancel: &dyn Fn() -> bool,
    ) -> io::Result<()> {
        // The MTP transport publishes files in place rather than through a
        // staging temp + rename (MTP cannot rename), so there are never any
        // Sustain staging files to reap.
        Ok(())
    }
}

/// Recursively delete a directory's contents, bounded by `budget` and
/// polling `cancel` between entries. The directory itself is removed by the
/// caller.
fn clear_directory(
    dir: &File,
    budget: &mut DirectoryWorkBudget,
    depth: usize,
    cancel: &dyn Fn() -> bool,
) -> io::Result<()> {
    budget.enter_directory(depth)?;
    let enumerator = dir
        .enumerate_children(
            "standard::name,standard::type",
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        )
        .map_err(map_glib_error)?;
    for info in enumerator {
        if cancel() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "device cleanup was cancelled",
            ));
        }
        let info = info.map_err(map_glib_error)?;
        budget.consume_entry()?;
        let child = dir.child(info.name());
        if info.file_type() == FileType::Directory {
            clear_directory(&child, budget, depth + 1, cancel)?;
        }
        child.delete(Cancellable::NONE).map_err(map_glib_error)?;
    }
    Ok(())
}

/// The parent of a device-relative path, or `None` when `path` is a single
/// component (its parent is the already-present root).
fn parent_of(path: &DeviceRelativePath) -> Option<DeviceRelativePath> {
    let mut components: Vec<&str> = path.components().collect();
    if components.len() <= 1 {
        return None;
    }
    components.pop();
    DeviceRelativePath::new(components.join("/"))
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
            return Err(budget_exceeded());
        }
        Ok(())
    }

    fn consume_entry(&mut self) -> io::Result<()> {
        self.remaining_entries = self
            .remaining_entries
            .checked_sub(1)
            .ok_or_else(budget_exceeded)?;
        Ok(())
    }
}

fn budget_exceeded() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "device cleanup exceeded its bounded directory-work budget",
    )
}

fn map_glib_error(error: glib::Error) -> io::Error {
    if is_not_found(&error) {
        io::Error::new(io::ErrorKind::NotFound, error.message().to_owned())
    } else if is_exists(&error) {
        io::Error::new(io::ErrorKind::AlreadyExists, error.message().to_owned())
    } else {
        io::Error::other(error.message().to_owned())
    }
}

fn is_not_found(error: &glib::Error) -> bool {
    error.matches(gio::IOErrorEnum::NotFound)
}

fn is_exists(error: &glib::Error) -> bool {
    error.matches(gio::IOErrorEnum::Exists)
}
