use crate::app::AdapterSet;
use crate::cli::GlobalArgs;
use crate::discovery;
use camino::Utf8PathBuf;
use cooldown_cargo::CargoTool;
use cooldown_conda::{CondaTool, PixiTool};
use cooldown_core::config::ScanConfig;
use cooldown_core::{CoreError, Project, ProjectDetection, ToolId, ToolRead};
use cooldown_go::GoTool;
use cooldown_hex::HexTool;
use cooldown_maven::{GradleTool, MavenTool};
use cooldown_npm::{BunTool, DenoTool, NpmCliTool, PnpmTool, YarnTool};
use cooldown_pip::{PipTool, PoetryTool};
use cooldown_registry::{HttpOptions, SharedHttp};
use cooldown_rubygems::BundlerTool;
use cooldown_swift::SwiftTool;
use cooldown_uv::UvTool;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

pub(super) fn workdir(global: &GlobalArgs) -> Result<Utf8PathBuf, CoreError> {
    let dir = match &global.dir {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(CoreError::from)?)
            .map_err(|_| CoreError::PathEncoding("current dir is not valid UTF-8".into()))?
            .join(dir),
        None => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(CoreError::from)?)
            .map_err(|_| CoreError::PathEncoding("current dir is not valid UTF-8".into()))?,
    };
    std::fs::canonicalize(&dir)
        .map_err(CoreError::from)
        .and_then(|path| {
            Utf8PathBuf::from_path_buf(path)
                .map_err(|_| CoreError::PathEncoding(format!("{dir} is not valid UTF-8")))
        })
}

/// `revalidate_npm_listings` is set for version-adopting commands that honor the dist-tag ceiling:
/// the npm-family `latest` dist-tag is mutable, so the ceiling must be judged against the
/// registry's current state, not a listing-TTL-stale cached copy (a maintainer's downward retag
/// within the hour must hold, not authorize, the adoption).
/// A run that ignores the tag reads it
/// never, and pays nothing for its freshness.
pub(super) fn adapter_set(
    offline: bool,
    fresh: bool,
    concurrency: usize,
    revalidate_npm_listings: bool,
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
    adapters.register_target_verified_mutator(Arc::new(GoTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(CargoTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(UvTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(
        NpmCliTool::from_http(http.clone()).with_listing_revalidation(revalidate_npm_listings),
    ))?;
    adapters.register_target_verified_mutator(Arc::new(
        PnpmTool::from_http(http.clone()).with_listing_revalidation(revalidate_npm_listings),
    ))?;
    adapters.register_target_verified_mutator(Arc::new(
        YarnTool::from_http(http.clone()).with_listing_revalidation(revalidate_npm_listings),
    ))?;
    adapters.register_target_verified_mutator(Arc::new(
        BunTool::from_http(http.clone()).with_listing_revalidation(revalidate_npm_listings),
    ))?;
    // Deno applies no dist-tag ceiling (`has_dist_tags` is false on that adapter), so it has
    // nothing to keep fresh — it stays on the cached listing path.
    adapters.register_target_verified_mutator(Arc::new(DenoTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(BundlerTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(HexTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(MavenTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(GradleTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(PipTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(PoetryTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(CondaTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(PixiTool::from_http(http.clone())))?;
    adapters.register_target_verified_mutator(Arc::new(SwiftTool::from_http(http.clone())))?;
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
    struct PendingDetection<'a> {
        adapter: &'a dyn ToolRead,
        id: ToolId,
        detection: ProjectDetection,
        exclude: Vec<String>,
    }

    let selected = adapters
        .readers()
        .filter_map(|adapter| {
            let id = adapter.id();
            // `--tool`/`--cargo` restrict *detection itself*: an unselected tool is never walked
            // or enumerated, so a polyglot monorepo doesn't pay for (or hang on) its discovery.
            if !tools.is_empty() && !tools.contains(&id) {
                tracing::debug!(tool = id.as_str(), "skipping detection (filtered out)");
                return None;
            }
            Some(PendingDetection {
                adapter: adapter.as_ref(),
                id,
                detection: adapter.project_detection(),
                exclude: scan.exclude_folders_for(exclude_folders_base, id.as_str()),
            })
        })
        .collect::<Vec<_>>();
    let mut groups = BTreeMap::<Vec<String>, Vec<usize>>::new();
    for (index, pending) in selected.iter().enumerate() {
        groups
            .entry(pending.exclude.clone())
            .or_default()
            .push(index);
    }
    let mut found_by_adapter = vec![None; selected.len()];
    for (exclude, indices) in groups {
        let detections = indices
            .iter()
            .filter_map(|index| selected.get(*index).map(|pending| pending.detection))
            .collect::<Vec<_>>();
        let found = crate::scan::find_project_marker_dirs_batch(
            workdir,
            &detections,
            respect_gitignore,
            &exclude,
        )?;
        for (index, found) in indices.into_iter().zip(found) {
            let slot = found_by_adapter.get_mut(index).ok_or_else(|| {
                CoreError::System("project discovery adapter index was invalid".to_string())
            })?;
            *slot = Some(found);
        }
    }

    let mut projects = Vec::new();
    for (pending, found) in selected.into_iter().zip(found_by_adapter) {
        // The orchestrator owns the scan: the adapter only declares its markers, and we apply the
        // shared gitignore/exclude policy here so a leaf crate can't diverge from it.
        let marker = pending.detection.primary();
        let mut found = found.ok_or_else(|| {
            CoreError::System(format!(
                "project discovery produced no result for {}",
                pending.id.as_str()
            ))
        })?;
        // The topmost-only rule assumed every nested lockfile root is covered by the workspace
        // above it; give the adapter its appeal for the ones that are not (a nested workspace
        // root the enclosing workspace can only exclude, never own).
        promote_nested(&mut found, |dir| {
            pending.adapter.nested_lockfile_root_escapes(dir)
        });
        let validation_only = validation_roots_outside_primary(&found);
        pending
            .adapter
            .validate_manifests_without_lock(&validation_only)?;
        let dirs = found.primary;
        tracing::info!(
            tool = pending.id.as_str(),
            projects = dirs.len(),
            gitignore = respect_gitignore,
            "detected projects"
        );
        for dir in dirs {
            tracing::debug!(tool = pending.id.as_str(), root = %dir, "detected project");
            let manifest = marker_manifest_path(&dir, &marker);
            projects.push((
                pending.id,
                Project {
                    manifest,
                    root: dir,
                    kind: pending.id,
                    exclude_newer: None,
                },
            ));
        }
    }
    Ok(projects)
}

/// Move every nested lockfile root that `escapes` says stands on its own into the primary set.
///
/// Escape is a property of the directory alone — a workspace root escapes any enclosure, however
/// deep — so each candidate is judged independently rather than against its nearest kept ancestor.
fn promote_nested(
    found: &mut crate::scan::ProjectMarkerDirs,
    escapes: impl Fn(&camino::Utf8Path) -> bool,
) {
    let promoted = found
        .nested
        .iter()
        .filter(|dir| escapes(dir))
        .cloned()
        .collect::<Vec<_>>();
    if promoted.is_empty() {
        return;
    }
    found.nested.retain(|dir| !promoted.contains(dir));
    found.primary.extend(promoted);
    found.primary.sort();
    found.primary.dedup();
    found
        .validation_only
        .retain(|candidate| found.primary.binary_search(candidate).is_err());
}

fn validation_roots_outside_primary(found: &crate::scan::ProjectMarkerDirs) -> Vec<Utf8PathBuf> {
    found
        .validation_only
        .iter()
        .filter(|candidate| {
            !found
                .primary
                .iter()
                .any(|primary| candidate.starts_with(primary))
        })
        .cloned()
        .collect()
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

    #[test]
    fn promotion_moves_escaping_nested_roots_into_primary() {
        let root = Utf8PathBuf::from("/repo");
        let escaping = root.join("incubator");
        let owned = root.join("member");
        let mut found = crate::scan::ProjectMarkerDirs {
            primary: vec![root.clone()],
            validation_only: vec![escaping.clone(), owned.clone()],
            nested: vec![escaping.clone(), owned.clone()],
        };

        promote_nested(&mut found, |dir| dir == escaping);

        assert_eq!(found.primary, vec![root, escaping]);
        assert_eq!(found.nested, vec![owned.clone()]);
        assert_eq!(found.validation_only, vec![owned]);
    }

    #[test]
    fn promotion_without_escaping_roots_changes_nothing() {
        let root = Utf8PathBuf::from("/repo");
        let mut found = crate::scan::ProjectMarkerDirs {
            primary: vec![root.clone()],
            validation_only: vec![root.join("member")],
            nested: vec![root.join("member")],
        };
        let unchanged = found.clone();

        promote_nested(&mut found, |_| false);

        assert_eq!(found, unchanged);
    }

    #[test]
    fn validation_skips_manifests_owned_by_a_detected_workspace() {
        let root = Utf8PathBuf::from("/repo");
        let sibling = Utf8PathBuf::from("/other");
        let found = crate::scan::ProjectMarkerDirs {
            primary: vec![root.clone()],
            validation_only: vec![root.clone(), root.join("member"), sibling.clone()],
            nested: vec![],
        };

        assert_eq!(validation_roots_outside_primary(&found), vec![sibling]);
    }
}
