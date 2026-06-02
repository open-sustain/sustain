// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Behavioral capability probe for organized-library roots.
//!
//! Reference mode intentionally does not use this probe: indexing and
//! playback only require readable files, while tag mirroring fails closed per
//! target file. Organized mode has a stronger contract because it publishes
//! and removes pathnames below the selected root. Filesystem-name allowlists
//! cannot prove that contract, so the validator exercises the actual durable
//! primitives in a private scratch directory.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use super::file_ops::{link_file_capability, open_regular_file};
use super::journal::publish_journal_without_overwrite;

const PROBE_DIRECTORY_PREFIX: &str = ".sustain-managed-root-probe";
const PROBE_BYTES: &[u8] = b"sustain managed-library filesystem probe\n";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedLibraryFilesystemError {
    RootUnavailable,
    RootIsNotDirectory,
    CreateProbeDirectoryFailed,
    CreateProbeFileFailed,
    WriteProbeFileFailed,
    SyncProbeFileFailed,
    HardLinkFailed,
    HardLinkIdentityMismatch,
    HardLinkOverwriteProtectionFailed,
    RemoveProbeLinkFailed,
    ReadSurvivingLinkFailed,
    RenamePublicationFailed,
    RenameOverwriteProtectionFailed,
    SyncProbeDirectoryFailed,
    CleanupFailed,
}

impl ManagedLibraryFilesystemError {
    pub fn user_message(self) -> &'static str {
        match self {
            Self::RootUnavailable => {
                "Organized library mode needs an accessible, writable library folder."
            }
            Self::RootIsNotDirectory => "The organized library path must be a folder.",
            Self::CreateProbeDirectoryFailed | Self::CreateProbeFileFailed => {
                "Organized library mode needs permission to create files and folders in the selected library folder."
            }
            Self::WriteProbeFileFailed | Self::SyncProbeFileFailed => {
                "The selected library folder cannot durably save managed files."
            }
            Self::HardLinkFailed | Self::HardLinkIdentityMismatch => {
                "The selected library folder does not support the hard-link moves required by organized library mode."
            }
            Self::HardLinkOverwriteProtectionFailed => {
                "The selected library folder cannot safely refuse overwriting an existing managed file."
            }
            Self::RemoveProbeLinkFailed => {
                "The selected library folder cannot safely remove managed pathnames."
            }
            Self::ReadSurvivingLinkFailed => {
                "The selected library folder did not preserve a managed file after a hard-link move."
            }
            Self::RenamePublicationFailed => {
                "The selected library folder cannot publish Sustain's recovery journal atomically."
            }
            Self::RenameOverwriteProtectionFailed => {
                "The selected library folder cannot safely protect Sustain's recovery journal from overwrite."
            }
            Self::SyncProbeDirectoryFailed => {
                "The selected library folder cannot durably save managed directory changes."
            }
            Self::CleanupFailed => {
                "The selected library folder could not remove Sustain's filesystem probe files."
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct ManagedLibraryFilesystemValidator {
    probe: Arc<dyn ManagedLibraryFilesystemProbe>,
}

impl ManagedLibraryFilesystemValidator {
    pub(crate) fn validate(
        &self,
        library_root: &Path,
    ) -> Result<(), ManagedLibraryFilesystemError> {
        self.probe.validate(library_root)
    }

    #[cfg(test)]
    pub(crate) fn rejecting(error: ManagedLibraryFilesystemError) -> Self {
        Self {
            probe: Arc::new(RejectingManagedLibraryFilesystemProbe { error }),
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_after(
        successful_validations: u64,
        error: ManagedLibraryFilesystemError,
    ) -> Self {
        Self {
            probe: Arc::new(SequencedManagedLibraryFilesystemProbe {
                successful_validations,
                validations: AtomicU64::new(0),
                error,
            }),
        }
    }
}

impl Default for ManagedLibraryFilesystemValidator {
    fn default() -> Self {
        Self {
            probe: Arc::new(SystemManagedLibraryFilesystemProbe),
        }
    }
}

trait ManagedLibraryFilesystemProbe: Send + Sync {
    fn validate(&self, library_root: &Path) -> Result<(), ManagedLibraryFilesystemError>;
}

struct SystemManagedLibraryFilesystemProbe;

impl ManagedLibraryFilesystemProbe for SystemManagedLibraryFilesystemProbe {
    fn validate(&self, library_root: &Path) -> Result<(), ManagedLibraryFilesystemError> {
        validate_managed_library_root(&SystemProbeFilesystem, library_root)
    }
}

#[cfg(test)]
struct RejectingManagedLibraryFilesystemProbe {
    error: ManagedLibraryFilesystemError,
}

#[cfg(test)]
impl ManagedLibraryFilesystemProbe for RejectingManagedLibraryFilesystemProbe {
    fn validate(&self, _library_root: &Path) -> Result<(), ManagedLibraryFilesystemError> {
        Err(self.error)
    }
}

#[cfg(test)]
struct SequencedManagedLibraryFilesystemProbe {
    successful_validations: u64,
    validations: AtomicU64,
    error: ManagedLibraryFilesystemError,
}

#[cfg(test)]
impl ManagedLibraryFilesystemProbe for SequencedManagedLibraryFilesystemProbe {
    fn validate(&self, _library_root: &Path) -> Result<(), ManagedLibraryFilesystemError> {
        let attempt = self.validations.fetch_add(1, Ordering::Relaxed);
        if attempt < self.successful_validations {
            Ok(())
        } else {
            Err(self.error)
        }
    }
}

trait ProbeFilesystem {
    fn create_private_directory(&self, path: &Path) -> io::Result<()>;
    fn create_new_file(&self, path: &Path) -> io::Result<File>;
    fn sync_file(&self, file: &File) -> io::Result<()>;
    fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn verify_hard_link_refuses_overwrite(
        &self,
        source: &Path,
        destination: &Path,
    ) -> io::Result<()>;
    fn remove_file(&self, path: &Path) -> io::Result<()>;
    fn read(&self, path: &Path) -> io::Result<Vec<u8>>;
    fn publish_without_overwrite(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn verify_rename_refuses_overwrite(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn sync_directory(&self, path: &Path) -> io::Result<()>;
    fn remove_directory(&self, path: &Path) -> io::Result<()>;
}

struct SystemProbeFilesystem;

impl ProbeFilesystem for SystemProbeFilesystem {
    fn create_private_directory(&self, path: &Path) -> io::Result<()> {
        fs::DirBuilder::new().mode(0o700).create(path)
    }

    fn create_new_file(&self, path: &Path) -> io::Result<File> {
        OpenOptions::new().write(true).create_new(true).open(path)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        file.sync_all()
    }

    fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
        let source =
            open_regular_file(source).map_err(|error| io::Error::other(format!("{error:?}")))?;
        link_file_capability(&source, destination)
    }

    fn verify_hard_link_refuses_overwrite(
        &self,
        source: &Path,
        destination: &Path,
    ) -> io::Result<()> {
        let source =
            open_regular_file(source).map_err(|error| io::Error::other(format!("{error:?}")))?;
        expect_already_exists(link_file_capability(&source, destination))
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)
    }

    fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
        fs::read(path)
    }

    fn publish_without_overwrite(&self, source: &Path, destination: &Path) -> io::Result<()> {
        publish_journal_without_overwrite(source, destination)
    }

    fn verify_rename_refuses_overwrite(&self, source: &Path, destination: &Path) -> io::Result<()> {
        expect_already_exists(publish_journal_without_overwrite(source, destination))
    }

    fn sync_directory(&self, path: &Path) -> io::Result<()> {
        File::open(path).and_then(|directory| directory.sync_all())
    }

    fn remove_directory(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir(path)
    }
}

fn validate_managed_library_root(
    filesystem: &dyn ProbeFilesystem,
    library_root: &Path,
) -> Result<(), ManagedLibraryFilesystemError> {
    match fs::symlink_metadata(library_root) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err(ManagedLibraryFilesystemError::RootIsNotDirectory),
        Err(_) => return Err(ManagedLibraryFilesystemError::RootUnavailable),
    }

    let paths = ProbePaths::new(library_root)?;
    let result = run_probe(filesystem, library_root, &paths);
    let cleanup_result = cleanup_probe(filesystem, library_root, &paths);
    match (result, cleanup_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(())) => Err(ManagedLibraryFilesystemError::CleanupFailed),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn run_probe(
    filesystem: &dyn ProbeFilesystem,
    library_root: &Path,
    paths: &ProbePaths,
) -> Result<(), ManagedLibraryFilesystemError> {
    filesystem
        .create_private_directory(&paths.private_root)
        .map_err(|_| ManagedLibraryFilesystemError::CreateProbeDirectoryFailed)?;
    filesystem
        .sync_directory(library_root)
        .map_err(|_| ManagedLibraryFilesystemError::SyncProbeDirectoryFailed)?;
    filesystem
        .create_private_directory(&paths.nested)
        .map_err(|_| ManagedLibraryFilesystemError::CreateProbeDirectoryFailed)?;
    filesystem
        .sync_directory(&paths.private_root)
        .map_err(|_| ManagedLibraryFilesystemError::SyncProbeDirectoryFailed)?;

    create_synced_file(filesystem, &paths.source, PROBE_BYTES)?;
    filesystem
        .sync_directory(&paths.nested)
        .map_err(|_| ManagedLibraryFilesystemError::SyncProbeDirectoryFailed)?;
    filesystem
        .hard_link(&paths.source, &paths.link)
        .map_err(|_| ManagedLibraryFilesystemError::HardLinkFailed)?;
    let source_identity = file_identity(&paths.source);
    if source_identity.is_none() || source_identity != file_identity(&paths.link) {
        return Err(ManagedLibraryFilesystemError::HardLinkIdentityMismatch);
    }
    filesystem
        .sync_directory(&paths.nested)
        .map_err(|_| ManagedLibraryFilesystemError::SyncProbeDirectoryFailed)?;

    create_synced_file(filesystem, &paths.existing_link, b"existing pathname\n")?;
    filesystem
        .verify_hard_link_refuses_overwrite(&paths.source, &paths.existing_link)
        .map_err(|_| ManagedLibraryFilesystemError::HardLinkOverwriteProtectionFailed)?;

    filesystem
        .remove_file(&paths.source)
        .map_err(|_| ManagedLibraryFilesystemError::RemoveProbeLinkFailed)?;
    filesystem
        .sync_directory(&paths.nested)
        .map_err(|_| ManagedLibraryFilesystemError::SyncProbeDirectoryFailed)?;
    if filesystem
        .read(&paths.link)
        .map_err(|_| ManagedLibraryFilesystemError::ReadSurvivingLinkFailed)?
        != PROBE_BYTES
    {
        return Err(ManagedLibraryFilesystemError::ReadSurvivingLinkFailed);
    }

    create_synced_file(
        filesystem,
        &paths.journal_temporary,
        b"journal publication\n",
    )?;
    filesystem
        .publish_without_overwrite(&paths.journal_temporary, &paths.journal)
        .map_err(|_| ManagedLibraryFilesystemError::RenamePublicationFailed)?;
    filesystem
        .sync_directory(library_root)
        .map_err(|_| ManagedLibraryFilesystemError::SyncProbeDirectoryFailed)?;
    create_synced_file(
        filesystem,
        &paths.journal_collision_temporary,
        b"journal collision\n",
    )?;
    filesystem
        .verify_rename_refuses_overwrite(&paths.journal_collision_temporary, &paths.journal)
        .map_err(|_| ManagedLibraryFilesystemError::RenameOverwriteProtectionFailed)?;

    Ok(())
}

fn create_synced_file(
    filesystem: &dyn ProbeFilesystem,
    path: &Path,
    bytes: &[u8],
) -> Result<(), ManagedLibraryFilesystemError> {
    let mut file = filesystem
        .create_new_file(path)
        .map_err(|_| ManagedLibraryFilesystemError::CreateProbeFileFailed)?;
    file.write_all(bytes)
        .map_err(|_| ManagedLibraryFilesystemError::WriteProbeFileFailed)?;
    file.flush()
        .map_err(|_| ManagedLibraryFilesystemError::WriteProbeFileFailed)?;
    filesystem
        .sync_file(&file)
        .map_err(|_| ManagedLibraryFilesystemError::SyncProbeFileFailed)
}

fn cleanup_probe(
    filesystem: &dyn ProbeFilesystem,
    library_root: &Path,
    paths: &ProbePaths,
) -> Result<(), ()> {
    let mut failed = false;
    for path in [
        &paths.journal_collision_temporary,
        &paths.journal_temporary,
        &paths.journal,
    ] {
        if let Err(error) = filesystem.remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            failed = true;
        }
    }
    if filesystem.sync_directory(library_root).is_err() {
        failed = true;
    }
    for path in [&paths.source, &paths.link, &paths.existing_link] {
        if let Err(error) = filesystem.remove_file(path)
            && error.kind() != io::ErrorKind::NotFound
        {
            failed = true;
        }
    }
    if filesystem.sync_directory(&paths.nested).is_err() {
        failed = true;
    }
    if let Err(error) = filesystem.remove_directory(&paths.nested)
        && error.kind() != io::ErrorKind::NotFound
    {
        failed = true;
    }
    if filesystem.sync_directory(&paths.private_root).is_err() {
        failed = true;
    }
    if let Err(error) = filesystem.remove_directory(&paths.private_root)
        && error.kind() != io::ErrorKind::NotFound
    {
        failed = true;
    }
    if filesystem.sync_directory(library_root).is_err() {
        failed = true;
    }
    (!failed).then_some(()).ok_or(())
}

fn expect_already_exists(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
        Ok(()) => Err(io::Error::other(
            "filesystem overwrote a pathname during a no-overwrite probe",
        )),
    }
}

fn file_identity(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.file_type().is_file())
        .map(|metadata| (metadata.dev(), metadata.ino()))
}

struct ProbePaths {
    private_root: PathBuf,
    nested: PathBuf,
    source: PathBuf,
    link: PathBuf,
    existing_link: PathBuf,
    journal_temporary: PathBuf,
    journal_collision_temporary: PathBuf,
    journal: PathBuf,
}

impl ProbePaths {
    fn new(library_root: &Path) -> Result<Self, ManagedLibraryFilesystemError> {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let unique = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let private_root = library_root.join(format!(
            "{PROBE_DIRECTORY_PREFIX}-{}-{timestamp}-{unique}",
            std::process::id()
        ));
        if private_root.exists() {
            return Err(ManagedLibraryFilesystemError::CreateProbeDirectoryFailed);
        }
        let nested = private_root.join("nested");
        Ok(Self {
            source: nested.join("source"),
            link: nested.join("link"),
            existing_link: nested.join("existing"),
            journal_temporary: library_root.join(format!(
                "{PROBE_DIRECTORY_PREFIX}-journal-{}-{timestamp}-{unique}.tmp",
                std::process::id()
            )),
            journal_collision_temporary: library_root.join(format!(
                "{PROBE_DIRECTORY_PREFIX}-journal-collision-{}-{timestamp}-{unique}.tmp",
                std::process::id()
            )),
            journal: library_root.join(format!(
                "{PROBE_DIRECTORY_PREFIX}-journal-{}-{timestamp}-{unique}",
                std::process::id()
            )),
            private_root,
            nested,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::Mutex,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
    enum Operation {
        CreateDirectory,
        CreateFile,
        SyncFile,
        HardLink,
        HardLinkNoOverwrite,
        RemoveFile,
        Rename,
        RenameNoOverwrite,
        SyncDirectory,
    }

    struct FailingProbeFilesystem {
        fail_once: Mutex<BTreeSet<Operation>>,
    }

    impl FailingProbeFilesystem {
        fn new(operation: Operation) -> Self {
            Self {
                fail_once: Mutex::new(BTreeSet::from([operation])),
            }
        }

        fn maybe_fail(&self, operation: Operation) -> io::Result<()> {
            if self
                .fail_once
                .lock()
                .expect("failure lock")
                .remove(&operation)
            {
                Err(io::Error::other("injected probe failure"))
            } else {
                Ok(())
            }
        }
    }

    impl ProbeFilesystem for FailingProbeFilesystem {
        fn create_private_directory(&self, path: &Path) -> io::Result<()> {
            self.maybe_fail(Operation::CreateDirectory)?;
            SystemProbeFilesystem.create_private_directory(path)
        }

        fn create_new_file(&self, path: &Path) -> io::Result<File> {
            self.maybe_fail(Operation::CreateFile)?;
            SystemProbeFilesystem.create_new_file(path)
        }

        fn sync_file(&self, file: &File) -> io::Result<()> {
            self.maybe_fail(Operation::SyncFile)?;
            SystemProbeFilesystem.sync_file(file)
        }

        fn hard_link(&self, source: &Path, destination: &Path) -> io::Result<()> {
            self.maybe_fail(Operation::HardLink)?;
            SystemProbeFilesystem.hard_link(source, destination)
        }

        fn verify_hard_link_refuses_overwrite(
            &self,
            source: &Path,
            destination: &Path,
        ) -> io::Result<()> {
            self.maybe_fail(Operation::HardLinkNoOverwrite)?;
            SystemProbeFilesystem.verify_hard_link_refuses_overwrite(source, destination)
        }

        fn remove_file(&self, path: &Path) -> io::Result<()> {
            self.maybe_fail(Operation::RemoveFile)?;
            SystemProbeFilesystem.remove_file(path)
        }

        fn read(&self, path: &Path) -> io::Result<Vec<u8>> {
            SystemProbeFilesystem.read(path)
        }

        fn publish_without_overwrite(&self, source: &Path, destination: &Path) -> io::Result<()> {
            self.maybe_fail(Operation::Rename)?;
            SystemProbeFilesystem.publish_without_overwrite(source, destination)
        }

        fn verify_rename_refuses_overwrite(
            &self,
            source: &Path,
            destination: &Path,
        ) -> io::Result<()> {
            self.maybe_fail(Operation::RenameNoOverwrite)?;
            SystemProbeFilesystem.verify_rename_refuses_overwrite(source, destination)
        }

        fn sync_directory(&self, path: &Path) -> io::Result<()> {
            self.maybe_fail(Operation::SyncDirectory)?;
            SystemProbeFilesystem.sync_directory(path)
        }

        fn remove_directory(&self, path: &Path) -> io::Result<()> {
            SystemProbeFilesystem.remove_directory(path)
        }
    }

    #[test]
    fn validator_accepts_test_filesystem_and_removes_probe_artifacts() {
        let root = unique_test_directory();
        fs::create_dir_all(&root).expect("create root");

        assert_eq!(
            ManagedLibraryFilesystemValidator::default().validate(&root),
            Ok(())
        );
        assert_no_probe_artifacts(&root);
        fs::remove_dir_all(root).expect("remove root");
    }

    #[test]
    fn validator_rejects_a_symlinked_library_root() {
        let target = unique_test_directory();
        let root = unique_test_directory();
        fs::create_dir_all(&target).expect("create target");
        std::os::unix::fs::symlink(&target, &root).expect("create root symlink");

        assert_eq!(
            ManagedLibraryFilesystemValidator::default().validate(&root),
            Err(ManagedLibraryFilesystemError::RootIsNotDirectory)
        );

        fs::remove_file(root).expect("remove symlink");
        fs::remove_dir_all(target).expect("remove target");
    }

    #[test]
    fn validator_reports_each_required_primitive_failure() {
        for (operation, expected) in [
            (
                Operation::CreateDirectory,
                ManagedLibraryFilesystemError::CreateProbeDirectoryFailed,
            ),
            (
                Operation::CreateFile,
                ManagedLibraryFilesystemError::CreateProbeFileFailed,
            ),
            (
                Operation::SyncFile,
                ManagedLibraryFilesystemError::SyncProbeFileFailed,
            ),
            (
                Operation::HardLink,
                ManagedLibraryFilesystemError::HardLinkFailed,
            ),
            (
                Operation::HardLinkNoOverwrite,
                ManagedLibraryFilesystemError::HardLinkOverwriteProtectionFailed,
            ),
            (
                Operation::RemoveFile,
                ManagedLibraryFilesystemError::RemoveProbeLinkFailed,
            ),
            (
                Operation::Rename,
                ManagedLibraryFilesystemError::RenamePublicationFailed,
            ),
            (
                Operation::RenameNoOverwrite,
                ManagedLibraryFilesystemError::RenameOverwriteProtectionFailed,
            ),
            (
                Operation::SyncDirectory,
                ManagedLibraryFilesystemError::SyncProbeDirectoryFailed,
            ),
        ] {
            let root = unique_test_directory();
            fs::create_dir_all(&root).expect("create root");
            assert_eq!(
                validate_managed_library_root(&FailingProbeFilesystem::new(operation), &root),
                Err(expected),
                "{operation:?}"
            );
            fs::remove_dir_all(root).expect("remove root");
        }
    }

    fn assert_no_probe_artifacts(root: &Path) {
        for entry in fs::read_dir(root).expect("read root") {
            let name = entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned();
            assert!(!name.starts_with(PROBE_DIRECTORY_PREFIX), "{name}");
        }
    }

    fn unique_test_directory() -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "sustain_managed_root_capability_test_{}_{}_{}",
            std::process::id(),
            timestamp,
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }
}
