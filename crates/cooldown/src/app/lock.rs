//! A per-project advisory file lock so a concurrent `cargo`/`go`/`uv` (or another `cooldown`) can't
//! corrupt a lockfile while a mutating command is applying changes.

use cooldown_core::CoreError;
use std::fs::OpenOptions;
use std::io::Write;

/// Holds an advisory lock file for the lifetime of the value; best-effort, removed on drop.
pub struct ProjectLock {
    path: camino::Utf8PathBuf,
}

impl ProjectLock {
    /// Acquire `<root>/.cooldown.lock`. Fails if it already exists (another mutator is running).
    pub fn acquire(root: &camino::Utf8Path) -> Result<Self, CoreError> {
        let path = root.join(".cooldown.lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut f) => {
                let _ = writeln!(f, "locked by cooldown pid {}", std::process::id());
                Ok(ProjectLock { path })
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(CoreError::Io(format!(
                "{path} exists; another mutating cooldown/package-manager run may be in progress (remove it if stale)"
            ))),
            Err(e) => Err(CoreError::Io(format!("{path}: {e}"))),
        }
    }
}

impl Drop for ProjectLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
