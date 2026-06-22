//! The Deno [`Tool`]: detection by `deno.lock`, the resolved graph read from that lock, and
//! publish times routed per dependency to the registry that owns it. Unlike npm/pnpm/yarn/bun, a
//! Deno project mixes two registries — `jsr:` specifiers resolve from [`JsrRegistry`] and `npm:`
//! specifiers from [`NpmRegistry`] — so this adapter carries both clients and dispatches on each
//! dependency's recorded registry. Both registries speak `SemVer`, so the version model is shared.

use crate::jsr::{JSR, JsrRegistry};
use crate::lock::split_name_version;
use crate::nodecmd::NodeCmd;
use crate::registry::{NPM, NpmRegistry};
use crate::tool::{build_releases, classify_quality};
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{skipped_on_apply_error, verify_current_report};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, Release, ReleaseFetcher, ReleaseOrder, Result, ToolId, ToolRead,
    ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use std::collections::HashSet;

/// The [`ToolId`] for the Deno adapter (`"deno"`).
pub const DENO_ID: ToolId = ToolId("deno");

/// The Deno implementation of the [`Tool`] port.
///
/// It reads `deno.lock` (the `workspace.dependencies` list for the direct set, the `jsr`/`npm`
/// maps for the resolved graph) and resolves each dependency's releases from the registry named on
/// its [`PackageId`]. Deno has no in-manifest cooldown field, so [`native_policy`] is always empty.
///
/// [`native_policy`]: ToolRead::native_policy
pub struct DenoTool {
    npm: NpmRegistry,
    jsr: JsrRegistry,
    cmd: NodeCmd,
}

impl DenoTool {
    /// Creates the adapter from the npm and JSR registry clients.
    #[must_use]
    pub fn new(npm: NpmRegistry, jsr: JsrRegistry) -> Self {
        DenoTool {
            npm,
            jsr,
            cmd: NodeCmd::new("deno"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building both registry clients.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        DenoTool::new(NpmRegistry::new(http.clone()), JsrRegistry::new(http))
    }

    /// Dispatches a release listing to the registry that owns `dep`: `jsr:` deps to JSR, everything
    /// else (the default) to npm.
    async fn raw_releases(&self, dep: &Dependency) -> Result<Vec<cooldown_core::RawRelease>> {
        if dep.package.registry.as_deref() == Some(JSR) {
            self.jsr.releases(&dep.package).await
        } else {
            self.npm.releases(&dep.package).await
        }
    }

    async fn locked_published_at(&self, dep: &Dependency) -> Result<Option<jiff::Timestamp>> {
        if dep.package.registry.as_deref() == Some(JSR) {
            self.jsr.published_at(&dep.package, &dep.current, &[]).await
        } else {
            self.npm.published_at(&dep.package, &dep.current, &[]).await
        }
    }
}

/// Splits a `deno.lock` specifier (`npm:lodash@4.17.15`, `jsr:@std/path@1.0.0`) into its registry,
/// package name, and requested version. An unknown scheme (e.g. an `https:` import) yields `None`,
/// so only registry-backed dependencies are surfaced.
fn split_specifier(spec: &str) -> Option<(&'static str, String, String)> {
    let (scheme, rest) = spec.split_once(':')?;
    let registry = match scheme {
        "npm" => NPM,
        "jsr" => JSR,
        _ => return None,
    };
    let (name, version) = split_name_version(rest)?;
    Some((registry, name, version))
}

impl DenoTool {
    fn read_deps(project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join("deno.lock"))?;
        let lock: serde_json::Value = serde_json::from_str(&content)
            .map_err(|e| cooldown_core::CoreError::Parse(format!("deno.lock: {e}")))?;

        // The workspace's declared specifiers are the direct set, keyed by (registry, name).
        let direct: HashSet<(&'static str, String)> = lock
            .pointer("/workspace/dependencies")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|spec| spec.as_str())
            .filter_map(|spec| split_specifier(spec).map(|(reg, name, _)| (reg, name)))
            .collect();

        let mut seen = HashSet::new();
        let mut deps = Vec::new();
        // The `jsr` and `npm` sections key every resolved package by its `name@version` identity.
        for registry in [JSR, NPM] {
            let Some(section) = lock.get(registry).and_then(|v| v.as_object()) else {
                continue;
            };
            for key in section.keys() {
                let Some((name, version)) = split_name_version(key) else {
                    continue;
                };
                let is_direct = direct.contains(&(registry, name.clone()));
                if scope == DepScope::Direct && !is_direct {
                    continue;
                }
                if !seen.insert((name.clone(), version.clone())) {
                    continue;
                }
                deps.push(Dependency {
                    package: PackageId::new(DENO_ID, name, Some(registry.to_string())),
                    current: Version::new(version.clone()),
                    current_quality: classify_quality(&version),
                    direct: is_direct,
                    artifacts: Vec::new(),
                    graph_floor: None,
                    graph_ceiling: None,
                    members: Vec::new(),
                    pinned: false,
                });
            }
        }
        Ok(deps)
    }
}

#[async_trait]
impl ToolRead for DenoTool {
    fn id(&self) -> ToolId {
        DENO_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        ProjectMarker {
            lockfile: "deno.lock",
            manifest: "deno.json",
            workspace_root: true,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        DenoTool::read_deps(project, scope)
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<VerifyReport> {
        Ok(verify_current_report(
            true,
            "lockfile taken as current",
            "lockfile is stale",
        ))
    }
}

#[async_trait]
impl ReleaseFetcher for DenoTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.raw_releases(dep).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self.locked_published_at(dep).await?;
        Ok(Release {
            version: dep.current.clone(),
            order: ReleaseOrder(Vec::new()),
            major: version::major_key(dep.current.as_str()),
            kind_from_current: None,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

#[async_trait]
impl ToolWrite for DenoTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        // Deno's manifest may be `deno.json` or `deno.jsonc`; capture both (an absent one is a
        // no-op to restore) plus the lock, since `deno add` rewrites the manifest and re-locks.
        Ok(ProjectMutationJournal {
            files: vec![
                ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("deno.json"))?,
                ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("deno.jsonc"))?,
                ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("deno.lock"))?,
            ],
        })
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            let scheme = if change.package.registry.as_deref() == Some(JSR) {
                "jsr"
            } else {
                "npm"
            };
            let spec = format!("{scheme}:{}@{}", change.package.name, change.to.as_str());
            match self.cmd.run(&project.root, &["add".into(), spec]).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cmd
            .verify(&project.root, &["install".into()], "deno install succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    #[test]
    fn splits_npm_and_jsr_specifiers() {
        assert_eq!(
            split_specifier("npm:lodash@4.17.15"),
            Some((NPM, "lodash".into(), "4.17.15".into()))
        );
        assert_eq!(
            split_specifier("jsr:@std/path@1.0.0"),
            Some((JSR, "@std/path".into(), "1.0.0".into()))
        );
        assert_eq!(split_specifier("https://example.com/mod.ts"), None);
    }

    #[test]
    fn reads_direct_and_graph_from_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = camino::Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        let lock_json = indoc! {r#"
            {
                "version": "5",
                "specifiers": {
                    "npm:lodash@4.17.15": "4.17.15",
                    "jsr:@std/path@1.0.0": "1.0.0"
                },
                "jsr": { "@std/path@1.0.0": { "integrity": "x" } },
                "npm": {
                    "lodash@4.17.15": { "integrity": "y" },
                    "ms@2.1.3": { "integrity": "z" }
                },
                "workspace": {
                    "dependencies": ["npm:lodash@4.17.15", "jsr:@std/path@1.0.0"]
                }
            }"#};
        std::fs::write(root.join("deno.lock"), lock_json).expect("write lock");
        let project = Project {
            root: root.clone(),
            kind: DENO_ID,
            manifest: root.join("deno.json"),
        };

        let mut direct = DenoTool::read_deps(&project, DepScope::Direct).expect("direct");
        direct.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        assert_eq!(direct.len(), 2);
        assert_eq!(direct[0].package.name, "@std/path");
        assert_eq!(direct[0].package.registry.as_deref(), Some(JSR));
        assert_eq!(direct[1].package.name, "lodash");
        assert_eq!(direct[1].package.registry.as_deref(), Some(NPM));

        let graph = DenoTool::read_deps(&project, DepScope::Graph).expect("graph");
        assert_eq!(graph.len(), 3); // + the transitive `ms`
        assert!(graph.iter().any(|d| d.package.name == "ms" && !d.direct));
    }
}
