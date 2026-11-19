// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Where the running instance really keeps its settings, library database
//! and caches (#358).
//!
//! [`launch::resolve_paths`](crate::launch::resolve_paths) says where each
//! of them is named; this module locates each by where it is, before any
//! lock is taken or store opened, so that everything later keyed off a
//! path — the instance locks, the database's own sidecars, the settings
//! file's atomic replacement — is keyed off the file itself and not the
//! spelling it was given by:
//!
//! * a name that exists is located by following its links; a name that
//!   does not exist yet is located under where its folder is. A name the
//!   lookup cannot reach, as behind a folder that denies access, is kept
//!   as given: nothing can be opened through it either, so nothing can be
//!   written over through it, and what opens it reports why it cannot. A
//!   link to nothing is refused, as a file created through it would be
//!   created where it leads;
//! * a database or settings file named as a folder (ending in `/`, `.`
//!   or `..`) is refused: either names nothing a file could be opened at,
//!   and its lock and sidecars would be made inside it;
//! * a database or settings file with several hard links is refused: the
//!   names Sustain derives from it (its lock, SQLite's journal, the
//!   settings file's replacement) would belong to one name only;
//! * the files each part owns must be apart: a settings file named like
//!   the database, its journal, its lock or its outbox, or a file inside
//!   the cache folder, is refused before anything is written over it.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::launch::ResolvedPaths;

/// The names SQLite derives from a database file for its journal: the
/// write-ahead log and its index, and the rollback journal.
const DATABASE_JOURNAL_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// The suffix of the file a lock is taken on, beside what it locks.
const LOCK_SUFFIX: &str = ".lock";

/// Each part of the instance's storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StoragePart {
    Settings,
    Database,
    Cache,
}

impl fmt::Display for StoragePart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Settings => "settings file",
            Self::Database => "library database",
            Self::Cache => "cache folder",
        })
    }
}

/// The settings file, library database and cache folder, each located by
/// where it really is.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct StorageLayout {
    pub(crate) settings: PathBuf,
    pub(crate) database: PathBuf,
    pub(crate) cache_dir: PathBuf,
}

/// Why the storage layout was refused.
#[derive(Debug)]
pub(crate) enum LayoutError {
    /// The location names a link to nothing, or is relative while the
    /// working directory is unavailable.
    Unlocatable {
        part: StoragePart,
        path: PathBuf,
        error: io::Error,
    },
    /// The file is named as a folder.
    NamesAFolder { part: StoragePart, path: PathBuf },
    /// The file has more than one hard link.
    HardLinked { part: StoragePart, path: PathBuf },
    /// A file one part owns is, or is inside, a file another part owns.
    Overlap {
        part: StoragePart,
        path: PathBuf,
        other_part: StoragePart,
        other_path: PathBuf,
    },
}

impl fmt::Display for LayoutError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unlocatable { part, path, error } => {
                write!(
                    formatter,
                    "the {part} {} cannot be located ({error})",
                    path.display()
                )
            }
            Self::NamesAFolder { part, path } => write!(
                formatter,
                "the {part} {} is named as a folder; give it as a file",
                path.display()
            ),
            Self::HardLinked { part, path } => write!(
                formatter,
                "the {part} {} has several hard links; give it by a single one",
                path.display()
            ),
            Self::Overlap {
                part,
                path,
                other_part,
                other_path,
            } => write!(
                formatter,
                "the {part} {} overlaps the {other_part} {}",
                path.display(),
                other_path.display()
            ),
        }
    }
}

impl StorageLayout {
    /// Locate every part of `paths` by where it is, and refuse a layout
    /// whose parts would write over one another.
    pub(crate) fn locate(paths: &ResolvedPaths) -> Result<Self, LayoutError> {
        refuse_folder_name(StoragePart::Settings, &paths.config)?;
        refuse_folder_name(StoragePart::Database, &paths.database)?;
        let layout = Self {
            settings: located(StoragePart::Settings, &paths.config)?,
            database: located(StoragePart::Database, &paths.database)?,
            cache_dir: located(StoragePart::Cache, &paths.cache_dir)?,
        };
        refuse_hard_links(StoragePart::Settings, &layout.settings)?;
        refuse_hard_links(StoragePart::Database, &layout.database)?;
        layout.refuse_overlaps()?;
        Ok(layout)
    }

    /// The file the library database's instance lock is taken on.
    pub(crate) fn database_lock(&self) -> PathBuf {
        with_suffix(&self.database, LOCK_SUFFIX)
    }

    /// The folder the settings file's instance lock is taken on: the one
    /// holding it, which is never named after it, so a settings file kept
    /// in a read-only folder or a dotfiles repository gets no lock file
    /// beside it.
    pub(crate) fn settings_lock(&self) -> &Path {
        self.settings.parent().unwrap_or(&self.settings)
    }

    /// Every file, or folder, each part owns.
    fn owned(&self) -> [(StoragePart, Vec<PathBuf>); 3] {
        let mut database = vec![
            self.database.clone(),
            self.database_lock(),
            sustain_library_store::tag_mirror_blob_root(&self.database),
        ];
        database.extend(
            DATABASE_JOURNAL_SUFFIXES
                .iter()
                .map(|suffix| with_suffix(&self.database, suffix)),
        );
        [
            (StoragePart::Settings, vec![self.settings.clone()]),
            (StoragePart::Database, database),
            (StoragePart::Cache, vec![self.cache_dir.clone()]),
        ]
    }

    fn refuse_overlaps(&self) -> Result<(), LayoutError> {
        let owned = self.owned();
        for (index, (part, paths)) in owned.iter().enumerate() {
            for (other_part, other_paths) in &owned[index + 1..] {
                for path in paths {
                    if let Some(other_path) = other_paths
                        .iter()
                        .find(|other| path.starts_with(other) || other.starts_with(path))
                    {
                        return Err(LayoutError::Overlap {
                            part: *part,
                            path: path.clone(),
                            other_part: *other_part,
                            other_path: other_path.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// `path` located by where it is, one name at a time: each name that
/// exists is followed through its links, and one that does not is kept
/// as named — a folder that does not exist is no link, so a `..` after it
/// is taken lexically, and what comes after that is looked up again. A
/// name the lookup cannot reach is kept as given; a link to nothing is
/// refused.
fn located(part: StoragePart, path: &Path) -> Result<PathBuf, LayoutError> {
    let unlocatable = |error| LayoutError::Unlocatable {
        part,
        path: path.to_path_buf(),
        error,
    };
    // Spelled as given, so a name kept as given keeps what the kernel
    // reads in it (`std::path::absolute` drops a trailing `/.`).
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_err(unlocatable)?.join(path)
    };
    // Holds no link: what exists of it is where it is, the rest missing.
    let mut located = PathBuf::new();
    let mut components = absolute.components().peekable();
    while let Some(component) = components.next() {
        match component {
            Component::Normal(name) => located.push(name),
            Component::ParentDir => {
                located.pop();
                continue;
            }
            Component::RootDir | Component::Prefix(_) => {
                located.push(component);
                continue;
            }
            Component::CurDir => continue,
        }
        let reached = match fs::symlink_metadata(&located) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                located = match fs::canonicalize(&located) {
                    Ok(target) => target,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        return Err(unlocatable(error));
                    }
                    Err(_) => return Ok(absolute),
                };
                fs::metadata(&located).ok()
            }
            Ok(metadata) => Some(metadata),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(_) => return Ok(absolute),
        };
        // A name followed by more is a folder: anything else names nothing
        // the kernel could open, so it is kept as given.
        if reached.is_some_and(|metadata| !metadata.is_dir()) && components.peek().is_some() {
            return Ok(absolute);
        }
    }
    Ok(located)
}

/// A file is never named by what names only a folder: a last name that is
/// empty (a trailing `/`), `.` or `..`.
fn refuse_folder_name(part: StoragePart, path: &Path) -> Result<(), LayoutError> {
    let last_name = path
        .as_os_str()
        .as_bytes()
        .rsplit(|byte| *byte == b'/')
        .next();
    if matches!(last_name, Some(b"" | b"." | b"..")) {
        return Err(LayoutError::NamesAFolder {
            part,
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

/// A file a lookup cannot reach cannot be told apart from another, nor
/// written through; only one that is reached can be refused.
fn refuse_hard_links(part: StoragePart, path: &Path) -> Result<(), LayoutError> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.nlink() > 1 => {
            Err(LayoutError::HardLinked {
                part,
                path: path.to_path_buf(),
            })
        }
        _ => Ok(()),
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test failures use panic! to report context")]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "sustain_storage_layout_{name}_{}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create scratch folder");
            Self(fs::canonicalize(&path).expect("locate scratch folder"))
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn paths(config: PathBuf, database: PathBuf, cache_dir: PathBuf) -> ResolvedPaths {
        ResolvedPaths {
            config,
            database,
            cache_dir,
        }
    }

    #[test]
    fn a_layout_is_located_by_where_each_part_is() {
        let scratch = Scratch::new("located");
        fs::create_dir(scratch.join("real")).expect("create real folder");
        std::os::unix::fs::symlink(scratch.join("real"), scratch.join("alias"))
            .expect("link the folder");
        fs::write(scratch.join("real/library.sqlite"), b"").expect("create database");

        let layout = StorageLayout::locate(&paths(
            scratch.join("alias/settings.toml"),
            scratch.join("alias/library.sqlite"),
            scratch.join("alias/new/cache"),
        ))
        .expect("a layout apart");

        assert_eq!(
            layout,
            StorageLayout {
                settings: scratch.join("real/settings.toml"),
                database: scratch.join("real/library.sqlite"),
                cache_dir: scratch.join("real/new/cache"),
            }
        );
        assert_eq!(
            layout.database_lock(),
            scratch.join("real/library.sqlite.lock")
        );
    }

    #[test]
    fn a_settings_file_given_by_a_link_is_located_where_the_link_leads() {
        let scratch = Scratch::new("settings_link");
        fs::create_dir(scratch.join("dotfiles")).expect("create dotfiles folder");
        fs::write(scratch.join("dotfiles/settings.toml"), b"").expect("create settings");
        std::os::unix::fs::symlink(
            scratch.join("dotfiles/settings.toml"),
            scratch.join("settings.toml"),
        )
        .expect("link the settings");

        let layout = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("library.sqlite"),
            scratch.join("cache"),
        ))
        .expect("a layout apart");

        assert_eq!(layout.settings, scratch.join("dotfiles/settings.toml"));
    }

    #[test]
    fn a_relative_name_is_located_under_the_working_directory() {
        let working_dir = fs::canonicalize(std::env::current_dir().expect("working directory"))
            .expect("locate working directory");
        let located = located(
            StoragePart::Database,
            Path::new("sustain-storage-layout-absent.sqlite"),
        )
        .expect("located");

        assert_eq!(
            located,
            working_dir.join("sustain-storage-layout-absent.sqlite")
        );
    }

    #[test]
    fn a_missing_folder_left_by_its_parent_name_is_located_lexically() {
        let scratch = Scratch::new("parent_name");
        let layout = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("new/../library.sqlite"),
            scratch.join("cache"),
        ))
        .expect("a layout apart");

        assert_eq!(layout.database, scratch.join("library.sqlite"));
    }

    #[test]
    fn a_link_reached_past_a_missing_folder_is_followed() {
        let scratch = Scratch::new("link_past_missing");
        fs::write(scratch.join("real.sqlite"), b"").expect("create database");
        std::os::unix::fs::symlink(scratch.join("real.sqlite"), scratch.join("alias.sqlite"))
            .expect("link the database");

        let layout = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("missing/../alias.sqlite"),
            scratch.join("cache"),
        ))
        .expect("a layout apart");

        assert_eq!(layout.database, scratch.join("real.sqlite"));
    }

    #[test]
    fn a_file_is_no_folder_to_leave_by_its_parent_name() {
        let scratch = Scratch::new("file_parent_name");
        fs::write(scratch.join("notes"), b"").expect("create a file");
        fs::write(scratch.join("library.sqlite"), b"").expect("create database");

        let layout = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("notes/../library.sqlite"),
            scratch.join("cache"),
        ))
        .expect("a layout apart");

        assert_eq!(layout.database, scratch.join("notes/../library.sqlite"));
    }

    #[test]
    fn a_database_or_settings_file_named_as_a_folder_is_refused() {
        let scratch = Scratch::new("file_as_folder");
        fs::write(scratch.join("settings.toml"), b"").expect("create settings");
        fs::write(scratch.join("library.sqlite"), b"").expect("create database");
        for spelling in [
            "settings.toml/",
            "settings.toml/.",
            "settings.toml/sub/..",
            "settings.toml/sub/../",
            "new.toml/",
            "new.toml/sub/../",
        ] {
            let given = PathBuf::from(format!("{}/{spelling}", scratch.0.display()));
            let cases = [
                (
                    StoragePart::Settings,
                    given.clone(),
                    scratch.join("library.sqlite"),
                ),
                (
                    StoragePart::Database,
                    scratch.join("settings.toml"),
                    given.clone(),
                ),
            ];
            for (expected, settings, database) in cases {
                let refused =
                    StorageLayout::locate(&paths(settings, database, scratch.join("cache")));

                match refused {
                    Err(LayoutError::NamesAFolder { part, path }) => {
                        assert_eq!(part, expected, "{spelling}");
                        assert_eq!(path.as_os_str(), given.as_os_str(), "{spelling}");
                    }
                    other => panic!("{spelling} as the {expected}: {other:?}"),
                }
            }
        }
    }

    #[test]
    fn a_cache_folder_named_as_a_folder_is_located() {
        let scratch = Scratch::new("cache_as_folder");

        let layout = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("library.sqlite"),
            PathBuf::from(format!("{}/cache/", scratch.0.display())),
        ))
        .expect("a layout apart");

        assert_eq!(layout.cache_dir, scratch.join("cache"));
    }

    #[test]
    fn a_location_behind_a_folder_that_denies_access_is_kept_as_given() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = Scratch::new("denied");
        fs::create_dir(scratch.join("closed")).expect("create a folder");
        fs::set_permissions(scratch.join("closed"), fs::Permissions::from_mode(0o000))
            .expect("deny access to it");
        let denied = fs::symlink_metadata(scratch.join("closed/cache"))
            .is_err_and(|error| error.kind() == io::ErrorKind::PermissionDenied);

        let layout = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("library.sqlite"),
            scratch.join("closed/cache"),
        ));
        fs::set_permissions(scratch.join("closed"), fs::Permissions::from_mode(0o700))
            .expect("restore access");

        // Root reaches every folder; there is nothing to deny then.
        if denied {
            assert_eq!(
                layout.expect("a layout apart").cache_dir,
                scratch.join("closed/cache")
            );
        }
    }

    #[test]
    fn a_link_to_nothing_is_refused() {
        let scratch = Scratch::new("dangling");
        std::os::unix::fs::symlink(scratch.join("gone.sqlite"), scratch.join("library.sqlite"))
            .expect("link to nothing");

        let refused = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("library.sqlite"),
            scratch.join("cache"),
        ));

        assert!(
            matches!(
                refused,
                Err(LayoutError::Unlocatable {
                    part: StoragePart::Database,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn a_settings_file_named_as_the_database_is_refused() {
        let scratch = Scratch::new("settings_is_database");
        let refused = StorageLayout::locate(&paths(
            scratch.join("sustain.sqlite"),
            scratch.join("sustain.sqlite"),
            scratch.join("cache"),
        ));

        assert!(
            matches!(
                &refused,
                Err(LayoutError::Overlap {
                    part: StoragePart::Settings,
                    other_part: StoragePart::Database,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn a_settings_file_named_as_a_name_the_database_owns_is_refused() {
        let scratch = Scratch::new("settings_is_journal");
        for owned in [
            "sustain.sqlite-wal",
            "sustain.sqlite-shm",
            "sustain.sqlite-journal",
            "sustain.sqlite.lock",
            "sustain.sqlite-tag-outbox/settings.toml",
        ] {
            let refused = StorageLayout::locate(&paths(
                scratch.join(owned),
                scratch.join("sustain.sqlite"),
                scratch.join("cache"),
            ));

            assert!(
                matches!(&refused, Err(LayoutError::Overlap { .. })),
                "{owned}: {refused:?}"
            );
        }
    }

    #[test]
    fn a_database_reached_by_a_link_beside_the_settings_overlaps_them() {
        let scratch = Scratch::new("database_link_is_settings");
        fs::write(scratch.join("sustain.toml"), b"").expect("create settings");
        std::os::unix::fs::symlink(scratch.join("sustain.toml"), scratch.join("library.sqlite"))
            .expect("link the database to the settings");

        let refused = StorageLayout::locate(&paths(
            scratch.join("sustain.toml"),
            scratch.join("library.sqlite"),
            scratch.join("cache"),
        ));

        assert!(
            matches!(&refused, Err(LayoutError::Overlap { .. })),
            "{refused:?}"
        );
    }

    #[test]
    fn a_file_inside_the_cache_folder_is_refused() {
        let scratch = Scratch::new("inside_cache");
        let refused = StorageLayout::locate(&paths(
            scratch.join("sustain.toml"),
            scratch.join("cache/library.sqlite"),
            scratch.join("cache"),
        ));

        assert!(
            matches!(
                &refused,
                Err(LayoutError::Overlap {
                    part: StoragePart::Database,
                    other_part: StoragePart::Cache,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn a_hard_linked_database_is_refused() {
        let scratch = Scratch::new("hard_link");
        fs::write(scratch.join("library.sqlite"), b"").expect("create database");
        fs::hard_link(scratch.join("library.sqlite"), scratch.join("twin.sqlite"))
            .expect("hard link the database");

        let refused = StorageLayout::locate(&paths(
            scratch.join("settings.toml"),
            scratch.join("twin.sqlite"),
            scratch.join("cache"),
        ));

        assert!(
            matches!(
                &refused,
                Err(LayoutError::HardLinked {
                    part: StoragePart::Database,
                    ..
                })
            ),
            "{refused:?}"
        );
    }

    #[test]
    fn a_hard_linked_settings_file_is_refused() {
        let scratch = Scratch::new("settings_hard_link");
        fs::write(scratch.join("settings.toml"), b"").expect("create settings");
        fs::hard_link(scratch.join("settings.toml"), scratch.join("twin.toml"))
            .expect("hard link the settings");

        let refused = StorageLayout::locate(&paths(
            scratch.join("twin.toml"),
            scratch.join("library.sqlite"),
            scratch.join("cache"),
        ));

        assert!(
            matches!(
                &refused,
                Err(LayoutError::HardLinked {
                    part: StoragePart::Settings,
                    ..
                })
            ),
            "{refused:?}"
        );
    }
}
