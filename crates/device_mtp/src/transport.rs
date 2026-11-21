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
//!
//! Android shared storage ignores letter case, as the engine expects of
//! every device ([`device_path_key`]): `Age of Love` and `AGE OF LOVE`
//! are one folder. GVfs matches names exactly, so the folder a path names
//! may be spelled otherwise on the phone, and asking for the path's own
//! spelling finds nothing there; writing to it would create an object
//! the phone already holds, which it refuses (#259). The transport
//! therefore reaches every path through the device's spelling of it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::rc::Rc;

use gio::prelude::*;
use gio::{Cancellable, File, FileCreateFlags, FileInfo, FileQueryInfoFlags, FileType};
use sustain_device_sync::{
    DeviceCapacity, DeviceTransport, DirectoryWorkBudget, MtpTarget, device_path_key,
    ensure_source_unchanged,
};
use sustain_domain::{DeviceRelativePath, SourceFileStat};

/// Streaming copy chunk size.
const COPY_CHUNK_BYTES: usize = 256 * 1024;

/// An opened MTP storage root. All paths resolve beneath `root`.
pub struct MtpTransport {
    root: File,
    /// The entries of each directory listed so far, by the directory's
    /// device spelling: each entry's [`device_path_key`] to its spelling.
    ///
    /// A transport serves one plan or one sync, so the listings are not
    /// refreshed. They only ever choose between spellings the device takes
    /// for one name: an entry this transport created since is reached
    /// under the spelling it was created with, and one it removed since
    /// is recreated under its old spelling, both names the device would
    /// give the object anyway.
    listings: RefCell<HashMap<String, Rc<HashMap<String, String>>>>,
}

impl MtpTransport {
    /// Open the storage root addressed by `target`. Constructing the gio
    /// file handles never performs I/O, so this cannot fail; errors surface
    /// later, when an operation actually talks to the device.
    pub fn open(target: &MtpTarget) -> Self {
        let root = File::for_uri(&target.volume_uri).child(&target.storage);
        Self {
            root,
            listings: RefCell::default(),
        }
    }

    /// `path` beneath the root, spelled as given.
    fn resolve(&self, path: &DeviceRelativePath) -> File {
        let mut file = self.root.clone();
        for component in path.components() {
            file = file.child(component);
        }
        file
    }

    /// `path` beneath the root as the device spells it: each component
    /// takes the spelling of the entry the device holds under it in any
    /// letter case, and keeps its own from the first one the device does
    /// not hold. This is where a write to `path` lands.
    fn device_spelling(&self, path: &DeviceRelativePath) -> io::Result<File> {
        let mut file = self.root.clone();
        let mut spelled = String::new();
        let mut held = true;
        for component in path.components() {
            let spelling = if held {
                self.listing(&spelled, &file)?
                    .get(&device_path_key(component))
                    .cloned()
            } else {
                None
            };
            held = spelling.is_some();
            let spelling = spelling.as_deref().unwrap_or(component);
            if !spelled.is_empty() {
                spelled.push('/');
            }
            spelled.push_str(spelling);
            file = file.child(spelling);
        }
        Ok(file)
    }

    /// The entries of the directory `dir`, spelled `spelled` beneath the
    /// root, listed once per transport. A directory the device does not
    /// hold lists nothing.
    fn listing(&self, spelled: &str, dir: &File) -> io::Result<Rc<HashMap<String, String>>> {
        if let Some(listing) = self.listings.borrow().get(spelled) {
            return Ok(Rc::clone(listing));
        }
        let mut entries = HashMap::new();
        match dir.enumerate_children(
            "standard::name",
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            Ok(children) => {
                for child in children {
                    let child = child.map_err(map_glib_error)?;
                    // A name that is not UTF-8 is no spelling of a
                    // device-relative path.
                    if let Some(name) = child.name().to_str() {
                        entries.insert(device_path_key(name), name.to_owned());
                    }
                }
            }
            Err(error) if is_not_found(&error) || is_not_directory(&error) => {}
            Err(error) => return Err(map_glib_error(error)),
        }
        let listing = Rc::new(entries);
        self.listings
            .borrow_mut()
            .insert(spelled.to_owned(), Rc::clone(&listing));
        Ok(listing)
    }

    /// The entry the device holds at `path`, in any letter case, with its
    /// `attributes`; `None` when it holds none.
    ///
    /// The path's own spelling is asked first: nearly always the device
    /// holds it so, and one query answers without listing a directory.
    fn lookup(
        &self,
        path: &DeviceRelativePath,
        attributes: &str,
    ) -> io::Result<Option<(File, FileInfo)>> {
        let query = |file: File| match file.query_info(
            attributes,
            FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            Cancellable::NONE,
        ) {
            Ok(info) => Ok(Some((file, info))),
            Err(error) if is_not_found(&error) => Ok(None),
            Err(error) => Err(map_glib_error(error)),
        };
        let given = self.resolve(path);
        if let Some(found) = query(given.clone())? {
            return Ok(Some(found));
        }
        let spelled = self.device_spelling(path)?;
        if spelled.equal(&given) {
            return Ok(None);
        }
        query(spelled)
    }

    fn stream_copy(
        source: &mut std::fs::File,
        stream: &gio::FileOutputStream,
        source_path: &Path,
        expected: &SourceFileStat,
    ) -> io::Result<()> {
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
        let dir = self.device_spelling(path)?;
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
        match self.lookup(path, "standard::type,standard::size")? {
            Some((_file, info)) if info.file_type() == FileType::Regular => {
                Ok(Some(info.size().max(0) as u64))
            }
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device path is not a regular file: {path}"),
            )),
            None => Ok(None),
        }
    }

    fn read_file(&self, path: &DeviceRelativePath, limit: u64) -> io::Result<Vec<u8>> {
        let (file, info) = self
            .lookup(path, "standard::type,standard::size")?
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, format!("no device file at {path}"))
            })?;
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
        // The size query and the transfer are separate requests to the
        // device: the limit is enforced on the bytes actually read.
        let mut bytes = Vec::new();
        file.read(Cancellable::NONE)
            .map_err(map_glib_error)?
            .into_read()
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("device file exceeds {limit} bytes: {path}"),
            ));
        }
        Ok(bytes)
    }

    fn write_file(&self, path: &DeviceRelativePath, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = parent_of(path) {
            self.ensure_dir_all(&parent)?;
        }
        let file = self.device_spelling(path)?;
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
        let file = self.device_spelling(path)?;
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
        let file = match self.lookup(path, "standard::type")? {
            Some((file, info)) if info.file_type() == FileType::Regular => file,
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing to unlink non-regular device path: {path}"),
                ));
            }
            None => return Ok(false),
        };
        match file.delete(Cancellable::NONE) {
            Ok(()) => Ok(true),
            Err(error) if is_not_found(&error) => Ok(false),
            Err(error) => Err(map_glib_error(error)),
        }
    }

    fn remove_dir_if_empty(&self, path: &DeviceRelativePath) -> io::Result<bool> {
        if path.is_root() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "refusing to remove the device storage root",
            ));
        }
        let dir = match self.lookup(path, "standard::type")? {
            Some((dir, info)) if info.file_type() == FileType::Directory => dir,
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing to remove non-directory device path: {path}"),
                ));
            }
            None => return Ok(false),
        };
        // Emptiness is checked here, not left to the delete: what an MTP
        // device does when asked to delete a folder that holds something
        // is the device's own business, and some delete everything in it.
        let mut children = dir
            .enumerate_children(
                "standard::name",
                FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
                Cancellable::NONE,
            )
            .map_err(map_glib_error)?;
        if let Some(child) = children.next() {
            child.map_err(map_glib_error)?;
            return Ok(false);
        }
        match dir.delete(Cancellable::NONE) {
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
        let dir = match self.lookup(path, "standard::type")? {
            Some((dir, info)) if info.file_type() == FileType::Directory => dir,
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("refusing to remove non-directory device path: {path}"),
                ));
            }
            None => return Ok(false),
        };
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

fn is_not_directory(error: &glib::Error) -> bool {
    error.matches(gio::IOErrorEnum::NotDirectory)
}

#[cfg(test)]
mod tests {
    use sustain_device_sync::{DeviceTransport, MtpTarget};
    use sustain_domain::DeviceRelativePath;

    use super::MtpTransport;

    /// A transport over a local directory: gio serves `file://` through the
    /// same `GFile` calls it serves an MTP storage through.
    fn local_transport(root: &std::path::Path) -> MtpTransport {
        std::fs::create_dir(root.join("storage")).expect("storage directory");
        MtpTransport::open(&MtpTarget {
            volume_uri: glib::filename_to_uri(root, None)
                .expect("file URI")
                .to_string(),
            storage: "storage".to_owned(),
        })
    }

    #[test]
    fn a_device_file_within_its_limit_is_read_whole() {
        let root = tempfile::tempdir().expect("tempdir");
        let transport = local_transport(root.path());
        std::fs::write(root.path().join("storage/marker"), "abc").expect("marker");
        let path = DeviceRelativePath::new("marker").expect("path");

        assert_eq!(transport.read_to_string(&path, 3).expect("read"), "abc");
        assert!(transport.read_to_string(&path, 2).is_err());
    }

    /// A phone's folder, spelled otherwise than the paths into it below.
    fn seed_device_folder(root: &std::path::Path) -> std::path::PathBuf {
        let album = root.join("storage/Music/AGE OF LOVE/The Age of Love");
        std::fs::create_dir_all(&album).expect("album folder");
        std::fs::write(album.join("16 Track.MP3"), "old").expect("device file");
        album
    }

    fn path(value: &str) -> DeviceRelativePath {
        DeviceRelativePath::new(value).expect("path")
    }

    #[test]
    fn a_device_file_is_found_under_any_letter_case() {
        let root = tempfile::tempdir().expect("tempdir");
        let transport = local_transport(root.path());
        seed_device_folder(root.path());
        let path = path("Music/Age of Love/the age of love/16 Track.mp3");

        assert_eq!(transport.regular_file_len(&path).expect("len"), Some(3));
        assert_eq!(transport.read_to_string(&path, 3).expect("read"), "old");
        assert!(transport.remove_file_if_exists(&path).expect("remove"));
        assert_eq!(transport.regular_file_len(&path).expect("len"), None);
    }

    #[test]
    fn a_copy_lands_in_the_device_spelling_of_its_slot() {
        let root = tempfile::tempdir().expect("tempdir");
        let transport = local_transport(root.path());
        let album = seed_device_folder(root.path());
        let source = tempfile::NamedTempFile::new().expect("source");
        std::fs::write(source.path(), "new").expect("source bytes");
        let stat = sustain_device_sync::source_file_stat(source.path()).expect("stat");

        for (target, landed) in [
            (
                "Music/Age of Love/The Age of Love/16 Track.mp3",
                "16 Track.MP3",
            ),
            (
                "Music/Age of Love/The Age of Love/17 Track.mp3",
                "17 Track.mp3",
            ),
        ] {
            transport
                .copy_file(source.path(), &path(target), &stat)
                .expect("copy");
            assert_eq!(
                std::fs::read_to_string(album.join(landed)).expect("landed"),
                "new"
            );
        }
        let mut artists: Vec<_> = std::fs::read_dir(root.path().join("storage/Music"))
            .expect("music folder")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        artists.sort();
        assert_eq!(artists, ["AGE OF LOVE"]);
        assert_eq!(std::fs::read_dir(&album).expect("album").count(), 2);
    }
}
