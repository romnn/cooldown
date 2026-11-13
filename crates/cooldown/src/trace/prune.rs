use super::paths;
use cooldown_core::CoreError;
use serde::Serialize;
use std::path::Path;
use std::time::{Duration, SystemTime};

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Pruned {
    pub(crate) traces: usize,
    pub(crate) trace_bytes: u64,
    pub(crate) active_traces: usize,
    pub(crate) cache: cooldown_registry::cache::PrunedCache,
    pub(crate) dry_run: bool,
    pub(crate) yarn: cooldown_registry::cache::PrunedCache,
}

pub(crate) fn run(
    older_than: &str,
    dry_run: bool,
    override_directory: Option<&Path>,
) -> Result<Pruned, CoreError> {
    let age = cooldown_core::duration::parse_duration(older_than)?;
    let seconds = u64::try_from(age.as_secs())
        .map_err(|_| CoreError::Config("--older-than cannot be negative".into()))?;
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(seconds))
        .ok_or_else(|| {
            CoreError::Config("--older-than is outside the supported time range".into())
        })?;
    let mut result = Pruned {
        dry_run,
        ..Pruned::default()
    };
    prune_traces(
        &override_directory.map_or_else(paths::directory, |path| Ok(path.to_owned()))?,
        cutoff,
        dry_run,
        &mut result,
    )?;
    result.cache =
        cooldown_registry::cache::prune(&crate::discovery::cache_dir()?, cutoff, dry_run)?;
    result.yarn = cooldown_npm::prune_yarn_cache(&crate::discovery::cache_dir()?, cutoff, dry_run)?;
    Ok(result)
}

fn prune_traces(
    directory: &Path,
    cutoff: SystemTime,
    dry_run: bool,
    result: &mut Pruned,
) -> Result<(), CoreError> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
        {
            continue;
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)?;
        match super::lock::try_lock(&file) {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                result.active_traces += 1;
                continue;
            }
            Err(std::fs::TryLockError::Error(err)) => return Err(err.into()),
        }
        let metadata = file.metadata()?;
        if !metadata.modified().is_ok_and(|modified| modified < cutoff) {
            continue;
        }
        let Some(header) = super::Header::read(&file)? else {
            continue;
        };
        if !header.owned() {
            continue;
        }
        if same_file::Handle::from_file(file.try_clone()?)? != same_file::Handle::from_path(&path)?
        {
            continue;
        }
        if !dry_run {
            std::fs::remove_file(&path)?;
        }
        result.traces += 1;
        result.trace_bytes = result.trace_bytes.saturating_add(metadata.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;

    #[test]
    fn pruning_keeps_active_and_unowned_files_and_honors_dry_run() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let recorder = crate::trace::Recorder::in_directory(
            directory.path(),
            Path::new("project"),
            crate::trace::TraceCommand::Upgrade,
        )?;
        let header = serde_json::value::to_raw_value(&serde_json::json!({"kind": "run_started"}))?;
        recorder.record(&header, &[]);
        let path = recorder.path.clone();
        let unrelated = directory.path().join("trace-unrelated.jsonl");
        std::fs::write(&unrelated, "{}\n")?;
        let cutoff = SystemTime::now() + Duration::from_secs(60);
        let mut result = Pruned::default();
        prune_traces(directory.path(), cutoff, false, &mut result)?;
        assert_eq!(result.active_traces, 1);
        assert_eq!(result.traces, 0);
        drop(recorder);
        let renamed = directory.path().join("a-different-filename-format.jsonl");
        std::fs::rename(&path, &renamed)?;
        let path = renamed;
        let mut result = Pruned::default();
        prune_traces(directory.path(), cutoff, true, &mut result)?;
        assert_eq!(result.traces, 1);
        assert!(path.exists());
        let mut result = Pruned::default();
        prune_traces(directory.path(), SystemTime::UNIX_EPOCH, false, &mut result)?;
        assert_eq!(result.traces, 0);
        prune_traces(directory.path(), cutoff, false, &mut result)?;
        assert_eq!(result.traces, 1);
        assert!(!path.exists());
        assert!(unrelated.exists());
        Ok(())
    }
}
