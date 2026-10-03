use crate::app::{AdapterSet, CoveredDir};
use crate::cli::GlobalArgs;
use crate::discovery;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_cargo::CargoTool;
use cooldown_conda::{CondaTool, PixiTool};
use cooldown_core::config::{CommandConfig, ScanConfig};
use cooldown_core::{CoreError, NestedOwnership, Project, ProjectMarker, ToolId, ToolRead};
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
    let canonical = std::fs::canonicalize(&dir)
        .map_err(|error| CoreError::Config(format!("--dir {dir}: {error}")))
        .and_then(|path| {
            Utf8PathBuf::from_path_buf(path)
                .map_err(|_| CoreError::PathEncoding(format!("{dir} is not valid UTF-8")))
        })?;
    // A file canonicalizes fine, but selecting one would scope the run to nothing and pass.
    if !canonical.is_dir() {
        return Err(CoreError::Config(format!("--dir {dir} is not a directory")));
    }
    Ok(canonical)
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
) -> Result<(AdapterSet, SharedHttp), CoreError> {
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
    // Returned beside the adapters so the advisory feed (OSV) can share the same client — one
    // cache, one per-host budget, the same offline/fresh modes.
    Ok((adapters, http))
}

/// Detects every project under `workdir` for the selected tools.
///
/// `selected_dir` is the directory an explicit `-C`/`--dir` (or the invocation's own working
/// directory) named below the scan root, when there is one: the `exclude-folders` globs never
/// prune it or the ancestors leading to it, so a run pointed at an excluded subtree still finds
/// the projects there.
/// `cfg` is the run's resolved command config, CLI overrides applied: its `exclude-folders` list
/// is the base every tool's `[tool.*]` list adds to, and its `include-hidden` list names the
/// dot-directories every tool's walk enters (see
/// [`WalkPolicy::include_hidden`](crate::scan::WalkPolicy::include_hidden)).
pub(super) fn detect_projects(
    adapters: &AdapterSet,
    workdir: &camino::Utf8Path,
    selected_dir: Option<&camino::Utf8Path>,
    scan: &ScanConfig,
    cfg: &CommandConfig,
    tools: &[ToolId],
    respect_gitignore: bool,
) -> Result<Detected, CoreError> {
    struct PendingDetection<'a> {
        adapter: &'a dyn ToolRead,
        id: ToolId,
        marker: ProjectMarker,
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
                marker: adapter.project_marker(),
                exclude: scan.exclude_folders_for(cfg.exclude_folders.patterns(), id.as_str()),
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
        let markers = indices
            .iter()
            .filter_map(|index| selected.get(*index).map(|pending| pending.marker))
            .collect::<Vec<_>>();
        let found = crate::scan::find_project_marker_dirs_batch(
            workdir,
            &markers,
            crate::scan::WalkPolicy {
                respect_gitignore,
                exclude: &exclude,
                include_hidden: cfg.include_hidden.patterns(),
                selected: selected_dir,
            },
        )?;
        for (index, found) in indices.into_iter().zip(found) {
            let slot = found_by_adapter.get_mut(index).ok_or_else(|| {
                CoreError::System("project discovery adapter index was invalid".to_string())
            })?;
            *slot = Some(found);
        }
    }

    let mut projects = Vec::new();
    let mut covered = Vec::new();
    for (pending, found) in selected.into_iter().zip(found_by_adapter) {
        // The orchestrator owns the scan: the adapter only declares its markers, and we apply the
        // shared gitignore/exclude policy here so a leaf crate can't diverge from it.
        let marker = pending.marker;
        let mut found = found.ok_or_else(|| {
            CoreError::System(format!(
                "project discovery produced no result for {}",
                pending.id.as_str()
            ))
        })?;
        // The topmost-only rule assumed every nested marked directory is covered by the root
        // above it; ask the adapter which of them a project this run evaluates actually resolves,
        // handing it both sides of the scan so it can only answer from projects that exist.
        let ownership = pending
            .adapter
            .nested_ownership(&found.primary, &found.nested);
        // A directory with a named resolver is where a `-C` into it has to run: the selection
        // belongs to the project whose lock holds that directory's dependencies. Collected before
        // promotion, which consumes the nested list.
        covered.extend(found.nested.iter().zip(&ownership).filter_map(
            |(dir, answer)| match answer {
                cooldown_core::NestedOwnership::Root(root) => Some(CoveredDir {
                    tool: pending.id,
                    dir: dir.clone(),
                    root: root.clone(),
                }),
                cooldown_core::NestedOwnership::Enclosing
                | cooldown_core::NestedOwnership::Standalone => None,
            },
        ));
        promote_nested(&mut found, &ownership)?;
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
                    generated_members: cooldown_core::GeneratedMembers::undeclared(),
                },
            ));
        }
    }
    Ok(Detected { projects, covered })
}

/// What one detection pass produced: the projects to run, and the marked directories some project
/// resolves rather than being projects themselves.
pub(super) struct Detected {
    pub(super) projects: Vec<(ToolId, Project)>,
    pub(super) covered: Vec<CoveredDir>,
}

/// Move every nested marked directory no evaluated project resolves into the primary set.
///
/// The adapter answers from the two sets it was handed, so the decision here is only to apply the
/// answers and to hold the adapter to its contract: a [`NestedOwnership::Root`] must name a
/// directory this run evaluates — a detected root, or a nested directory the same batch answered
/// [`NestedOwnership::Standalone`]. A claim on anything else would leave the directory covered by a
/// resolve that never happens, which is exactly how a project passes the gate unevaluated, so it is
/// reported as the adapter bug it is rather than silently trusted.
///
/// # Errors
///
/// Returns [`CoreError::System`] if `ownership` is not index-aligned with the nested directories,
/// or if an answer names a project this run does not evaluate.
fn promote_nested(
    found: &mut crate::scan::ProjectMarkerDirs,
    ownership: &[NestedOwnership],
) -> Result<(), CoreError> {
    if ownership.len() != found.nested.len() {
        return Err(CoreError::System(format!(
            "project discovery returned {} nested-ownership answers for {} nested directories",
            ownership.len(),
            found.nested.len()
        )));
    }
    let evaluated =
        |root: &Utf8Path| {
            found.primary.iter().any(|primary| primary == root)
                || found.nested.iter().zip(ownership).any(|(nested, answer)| {
                    nested == root && *answer == NestedOwnership::Standalone
                })
        };
    for (dir, answer) in found.nested.iter().zip(ownership) {
        if let NestedOwnership::Root(root) = answer
            && !evaluated(root)
        {
            return Err(CoreError::System(format!(
                "project discovery claimed {dir} is resolved by {root}, which this run does not \
                 evaluate"
            )));
        }
    }
    let promoted = found
        .nested
        .iter()
        .zip(ownership)
        .filter(|(_, answer)| **answer == NestedOwnership::Standalone)
        .map(|(dir, _)| dir.clone())
        .collect::<Vec<_>>();
    if promoted.is_empty() {
        return Ok(());
    }
    found.nested.retain(|dir| !promoted.contains(dir));
    found.primary.extend(promoted);
    found.primary.sort();
    found.primary.dedup();
    Ok(())
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
            marker: "deno.lock",
            manifest: "deno.json",
            alternate_manifests: &["deno.jsonc"],
            workspace_root: true,
        };

        assert_eq!(marker_manifest_path(root, &marker), root.join("deno.jsonc"));
    }

    /// Only a directory no evaluated project resolves becomes a project of its own; a claimed
    /// project the run really evaluates keeps its directory where it is.
    #[test]
    fn promotion_keeps_what_an_evaluated_project_resolves() -> Result<(), CoreError> {
        let root = Utf8PathBuf::from("/repo");
        let silent = root.join("fixtures");
        let standalone = root.join("fuzz");
        let member = root.join("crates/app");
        let sibling = root.join("fuzz/helper");
        let mut found = crate::scan::ProjectMarkerDirs {
            primary: vec![root.clone()],
            nested: vec![
                member.clone(),
                silent.clone(),
                standalone.clone(),
                sibling.clone(),
            ],
        };

        promote_nested(
            &mut found,
            &[
                // Resolved by a root detection already holds.
                NestedOwnership::Root(root.clone()),
                NestedOwnership::Enclosing,
                NestedOwnership::Standalone,
                // Resolved by a directory this same batch promotes, which the run then evaluates.
                NestedOwnership::Root(standalone.clone()),
            ],
        )?;

        assert_eq!(
            found.primary,
            vec![root, standalone],
            "only the unresolved directory joins the detected roots"
        );
        assert_eq!(found.nested, vec![member, silent, sibling]);
        Ok(())
    }

    #[test]
    fn promotion_without_unresolved_directories_changes_nothing() -> Result<(), CoreError> {
        let root = Utf8PathBuf::from("/repo");
        let mut found = crate::scan::ProjectMarkerDirs {
            primary: vec![root.clone()],
            nested: vec![root.join("member")],
        };
        let unchanged = found.clone();

        promote_nested(&mut found, &[NestedOwnership::Enclosing])?;

        assert_eq!(found, unchanged);
        Ok(())
    }

    /// A claim on a project the run never evaluates would leave the directory covered by a resolve
    /// that never happens — the very shape the gate exists to catch — so it is an adapter bug, not
    /// a coverage answer to trust.
    #[test]
    fn promotion_rejects_a_claim_on_a_project_the_run_does_not_evaluate() {
        let root = Utf8PathBuf::from("/repo");
        let claimed = root.join("member");
        let mut found = crate::scan::ProjectMarkerDirs {
            primary: vec![root.clone()],
            nested: vec![claimed.clone(), root.join("pruned")],
        };

        let error = promote_nested(
            &mut found,
            &[
                // `pruned` is nested but answered `Enclosing`, so the run never evaluates it.
                NestedOwnership::Root(root.join("pruned")),
                NestedOwnership::Enclosing,
            ],
        )
        .expect_err("a claim on an unevaluated project must not be applied");

        std::assert_matches!(&error, CoreError::System(message)
            if message.contains(claimed.as_str()) && message.contains("does not"));
    }

    /// A misaligned answer could silently reassign ownership between directories, so it is a bug
    /// report rather than a guess.
    #[test]
    fn promotion_rejects_answers_that_do_not_match_the_directories() {
        let root = Utf8PathBuf::from("/repo");
        let mut found = crate::scan::ProjectMarkerDirs {
            primary: vec![root.clone()],
            nested: vec![root.join("a"), root.join("b")],
        };

        let error = promote_nested(&mut found, &[NestedOwnership::Standalone])
            .expect_err("a short answer list must not be applied");

        std::assert_matches!(error, CoreError::System(_));
    }
}
