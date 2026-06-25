use crate::app::AdapterSet;
use crate::cli::GlobalArgs;
use crate::discovery;
use crate::scan::find_marker_dirs;
use camino::Utf8PathBuf;
use cooldown_cargo::CargoTool;
use cooldown_conda::{CondaTool, PixiTool};
use cooldown_core::config::ScanConfig;
use cooldown_core::{CoreError, Project, ToolId};
use cooldown_go::GoTool;
use cooldown_hex::HexTool;
use cooldown_maven::{GradleTool, MavenTool};
use cooldown_npm::{BunTool, DenoTool, NpmCliTool, PnpmTool, YarnTool};
use cooldown_pip::{PipTool, PoetryTool};
use cooldown_registry::{HttpOptions, SharedHttp};
use cooldown_rubygems::BundlerTool;
use cooldown_swift::SwiftTool;
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

pub(super) fn adapter_set(
    offline: bool,
    fresh: bool,
    concurrency: usize,
) -> Result<AdapterSet, CoreError> {
    let http = SharedHttp::new(
        discovery::cache_dir().into_std_path_buf(),
        HttpOptions {
            offline,
            fresh,
            // The resolve knob caps both the fan-out width and the per-host in-flight requests, so
            // raising `--concurrency` actually widens the registry fetch (the per-host semaphore,
            // not the fan-out, is otherwise the binding cap since every dep of one tool hits one host).
            per_host_concurrency: concurrency.max(1),
            request_timeout: Duration::from_secs(30),
            ..Default::default()
        },
    )?;

    let mut adapters = AdapterSet::new();
    adapters.register_target_verified_mutator(Arc::new(GoTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(CargoTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(UvTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(NpmCliTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(PnpmTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(YarnTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(BunTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(DenoTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(BundlerTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(HexTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(MavenTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(GradleTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(PipTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(PoetryTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(CondaTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(PixiTool::from_http(http.clone())));
    adapters.register_target_verified_mutator(Arc::new(SwiftTool::from_http(http.clone())));
    Ok(adapters)
}

pub(super) fn detect_projects(
    adapters: &AdapterSet,
    workdir: &camino::Utf8Path,
    scan: &ScanConfig,
    exclude_folders_base: &[String],
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
        let exclude = scan.exclude_folders_for(exclude_folders_base, id.as_str());
        let dirs = find_marker_dirs(
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
            let manifest = marker_manifest_path(&dir, &marker);
            projects.push((
                id,
                Project {
                    manifest,
                    root: dir,
                    kind: id,
                    exclude_newer: None,
                },
            ));
        }
    }
    Ok(projects)
}

fn marker_manifest_path(
    dir: &camino::Utf8Path,
    marker: &cooldown_core::ProjectMarker,
) -> Utf8PathBuf {
    std::iter::once(marker.manifest)
        .chain(marker.alternate_manifests.iter().copied())
        .find_map(|name| {
            let path = dir.join(name);
            path.exists().then_some(path)
        })
        .unwrap_or_else(|| dir.join(marker.manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::ProjectMarker;

    #[test]
    fn marker_manifest_path_uses_existing_alternate_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let root = camino::Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::write(root.join("deno.jsonc"), "{}").unwrap();

        let marker = ProjectMarker {
            lockfile: "deno.lock",
            manifest: "deno.json",
            alternate_manifests: &["deno.jsonc"],
            workspace_root: true,
        };

        assert_eq!(marker_manifest_path(root, &marker), root.join("deno.jsonc"));
    }
}
