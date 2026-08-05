//! Performs bounded, identity-checked recovery artifact publication and cleanup.

use super::model::RECOVERY_MARKER;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Result, fs::RecoveryAuthority};
use std::fs::{File, OpenOptions};
use std::io::{Read as _, Write as _};

pub(super) const MAX_RECOVERY_ANCHOR_BYTES: u64 = 16 * 1024;
pub(super) const MAX_RECOVERY_RECORD_BYTES: u64 = 64 * 1024 * 1024;

pub(super) struct TrustedArtifact {
    pub(super) path: Utf8PathBuf,
    pub(super) contents: Vec<u8>,
    pub(super) identity: same_file::Handle,
    pub(super) owner_private: bool,
}

#[derive(Debug)]
pub(super) enum PublicationError {
    NotPublished(CoreError),
    DurabilityUncertain(CoreError),
}

#[derive(Debug)]
pub(super) enum PublicationOutcome {
    Published,
    CleanupPending { path: Utf8PathBuf, error: CoreError },
}

#[derive(Debug)]
pub(super) enum RemovalError {
    NotRemoved(CoreError),
    DurabilityUncertain(CoreError),
}

/// The visible commit state after consuming a recovery marker.
#[derive(Debug)]
pub(super) enum CommitOutcome {
    Committed,
    DurabilityUncertain(CoreError),
}

impl PublicationError {
    pub(super) fn into_core_error(self, path: &Utf8Path) -> CoreError {
        match self {
            PublicationError::NotPublished(error) => error,
            PublicationError::DurabilityUncertain(error) => CoreError::LockConflict(format!(
                "published recovery state at {path}, but syncing its directory failed; recovery evidence was left intact: {error}"
            )),
        }
    }
}

impl RemovalError {
    pub(super) fn into_core_error(self, path: &Utf8Path) -> CoreError {
        match self {
            RemovalError::NotRemoved(error) => error,
            RemovalError::DurabilityUncertain(error) => CoreError::LockConflict(format!(
                "removed recovery state at {path}, but syncing its directory failed; remaining recovery evidence was left intact: {error}"
            )),
        }
    }
}

impl TrustedArtifact {
    pub(super) fn open(path: &Utf8Path, limit: u64, owner_private: bool) -> Result<Self> {
        let (mut file, identity) = open_recovery_artifact(path)?.ok_or_else(|| {
            CoreError::LockUnreadable(format!(
                "Cargo.lock recovery artifact disappeared at {path}; left all files untouched"
            ))
        })?;
        if owner_private {
            cooldown_core::fs::validate_owner_private_file(&file, path.as_std_path()).map_err(
                |error| {
                    CoreError::LockUnreadable(format!(
                        "untrusted Cargo.lock recovery authority at {path}: {error}; left all files untouched"
                    ))
                },
            )?;
        }
        let contents = read_bounded(&mut file, path, limit)?;
        if identity != same_file::Handle::from_path(path)? {
            return Err(untrusted_record(path));
        }
        Ok(TrustedArtifact {
            path: path.to_owned(),
            contents,
            identity,
            owner_private,
        })
    }

    pub(super) fn decode<T: serde::de::DeserializeOwned>(&self, label: &str) -> Result<T> {
        serde_json::from_slice(&self.contents).map_err(|error| {
            CoreError::LockUnreadable(format!(
                "invalid Cargo.lock {label} at {}: {error}; left all files untouched",
                self.path
            ))
        })
    }

    pub(super) fn validate_current(&self) -> Result<()> {
        let limit = u64::try_from(self.contents.len()).map_err(|error| {
            CoreError::System(format!(
                "recovery evidence size cannot be represented: {error}"
            ))
        })?;
        let current = Self::open(&self.path, limit, self.owner_private)?;
        if self.identity != current.identity || self.contents != current.contents {
            return Err(CoreError::LockUnreadable(format!(
                "Cargo.lock recovery evidence changed at {}; left it untouched",
                self.path
            )));
        }
        Ok(())
    }
}

pub(super) fn path_exists(path: &Utf8Path) -> Result<bool> {
    Ok(open_recovery_artifact(path)?.is_some())
}

pub(super) fn recovery_path(lock_path: &Utf8Path) -> Utf8PathBuf {
    lock_path.with_file_name(RECOVERY_MARKER)
}

pub(super) fn recovery_anchor_path(authority: &RecoveryAuthority) -> Result<Utf8PathBuf> {
    let name = super::recovery_anchor_name(authority.project());
    Utf8PathBuf::from_path_buf(authority.directory().join(name)).map_err(|path| {
        CoreError::PathEncoding(format!(
            "non-UTF-8 Cargo recovery authority path: {}",
            path.display()
        ))
    })
}

pub(super) fn open_recovery_artifact(path: &Utf8Path) -> Result<Option<(File, same_file::Handle)>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(untrusted_record(path));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| {
        CoreError::LockUnreadable(format!(
            "cannot safely open Cargo.lock recovery artifact at {path}: {error}; left all files untouched"
        ))
    })?;
    let metadata = file.metadata()?;
    let path_metadata = std::fs::symlink_metadata(path)?;
    let path_identity = same_file::Handle::from_path(path)?;
    let final_metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || !path_metadata.file_type().is_file()
        || !final_metadata.file_type().is_file()
        || same_file::Handle::from_file(file.try_clone()?)? != path_identity
    {
        return Err(untrusted_record(path));
    }
    Ok(Some((file, path_identity)))
}

pub(super) fn publish_exclusive_json<T: serde::Serialize>(
    path: &Utf8Path,
    value: &T,
) -> Result<PublicationOutcome, PublicationError> {
    let contents = serde_json::to_vec(value).map_err(|error| {
        PublicationError::NotPublished(CoreError::Serialization(error.to_string()))
    })?;
    publish_exclusive_bytes(path, &contents)
}

pub(super) fn publish_exclusive_bytes(
    path: &Utf8Path,
    contents: &[u8],
) -> Result<PublicationOutcome, PublicationError> {
    #[cfg(unix)]
    let sync_parent = sync_parent_directory;
    #[cfg(not(unix))]
    let sync_parent = |_path: &Utf8Path| Ok(());
    publish_exclusive_bytes_with(path, contents, sync_parent, remove_file)
}

#[cfg(test)]
pub(super) fn publish_exclusive_json_with<T, S, C>(
    path: &Utf8Path,
    value: &T,
    sync_parent: S,
    cleanup_private: C,
) -> Result<PublicationOutcome, PublicationError>
where
    T: serde::Serialize,
    S: FnOnce(&Utf8Path) -> Result<()>,
    C: FnOnce(&Utf8Path) -> Result<()>,
{
    let contents = serde_json::to_vec(value).map_err(|error| {
        PublicationError::NotPublished(CoreError::Serialization(error.to_string()))
    })?;
    publish_exclusive_bytes_with(path, &contents, sync_parent, cleanup_private)
}

pub(super) fn publish_exclusive_bytes_with<S, C>(
    path: &Utf8Path,
    contents: &[u8],
    sync_parent: S,
    cleanup_private: C,
) -> Result<PublicationOutcome, PublicationError>
where
    S: FnOnce(&Utf8Path) -> Result<()>,
    C: FnOnce(&Utf8Path) -> Result<()>,
{
    let temp =
        create_synced_private_file(path, contents).map_err(PublicationError::NotPublished)?;
    if let Err(error) = std::fs::hard_link(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(PublicationError::NotPublished(
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CoreError::LockConflict(format!(
                    "pending Cargo.lock transaction state already exists at {path}"
                ))
            } else {
                error.into()
            },
        ));
    }
    sync_parent(path).map_err(PublicationError::DurabilityUncertain)?;
    if let Err(error) = cleanup_private(&temp) {
        tracing::warn!(
            path = %temp,
            error = %error,
            "could not remove private Cargo.lock recovery publication file"
        );
        return Ok(PublicationOutcome::CleanupPending { path: temp, error });
    }
    Ok(PublicationOutcome::Published)
}

pub(super) fn create_synced_private_file(path: &Utf8Path, contents: &[u8]) -> Result<Utf8PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let name = path
        .file_name()
        .ok_or_else(|| CoreError::PathEncoding(format!("path has no file name: {path}")))?;
    for attempt in 0..100_u8 {
        let temp = parent.join(format!(
            ".{name}.{}.{}.publish",
            std::process::id(),
            attempt
        ));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = match options.open(&temp) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        if let Err(error) = file.write_all(contents).and_then(|()| file.sync_all()) {
            let _ = std::fs::remove_file(&temp);
            return Err(error.into());
        }
        return Ok(temp);
    }
    Err(CoreError::Filesystem(format!(
        "could not create a private recovery file for {path}"
    )))
}

pub(super) fn ensure_no_orphan_artifacts(lock_path: &Utf8Path) -> Result<()> {
    if let Some(path) = project_recovery_artifact(lock_path)? {
        return Err(CoreError::LockUnreadable(format!(
            "unreferenced Cargo.lock recovery artifact at {path}; left it untouched; inspect and remove it explicitly"
        )));
    }
    Ok(())
}

pub(crate) fn has_project_recovery_artifacts(lock_path: &Utf8Path) -> Result<bool> {
    Ok(project_recovery_artifact(lock_path)?.is_some())
}

fn project_recovery_artifact(lock_path: &Utf8Path) -> Result<Option<Utf8PathBuf>> {
    let parent = lock_path.parent().unwrap_or_else(|| Utf8Path::new(""));
    let entries = match std::fs::read_dir(parent) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_recovery_artifact_name(&name) {
            return Ok(Some(parent.join(name)));
        }
    }
    Ok(None)
}

pub(crate) fn is_recovery_artifact_name(name: &str) -> bool {
    name == RECOVERY_MARKER
        || publication_target(name).is_some_and(|target| target == RECOVERY_MARKER)
}

pub(super) fn publication_target(name: &str) -> Option<&str> {
    let body = name.strip_prefix('.')?.strip_suffix(".publish")?;
    let (body, attempt) = body.rsplit_once('.')?;
    let (target, process) = body.rsplit_once('.')?;
    (!attempt.is_empty()
        && !process.is_empty()
        && attempt.bytes().all(|byte| byte.is_ascii_digit())
        && process.bytes().all(|byte| byte.is_ascii_digit()))
    .then_some(target)
}

pub(super) fn cleanup_linked_publication_files(public_path: &Utf8Path) -> Result<()> {
    let Some(parent) = public_path.parent() else {
        return Err(CoreError::PathEncoding(format!(
            "recovery path has no parent: {public_path}"
        )));
    };
    let Some(public_name) = public_path.file_name() else {
        return Err(CoreError::PathEncoding(format!(
            "recovery path has no file name: {public_path}"
        )));
    };
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if publication_target(name) != Some(public_name) {
            continue;
        }
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            CoreError::PathEncoding(format!(
                "non-UTF-8 recovery artifact path: {}",
                path.display()
            ))
        })?;
        if same_file::is_same_file(public_path, &path)? {
            remove_file(&path)?;
        }
    }
    Ok(())
}

pub(super) fn private_publication_paths(public_path: &Utf8Path) -> Result<Vec<Utf8PathBuf>> {
    let parent = public_path.parent().ok_or_else(|| {
        CoreError::PathEncoding(format!("recovery path has no parent: {public_path}"))
    })?;
    let public_name = public_path.file_name().ok_or_else(|| {
        CoreError::PathEncoding(format!("recovery path has no file name: {public_path}"))
    })?;
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(parent)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if publication_target(&name) != Some(public_name) {
            continue;
        }
        paths.push(Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            CoreError::PathEncoding(format!(
                "non-UTF-8 recovery publication path: {}",
                path.display()
            ))
        })?);
    }
    paths.sort();
    Ok(paths)
}

pub(super) fn remove_recovery_anchor(path: &Utf8Path) -> Result<CommitOutcome> {
    cleanup_linked_publication_files(path)?;
    remove_transaction_marker(path)
}

pub(super) fn read_bounded(file: &mut File, path: &Utf8Path, limit: u64) -> Result<Vec<u8>> {
    if file.metadata()?.len() > limit {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock recovery artifact at {path} exceeds the {limit}-byte safety limit; left it untouched"
        )));
    }
    let capacity = usize::try_from(file.metadata()?.len()).map_err(|error| {
        CoreError::LockUnreadable(format!(
            "Cargo.lock recovery artifact size at {path} cannot be represented: {error}; left it untouched"
        ))
    })?;
    let mut contents = Vec::with_capacity(capacity);
    file.take(limit.saturating_add(1))
        .read_to_end(&mut contents)?;
    if u64::try_from(contents.len()).map_or(true, |size| size > limit) {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock recovery artifact at {path} exceeds the {limit}-byte safety limit; left it untouched"
        )));
    }
    Ok(contents)
}

pub(super) fn validate_record_size(size: usize, path: &Utf8Path) -> Result<()> {
    let size = u64::try_from(size).map_err(|error| {
        CoreError::LockUnreadable(format!(
            "Cargo.lock recovery record size for {path} cannot be represented: {error}; left the project untouched"
        ))
    })?;
    if size > MAX_RECOVERY_RECORD_BYTES {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock recovery record for {path} would exceed the {MAX_RECOVERY_RECORD_BYTES}-byte safety limit; left the project untouched"
        )));
    }
    Ok(())
}

pub(super) fn remove_file(path: &Utf8Path) -> Result<()> {
    #[cfg(unix)]
    let sync_parent = sync_parent_directory;
    #[cfg(not(unix))]
    let sync_parent = |_path: &Utf8Path| Ok(());
    remove_file_with(path, sync_parent).map_err(|error| error.into_core_error(path))
}

pub(super) fn remove_transaction_marker(path: &Utf8Path) -> Result<CommitOutcome> {
    #[cfg(unix)]
    let sync_parent = sync_parent_directory;
    #[cfg(not(unix))]
    let sync_parent = |_path: &Utf8Path| Ok(());
    remove_transaction_marker_with(path, sync_parent)
}

pub(super) fn remove_transaction_marker_with<S>(
    path: &Utf8Path,
    sync_parent: S,
) -> Result<CommitOutcome>
where
    S: FnOnce(&Utf8Path) -> Result<()>,
{
    match remove_file_with(path, sync_parent) {
        Ok(()) => Ok(CommitOutcome::Committed),
        Err(RemovalError::NotRemoved(error)) => Err(error),
        Err(error @ RemovalError::DurabilityUncertain(_)) => Ok(
            CommitOutcome::DurabilityUncertain(error.into_core_error(path)),
        ),
    }
}

pub(super) fn remove_file_with<S>(path: &Utf8Path, sync_parent: S) -> Result<(), RemovalError>
where
    S: FnOnce(&Utf8Path) -> Result<()>,
{
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent(path).map_err(RemovalError::DurabilityUncertain),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(RemovalError::NotRemoved(error.into())),
    }
}

#[cfg(unix)]
pub(super) fn sync_parent_directory(path: &Utf8Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Utf8Path::new(""));
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) fn untrusted_record(path: &Utf8Path) -> CoreError {
    CoreError::LockUnreadable(format!(
        "untrusted Cargo.lock recovery record at {path}; left all files untouched"
    ))
}

pub(super) fn pending_recovery(path: &Utf8Path, error: &CoreError) -> CoreError {
    CoreError::PendingRecovery(format!(
        "{error}; recovery evidence at {path} remains authoritative; run `cooldown recover`"
    ))
}
