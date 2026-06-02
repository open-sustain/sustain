// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Durable pathname-level copy-on-write replacement for audio tag edits.
//!
//! Tag mirroring deliberately publishes a new inode at the edited pathname.
//! Readers that already hold the prior inode, including active GStreamer
//! playback, keep reading stable bytes until they close their descriptors.
//! Other hard links to the source inode intentionally retain the old bytes.
//!
//! Publication is integrity-first: a sibling staging file is created
//! exclusively, populated from a no-follow source fd, modified by the caller,
//! assigned the source uid/gid and mode, given every readable source xattr
//! (including POSIX ACL xattrs), synced, exchanged into place, and followed by
//! removal of the old alias and a containing-directory fsync. Any failure
//! before exchange leaves the original pathname untouched. A directory-sync
//! failure after exchange is reported as a failure because crash durability is
//! uncertain; rolling back would add a second unsafe namespace mutation.

use std::{
    ffi::{CStr, CString, OsStr, OsString},
    fs::File,
    io::{self, Write},
    os::fd::{AsRawFd, OwnedFd},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::{
    fs::{
        AtFlags, CWD, FileType, Mode, OFlags, RenameFlags, Stat, XattrFlags, fchmod, fchown,
        fgetxattr, flistxattr, fremovexattr, fsetxattr, fstat, fsync, openat, renameat_with,
        statat, unlinkat,
    },
    io::Errno,
};

use crate::{MetadataError, MetadataResult};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const SOURCE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const TEMPORARY_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
const TEMPORARY_MODE: Mode = Mode::RUSR.union(Mode::WUSR);
const MAX_TEMPORARY_ATTEMPTS: usize = 100;
const MAX_XATTR_READ_ATTEMPTS: usize = 4;

/// Replace `path` with a durably published sibling copy after applying
/// `modify_temp`. This never falls back to an in-place write.
pub(super) fn atomic_write_via_rename<F>(path: &Path, modify_temp: F) -> MetadataResult<()>
where
    F: FnOnce(&Path) -> MetadataResult<()>,
{
    atomic_write_via_rename_with(path, modify_temp, temporary_sibling_name, |parent| {
        fsync(parent).map_err(io::Error::from)
    })
}

fn atomic_write_via_rename_with<F, N, S>(
    path: &Path,
    modify_temp: F,
    mut next_temporary_name: N,
    sync_parent: S,
) -> MetadataResult<()>
where
    F: FnOnce(&Path) -> MetadataResult<()>,
    N: FnMut(&OsStr) -> OsString,
    S: FnOnce(&OwnedFd) -> io::Result<()>,
{
    let file_name = path.file_name().ok_or(MetadataError::WriteFailed)?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_directory_path(parent_path)?;
    let source_fd = openat(&parent, file_name, SOURCE_FLAGS, Mode::empty())
        .map_err(|_| MetadataError::WriteFailed)?;
    let mut source = File::from(source_fd);
    let source_stat = regular_file_stat(&source)?;

    let (temporary_name, temporary_path, mut temporary) =
        create_temporary_sibling(&parent, file_name, &mut next_temporary_name)?;
    let temporary_stat = match regular_file_stat(&temporary) {
        Ok(stat) => stat,
        Err(error) => {
            remove_temporary_if_owned(&parent, &temporary_name, &temporary);
            return Err(error);
        }
    };
    let staged = (|| {
        io::copy(&mut source, &mut temporary).map_err(|_| MetadataError::WriteFailed)?;
        temporary.flush().map_err(|_| MetadataError::WriteFailed)?;

        modify_temp(&temporary_path)?;

        if !pathname_refers_to_open_file(&parent, &temporary_name, &temporary, &temporary_stat)? {
            return Err(MetadataError::WriteFailed);
        }
        preserve_filesystem_metadata(&source, &source_stat, &temporary)?;
        temporary
            .sync_all()
            .map_err(|_| MetadataError::WriteFailed)?;
        ensure_source_unchanged(&parent, file_name, &source, &source_stat)?;
        if !pathname_refers_to_open_file(&parent, &temporary_name, &temporary, &temporary_stat)? {
            return Err(MetadataError::WriteFailed);
        }
        Ok(())
    })();
    if staged.is_err() {
        remove_temporary_if_owned(&parent, &temporary_name, &temporary);
        return staged;
    }

    if renameat_with(
        &parent,
        &temporary_name,
        &parent,
        file_name,
        RenameFlags::EXCHANGE,
    )
    .is_err()
    {
        remove_temporary_if_owned(&parent, &temporary_name, &temporary);
        return Err(MetadataError::WriteFailed);
    }
    if !pathname_refers_to_open_file(&parent, file_name, &temporary, &temporary_stat)?
        || !pathname_refers_to_open_file(&parent, &temporary_name, &source, &source_stat)?
    {
        if renameat_with(
            &parent,
            &temporary_name,
            &parent,
            file_name,
            RenameFlags::EXCHANGE,
        )
        .is_ok()
        {
            remove_temporary_if_owned(&parent, &temporary_name, &temporary);
        }
        return Err(MetadataError::WriteFailed);
    }
    let remove_old_source = unlink_temporary_if_owned(&parent, &temporary_name, &source);
    let sync_result = sync_parent(&parent).map_err(|_| MetadataError::WriteFailed);
    remove_old_source?;
    sync_result
}

fn open_directory_path(path: &Path) -> MetadataResult<OwnedFd> {
    let mut directory =
        openat(CWD, ".", DIRECTORY_FLAGS, Mode::empty()).map_err(|_| MetadataError::WriteFailed)?;
    for component in path.components() {
        match component {
            Component::RootDir => {
                directory = openat(CWD, "/", DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|_| MetadataError::WriteFailed)?;
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                directory = openat(&directory, name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|_| MetadataError::WriteFailed)?;
            }
            Component::ParentDir | Component::Prefix(_) => {
                return Err(MetadataError::WriteFailed);
            }
        }
    }
    Ok(directory)
}

fn create_temporary_sibling<N>(
    parent: &OwnedFd,
    file_name: &OsStr,
    next_temporary_name: &mut N,
) -> MetadataResult<(OsString, PathBuf, File)>
where
    N: FnMut(&OsStr) -> OsString,
{
    for _ in 0..MAX_TEMPORARY_ATTEMPTS {
        let temporary_name = next_temporary_name(file_name);
        if !is_single_normal_component(&temporary_name) {
            return Err(MetadataError::WriteFailed);
        }
        match openat(parent, &temporary_name, TEMPORARY_FLAGS, TEMPORARY_MODE) {
            Ok(fd) => {
                let temporary_path = proc_self_fd_path(parent.as_raw_fd()).join(&temporary_name);
                return Ok((temporary_name, temporary_path, File::from(fd)));
            }
            Err(Errno::EXIST) => continue,
            Err(_) => return Err(MetadataError::WriteFailed),
        }
    }
    Err(MetadataError::WriteFailed)
}

fn proc_self_fd_path(fd: std::os::fd::RawFd) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{fd}"))
}

fn is_single_normal_component(name: &OsStr) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(Component::Normal(_)), None)
    )
}

fn temporary_sibling_name(file_name: &OsStr) -> OsString {
    static NEXT_TEMPORARY_ID: AtomicU64 = AtomicU64::new(0);

    let id = NEXT_TEMPORARY_ID.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(
        ".sustain-tag-write-{}-{id}.tmp",
        std::process::id()
    ));
    name
}

fn remove_temporary_if_owned(parent: &OwnedFd, temporary_name: &OsStr, temporary: &File) {
    let _ = unlink_temporary_if_owned(parent, temporary_name, temporary);
}

fn unlink_temporary_if_owned(
    parent: &OwnedFd,
    temporary_name: &OsStr,
    temporary: &File,
) -> MetadataResult<()> {
    let Ok(temporary_stat) = regular_file_stat(temporary) else {
        return Err(MetadataError::WriteFailed);
    };
    if pathname_refers_to_open_file(parent, temporary_name, temporary, &temporary_stat)
        .unwrap_or(false)
    {
        unlinkat(parent, temporary_name, AtFlags::empty()).map_err(|_| MetadataError::WriteFailed)
    } else {
        Err(MetadataError::WriteFailed)
    }
}

fn pathname_refers_to_open_file(
    parent: &OwnedFd,
    name: &OsStr,
    file: &File,
    original: &Stat,
) -> MetadataResult<bool> {
    let open_file = regular_file_stat(file)?;
    let pathname =
        statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| MetadataError::WriteFailed)?;
    Ok(
        FileType::from_raw_mode(pathname.st_mode) == FileType::RegularFile
            && same_inode(original, &open_file)
            && same_inode(original, &pathname),
    )
}

fn regular_file_stat(file: &File) -> MetadataResult<Stat> {
    let stat = fstat(file).map_err(|_| MetadataError::WriteFailed)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(MetadataError::WriteFailed);
    }
    Ok(stat)
}

fn preserve_filesystem_metadata(
    source: &File,
    source_stat: &Stat,
    staged: &File,
) -> MetadataResult<()> {
    let staged_stat = fstat(staged).map_err(|_| MetadataError::WriteFailed)?;
    if staged_stat.st_uid != source_stat.st_uid || staged_stat.st_gid != source_stat.st_gid {
        fchown(
            staged,
            Some(rustix::fs::Uid::from_raw(source_stat.st_uid)),
            Some(rustix::fs::Gid::from_raw(source_stat.st_gid)),
        )
        .map_err(|_| MetadataError::WriteFailed)?;
    }
    fchmod(staged, Mode::from_raw_mode(source_stat.st_mode))
        .map_err(|_| MetadataError::WriteFailed)?;
    let readable_xattrs = reconcile_readable_xattrs(source, staged)?;
    fchmod(staged, Mode::from_raw_mode(source_stat.st_mode))
        .map_err(|_| MetadataError::WriteFailed)?;
    ensure_readable_xattrs_match(staged, &readable_xattrs)?;

    let staged_stat = fstat(staged).map_err(|_| MetadataError::WriteFailed)?;
    if staged_stat.st_uid != source_stat.st_uid
        || staged_stat.st_gid != source_stat.st_gid
        || Mode::from_raw_mode(staged_stat.st_mode) != Mode::from_raw_mode(source_stat.st_mode)
    {
        return Err(MetadataError::WriteFailed);
    }
    Ok(())
}

fn reconcile_readable_xattrs(
    source: &File,
    staged: &File,
) -> MetadataResult<Vec<(CString, Vec<u8>)>> {
    let source_xattrs = readable_xattrs(source)?;
    for (name, _) in readable_xattrs(staged)? {
        if source_xattrs
            .iter()
            .all(|(source_name, _)| source_name != &name)
        {
            fremovexattr(staged, &name).map_err(|_| MetadataError::WriteFailed)?;
        }
    }
    for (name, value) in &source_xattrs {
        fsetxattr(staged, name, value, XattrFlags::empty())
            .map_err(|_| MetadataError::WriteFailed)?;
    }
    ensure_readable_xattrs_match(staged, &source_xattrs)?;
    Ok(source_xattrs)
}

fn readable_xattrs(file: &File) -> MetadataResult<Vec<(CString, Vec<u8>)>> {
    let mut xattrs = Vec::new();
    for name in list_xattrs(file).map_err(|_| MetadataError::WriteFailed)? {
        let Some(value) = read_xattr(file, &name).map_err(|_| MetadataError::WriteFailed)? else {
            continue;
        };
        xattrs.push((name, value));
    }
    Ok(xattrs)
}

fn ensure_readable_xattrs_match(
    file: &File,
    expected: &[(CString, Vec<u8>)],
) -> MetadataResult<()> {
    let actual = readable_xattrs(file)?;
    if actual.len() != expected.len()
        || expected.iter().any(|attribute| !actual.contains(attribute))
    {
        return Err(MetadataError::WriteFailed);
    }
    Ok(())
}

fn list_xattrs(file: &File) -> io::Result<Vec<CString>> {
    let bytes = read_xattr_buffer(|buffer| flistxattr(file, buffer))?;
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.last() != Some(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "xattr list lacks a trailing NUL",
        ));
    }
    bytes
        .split_inclusive(|byte| *byte == 0)
        .map(|name| {
            CString::from_vec_with_nul(name.to_vec()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid xattr name returned by kernel",
                )
            })
        })
        .collect()
}

fn read_xattr(file: &File, name: &CStr) -> io::Result<Option<Vec<u8>>> {
    match read_xattr_buffer(|buffer| fgetxattr(file, name, buffer)) {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_missing_xattr(&error) || is_unreadable_xattr(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_xattr_buffer(
    mut read: impl FnMut(&mut [u8]) -> Result<usize, Errno>,
) -> io::Result<Vec<u8>> {
    for _ in 0..MAX_XATTR_READ_ATTEMPTS {
        let required = match read(&mut []) {
            Ok(required) => required,
            Err(error) if is_unsupported_xattr(error) => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        if required == 0 {
            return Ok(Vec::new());
        }
        let mut bytes = vec![0; required];
        match read(&mut bytes) {
            Ok(written) => {
                bytes.truncate(written);
                return Ok(bytes);
            }
            Err(Errno::RANGE) => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "xattr changed repeatedly while being read",
    ))
}

fn is_unsupported_xattr(error: Errno) -> bool {
    error == Errno::NOTSUP || error == Errno::OPNOTSUPP
}

fn is_missing_xattr(error: &io::Error) -> bool {
    error.raw_os_error() == Some(Errno::NODATA.raw_os_error())
}

fn is_unreadable_xattr(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(raw) if raw == Errno::ACCESS.raw_os_error() || raw == Errno::PERM.raw_os_error()
    )
}

fn ensure_source_unchanged(
    parent: &OwnedFd,
    file_name: &OsStr,
    source: &File,
    original: &Stat,
) -> MetadataResult<()> {
    let open_source = fstat(source).map_err(|_| MetadataError::WriteFailed)?;
    let pathname_source = statat(parent, file_name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| MetadataError::WriteFailed)?;
    if FileType::from_raw_mode(pathname_source.st_mode) != FileType::RegularFile
        || !same_source_version(original, &open_source)
        || !same_source_version(original, &pathname_source)
    {
        return Err(MetadataError::WriteFailed);
    }
    Ok(())
}

fn same_source_version(left: &Stat, right: &Stat) -> bool {
    same_inode(left, right)
        && left.st_mode == right.st_mode
        && left.st_uid == right.st_uid
        && left.st_gid == right.st_gid
        && left.st_size == right.st_size
        && left.st_mtime == right.st_mtime
        && left.st_mtime_nsec == right.st_mtime_nsec
        && left.st_ctime == right.st_ctime
        && left.st_ctime_nsec == right.st_ctime_nsec
}

fn same_inode(left: &Stat, right: &Stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        ffi::OsString,
        fs, io,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        path::{Path, PathBuf},
    };

    use rustix::{
        fs::{XattrFlags, fsync, getxattr, setxattr},
        io::Errno,
    };

    use super::{atomic_write_via_rename, atomic_write_via_rename_with};
    use crate::MetadataError;

    #[test]
    fn retries_exclusive_temp_collisions_without_following_symlinks() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("audio.bin");
        fs::write(&path, b"original").expect("seed audio");
        let host_target = root.join("host-target");
        fs::write(&host_target, b"host bytes").expect("seed host target");
        fs::write(root.join(".collision"), b"collision bytes").expect("seed collision");
        symlink(&host_target, root.join(".symlink-collision")).expect("seed symlink collision");

        let mut names = VecDeque::from([
            OsString::from(".collision"),
            OsString::from(".symlink-collision"),
            OsString::from(".owned-temp"),
        ]);
        atomic_write_via_rename_with(
            &path,
            |temporary| {
                fs::write(temporary, b"replacement").map_err(|_| MetadataError::WriteFailed)
            },
            |_| names.pop_front().expect("candidate name"),
            |parent| fsync(parent).map_err(io::Error::from),
        )
        .expect("replace");

        assert_eq!(fs::read(&path).expect("read audio"), b"replacement");
        assert_eq!(
            fs::read(root.join(".collision")).expect("read collision"),
            b"collision bytes"
        );
        assert_eq!(
            fs::read(&host_target).expect("read host target"),
            b"host bytes"
        );
        assert!(root.join(".symlink-collision").is_symlink());
        assert!(!root.join(".owned-temp").exists());

        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn reports_directory_sync_failure_after_atomic_publication() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("audio.bin");
        fs::write(&path, b"original").expect("seed audio");

        let result = atomic_write_via_rename_with(
            &path,
            |temporary| {
                fs::write(temporary, b"replacement").map_err(|_| MetadataError::WriteFailed)
            },
            |_| OsString::from(".owned-temp"),
            |_| Err(io::Error::other("injected directory fsync failure")),
        );

        assert_eq!(result, Err(MetadataError::WriteFailed));
        assert_eq!(fs::read(&path).expect("read audio"), b"replacement");
        assert!(!root.join(".owned-temp").exists());

        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn preserves_mode_owner_group_and_readable_xattrs() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("audio.bin");
        fs::write(&path, b"original").expect("seed audio");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).expect("set mode");
        match setxattr(
            &path,
            "user.sustain-test",
            b"xattr bytes",
            XattrFlags::empty(),
        ) {
            Ok(()) => {}
            Err(error) => {
                assert!(
                    error == Errno::NOTSUP || error == Errno::OPNOTSUPP,
                    "set source xattr: {error}"
                );
                fs::remove_dir_all(root).expect("remove root");
                return;
            }
        }
        let before = fs::metadata(&path).expect("source metadata");

        atomic_write_via_rename(&path, |temporary| {
            fs::write(temporary, b"replacement").map_err(|_| MetadataError::WriteFailed)?;
            setxattr(
                temporary,
                "user.sustain-staged-only",
                b"remove me",
                XattrFlags::empty(),
            )
            .map_err(|_| MetadataError::WriteFailed)
        })
        .expect("replace");

        let after = fs::metadata(&path).expect("replacement metadata");
        assert_eq!(after.mode() & 0o7777, 0o640);
        assert_eq!(after.uid(), before.uid());
        assert_eq!(after.gid(), before.gid());
        let mut xattr = vec![0; 32];
        let xattr_len = getxattr(&path, "user.sustain-test", &mut xattr).expect("get xattr");
        xattr.truncate(xattr_len);
        assert_eq!(xattr, b"xattr bytes");
        assert_eq!(
            getxattr(&path, "user.sustain-staged-only", &mut xattr),
            Err(Errno::NODATA)
        );

        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn hard_link_replacement_is_intentionally_pathname_level_copy_on_write() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("audio.bin");
        let other_link = root.join("other-link.bin");
        fs::write(&path, b"original").expect("seed audio");
        fs::hard_link(&path, &other_link).expect("create hard link");

        atomic_write_via_rename(&path, |temporary| {
            fs::write(temporary, b"replacement").map_err(|_| MetadataError::WriteFailed)
        })
        .expect("replace");

        assert_eq!(fs::read(&path).expect("read replacement"), b"replacement");
        assert_eq!(fs::read(&other_link).expect("read other link"), b"original");

        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn rejects_symlink_audio_path_without_touching_its_target() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let target = root.join("target.bin");
        let path = root.join("audio.bin");
        fs::write(&target, b"target bytes").expect("seed target");
        symlink(&target, &path).expect("seed audio symlink");

        let result = atomic_write_via_rename(&path, |temporary| {
            fs::write(temporary, b"replacement").map_err(|_| MetadataError::WriteFailed)
        });

        assert_eq!(result, Err(MetadataError::WriteFailed));
        assert_eq!(fs::read(&target).expect("read target"), b"target bytes");
        assert!(path.is_symlink());

        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn rejects_symlinked_parent_without_touching_audio_outside_that_namespace() {
        let root = unique_test_directory();
        let outside = root.join("Outside");
        fs::create_dir_all(&outside).expect("create outside");
        let target = outside.join("audio.bin");
        fs::write(&target, b"target bytes").expect("seed target");
        symlink(&outside, root.join("Linked")).expect("seed parent symlink");

        let result = atomic_write_via_rename(&root.join("Linked/audio.bin"), |temporary| {
            fs::write(temporary, b"replacement").map_err(|_| MetadataError::WriteFailed)
        });

        assert_eq!(result, Err(MetadataError::WriteFailed));
        assert_eq!(fs::read(&target).expect("read target"), b"target bytes");
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn parent_directory_replacement_does_not_redirect_tag_publication() {
        let root = unique_test_directory();
        let album = root.join("Album");
        let displaced = root.join("Displaced");
        let outside = root.join("Outside");
        fs::create_dir_all(&album).expect("create album");
        fs::create_dir(&outside).expect("create outside");
        let path = album.join("audio.bin");
        fs::write(&path, b"original").expect("seed audio");
        fs::write(outside.join("audio.bin"), b"unrelated").expect("seed unrelated audio");

        atomic_write_via_rename(&path, |temporary| {
            fs::rename(&album, &displaced).map_err(|_| MetadataError::WriteFailed)?;
            symlink(&outside, &album).map_err(|_| MetadataError::WriteFailed)?;
            fs::write(temporary, b"replacement").map_err(|_| MetadataError::WriteFailed)
        })
        .expect("replace through pinned parent");

        assert_eq!(
            fs::read(displaced.join("audio.bin")).expect("read replaced audio"),
            b"replacement"
        );
        assert_eq!(
            fs::read(outside.join("audio.bin")).expect("read unrelated audio"),
            b"unrelated"
        );
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn rejects_source_changes_during_staging_without_overwriting_newer_bytes() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("audio.bin");
        fs::write(&path, b"original").expect("seed audio");

        let result = atomic_write_via_rename(&path, |temporary| {
            fs::write(temporary, b"staged replacement").map_err(|_| MetadataError::WriteFailed)?;
            fs::write(&path, b"newer external bytes").map_err(|_| MetadataError::WriteFailed)
        });

        assert_eq!(result, Err(MetadataError::WriteFailed));
        assert_eq!(
            fs::read(&path).expect("read audio"),
            b"newer external bytes"
        );
        assert_no_temporary_files(&root);

        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn rejects_replaced_staging_path_without_publishing_unowned_inode() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("audio.bin");
        fs::write(&path, b"original").expect("seed audio");

        let result = atomic_write_via_rename(&path, |temporary| {
            fs::remove_file(temporary).map_err(|_| MetadataError::WriteFailed)?;
            fs::write(temporary, b"unowned replacement").map_err(|_| MetadataError::WriteFailed)
        });

        assert_eq!(result, Err(MetadataError::WriteFailed));
        assert_eq!(fs::read(&path).expect("read audio"), b"original");
        let leftovers = temporary_files(&root);
        assert_eq!(leftovers.len(), 1);
        assert_eq!(
            fs::read(root.join(&leftovers[0])).expect("read unowned replacement"),
            b"unowned replacement"
        );

        fs::remove_dir_all(root).expect("remove root");
    }

    fn assert_no_temporary_files(root: &Path) {
        let leftovers = temporary_files(root);
        assert!(
            leftovers.is_empty(),
            "temporary files remain: {leftovers:?}"
        );
    }

    fn temporary_files(root: &Path) -> Vec<OsString> {
        fs::read_dir(root)
            .expect("list root")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains(".sustain-tag-write-"))
            .collect()
    }

    fn unique_test_directory() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        // A wall-clock timestamp is not actually unique: two tests on
        // parallel harness threads can read the same tick (or the clock
        // can step backwards), landing in the same directory and racing
        // each other's `remove_dir_all`. Mirror the production temp-name
        // scheme (`temporary_sibling_name`) instead: a process id plus a
        // monotonic counter is collision-free within and across runs.
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "sustain_atomic_metadata_test_{}_{id}",
            std::process::id()
        ))
    }
}
