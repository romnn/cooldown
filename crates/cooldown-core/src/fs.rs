//! Filesystem helpers shared across the workspace: atomic file replacement, Unix parent-directory
//! syncing, and stable file-name hashing.
//! They keep trust-bearing state transitions and per-path lock/cache names consistent across
//! crates.

use crate::error::CoreError;
use camino::{Utf8Path, Utf8PathBuf};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A private target-derived namespace shared by every process coordinating one project.
#[derive(Debug, Clone)]
pub struct ProjectCoordination {
    directory: PathBuf,
    directory_identity: std::sync::Arc<same_file::Handle>,
    project: Utf8PathBuf,
    recovery_authority: Option<RecoveryAuthority>,
}

/// Trusted out-of-project authority for one project's recoverable mutations.
#[derive(Debug, Clone)]
pub struct RecoveryAuthority {
    directory: PathBuf,
    directory_identity: std::sync::Arc<same_file::Handle>,
    project: Utf8PathBuf,
}

impl ProjectCoordination {
    /// Resolves and securely creates the coordination namespace for `target`.
    ///
    /// Git projects rendezvous beneath the Git common directory.
    /// Non-Git projects use an owner-private `.cooldown/locks` directory beneath the canonical
    /// project root.
    ///
    /// # Errors
    ///
    /// Returns a filesystem or path-encoding error when the target cannot be canonicalized or the
    /// coordination namespace is not an owner-private regular directory tree.
    pub fn resolve(target: &Utf8Path) -> Result<Self, CoreError> {
        let project = canonical_project_root(target)?;
        let marker = git_marker(project.as_std_path())?;
        let (base, components, trusted): (PathBuf, &[&str], bool) = match marker {
            Some(marker) => (git_common_directory(&marker)?, &["cooldown", "locks"], true),
            None => (
                project.as_std_path().to_owned(),
                &[".cooldown", "locks"],
                false,
            ),
        };
        let directory = secure_create_coordination_directory(&base, components)?;
        let directory_identity = std::sync::Arc::new(same_file::Handle::from_path(&directory)?);
        let recovery_authority = trusted.then(|| RecoveryAuthority {
            directory: directory.clone(),
            directory_identity: directory_identity.clone(),
            project: project.clone(),
        });
        Ok(ProjectCoordination {
            directory,
            directory_identity,
            project,
            recovery_authority,
        })
    }

    /// Returns the owner-private coordination directory.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the canonical project identity used to derive the namespace.
    #[must_use]
    pub fn project(&self) -> &Utf8Path {
        &self.project
    }

    /// Returns trusted recovery authority when coordination is outside project-controlled state.
    #[must_use]
    pub fn recovery_authority(&self) -> Option<&RecoveryAuthority> {
        self.recovery_authority.as_ref()
    }

    /// Revalidates the project identity and its current coordination namespace.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the project or coordination namespace changed
    /// after this capability was captured.
    pub fn validate_current(&self) -> Result<(), CoreError> {
        let current = Self::resolve(&self.project).map_err(|error| {
            CoreError::LockConflict(format!(
                "could not revalidate project coordination for {}: {error}",
                self.project
            ))
        })?;
        if self.project != current.project
            || self.directory != current.directory
            || self.directory_identity != current.directory_identity
        {
            return Err(CoreError::LockConflict(format!(
                "project coordination changed after access was acquired for {}",
                self.project
            )));
        }
        Ok(())
    }
}

impl RecoveryAuthority {
    /// Returns the trusted directory containing recovery authority records.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Returns the canonical project identity governed by this authority.
    #[must_use]
    pub fn project(&self) -> &Utf8Path {
        &self.project
    }

    /// Revalidates that this project still resolves to the captured trusted namespace.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockConflict`] when the project no longer resolves to the same Git
    /// coordination directory.
    pub fn validate_current(&self) -> Result<(), CoreError> {
        let current = ProjectCoordination::resolve(&self.project).map_err(|error| {
            CoreError::LockConflict(format!(
                "could not revalidate recovery authority for {}: {error}",
                self.project
            ))
        })?;
        let Some(authority) = current.recovery_authority else {
            return Err(CoreError::LockConflict(format!(
                "trusted recovery authority disappeared for {}",
                self.project
            )));
        };
        if self.directory != authority.directory
            || self.directory_identity != authority.directory_identity
        {
            return Err(CoreError::LockConflict(format!(
                "recovery authority changed after project access was acquired for {}",
                self.project
            )));
        }
        Ok(())
    }
}

/// Verifies that an opened authority file is private to the current user where supported.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] on Unix when the file belongs to another user or grants any
/// group or other permission.
pub fn validate_owner_private_file(file: &std::fs::File, path: &Path) -> Result<(), CoreError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let metadata = file.metadata()?;
        let current_user = rustix::process::geteuid().as_raw();
        if metadata.uid() != current_user || metadata.mode() & 0o077 != 0 {
            return Err(CoreError::Filesystem(format!(
                "authority file is not private to the current user: {}",
                path.display()
            )));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
    }
    Ok(())
}

fn canonical_project_root(root: &Utf8Path) -> Result<Utf8PathBuf, CoreError> {
    let path = std::fs::canonicalize(root)?;
    Utf8PathBuf::from_path_buf(path)
        .map_err(|path| CoreError::PathEncoding(format!("non-UTF-8 path: {}", path.display())))
}

fn git_marker(root: &Path) -> Result<Option<PathBuf>, CoreError> {
    for ancestor in root.ancestors() {
        let marker = ancestor.join(".git");
        match std::fs::symlink_metadata(&marker) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    || (metadata.file_type().is_dir() && marker.join("HEAD").is_file()) =>
            {
                return Ok(Some(marker));
            }
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(CoreError::Filesystem(format!(
                    "Git coordination marker is not a regular file or directory: {}",
                    marker.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(CoreError::Filesystem(format!(
                    "cannot inspect the Git coordination marker at {}: {error}",
                    marker.display()
                )));
            }
        }
    }
    Ok(None)
}

fn git_common_directory(marker: &Path) -> Result<PathBuf, CoreError> {
    if marker.is_dir() {
        return std::fs::canonicalize(marker).map_err(Into::into);
    }
    let contents = std::fs::read_to_string(marker)?;
    let git_dir = contents
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| {
            CoreError::Filesystem(format!(
                "invalid Git directory pointer at {}",
                marker.display()
            ))
        })?;
    let git_dir = resolve_relative(marker.parent().unwrap_or_else(|| Path::new("")), git_dir);
    let git_dir = std::fs::canonicalize(git_dir)?;
    let common_marker = git_dir.join("commondir");
    if !common_marker.is_file() {
        return Ok(git_dir);
    }
    let common = std::fs::read_to_string(&common_marker)?;
    let common = common.trim();
    if common.is_empty() {
        return Err(CoreError::Filesystem(format!(
            "empty Git common-directory pointer at {}",
            common_marker.display()
        )));
    }
    std::fs::canonicalize(resolve_relative(&git_dir, common)).map_err(Into::into)
}

fn resolve_relative(base: &Path, value: &str) -> PathBuf {
    let value = Path::new(value);
    if value.is_absolute() {
        value.to_owned()
    } else {
        base.join(value)
    }
}

fn secure_create_coordination_directory(
    base: &Path,
    components: &[&str],
) -> Result<PathBuf, CoreError> {
    let mut current = base.to_owned();
    let base_metadata = std::fs::symlink_metadata(&current)?;
    if !base_metadata.file_type().is_dir() {
        return Err(CoreError::Filesystem(format!(
            "coordination root is not a regular directory: {}",
            current.display()
        )));
    }
    for component in components {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_dir() => {}
            Ok(_) => {
                return Err(CoreError::Filesystem(format!(
                    "coordination path is not a regular directory: {}",
                    current.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match create_coordination_directory(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error.into()),
                }
                let metadata = std::fs::symlink_metadata(&current)?;
                if !metadata.file_type().is_dir() {
                    return Err(CoreError::Filesystem(format!(
                        "coordination path changed while it was created: {}",
                        current.display()
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        validate_coordination_directory(&std::fs::symlink_metadata(&current)?, &current)?;
    }
    let canonical = std::fs::canonicalize(&current)?;
    if canonical != current {
        return Err(CoreError::Filesystem(format!(
            "coordination directory traverses a symbolic link: {}",
            current.display()
        )));
    }
    Ok(current)
}

#[cfg(unix)]
fn validate_coordination_directory(
    metadata: &std::fs::Metadata,
    path: &Path,
) -> Result<(), CoreError> {
    use std::os::unix::fs::MetadataExt as _;
    let current_user = rustix::process::geteuid().as_raw();
    if metadata.uid() != current_user || metadata.mode() & 0o022 != 0 {
        return Err(CoreError::Filesystem(format!(
            "coordination directory is not private to the current user: {}",
            path.display()
        )));
    }
    Ok(())
}

fn create_coordination_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::DirBuilder::new().create(path)
    }
}

/// The commit state of an atomic replacement with parent-directory durability on Unix.
#[derive(Debug, thiserror::Error)]
pub enum DurableWriteError {
    /// The public path was not replaced.
    #[error("the replacement was not committed: {0}")]
    NotCommitted(CoreError),
    /// The public path was replaced, but syncing its parent directory failed.
    #[error("the replacement is visible but its directory durability is uncertain: {0}")]
    DurabilityUncertain(CoreError),
}

impl DurableWriteError {
    /// Converts the commit-aware failure into the workspace error type.
    #[must_use]
    pub fn into_core_error(self, path: &Path) -> CoreError {
        match self {
            DurableWriteError::NotCommitted(error) => error,
            DurableWriteError::DurabilityUncertain(error) => CoreError::LockConflict(format!(
                "{} was replaced, but syncing its parent directory failed; the replacement is visible but power-loss durability is uncertain: {error}",
                path.display()
            )),
        }
    }
}

/// Writes `bytes` to `path` atomically: readers observe either the old contents or the new ones,
/// never a torn file. The bytes go to a `.{name}.{pid}.{attempt}.tmp` sibling first (created with
/// `create_new` so concurrent writers never share a temp file), are fsynced, and are then renamed
/// over `path` — rename within one directory is atomic on the platforms cooldown supports.
///
/// # Errors
///
/// Returns [`CoreError::PathEncoding`] when `path` has no UTF-8 file name,
/// [`CoreError::Filesystem`] when no temp file could be created after 100 attempts, or the
/// underlying I/O error from writing, syncing, or renaming (the temp file is removed on failure).
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), CoreError> {
    let permissions = existing_permissions(path)?;
    write_replacement(path, bytes, permissions.as_ref())
}

/// Writes `bytes` atomically using the supplied permissions for the replacement.
///
/// # Errors
///
/// Returns the same errors as [`atomic_write`], plus an I/O error if applying `permissions` to the
/// temporary replacement fails.
pub fn atomic_write_with_permissions(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> Result<(), CoreError> {
    write_replacement(path, bytes, permissions)
}

/// Prepares an atomic replacement, calls `validate` after the temporary file is synced, and only
/// then renames it over `path`.
///
/// This supports compare-before-restore protocols without opening a long validation window while a
/// large replacement is written.
/// Filesystems do not provide a portable compare-and-swap rename, so writers that ignore the
/// caller's lease can still race the final validation and rename.
///
/// # Errors
///
/// Returns the same errors as [`atomic_write_with_permissions`] or the error returned by `validate`.
pub fn atomic_write_with_permissions_checked<F>(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
    validate: F,
) -> Result<(), CoreError>
where
    F: FnOnce() -> Result<(), CoreError>,
{
    write_replacement_checked(path, bytes, permissions, validate)
}

/// Replaces `path` after validation and makes the directory entry durable when supported.
///
/// # Errors
///
/// Returns [`DurableWriteError::NotCommitted`] if validation or replacement fails before rename.
/// Returns [`DurableWriteError::DurabilityUncertain`] if rename succeeds but the parent directory
/// cannot be synced.
pub fn atomic_write_durable_with_permissions_checked<F>(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
    validate: F,
) -> Result<(), DurableWriteError>
where
    F: FnOnce() -> Result<(), CoreError>,
{
    write_replacement_checked(path, bytes, permissions, validate)
        .map_err(DurableWriteError::NotCommitted)?;
    sync_parent_after_commit(path)
}

/// Removes `path` and makes the directory-entry removal durable when supported.
///
/// A missing path is already in the requested state and succeeds without a directory sync.
///
/// # Errors
///
/// Returns [`DurableWriteError::NotCommitted`] if removal fails while the path remains visible.
/// Returns [`DurableWriteError::DurabilityUncertain`] if removal succeeds but the parent directory
/// cannot be synced.
pub fn remove_file_durable(path: &Path) -> Result<(), DurableWriteError> {
    match std::fs::remove_file(path) {
        Ok(()) => sync_parent_after_commit(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(DurableWriteError::NotCommitted(error.into())),
    }
}

/// Writes `bytes` atomically and, on Unix, syncs the containing directory after replacement.
///
/// Callers that maintain a recovery protocol can distinguish a failed replacement from a visible
/// replacement whose directory entry may not survive power loss.
///
/// # Errors
///
/// Returns [`DurableWriteError::NotCommitted`] if the public path was not replaced, or
/// [`DurableWriteError::DurabilityUncertain`] if replacement succeeded but the parent directory
/// could not be synced.
pub fn atomic_write_durable(path: &Path, bytes: &[u8]) -> Result<(), DurableWriteError> {
    let permissions = existing_permissions(path).map_err(DurableWriteError::NotCommitted)?;
    #[cfg(unix)]
    {
        atomic_write_durable_with(path, bytes, permissions.as_ref(), |parent| {
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })
    }
    #[cfg(not(unix))]
    {
        write_replacement(path, bytes, permissions.as_ref())
            .map_err(DurableWriteError::NotCommitted)
    }
}

#[cfg_attr(
    not(unix),
    expect(
        clippy::unnecessary_wraps,
        reason = "the shared cross-platform signature remains fallible on Unix"
    )
)]
fn sync_parent_after_commit(path: &Path) -> Result<(), DurableWriteError> {
    #[cfg(unix)]
    {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(CoreError::from)
            .map_err(DurableWriteError::DurabilityUncertain)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(unix)]
fn atomic_write_durable_with<F>(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
    sync_parent: F,
) -> Result<(), DurableWriteError>
where
    F: FnOnce(&Path) -> Result<(), CoreError>,
{
    write_replacement(path, bytes, permissions).map_err(DurableWriteError::NotCommitted)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    sync_parent(parent).map_err(DurableWriteError::DurabilityUncertain)
}

fn existing_permissions(path: &Path) -> Result<Option<std::fs::Permissions>, CoreError> {
    match std::fs::metadata(path) {
        Ok(metadata) => Ok(Some(metadata.permissions())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_replacement(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
) -> Result<(), CoreError> {
    write_replacement_checked(path, bytes, permissions, || Ok(()))
}

fn write_replacement_checked<F>(
    path: &Path,
    bytes: &[u8],
    permissions: Option<&std::fs::Permissions>,
    validate: F,
) -> Result<(), CoreError>
where
    F: FnOnce() -> Result<(), CoreError>,
{
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| CoreError::PathEncoding(format!("non-utf8 path: {}", path.display())))?;

    let mut validate = Some(validate);
    for attempt in 0..100_u8 {
        let tmp = parent.join(format!(
            ".{file_name}.{}.{}.tmp",
            std::process::id(),
            attempt
        ));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let written = file.write_all(bytes).and_then(|()| {
            if let Some(permissions) = permissions {
                file.set_permissions(permissions.clone())?;
            }
            file.sync_all()
        });
        if let Err(error) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
        let validation = validate.take().ok_or_else(|| {
            CoreError::System("atomic replacement validation was already consumed".to_string())
        })?;
        if let Err(error) = validation() {
            let _ = std::fs::remove_file(&tmp);
            return Err(error);
        }
        if let Err(error) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
        }
        return Ok(());
    }

    Err(CoreError::Filesystem(format!(
        "could not create temporary file for atomic write to {}",
        path.display()
    )))
}

/// A deterministic 64-bit FNV-1a hash, used to derive stable file names (cache entries, per-project
/// lock files) across runs — the std hasher is randomized per process. **Not** cryptographic: never
/// use it where an adversary choosing the input to collide matters without a secondary check (the
/// HTTP cache re-verifies the stored URL on read for exactly this reason).
#[must_use]
pub fn fnv1a_64(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::CoreError;
    #[cfg(unix)]
    use super::DurableWriteError;
    use super::{ProjectCoordination, atomic_write, atomic_write_with_permissions_checked};
    use camino::Utf8Path;
    use color_eyre::eyre;

    #[test]
    fn atomic_write_writes_exact_bytes_and_leaves_no_temp_file() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.json");

        atomic_write(&path, b"first")?;
        atomic_write(&path, b"second")?;

        assert_eq!(std::fs::read(&path)?, b"second");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
        Ok(())
    }

    #[test]
    fn checked_atomic_write_validates_after_preparing_the_replacement() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.json");
        atomic_write(&path, b"external")?;

        let result = atomic_write_with_permissions_checked(&path, b"rollback", None, || {
            Err(CoreError::LockConflict("external edit".to_string()))
        });

        std::assert_matches!(result, Err(CoreError::LockConflict(_)));
        assert_eq!(std::fs::read(path)?, b"external");
        Ok(())
    }

    #[test]
    fn non_git_coordination_does_not_grant_recovery_authority() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;

        let coordination = ProjectCoordination::resolve(root)?;

        assert!(coordination.recovery_authority().is_none());
        assert!(coordination.directory().starts_with(root.join(".cooldown")));
        Ok(())
    }

    #[test]
    fn replaced_git_namespace_invalidates_captured_coordination() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")?;
        let coordination = ProjectCoordination::resolve(root)?;
        std::fs::rename(root.join(".git"), root.join(".git.previous"))?;
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")?;

        std::assert_matches!(
            coordination.validate_current(),
            Err(CoreError::LockConflict(_))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn durable_write_reports_a_visible_replacement_when_directory_sync_fails() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("state.json");
        atomic_write(&path, b"first")?;

        let result = super::atomic_write_durable_with(&path, b"second", None, |_parent| {
            Err(CoreError::Filesystem(
                "injected directory sync failure".to_string(),
            ))
        });

        std::assert_matches!(result, Err(DurableWriteError::DurabilityUncertain(_)));
        assert_eq!(std::fs::read(path)?, b"second");
        Ok(())
    }
}
