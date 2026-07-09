//! A per-project filesystem lock so concurrent `cooldown` mutating runs cannot overlap on the same
//! project state.

use cooldown_core::CoreError;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Holds an OS-backed exclusive lock for a project root for the lifetime of the value.
#[derive(Debug)]
pub struct ProjectLock {
    file: File,
}

impl ProjectLock {
    /// Acquire this project's state-dir lock, failing immediately if another process already holds it.
    pub fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let preferred = lock_path(root);
        let (path, mut file) = match open_lock_file(&preferred) {
            Ok(file) => (preferred, file),
            Err(preferred_error) => {
                let fallback = fallback_lock_path(root);
                match open_lock_file(&fallback) {
                    Ok(file) => {
                        tracing::debug!(
                            preferred = %preferred.display(),
                            error = %preferred_error,
                            "state-dir lock unavailable; using temp-dir fallback"
                        );
                        (fallback, file)
                    }
                    Err(fallback_error) => {
                        return Err(CoreError::Filesystem(format!(
                            "cannot open a lock file: {} ({preferred_error}); fallback {} ({fallback_error})",
                            preferred.display(),
                            fallback.display()
                        )));
                    }
                }
            }
        };
        let file_path = path.display().to_string();
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                // Best-effort: name the holder recorded in the lock file, so "who is blocking me?"
                // (and any hash-collision surprise between two roots) is self-diagnosing.
                let holder = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|contents| contents.lines().next().map(str::to_string))
                    .filter(|line| !line.is_empty())
                    .map(|line| format!(" ({line})"))
                    .unwrap_or_default();
                return Err(CoreError::LockConflict(format!(
                    "{file_path} is locked by another mutating cooldown run{holder}"
                )));
            }
            Err(TryLockError::Error(e)) => {
                return Err(CoreError::Filesystem(format!("{file_path}: {e}")));
            }
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
        Ok(ProjectLock { file })
    }
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

impl ProjectLock {
    #[cfg(test)]
    fn path_for_test(root: &camino::Utf8Path) -> PathBuf {
        lock_path(root)
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_first_is_held() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let _first = ProjectLock::acquire(root).expect("first lock");

        let err = ProjectLock::acquire(root).expect_err("second lock must fail");
        assert!(matches!(err, CoreError::LockConflict(_)));
    }

    #[test]
    fn lock_can_be_reacquired_after_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");

        {
            let _lock = ProjectLock::acquire(root).expect("first lock");
        }

        ProjectLock::acquire(root).expect("lock reacquired");
    }

    #[test]
    fn project_lock_does_not_create_repo_local_lock_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8Path::from_path(dir.path()).expect("utf8 path");
        let repo_local = root.join(".cooldown.lock");

        let _lock = ProjectLock::acquire(root).expect("lock acquired");

        assert!(!repo_local.exists());
        assert_ne!(ProjectLock::path_for_test(root), repo_local.as_std_path());
    }
}
