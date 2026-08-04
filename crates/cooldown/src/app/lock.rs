//! Shared read and exclusive write access for project and repository package-manager resources.

use cooldown_core::fs::{
    ProjectCoordination, ProjectReadLease, ProjectWriteLease, RepositoryResourceReadLease,
    RepositoryResourceWriteLease,
};
use cooldown_core::{CoreError, ToolId};
#[cfg(unix)]
use std::collections::HashSet;
#[cfg(unix)]
use std::fs::TryLockError;
#[cfg(any(unix, test))]
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(any(unix, test))]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::{Mutex, OnceLock};
#[cfg(unix)]
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
const STALE_LOCK_AGE: Duration = Duration::from_hours(720);
#[cfg(unix)]
const STALE_COLLECTION_INTERVAL: Duration = Duration::from_hours(24);

/// The target-derived directory where every process rendezvouses for project access.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct CoordinationRoot(PathBuf);

#[cfg(test)]
impl CoordinationRoot {
    fn resolve(target: &camino::Utf8Path) -> Result<(Self, camino::Utf8PathBuf), CoreError> {
        let coordination = ProjectCoordination::resolve(target)?;
        Ok((
            CoordinationRoot(coordination.directory().to_owned()),
            coordination.project().to_owned(),
        ))
    }

    #[cfg(test)]
    fn project_lock(&self, project: &camino::Utf8Path) -> PathBuf {
        self.0.join(format!(
            "{:016x}.lock",
            cooldown_core::fs::fnv1a_64(project.as_str())
        ))
    }
}

/// Holds an OS-backed shared lock for project reads.
#[derive(Debug)]
pub(crate) struct ProjectReadGuard {
    lease: ProjectReadLease,
}

/// Holds an OS-backed exclusive lock for project mutations.
#[derive(Debug)]
pub(crate) struct ProjectWriteGuard {
    lease: ProjectWriteLease,
}

/// Holds an OS-backed shared lock for one repository-wide tool resource.
#[derive(Debug)]
pub(crate) struct RepoToolReadGuard {
    #[expect(dead_code, reason = "the field keeps the repository read lease alive")]
    lease: RepositoryResourceReadLease,
}

/// Holds an OS-backed exclusive lock for one repository-wide tool resource.
#[derive(Debug)]
pub(crate) struct RepoToolWriteGuard {
    #[expect(dead_code, reason = "the field keeps the repository write lease alive")]
    lease: RepositoryResourceWriteLease,
}

/// Holds every shared lease needed to read one project's package-manager state.
#[derive(Debug)]
pub(crate) struct ProjectAccessReadGuard {
    #[expect(dead_code, reason = "the field keeps the repository read lease alive")]
    repo: Option<RepoToolReadGuard>,
    project: ProjectReadGuard,
}

/// Holds every exclusive/shared lease needed to mutate one project's package-manager state.
#[derive(Debug)]
pub(crate) struct ProjectAccessWriteGuard {
    #[expect(dead_code, reason = "the field keeps the repository read lease alive")]
    repo: Option<RepoToolReadGuard>,
    project: ProjectWriteGuard,
}

impl ProjectReadGuard {
    /// Acquires shared access, failing immediately while a writer owns the project.
    pub(crate) fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let coordination = project_coordination(root)?;
        Ok(ProjectReadGuard {
            lease: ProjectReadLease::acquire_coordination(coordination)?,
        })
    }

    /// Returns the coordination identity captured by this shared lease.
    pub(crate) fn coordination(&self) -> &ProjectCoordination {
        self.lease.coordination()
    }
}

impl ProjectWriteGuard {
    /// Acquires exclusive access, failing immediately while another reader or writer is active.
    pub(crate) fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let coordination = project_coordination(root)?;
        Ok(ProjectWriteGuard {
            lease: ProjectWriteLease::acquire_coordination(coordination)?,
        })
    }

    /// Returns the coordination identity captured by this exclusive lease.
    pub(crate) fn coordination(&self) -> &ProjectCoordination {
        self.lease.coordination()
    }
}

impl RepoToolReadGuard {
    /// Acquires shared access to one tool's repository-wide native state.
    pub(crate) fn acquire(root: &camino::Utf8Path, tool: ToolId) -> Result<Self, CoreError> {
        let coordination = project_coordination(root)?;
        Ok(RepoToolReadGuard {
            lease: RepositoryResourceReadLease::acquire_coordination(coordination, tool)?,
        })
    }
}

impl RepoToolWriteGuard {
    /// Acquires exclusive access to one tool's repository-wide native state.
    pub(crate) fn acquire(root: &camino::Utf8Path, tool: ToolId) -> Result<Self, CoreError> {
        let coordination = project_coordination(root)?;
        Ok(RepoToolWriteGuard {
            lease: RepositoryResourceWriteLease::acquire_coordination(coordination, tool)?,
        })
    }
}

impl ProjectAccessReadGuard {
    /// Acquires the repository resource before the project lease to preserve global lock order.
    pub(crate) fn acquire(
        repo_root: &camino::Utf8Path,
        project_root: &camino::Utf8Path,
        tool: ToolId,
        repo_scoped: bool,
    ) -> Result<Self, CoreError> {
        let repo = repo_scoped
            .then(|| RepoToolReadGuard::acquire(repo_root, tool))
            .transpose()?;
        let project = ProjectReadGuard::acquire(project_root)?;
        Ok(ProjectAccessReadGuard { repo, project })
    }

    /// Returns the coordination identity captured by the project read lease.
    pub(crate) fn coordination(&self) -> &ProjectCoordination {
        self.project.coordination()
    }
}

impl ProjectAccessWriteGuard {
    /// Acquires a shared repository-resource lease before exclusive project access.
    pub(crate) fn acquire(
        repo_root: &camino::Utf8Path,
        project_root: &camino::Utf8Path,
        tool: ToolId,
        repo_scoped: bool,
    ) -> Result<Self, CoreError> {
        let repo = repo_scoped
            .then(|| RepoToolReadGuard::acquire(repo_root, tool))
            .transpose()?;
        let project = ProjectWriteGuard::acquire(project_root)?;
        Ok(ProjectAccessWriteGuard { repo, project })
    }

    /// Returns the coordination identity captured by the project write lease.
    pub(crate) fn coordination(&self) -> &ProjectCoordination {
        self.project.coordination()
    }
}

fn project_coordination(root: &camino::Utf8Path) -> Result<ProjectCoordination, CoreError> {
    let coordination = ProjectCoordination::resolve(root)?;
    collect_stale_locks_once(coordination.directory());
    Ok(coordination)
}

#[cfg(unix)]
fn lock_error(path: &Path, error: &std::io::Error) -> CoreError {
    CoreError::Filesystem(format!("{}: {error}", path.display()))
}

#[cfg(any(unix, test))]
fn open_lock_file(path: &Path) -> Result<File, CoreError> {
    let parent = path.parent().ok_or_else(|| {
        CoreError::Filesystem(format!(
            "coordination lock has no parent: {}",
            path.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir() {
        return Err(CoreError::Filesystem(format!(
            "coordination lock parent is not a regular directory: {}",
            parent.display()
        )));
    }
    let parent_identity = same_file::Handle::from_path(parent)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(CoreError::Filesystem(format!(
                "coordination lock is not a regular file: {}",
                path.display()
            )));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(CoreError::Filesystem(format!(
            "coordination lock did not open as a regular file: {}",
            path.display()
        )));
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || same_file::Handle::from_file(file.try_clone()?)? != same_file::Handle::from_path(path)?
        || parent_identity != same_file::Handle::from_path(parent)?
    {
        return Err(CoreError::Filesystem(format!(
            "coordination lock changed identity while opening: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    require_single_link(&file.metadata()?, path)?;
    Ok(file)
}

#[cfg(test)]
fn open_coordinated_lock_file(path: &Path) -> Result<(File, File), CoreError> {
    let Some(directory) = path.parent() else {
        return Err(CoreError::Filesystem(format!(
            "lock path has no parent: {}",
            path.display()
        )));
    };
    collect_stale_locks_once(directory);
    let coordination_path = directory.join(".maintenance.lock");
    let coordination = open_lock_file(&coordination_path)?;
    coordination.lock_shared().map_err(CoreError::from)?;
    let file = open_lock_file(path)?;
    Ok((file, coordination))
}

#[cfg(unix)]
fn collect_stale_locks_once(directory: &Path) {
    static COLLECTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let collected = COLLECTED.get_or_init(|| Mutex::new(HashSet::new()));
    {
        let Ok(mut collected) = collected.lock() else {
            return;
        };
        if !collected.insert(directory.to_owned()) {
            return;
        }
    }
    if !matches!(
        collect_stale_lock_files(directory, STALE_LOCK_AGE),
        Ok(true)
    ) && let Ok(mut collected) = collected.lock()
    {
        collected.remove(directory);
    }
}

#[cfg(unix)]
fn collect_stale_lock_files(directory: &Path, minimum_age: Duration) -> Result<bool, CoreError> {
    let coordination_path = directory.join(".maintenance.lock");
    let mut coordination = open_lock_file(&coordination_path)?;
    match coordination.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(lock_error(&coordination_path, &error)),
    }
    require_single_link(&coordination.metadata()?, &coordination_path)?;

    let now = SystemTime::now();
    if maintenance_is_recent(&mut coordination, now, STALE_COLLECTION_INTERVAL)? {
        return Ok(true);
    }

    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if path == coordination_path
            || path.extension().and_then(|value| value.to_str()) != Some("lock")
        {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let old_enough = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age >= minimum_age);
        if !old_enough {
            continue;
        }
        let Ok(file) = open_lock_file(&path) else {
            continue;
        };
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => continue,
            Err(TryLockError::Error(error)) => {
                tracing::debug!(path = %path.display(), %error, "could not inspect stale access lock");
                continue;
            }
        }
        drop(file);
        if let Err(error) = std::fs::remove_file(&path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(path = %path.display(), %error, "could not remove stale access lock");
        }
    }
    record_maintenance(&mut coordination, now)?;
    Ok(true)
}

#[cfg(not(unix))]
fn collect_stale_locks_once(_directory: &std::path::Path) {}

#[cfg(unix)]
fn maintenance_is_recent(
    file: &mut File,
    now: SystemTime,
    interval: Duration,
) -> Result<bool, CoreError> {
    file.seek(SeekFrom::Start(0))?;
    let mut timestamp = String::new();
    file.read_to_string(&mut timestamp)?;
    let Some(previous) = timestamp.trim().parse::<u64>().ok() else {
        return Ok(false);
    };
    let now = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    Ok(previous <= now && now - previous < interval.as_secs())
}

#[cfg(unix)]
fn record_maintenance(file: &mut File, now: SystemTime) -> Result<(), CoreError> {
    let timestamp = now
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    writeln!(file, "{timestamp}")?;
    file.sync_data()?;
    Ok(())
}

#[cfg(unix)]
fn require_single_link(metadata: &std::fs::Metadata, path: &Path) -> Result<(), CoreError> {
    use std::os::unix::fs::MetadataExt as _;
    if metadata.nlink() == 1 {
        return Ok(());
    }
    Err(CoreError::Filesystem(format!(
        "coordination lock has multiple hard links: {}",
        path.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;

    #[cfg(unix)]
    #[test]
    fn maintenance_lock_symlink_cannot_truncate_a_project_file() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let manifest = root.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"safe\"\n")?;
        let (coordination, canonical) = CoordinationRoot::resolve(root)?;
        symlink("../../Cargo.toml", coordination.0.join(".maintenance.lock"))?;

        let result = open_coordinated_lock_file(&coordination.project_lock(&canonical));

        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        assert_eq!(
            std::fs::read_to_string(&manifest)?,
            "[package]\nname = \"safe\"\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn project_lock_symlink_cannot_truncate_a_project_file() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let manifest = root.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"safe\"\n")?;
        let (coordination, canonical) = CoordinationRoot::resolve(root)?;
        symlink("../../Cargo.toml", coordination.project_lock(&canonical))?;

        let error = open_coordinated_lock_file(&coordination.project_lock(&canonical))
            .err()
            .ok_or_else(|| eyre::eyre!("project-lock symlink was accepted"))?;

        std::assert_matches!(error, CoreError::Filesystem(_), "{error:?}");
        assert_eq!(
            std::fs::read_to_string(&manifest)?,
            "[package]\nname = \"safe\"\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn coordination_directory_symlink_is_rejected() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir()?;
        let outside = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        std::fs::create_dir_all(outside.path().join("locks"))?;
        symlink(outside.path(), root.join(".cooldown"))?;
        let error = CoordinationRoot::resolve(root)
            .err()
            .ok_or_else(|| eyre::eyre!("coordination symlink was accepted"))?;
        std::assert_matches!(error, CoreError::Filesystem(_), "{error:?}");
        Ok(())
    }

    #[test]
    fn maintenance_hard_link_cannot_truncate_a_project_file() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let manifest = root.join("Cargo.toml");
        std::fs::write(&manifest, "[package]\nname = \"safe\"\n")?;
        let (coordination, canonical) = CoordinationRoot::resolve(root)?;
        std::fs::hard_link(&manifest, coordination.0.join(".maintenance.lock"))?;

        let result = open_coordinated_lock_file(&coordination.project_lock(&canonical));
        #[cfg(unix)]
        std::assert_matches!(result, Err(CoreError::Filesystem(_)));
        #[cfg(not(unix))]
        drop(result?);
        assert_eq!(
            std::fs::read_to_string(&manifest)?,
            "[package]\nname = \"safe\"\n"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn shared_coordination_directory_is_rejected() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        std::fs::create_dir(root.join(".cooldown"))?;
        std::fs::set_permissions(
            root.join(".cooldown"),
            std::fs::Permissions::from_mode(0o777),
        )?;
        let error = CoordinationRoot::resolve(root)
            .err()
            .ok_or_else(|| eyre::eyre!("shared coordination directory was accepted"))?;

        std::assert_matches!(error, CoreError::Filesystem(_), "{error:?}");
        Ok(())
    }

    #[test]
    fn readers_can_share_project_access() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(dir.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let _first = ProjectReadGuard::acquire(root)?;

        ProjectReadGuard::acquire(root)?;
        Ok(())
    }

    #[test]
    fn reader_and_writer_exclude_each_other() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(dir.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let _reader = ProjectReadGuard::acquire(root)?;

        std::assert_matches!(
            ProjectWriteGuard::acquire(root),
            Err(CoreError::LockConflict(_))
        );
        Ok(())
    }

    #[test]
    fn writer_excludes_readers_and_writers() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(dir.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let _writer = ProjectWriteGuard::acquire(root)?;

        std::assert_matches!(
            ProjectReadGuard::acquire(root),
            Err(CoreError::LockConflict(_))
        );
        std::assert_matches!(
            ProjectWriteGuard::acquire(root),
            Err(CoreError::LockConflict(_))
        );
        Ok(())
    }

    #[test]
    fn write_access_can_be_reacquired_after_drop() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(dir.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;

        {
            let _guard = ProjectWriteGuard::acquire(root)?;
        }

        ProjectWriteGuard::acquire(root)?;
        Ok(())
    }

    #[test]
    fn git_projects_rendezvous_in_the_repository_metadata() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let nested = root.join("packages/app");
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n")?;
        std::fs::create_dir_all(&nested)?;

        let (coordination, _) = CoordinationRoot::resolve(&nested)?;

        assert_eq!(coordination.0, root.join(".git/cooldown/locks"));
        Ok(())
    }

    #[test]
    fn linked_worktrees_share_the_git_common_directory() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let common = root.join("common.git");
        let git_dir = common.join("worktrees/demo");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(&git_dir)?;
        std::fs::create_dir_all(&worktree)?;
        std::fs::write(git_dir.join("commondir"), "../..\n")?;
        std::fs::write(worktree.join(".git"), format!("gitdir: {git_dir}\n"))?;

        let (coordination, _) = CoordinationRoot::resolve(&worktree)?;

        assert_eq!(coordination.0, common.join("cooldown/locks"));
        Ok(())
    }

    #[test]
    fn non_git_projects_use_an_adjacent_coordination_directory() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(directory.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let (coordination, canonical) = CoordinationRoot::resolve(root)?;

        assert_eq!(coordination.0, canonical.join(".cooldown/locks"));
        Ok(())
    }

    #[test]
    fn repository_tool_access_coordinates_the_shared_resource() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(dir.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let _reader = RepoToolReadGuard::acquire(root, ToolId("uv"))?;

        // Readers share one tool resource, its writer conflicts, and another tool stays
        // independent.
        RepoToolReadGuard::acquire(root, ToolId("uv"))?;
        std::assert_matches!(
            RepoToolWriteGuard::acquire(root, ToolId("uv")),
            Err(CoreError::LockConflict(_))
        );
        RepoToolWriteGuard::acquire(root, ToolId("cargo"))?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn stale_unheld_lock_entries_are_collected_safely() -> eyre::Result<()> {
        let locks = tempfile::tempdir()?;
        let stale = locks.path().join("stale.lock");
        let held = locks.path().join("held.lock");
        drop(open_lock_file(&stale)?);
        let held_file = open_lock_file(&held)?;
        held_file.try_lock()?;

        assert!(collect_stale_lock_files(locks.path(), Duration::ZERO)?);

        // Maintenance removes only an inactive inode and preserves the one with a live lease.
        assert!(!stale.exists());
        assert!(held.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn maintenance_timestamp_throttles_collection_across_calls() -> eyre::Result<()> {
        let locks = tempfile::tempdir()?;
        let first = locks.path().join("first.lock");
        drop(open_lock_file(&first)?);
        assert!(collect_stale_lock_files(locks.path(), Duration::ZERO)?);
        assert!(!first.exists());

        let second = locks.path().join("second.lock");
        drop(open_lock_file(&second)?);
        assert!(collect_stale_lock_files(locks.path(), Duration::ZERO)?);

        assert!(second.exists());
        Ok(())
    }
}
