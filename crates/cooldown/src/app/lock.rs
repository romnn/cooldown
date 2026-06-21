//! A per-project filesystem lock so concurrent `cooldown` mutating runs cannot overlap on the same
//! project state.

use cooldown_core::CoreError;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Seek, SeekFrom, Write};

/// Holds an OS-backed exclusive lock on `<root>/.cooldown.lock` for the lifetime of the value.
#[derive(Debug)]
pub struct ProjectLock {
    file: File,
}

impl ProjectLock {
    /// Acquire `<root>/.cooldown.lock`, failing immediately if another process already holds it.
    pub fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let path = root.join(".cooldown.lock");
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(CoreError::LockConflict(format!(
                    "{path} is locked by another mutating cooldown run"
                )));
            }
            Err(TryLockError::Error(e)) => {
                return Err(CoreError::Filesystem(format!("{path}: {e}")));
            }
        }

        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        let _ = writeln!(file, "locked by cooldown pid {}", std::process::id());
        let _ = file.sync_data();
        Ok(ProjectLock { file })
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
}
