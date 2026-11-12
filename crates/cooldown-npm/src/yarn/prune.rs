use cooldown_core::CoreError;
use cooldown_registry::cache::{PrunedCache, prune_lease};
use std::path::Path;
use std::time::SystemTime;

/// Removes abandoned verification copies, excluding every active registry-backed adapter.
///
/// Only cooldown's scratch-directory prefix below its dedicated cache namespace is eligible.
///
/// # Errors
///
/// Returns filesystem errors inspecting or deleting an eligible copy.
pub fn prune_cache(
    cache_dir: &Path,
    cutoff: SystemTime,
    dry_run: bool,
) -> Result<PrunedCache, CoreError> {
    let directory = cache_dir.join("yarn-verify");
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Ok(PrunedCache::default()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(PrunedCache::default()),
        Err(err) => return Err(err.into()),
    }
    let Some(_lease) = prune_lease(cache_dir)? else {
        return Ok(PrunedCache {
            busy: true,
            ..PrunedCache::default()
        });
    };
    let mut result = PrunedCache::default();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir()
            || !entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with("cooldown-yarn-"))
            || !entry
                .metadata()?
                .modified()
                .is_ok_and(|modified| modified < cutoff)
        {
            continue;
        }
        let mut pending = vec![entry.path()];
        let mut files = 0;
        let mut bytes = 0_u64;
        while let Some(directory) = pending.pop() {
            for child in std::fs::read_dir(directory)? {
                let child = child?;
                if child.file_type()?.is_dir() {
                    pending.push(child.path());
                } else if child.file_type()?.is_file() {
                    files += 1;
                    bytes = bytes.saturating_add(child.metadata()?.len());
                }
            }
        }
        if !dry_run {
            std::fs::remove_dir_all(entry.path())?;
        }
        result.files += files;
        result.bytes = result.bytes.saturating_add(bytes);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_skips_active_clients_and_unrelated_directories() -> Result<(), CoreError> {
        let directory = tempfile::tempdir()?;
        let cache = directory.path();
        let scratch = cache.join("yarn-verify").join("cooldown-yarn-abandoned");
        let unrelated = cache.join("yarn-verify").join("unrelated");
        std::fs::create_dir_all(&scratch)?;
        std::fs::create_dir_all(&unrelated)?;
        std::fs::write(scratch.join("input"), "original bytes")?;
        let cutoff = SystemTime::now() + std::time::Duration::from_secs(60);
        let client =
            cooldown_registry::SharedHttp::new(cache, cooldown_registry::HttpOptions::default())?;
        assert!(prune_cache(cache, cutoff, false)?.busy);
        drop(client);
        assert_eq!(prune_cache(cache, cutoff, true)?.files, 1);
        assert!(scratch.exists());
        assert_eq!(prune_cache(cache, cutoff, false)?.files, 1);
        assert!(!scratch.exists());
        assert!(unrelated.exists());
        Ok(())
    }
}
