//! Filesystem helpers shared across the workspace: crash-safe file writes and stable file-name
//! hashing. Both exist so trust-bearing state files (the publish-time floor, HTTP cache entries,
//! the committed baseline) and per-path lock/cache names are produced the same way everywhere.

use crate::error::CoreError;
use std::io::Write;
use std::path::Path;

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
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| CoreError::PathEncoding(format!("non-utf8 path: {}", path.display())))?;

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
        if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
            let _ = std::fs::remove_file(&tmp);
            return Err(error.into());
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
    use super::atomic_write;

    #[test]
    fn atomic_write_writes_exact_bytes_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");

        atomic_write(&path, b"first").unwrap();
        atomic_write(&path, b"second").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"second");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }
}
