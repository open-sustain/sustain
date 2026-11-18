// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Descriptor-safe publication of regular files into the Linux desktop trash.
//!
//! The generic `trash` crate accepts only a pathname. That leaves a leaf-name
//! race between Sustain's descriptor verification and the backend resolving
//! the pathname. Sustain only trashes regular audio files, so owning this small
//! Freedesktop Trash publication path is both narrower and safer.

use std::{
    env,
    ffi::OsString,
    fs::File,
    io::{self, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::MetadataExt,
    },
    path::{Path, PathBuf},
};

use rustix::{fs::fchmod, process::getuid};

use crate::managed_library::file_ops::{
    PRIVATE_DIRECTORY_MODE, PinnedFilePath, RegularFileCapability, ensure_directory_all_open,
    open_directory_path, regular_file_capability_from_file,
    remove_pinned_regular_file_matching_capability,
};

const STICKY_BIT: u32 = 0o1000;
const MAX_NAME_ATTEMPTS: usize = 10_000;
const TRASH_INFO_EXTENSION: &str = ".trashinfo";
/// Linux `NAME_MAX`: the byte limit of one path component.
const NAME_MAX_BYTES: usize = 255;

pub(crate) fn trash_regular_file(path: &Path, source: &RegularFileCapability) -> io::Result<()> {
    let source_path = PinnedFilePath::existing_parent(path)?;
    if !source_path.refers_to(source)? {
        return Err(io::Error::other("trash source changed before publication"));
    }
    let original_path = absolute_path(path)?;
    let location = trash_location(&original_path, source.identity().device)?;
    trash_regular_file_to_location(&source_path, source, &location)
}

fn trash_regular_file_to_location(
    source_path: &PinnedFilePath,
    source: &RegularFileCapability,
    location: &TrashLocation,
) -> io::Result<()> {
    let files_directory = location.root.join("files");
    let info_directory = location.root.join("info");
    ensure_private_directory(&files_directory)?;
    ensure_private_directory(&info_directory)?;

    let original_name = source_path.path().file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "trash source has no file name")
    })?;
    for attempt in 0..MAX_NAME_ATTEMPTS {
        let trash_name = collision_free_name(original_name, attempt);
        let data = PinnedFilePath::existing_parent(&files_directory.join(&trash_name))?;
        let info = PinnedFilePath::existing_parent(
            &info_directory.join(trash_info_name(trash_name.clone())),
        )?;
        let Some(info_capability) = create_trash_info(&info, &location.info_path)? else {
            continue;
        };

        match source_path.rename_without_overwrite_to(&data) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                remove_info_file(&info, &info_capability)?;
                continue;
            }
            Err(error) => {
                remove_info_file(&info, &info_capability)?;
                return Err(error);
            }
        }
        let staged = match data.open_regular_file() {
            Ok(staged) => staged,
            Err(_) => {
                let _ = remove_info_file(&info, &info_capability);
                return Err(io::Error::other(
                    "desktop-trash data entry is not a regular file",
                ));
            }
        };
        if staged.identity() != source.identity() {
            let _ = rollback_trash_publication(&data, source_path, &staged);
            let _ = remove_info_file(&info, &info_capability);
            return Err(io::Error::other(
                "trash source changed while publishing the desktop-trash entry",
            ));
        }
        source_path.sync_parent()?;
        data.sync_parent()?;
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a unique desktop-trash entry",
    ))
}

fn rollback_trash_publication(
    data: &PinnedFilePath,
    source: &PinnedFilePath,
    expected: &RegularFileCapability,
) -> io::Result<()> {
    if !data.refers_to(expected)? {
        return Err(io::Error::other(
            "desktop-trash data entry changed before rollback",
        ));
    }
    data.rename_without_overwrite_to(source)?;
    data.sync_parent()?;
    source.sync_parent()
}

struct TrashLocation {
    root: PathBuf,
    info_path: PathBuf,
}

fn trash_location(path: &Path, source_device: u64) -> io::Result<TrashLocation> {
    let home_trash = home_trash_directory()?;
    let home_trash_directory = ensure_private_directory(&home_trash)?;
    if home_trash_directory.metadata()?.dev() == source_device {
        return Ok(TrashLocation {
            root: home_trash,
            info_path: path.to_path_buf(),
        });
    }

    let topdir = filesystem_topdir(path, source_device)?;
    let root = topdir_trash_root(&topdir, getuid().as_raw())?;
    let info_path = path
        .strip_prefix(&topdir)
        .map_err(|_| io::Error::other("trash source escaped its filesystem top directory"))?
        .to_path_buf();
    Ok(TrashLocation { root, info_path })
}

/// The trash directory on a volume other than the home trash's. The spec
/// prefers the user's directory inside a sticky, administrator-provided
/// `$topdir/.Trash`; when that is absent, not sticky, or its per-user
/// directory cannot be used, the private `$topdir/.Trash-$uid` serves.
fn topdir_trash_root(topdir: &Path, uid: u32) -> io::Result<PathBuf> {
    let shared = topdir.join(".Trash");
    let shared_is_sticky = open_directory_path(&shared)
        .and_then(|directory| directory.metadata())
        .is_ok_and(|metadata| metadata.mode() & STICKY_BIT != 0);
    if shared_is_sticky {
        let user_trash = shared.join(uid.to_string());
        if ensure_private_directory(&user_trash).is_ok() {
            return Ok(user_trash);
        }
    }
    let user_trash = topdir.join(format!(".Trash-{uid}"));
    ensure_private_directory(&user_trash)?;
    Ok(user_trash)
}

fn ensure_private_directory(path: &Path) -> io::Result<File> {
    let directory = ensure_directory_all_open(path, PRIVATE_DIRECTORY_MODE)?;
    if directory.metadata()?.uid() != getuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "desktop-trash directory does not belong to the current user",
        ));
    }
    fchmod(&directory, PRIVATE_DIRECTORY_MODE).map_err(io::Error::from)?;
    directory.sync_all()?;
    if directory.metadata()?.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "desktop-trash directory is not private to the current user",
        ));
    }
    Ok(directory)
}

fn filesystem_topdir(path: &Path, source_device: u64) -> io::Result<PathBuf> {
    let mut directory = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "trash source has no parent"))?;
    if open_directory_path(directory)?.metadata()?.dev() != source_device {
        return Err(io::Error::other(
            "trash source and containing directory are on different filesystems",
        ));
    }
    loop {
        let Some(parent) = directory.parent() else {
            return Ok(directory.to_path_buf());
        };
        if parent == directory || open_directory_path(parent)?.metadata()?.dev() != source_device {
            return Ok(directory.to_path_buf());
        }
        directory = parent;
    }
}

/// `$XDG_DATA_HOME/Trash`. An empty or relative `XDG_DATA_HOME` is
/// ignored, as the base-directory specification requires.
fn home_trash_directory() -> io::Result<PathBuf> {
    directories::BaseDirs::new()
        .map(|dirs| dirs.data_dir().join("Trash"))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no home directory is known"))
}

fn create_trash_info(
    info: &PinnedFilePath,
    original_path: &Path,
) -> io::Result<Option<RegularFileCapability>> {
    let mut file = match info.create_new_file() {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(None),
        Err(error) => return Err(error),
    };
    let capability = regular_file_capability_from_file(file.try_clone()?)
        .map_err(|_| io::Error::other("trash info staging file is not regular"))?;
    let result = (|| {
        writeln!(file, "[Trash Info]")?;
        writeln!(file, "Path={}", encode_uri_path(original_path))?;
        writeln!(file, "DeletionDate={}", local_deletion_date()?)?;
        file.flush()?;
        file.sync_all()?;
        info.sync_parent()
    })();
    if let Err(error) = result {
        let _ = remove_info_file(info, &capability);
        return Err(error);
    }
    Ok(Some(capability))
}

/// The trash-spec `DeletionDate`: local time, `YYYY-MM-DDThh:mm:ss`.
fn local_deletion_date() -> io::Result<glib::GString> {
    glib::DateTime::now_local()
        .and_then(|now| now.format("%Y-%m-%dT%H:%M:%S"))
        .map_err(|_| io::Error::other("could not format the local deletion time"))
}

fn remove_info_file(info: &PinnedFilePath, capability: &RegularFileCapability) -> io::Result<()> {
    remove_pinned_regular_file_matching_capability(info, capability)
        .map_err(|_| io::Error::other("could not remove desktop-trash metadata"))
}

/// A trash entry name that, with its collision suffix and the
/// `.trashinfo` extension of its info file, fits one Linux path component.
/// The info file's `Path=` keeps the complete original name, so a shortened
/// entry still restores under it.
fn collision_free_name(original: &std::ffi::OsStr, attempt: usize) -> OsString {
    let suffix = if attempt > 0 {
        format!(".{}", attempt + 1)
    } else {
        String::new()
    };
    let budget = NAME_MAX_BYTES - TRASH_INFO_EXTENSION.len() - suffix.len();
    let mut name = OsString::from_vec(truncated_name(original.as_bytes(), budget).to_vec());
    name.push(suffix);
    name
}

/// The longest prefix of `name` within `budget` bytes that does not split a
/// UTF-8 character, so a UTF-8 name stays readable in a file manager.
fn truncated_name(name: &[u8], budget: usize) -> &[u8] {
    if name.len() <= budget {
        return name;
    }
    let mut end = budget;
    if std::str::from_utf8(name).is_ok() {
        while std::str::from_utf8(&name[..end]).is_err() {
            end -= 1;
        }
    }
    &name[..end]
}

fn trash_info_name(mut trash_name: OsString) -> OsString {
    trash_name.push(TRASH_INFO_EXTENSION);
    trash_name
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        env::current_dir().map(|directory| directory.join(path))
    }
}

fn encode_uri_path(path: &Path) -> String {
    let mut encoded = String::new();
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(*byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::{fs, path::Path};

    use super::{
        NAME_MAX_BYTES, TrashLocation, encode_uri_path, ensure_private_directory,
        topdir_trash_root, trash_regular_file_to_location,
    };
    use crate::managed_library::file_ops::{PinnedFilePath, open_regular_file};

    #[test]
    fn uri_encoding_preserves_slashes_and_escapes_bytes() {
        assert_eq!(
            encode_uri_path(Path::new("/Music/AC DC/100%.flac")),
            "/Music/AC%20DC/100%25.flac"
        );
    }

    #[test]
    fn owned_trash_directory_permissions_are_normalized_to_private() {
        let root = tempfile::tempdir().expect("create test root");
        let trash = root.path().join("Trash");
        fs::create_dir(&trash).expect("create trash");
        fs::set_permissions(&trash, fs::Permissions::from_mode(0o755))
            .expect("make trash permissive");

        ensure_private_directory(&trash).expect("normalize trash permissions");

        assert_eq!(
            fs::metadata(&trash)
                .expect("trash metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }

    #[test]
    fn desktop_trash_publication_moves_the_retained_regular_file() {
        let root = tempfile::tempdir().expect("create test root");
        let trash = root.path().join("Trash");
        let source = root.path().join("song.flac");
        fs::write(&source, b"audio bytes").expect("write source");
        let capability = open_regular_file(&source).expect("open source");
        let source_path = PinnedFilePath::existing_parent(&source).expect("pin source");

        trash_regular_file_to_location(
            &source_path,
            &capability,
            &TrashLocation {
                root: trash.clone(),
                info_path: source.clone(),
            },
        )
        .expect("trash source");

        assert!(!source.exists());
        assert_eq!(
            fs::read(trash.join("files/song.flac")).expect("read trashed file"),
            b"audio bytes"
        );
        let trash_info =
            fs::read_to_string(trash.join("info/song.flac.trashinfo")).expect("read trash info");
        assert!(trash_info.contains("Path="));
        let deletion_date = trash_info
            .lines()
            .find_map(|line| line.strip_prefix("DeletionDate="))
            .expect("DeletionDate line");
        let shape: String = deletion_date
            .chars()
            .map(|c| if c.is_ascii_digit() { 'd' } else { c })
            .collect();
        assert_eq!(shape, "dddd-dd-ddTdd:dd:dd");
    }

    #[test]
    fn a_name_that_fills_the_component_limit_is_trashed_under_its_full_name() {
        let root = tempfile::tempdir().expect("create test root");
        let trash = root.path().join("Trash");
        // Multi-byte characters, so truncation has a boundary to respect.
        let stem = "é".repeat((NAME_MAX_BYTES - ".flac".len()) / 2);
        let name = format!("{stem}{}.flac", "x".repeat((NAME_MAX_BYTES - 5) % 2));
        assert_eq!(name.len(), NAME_MAX_BYTES);
        let source = root.path().join(&name);
        fs::write(&source, b"audio bytes").expect("write source");
        let capability = open_regular_file(&source).expect("open source");
        let source_path = PinnedFilePath::existing_parent(&source).expect("pin source");

        trash_regular_file_to_location(
            &source_path,
            &capability,
            &TrashLocation {
                root: trash.clone(),
                info_path: source.clone(),
            },
        )
        .expect("trash a maximal file name");

        let entries = fs::read_dir(trash.join("files"))
            .expect("list trashed files")
            .map(|entry| entry.expect("trash entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        let entry = entries[0].to_str().expect("a UTF-8 entry name");
        assert!(name.starts_with(entry));
        let info = fs::read_to_string(trash.join("info").join(format!("{entry}.trashinfo")))
            .expect("read trash info");
        assert!(info.contains(&encode_uri_path(&source)));
    }

    #[test]
    fn an_unusable_shared_user_trash_falls_back_to_the_private_volume_trash() {
        let topdir = tempfile::tempdir().expect("create volume top directory");
        let shared = topdir.path().join(".Trash");
        fs::create_dir(&shared).expect("create shared trash");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777))
            .expect("make shared trash sticky");
        let uid = rustix::process::getuid().as_raw();
        fs::write(shared.join(uid.to_string()), b"").expect("obstruct the user's directory");

        let root = topdir_trash_root(topdir.path(), uid).expect("pick a volume trash");

        assert_eq!(root, topdir.path().join(format!(".Trash-{uid}")));
        assert!(root.is_dir());
    }

    #[test]
    fn desktop_trash_publication_uses_the_pinned_source_parent() {
        let root = tempfile::tempdir().expect("create test root");
        let library = root.path().join("library");
        let displaced = root.path().join("displaced");
        let outside = root.path().join("outside");
        let trash = root.path().join("Trash");
        fs::create_dir(&library).expect("create library");
        fs::create_dir(&outside).expect("create outside");
        let source = library.join("song.flac");
        fs::write(&source, b"audio bytes").expect("write source");
        let capability = open_regular_file(&source).expect("open source");
        let source_path = PinnedFilePath::existing_parent(&source).expect("pin source");
        fs::rename(&library, &displaced).expect("displace source parent");
        symlink(&outside, &library).expect("redirect original source parent path");
        fs::write(outside.join("song.flac"), b"unrelated bytes").expect("write unrelated file");

        trash_regular_file_to_location(
            &source_path,
            &capability,
            &TrashLocation {
                root: trash.clone(),
                info_path: source,
            },
        )
        .expect("trash retained source");

        assert!(!displaced.join("song.flac").exists());
        assert_eq!(
            fs::read(trash.join("files/song.flac")).expect("read trashed file"),
            b"audio bytes"
        );
        assert_eq!(
            fs::read(outside.join("song.flac")).expect("read unrelated file"),
            b"unrelated bytes"
        );
    }
}
