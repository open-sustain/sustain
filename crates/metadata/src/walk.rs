// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The walk over a tree of files, and what it makes of symbolic links.
//!
//! A link is followed: a linked folder is walked and a linked file found,
//! so a library may gather music kept elsewhere. Every folder is walked
//! once, under the first name the walk reaches it by. The roots are walked
//! one after the other, in the order given, each under the name given,
//! link or not; under each, the walk goes through folder entries in name
//! order, and through every entry reached without a link before following
//! any link. Another name for a folder
//! already walked — a link back to an ancestor included — is skipped, as
//! everything in it has been found; that is not a failure. A link that
//! leads nowhere is: what it named cannot be told missing.
//!
//! A link is also another name for a file. The walk reports each file
//! once, with every name it was reached by that the caller accepts, each
//! marked if the caller already knows the file by it. The caller judges a
//! name together with the file's own path, which differ for a link to the
//! file. A name the caller knows a file by, under a root, that the walk
//! did not pass — behind a folder alias it skipped, or in a folder it
//! could pass through but not list — is looked up on its own and given to
//! the file it names, so that the file is not taken for one the caller
//! does not know. Which of a file's names to keep is the caller's;
//! [`FileWalk::kept_names`] is the rule a library scan keeps them by.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// The filesystem calls the walk makes, so tests can make them fail.
pub(crate) trait WalkFilesystem {
    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>>;
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StdWalkFilesystem;

impl WalkFilesystem for StdWalkFilesystem {
    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>> {
        Ok(Box::new(
            fs::read_dir(path)?.map(|entry| entry.map(|entry| entry.path())),
        ))
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }
}

/// What a walk is asked to find.
pub struct WalkRequest<'a> {
    /// The folders and files to walk, each possibly a link to one.
    pub roots: &'a [PathBuf],
    /// The names the caller already knows files by.
    pub known: &'a HashSet<PathBuf>,
    /// Whether a name for a file, and the file's own path, are ones the
    /// caller wants. They differ only for a link to the file itself.
    pub accept: &'a dyn Fn(&Path, &Path) -> bool,
    /// Whether an entry under a root is to be passed over, with anything
    /// under it.
    pub skip: &'a dyn Fn(&Path) -> bool,
    pub cancellation: &'a AtomicBool,
}

/// A name a file was reached by.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileName {
    pub path: PathBuf,
    /// The name goes through a link.
    pub through_link: bool,
    /// The caller knows the file by this name.
    pub known: bool,
}

/// A regular file the walk found, and the accepted names it has.
#[derive(Debug)]
pub struct FoundFile {
    /// In the order the walk reached them.
    pub names: Vec<FileName>,
    /// The metadata of the file itself, a link's target's for a link.
    pub metadata: fs::Metadata,
}

/// Everything a walk found, and the paths it could not look into.
#[derive(Debug, Default)]
pub struct FileWalk {
    /// In the order the walk first reached them.
    pub files: Vec<FoundFile>,
    /// The names of regular files the caller did not accept.
    pub rejected_names: usize,
    /// A path that could not be read, or a link that leads nowhere.
    pub failures: Vec<PathBuf>,
    /// The walk stopped because cancellation was requested.
    pub cancelled: bool,
    /// A name the caller knows a file by could not be looked up, so a file
    /// found under other names alone may be one the caller knows.
    pub known_name_unresolved: bool,
}

impl FileWalk {
    /// The names a library keeps `file` under, each one of its tracks. A
    /// file is one track: the names the library already holds it by, if
    /// any, so that no track changes path because the walk found another
    /// way to the file; otherwise the first name with no link in it —
    /// where the file is — or, if links alone reach it, the first name the
    /// walk reached it by. The same name on every walk. A file links alone
    /// reach is not kept under a new name while a known name could not be
    /// looked up: it may be the file that name is for.
    pub fn kept_names<'a>(&self, file: &'a FoundFile) -> Vec<&'a FileName> {
        if file.names.iter().any(|name| name.known) {
            return file.names.iter().filter(|name| name.known).collect();
        }
        match file.names.iter().find(|name| !name.through_link) {
            Some(name) => vec![name],
            None if self.known_name_unresolved => Vec::new(),
            None => file.names.first().into_iter().collect(),
        }
    }
}

/// Walk as the module documentation describes.
pub fn walk_files(request: &WalkRequest<'_>) -> FileWalk {
    walk_files_with(request, &StdWalkFilesystem)
}

pub(crate) fn walk_files_with(
    request: &WalkRequest<'_>,
    filesystem: &dyn WalkFilesystem,
) -> FileWalk {
    let mut walk = Walk {
        request,
        filesystem,
        walked_directories: HashSet::new(),
        files: HashMap::new(),
        files_in_order: Vec::new(),
        links: VecDeque::new(),
        found: FileWalk::default(),
    };
    for root in request.roots {
        if walk.stopped() {
            break;
        }
        walk.root(root.clone());
        while let Some(link) = walk.links.pop_front() {
            if walk.stopped() {
                break;
            }
            walk.link(link);
        }
    }
    if !walk.stopped() {
        walk.look_up_known_names_not_reached();
    }
    walk.finish()
}

/// A file or folder, told apart from any other whatever its name.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    pub(crate) fn of(metadata: &fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

struct Walk<'a> {
    request: &'a WalkRequest<'a>,
    filesystem: &'a dyn WalkFilesystem,
    walked_directories: HashSet<Identity>,
    files: HashMap<Identity, FoundFile>,
    files_in_order: Vec<Identity>,
    /// Links found and not yet followed: the walk follows them once every
    /// entry reached without one has been found.
    links: VecDeque<PathBuf>,
    found: FileWalk,
}

impl Walk<'_> {
    fn stopped(&mut self) -> bool {
        if self.request.cancellation.load(Ordering::SeqCst) {
            self.found.cancelled = true;
        }
        self.found.cancelled
    }

    /// A root, walked under the name it was given even if it is a link.
    fn root(&mut self, path: PathBuf) {
        match self.filesystem.symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => self.link(path),
            _ => self.entry(path, false),
        }
    }

    /// An entry reached by `path`, which goes through a link if
    /// `through_link`.
    fn entry(&mut self, path: PathBuf, through_link: bool) {
        let Ok(metadata) = self.filesystem.symlink_metadata(&path) else {
            self.found.failures.push(path);
            return;
        };
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            self.links.push_back(path);
        } else if file_type.is_dir() {
            self.directory(path, &metadata, through_link);
        } else if file_type.is_file() {
            self.file(path.clone(), path, metadata, through_link);
        }
    }

    fn link(&mut self, path: PathBuf) {
        let Ok(metadata) = self.filesystem.metadata(&path) else {
            self.found.failures.push(path);
            return;
        };
        if metadata.is_dir() {
            self.directory(path, &metadata, true);
        } else if metadata.is_file() {
            let Ok(own_path) = self.filesystem.canonicalize(&path) else {
                self.found.failures.push(path);
                return;
            };
            self.file(path, own_path, metadata, true);
        }
    }

    fn directory(&mut self, path: PathBuf, metadata: &fs::Metadata, through_link: bool) {
        if !self.walked_directories.insert(Identity::of(metadata)) {
            return;
        }
        let entries = match self.filesystem.read_directory(&path) {
            Ok(entries) => entries,
            Err(_) => {
                self.found.failures.push(path);
                return;
            }
        };
        let mut names = Vec::new();
        for entry in entries {
            if self.stopped() {
                return;
            }
            match entry {
                Ok(entry) => names.push(entry),
                // An entry the listing itself failed on has no name of
                // its own: the folder is what could not be read in full.
                Err(_) => self.found.failures.push(path.clone()),
            }
        }
        names.sort();
        for entry in names {
            if self.stopped() {
                return;
            }
            if !(self.request.skip)(&entry) {
                self.entry(entry, through_link);
            }
        }
    }

    /// A file reached by `path`, whose own path is `own_path`.
    fn file(
        &mut self,
        path: PathBuf,
        own_path: PathBuf,
        metadata: fs::Metadata,
        through_link: bool,
    ) {
        if !(self.request.accept)(&path, &own_path) {
            self.found.rejected_names += 1;
            return;
        }
        let known = self.request.known.contains(&path);
        let identity = Identity::of(&metadata);
        let name = FileName {
            path,
            through_link,
            known,
        };
        match self.files.get_mut(&identity) {
            Some(found) => found.names.push(name),
            None => {
                self.files_in_order.push(identity);
                self.files.insert(
                    identity,
                    FoundFile {
                        names: vec![name],
                        metadata,
                    },
                );
            }
        }
    }

    /// Give each file found the names the caller knows it by, under a root,
    /// that the walk did not reach it by. A known name that names nothing
    /// is one whose file is gone, which is the caller's to tell. One that
    /// cannot be looked up leaves the walk unsure which files the caller
    /// knows; it is a failure, unless the walk already failed on it.
    fn look_up_known_names_not_reached(&mut self) {
        let roots: HashSet<&Path> = self.request.roots.iter().map(PathBuf::as_path).collect();
        let reached: HashSet<&Path> = self
            .files
            .values()
            .flat_map(|file| file.names.iter().map(|name| name.path.as_path()))
            .collect();
        let mut not_reached = Vec::new();
        for name in self.request.known {
            if self.request.cancellation.load(Ordering::SeqCst) {
                break;
            }
            if !reached.contains(name.as_path())
                && name.ancestors().any(|ancestor| roots.contains(ancestor))
            {
                not_reached.push(name.clone());
            }
        }
        if self.stopped() {
            return;
        }
        not_reached.sort();
        let failed: HashSet<PathBuf> = self.found.failures.iter().cloned().collect();
        for name in not_reached {
            if self.stopped() {
                return;
            }
            let found = self.filesystem.canonicalize(&name).and_then(|own_path| {
                self.filesystem
                    .metadata(&own_path)
                    .map(|metadata| (own_path, metadata))
            });
            match found {
                Ok((own_path, metadata)) => {
                    if !(self.request.accept)(&name, &own_path) {
                        continue;
                    }
                    if let Some(file) = self.files.get_mut(&Identity::of(&metadata)) {
                        file.names.push(FileName {
                            path: name,
                            through_link: true,
                            known: true,
                        });
                    }
                }
                Err(error) if crate::names_nothing(&error) => {}
                Err(_) => {
                    self.found.known_name_unresolved = true;
                    if !failed.contains(&name) {
                        self.found.failures.push(name);
                    }
                }
            }
        }
    }

    fn finish(mut self) -> FileWalk {
        for identity in self.files_in_order {
            if let Some(found) = self.files.remove(&identity) {
                self.found.files.push(found);
            }
        }
        self.found
    }
}
