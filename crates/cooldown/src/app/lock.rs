//! Shared read and exclusive write access for project and repository package-manager resources.

use cooldown_core::{CoreError, ToolId};
use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const STALE_LOCK_AGE: Duration = Duration::from_hours(720);
const STALE_COLLECTION_INTERVAL: Duration = Duration::from_hours(24);

/// Holds an OS-backed shared lock for project reads.
#[derive(Debug)]
pub(crate) struct ProjectReadGuard {
    file: File,
}

/// Holds an OS-backed exclusive lock for project mutations.
#[derive(Debug)]
pub(crate) struct ProjectWriteGuard {
    file: File,
}

/// Holds an OS-backed shared lock for one repository-wide tool resource.
#[derive(Debug)]
pub(crate) struct RepoToolReadGuard {
    file: File,
}

/// Holds an OS-backed exclusive lock for one repository-wide tool resource.
#[derive(Debug)]
pub(crate) struct RepoToolWriteGuard {
    file: File,
}

/// Holds every shared lease needed to read one project's package-manager state.
#[derive(Debug)]
pub(crate) struct ProjectAccessReadGuard {
    #[expect(dead_code, reason = "the field keeps the repository read lease alive")]
    repo: Option<RepoToolReadGuard>,
    #[expect(dead_code, reason = "the field keeps the project read lease alive")]
    project: ProjectReadGuard,
}

/// Holds every exclusive/shared lease needed to mutate one project's package-manager state.
#[derive(Debug)]
pub(crate) struct ProjectAccessWriteGuard {
    #[expect(dead_code, reason = "the field keeps the repository read lease alive")]
    repo: Option<RepoToolReadGuard>,
    #[expect(dead_code, reason = "the field keeps the project write lease alive")]
    project: ProjectWriteGuard,
}

impl ProjectReadGuard {
    /// Acquires shared access, failing immediately while a writer owns the project.
    pub(crate) fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let (path, file, coordination) = open_project_lock(root)?;
        let guard = Self::acquire_file(&path, file)?;
        drop(coordination);
        Ok(guard)
    }

    fn acquire_file(path: &Path, file: File) -> Result<Self, CoreError> {
        match file.try_lock_shared() {
            Ok(()) => Ok(ProjectReadGuard { file }),
            Err(TryLockError::WouldBlock) => {
                Err(lock_conflict(path, "a mutating cooldown run", true))
            }
            Err(TryLockError::Error(error)) => Err(lock_error(path, &error)),
        }
    }

    #[cfg(test)]
    fn acquire_in(root: &camino::Utf8Path, directory: &Path) -> Result<Self, CoreError> {
        let path = directory.join(lock_file_name(root));
        let file = open_lock_file(&path)?;
        Self::acquire_file(&path, file)
    }
}

impl ProjectWriteGuard {
    /// Acquires exclusive access, failing immediately while another reader or writer is active.
    pub(crate) fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let (path, mut file, coordination) = open_project_lock(root)?;
        Self::acquire_file(root, &path, &mut file)?;
        drop(coordination);
        Ok(ProjectWriteGuard { file })
    }

    fn acquire_file(
        root: &camino::Utf8Path,
        path: &Path,
        file: &mut File,
    ) -> Result<(), CoreError> {
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(lock_conflict(path, "another cooldown run", false));
            }
            Err(TryLockError::Error(error)) => return Err(lock_error(path, &error)),
        }

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let _ = writeln!(
            file,
            "locked by cooldown pid {} for {}",
            std::process::id(),
            root
        );
        let _ = file.sync_data();
        tracing::trace!(path = %path.display(), "acquired exclusive project access");
        Ok(())
    }

    #[cfg(test)]
    fn acquire_in(root: &camino::Utf8Path, directory: &Path) -> Result<Self, CoreError> {
        let path = directory.join(lock_file_name(root));
        let mut file = open_lock_file(&path)?;
        Self::acquire_file(root, &path, &mut file)?;
        Ok(ProjectWriteGuard { file })
    }
}

impl RepoToolReadGuard {
    /// Acquires shared access to one tool's repository-wide native state.
    pub(crate) fn acquire(root: &camino::Utf8Path, tool: ToolId) -> Result<Self, CoreError> {
        let (path, file, coordination) = open_repo_tool_lock(root, tool)?;
        let file = acquire_shared_file(&path, file)?;
        drop(coordination);
        Ok(RepoToolReadGuard { file })
    }

    #[cfg(test)]
    fn acquire_in(
        root: &camino::Utf8Path,
        tool: ToolId,
        directory: &Path,
    ) -> Result<Self, CoreError> {
        let path = directory.join(repo_tool_lock_file_name(root, tool));
        let file = acquire_shared_file(&path, open_lock_file(&path)?)?;
        Ok(RepoToolReadGuard { file })
    }
}

impl RepoToolWriteGuard {
    /// Acquires exclusive access to one tool's repository-wide native state.
    pub(crate) fn acquire(root: &camino::Utf8Path, tool: ToolId) -> Result<Self, CoreError> {
        let (path, file, coordination) = open_repo_tool_lock(root, tool)?;
        let identity = format!("{} repository resource at {root}", tool.as_str());
        let file = acquire_exclusive_file(&identity, &path, file)?;
        drop(coordination);
        Ok(RepoToolWriteGuard { file })
    }

    #[cfg(test)]
    fn acquire_in(
        root: &camino::Utf8Path,
        tool: ToolId,
        directory: &Path,
    ) -> Result<Self, CoreError> {
        let path = directory.join(repo_tool_lock_file_name(root, tool));
        let identity = format!("{} repository resource at {root}", tool.as_str());
        let file = acquire_exclusive_file(&identity, &path, open_lock_file(&path)?)?;
        Ok(RepoToolWriteGuard { file })
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
}

fn open_project_lock(root: &camino::Utf8Path) -> Result<(PathBuf, File, File), CoreError> {
    let path = lock_path(root)?;
    let (file, coordination) = open_coordinated_lock_file(&path).map_err(|error| {
        CoreError::Filesystem(format!(
            "cannot open the project lock in the configured state namespace at {}: {error}",
            path.display()
        ))
    })?;
    Ok((path, file, coordination))
}

fn open_repo_tool_lock(
    root: &camino::Utf8Path,
    tool: ToolId,
) -> Result<(PathBuf, File, File), CoreError> {
    let path = state_lock_dir()?.join(repo_tool_lock_file_name(root, tool));
    let (file, coordination) = open_coordinated_lock_file(&path).map_err(|error| {
        CoreError::Filesystem(format!(
            "cannot open the repository resource lock in the configured state namespace at {}: {error}",
            path.display()
        ))
    })?;
    Ok((path, file, coordination))
}

fn lock_conflict(path: &Path, owner: &str, include_holder: bool) -> CoreError {
    let holder = include_holder
        .then(|| std::fs::read_to_string(path).ok())
        .flatten()
        .and_then(|contents| contents.lines().next().map(str::to_string))
        .filter(|line| !line.is_empty())
        .map(|line| format!(" ({line})"))
        .unwrap_or_default();
    CoreError::LockConflict(format!("{} is locked by {owner}{holder}", path.display()))
}

fn lock_error(path: &Path, error: &std::io::Error) -> CoreError {
    CoreError::Filesystem(format!("{}: {error}", path.display()))
}

fn open_lock_file(path: &Path) -> Result<File, CoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(CoreError::from)
}

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

fn acquire_shared_file(path: &Path, file: File) -> Result<File, CoreError> {
    match file.try_lock_shared() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(lock_conflict(path, "a mutating cooldown run", true)),
        Err(TryLockError::Error(error)) => Err(lock_error(path, &error)),
    }
}

fn acquire_exclusive_file(identity: &str, path: &Path, mut file: File) -> Result<File, CoreError> {
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(lock_conflict(path, "another cooldown run", false));
        }
        Err(TryLockError::Error(error)) => return Err(lock_error(path, &error)),
    }

    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    let _ = writeln!(
        file,
        "locked by cooldown pid {} for {identity}",
        std::process::id()
    );
    let _ = file.sync_data();
    tracing::trace!(path = %path.display(), "acquired exclusive resource access");
    Ok(file)
}

fn collect_stale_locks_once(directory: &Path) {
    static COLLECTED: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let collected = COLLECTED.get_or_init(|| Mutex::new(HashSet::new()));
    let Ok(mut collected) = collected.lock() else {
        return;
    };
    if collected.contains(directory) {
        return;
    }
    if matches!(
        collect_stale_lock_files(directory, STALE_LOCK_AGE),
        Ok(true)
    ) {
        collected.insert(directory.to_owned());
    }
}

fn collect_stale_lock_files(directory: &Path, minimum_age: Duration) -> Result<bool, CoreError> {
    std::fs::create_dir_all(directory)?;
    let coordination_path = directory.join(".maintenance.lock");
    let mut coordination = open_lock_file(&coordination_path)?;
    match coordination.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(false),
        Err(TryLockError::Error(error)) => return Err(lock_error(&coordination_path, &error)),
    }

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

fn lock_path(root: &camino::Utf8Path) -> Result<PathBuf, CoreError> {
    Ok(state_lock_dir()?.join(lock_file_name(root)))
}

fn state_lock_dir() -> Result<PathBuf, CoreError> {
    if let Some(path) = env_path("XDG_STATE_HOME") {
        return Ok(path.join("cooldown").join("locks"));
    }
    if let Some(home) = env_path("HOME") {
        return Ok(home
            .join(".local")
            .join("state")
            .join("cooldown")
            .join("locks"));
    }
    Err(CoreError::Filesystem(
        "cannot determine cooldown's state directory: neither XDG_STATE_HOME nor HOME is set"
            .to_string(),
    ))
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn lock_file_name(root: &camino::Utf8Path) -> String {
    let root = canonical_lock_root(root);
    format!("{:016x}.lock", cooldown_core::fs::fnv1a_64(root.as_str()))
}

fn repo_tool_lock_file_name(root: &camino::Utf8Path, tool: ToolId) -> String {
    let root = canonical_lock_root(root);
    let identity = format!("{}\0{}", root.as_str(), tool.as_str());
    format!("repo-{:016x}.lock", cooldown_core::fs::fnv1a_64(&identity))
}

fn canonical_lock_root(root: &camino::Utf8Path) -> camino::Utf8PathBuf {
    std::fs::canonicalize(root)
        .ok()
        .and_then(|path| camino::Utf8PathBuf::from_path_buf(path).ok())
        .unwrap_or_else(|| root.to_owned())
}

impl Drop for ProjectReadGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Drop for ProjectWriteGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Drop for RepoToolReadGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl Drop for RepoToolWriteGuard {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_can_share_project_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let locks = tempfile::tempdir().expect("lock dir");
        let _first = ProjectReadGuard::acquire_in(root, locks.path()).expect("first reader");

        ProjectReadGuard::acquire_in(root, locks.path()).expect("second reader");
    }

    #[test]
    fn reader_and_writer_exclude_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let locks = tempfile::tempdir().expect("lock dir");
        let _reader = ProjectReadGuard::acquire_in(root, locks.path()).expect("reader");

        let error =
            ProjectWriteGuard::acquire_in(root, locks.path()).expect_err("writer must fail");
        assert!(matches!(error, CoreError::LockConflict(_)));
    }

    #[test]
    fn writer_excludes_readers_and_writers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let locks = tempfile::tempdir().expect("lock dir");
        let _writer = ProjectWriteGuard::acquire_in(root, locks.path()).expect("writer");

        assert!(matches!(
            ProjectReadGuard::acquire_in(root, locks.path()).expect_err("reader must fail"),
            CoreError::LockConflict(_)
        ));
        assert!(matches!(
            ProjectWriteGuard::acquire_in(root, locks.path()).expect_err("second writer must fail"),
            CoreError::LockConflict(_)
        ));
    }

    #[test]
    fn write_access_can_be_reacquired_after_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let locks = tempfile::tempdir().expect("lock dir");

        {
            let _guard = ProjectWriteGuard::acquire_in(root, locks.path()).expect("first writer");
        }

        ProjectWriteGuard::acquire_in(root, locks.path()).expect("writer reacquired");
    }

    #[test]
    fn project_access_does_not_create_repo_local_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let locks = tempfile::tempdir().expect("lock dir");
        let repo_local = root.join(".cooldown.lock");

        let _guard = ProjectWriteGuard::acquire_in(root, locks.path()).expect("writer acquired");

        assert!(!repo_local.exists());
        assert!(!locks.path().join(".cooldown.lock").exists());
    }

    #[test]
    fn repository_tool_access_coordinates_the_shared_resource() -> color_eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = camino::Utf8Path::from_path(dir.path())
            .ok_or_else(|| color_eyre::eyre::eyre!("temporary path is not UTF-8"))?;
        let locks = tempfile::tempdir()?;
        let _reader = RepoToolReadGuard::acquire_in(root, ToolId("uv"), locks.path())?;

        // Readers share one tool resource, its writer conflicts, and another tool stays
        // independent.
        RepoToolReadGuard::acquire_in(root, ToolId("uv"), locks.path())?;
        assert!(matches!(
            RepoToolWriteGuard::acquire_in(root, ToolId("uv"), locks.path()),
            Err(CoreError::LockConflict(_))
        ));
        RepoToolWriteGuard::acquire_in(root, ToolId("cargo"), locks.path())?;
        Ok(())
    }

    #[test]
    fn stale_unheld_lock_entries_are_collected_safely() -> color_eyre::Result<()> {
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

    #[test]
    fn maintenance_timestamp_throttles_collection_across_calls() -> color_eyre::Result<()> {
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
