use crate::app::AdapterSet;
use crate::cli::GlobalArgs;
use crate::discovery;
use camino::Utf8PathBuf;
use cooldown_cargo::CargoTool;
use cooldown_core::config::ScanConfig;
use cooldown_core::{CoreError, Project, ToolId};
use cooldown_go::GoTool;
use cooldown_registry::{HttpOptions, SharedHttp};
use cooldown_uv::UvTool;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn workdir(global: &GlobalArgs) -> Result<Utf8PathBuf, CoreError> {
    match &global.dir {
        Some(dir) => Ok(dir.clone()),
        None => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(CoreError::from)?)
            .map_err(|_| CoreError::PathEncoding("current dir is not valid UTF-8".into())),
    }
}

pub(super) fn adapter_set(offline: bool, fresh: bool) -> Result<AdapterSet, CoreError> {
    let http = SharedHttp::new(
        discovery::cache_dir().into_std_path_buf(),
        HttpOptions {
            offline,
            fresh,
            request_timeout: Duration::from_secs(30),
            ..Default::default()
        },
    )?;

    let mut adapters = AdapterSet::new();
    adapters.register(Arc::new(GoTool::from_http(http.clone())));
    adapters.register(Arc::new(CargoTool::from_http(http.clone())));
    adapters.register(Arc::new(UvTool::from_http(http.clone())));
    Ok(adapters)
}

pub(super) fn detect_projects(
    adapters: &AdapterSet,
    workdir: &camino::Utf8Path,
    scan: &ScanConfig,
    command_key: &str,
    tools: &[ToolId],
    respect_gitignore: bool,
) -> Result<Vec<(ToolId, Project)>, CoreError> {
    let mut projects = Vec::new();
    for adapter in adapters.readers() {
        let id = adapter.id();
        // `--tool`/`--cargo` restrict *detection itself*: an unselected tool is never walked
        // or enumerated, so a polyglot monorepo doesn't pay for (or hang on) Go/Python discovery.
        if !tools.is_empty() && !tools.contains(&id) {
            tracing::debug!(tool = id.as_str(), "skipping detection (filtered out)");
            continue;
        }
        // The orchestrator owns the scan: the adapter only declares its marker, and we apply the
        // shared gitignore/exclude policy here so a leaf crate can't diverge from it.
        let marker = adapter.project_marker();
        let exclude = scan.exclude_for(command_key, id.as_str());
        let dirs = cooldown_scan::find_marker_dirs(
            workdir,
            marker.lockfile,
            respect_gitignore,
            &exclude,
            marker.workspace_root,
        )?;
        tracing::info!(
            tool = id.as_str(),
            projects = dirs.len(),
            gitignore = respect_gitignore,
            "detected projects"
        );
        for dir in dirs {
            tracing::debug!(tool = id.as_str(), root = %dir, "detected project");
            projects.push((
                id,
                Project {
                    manifest: dir.join(marker.manifest),
                    root: dir,
                    kind: id,
                },
            ));
        }
    }
    Ok(projects)
}
