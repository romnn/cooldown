//! Shared read and exclusive write access for project-local package-manager state.

use cooldown_core::CoreError;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

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

impl ProjectReadGuard {
    /// Acquires shared access, failing immediately while a writer owns the project.
    pub(crate) fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let (path, file) = open_project_lock(root)?;
        match file.try_lock_shared() {
            Ok(()) => Ok(ProjectReadGuard { file }),
            Err(TryLockError::WouldBlock) => {
                Err(lock_conflict(&path, "a mutating cooldown run", true))
            }
            Err(TryLockError::Error(error)) => Err(lock_error(&path, &error)),
        }
    }
}

impl ProjectWriteGuard {
    /// Acquires exclusive access, failing immediately while another reader or writer is active.
    pub(crate) fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let (path, mut file) = open_project_lock(root)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(lock_conflict(&path, "another cooldown run", false));
            }
            Err(TryLockError::Error(error)) => return Err(lock_error(&path, &error)),
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
        Ok(ProjectWriteGuard { file })
    }

    #[cfg(test)]
    fn path_for_test(root: &camino::Utf8Path) -> PathBuf {
        lock_path(root)
    }
}

fn open_project_lock(root: &camino::Utf8Path) -> Result<(PathBuf, File), CoreError> {
    let preferred = lock_path(root);
    match open_lock_file(&preferred) {
        Ok(file) => Ok((preferred, file)),
        Err(preferred_error) => {
            let fallback = fallback_lock_path(root);
            match open_lock_file(&fallback) {
                Ok(file) => {
                    tracing::debug!(
                        preferred = %preferred.display(),
                        error = %preferred_error,
                        "state-dir lock unavailable; using temp-dir fallback"
                    );
                    Ok((fallback, file))
                }
                Err(fallback_error) => Err(CoreError::Filesystem(format!(
                    "cannot open a lock file: {} ({preferred_error}); fallback {} ({fallback_error})",
                    preferred.display(),
                    fallback.display()
                ))),
            }
        }
    }
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

fn lock_path(root: &camino::Utf8Path) -> PathBuf {
    state_lock_dir().join(lock_file_name(root))
}

fn fallback_lock_path(root: &camino::Utf8Path) -> PathBuf {
    std::env::temp_dir()
        .join("cooldown")
        .join("locks")
        .join(lock_file_name(root))
}

fn state_lock_dir() -> PathBuf {
    if let Some(path) = env_path("XDG_STATE_HOME") {
        return path.join("cooldown").join("locks");
    }
    if let Some(home) = env_path("HOME") {
        return home
            .join(".local")
            .join("state")
            .join("cooldown")
            .join("locks");
    }
    std::env::temp_dir().join("cooldown").join("locks")
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn lock_file_name(root: &camino::Utf8Path) -> String {
    let root = std::fs::canonicalize(root)
        .ok()
        .and_then(|path| camino::Utf8PathBuf::from_path_buf(path).ok())
        .unwrap_or_else(|| root.to_owned());
    format!("{:016x}.lock", cooldown_core::fs::fnv1a_64(root.as_str()))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readers_can_share_project_access() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let _first = ProjectReadGuard::acquire(root).expect("first reader");

        ProjectReadGuard::acquire(root).expect("second reader");
    }

    #[test]
    fn reader_and_writer_exclude_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let _reader = ProjectReadGuard::acquire(root).expect("reader");

        let error = ProjectWriteGuard::acquire(root).expect_err("writer must fail");
        assert!(matches!(error, CoreError::LockConflict(_)));
    }

    #[test]
    fn writer_excludes_readers_and_writers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let _writer = ProjectWriteGuard::acquire(root).expect("writer");

        assert!(matches!(
            ProjectReadGuard::acquire(root).expect_err("reader must fail"),
            CoreError::LockConflict(_)
        ));
        assert!(matches!(
            ProjectWriteGuard::acquire(root).expect_err("second writer must fail"),
            CoreError::LockConflict(_)
        ));
    }

    #[test]
    fn write_access_can_be_reacquired_after_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");

        {
            let _guard = ProjectWriteGuard::acquire(root).expect("first writer");
        }

        ProjectWriteGuard::acquire(root).expect("writer reacquired");
    }

    #[test]
    fn project_access_does_not_create_repo_local_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let repo_local = root.join(".cooldown.lock");

        let _guard = ProjectWriteGuard::acquire(root).expect("writer acquired");

        assert!(!repo_local.exists());
        assert_ne!(
            ProjectWriteGuard::path_for_test(root),
            repo_local.as_std_path()
        );
    }
}
