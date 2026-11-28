//! The conda-lock [`Tool`](cooldown_core::Tool).
//! The lockfile pins a mix of conda-channel and PyPI packages, so — like the
//! Deno adapter — this tool carries two registry clients and dispatches each
//! dependency's publish-time lookup to the one named on its [`PackageId`].
//! Versions use the PEP 440 model reused from [`cooldown_uv`] (a close
//! approximation of conda's own ordering for the common `X.Y.Z` shapes).

use crate::lock;
use crate::registry::{CONDA, Conda};
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    Driver, RegistryVersionClassifier, build_registry_releases, skipped_on_apply_error,
    verify_current_unknown,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    LockVerifyReport, NativePolicyLayer, PackageId, PackageRegistry, Plan, PreparedMutation,
    Project, ProjectMarker, ProjectMutationJournal, RawRelease, Release, ReleaseFetcher,
    ReleaseOrder, ReleaseQuality, Result, ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport,
    Version,
};
use cooldown_registry::SharedHttp;
use cooldown_uv::PyPi;
use cooldown_uv::pypi::PYPI;
use cooldown_uv::version;
use std::collections::HashSet;

/// The [`ToolId`] for the conda-lock adapter (`"conda"`).
const CONDA_ID: ToolId = ToolId("conda");

/// The lock conda-lock writes, which is the project marker and the resolved graph.
const LOCKFILE: &str = "conda-lock.yml";

/// The conda-lock implementation of the [`Tool`](cooldown_core::Tool) port.
pub struct CondaTool {
    conda: Conda,
    pypi: PyPi,
    driver: Driver,
}

impl CondaTool {
    /// Creates the adapter from the conda and PyPI registry clients.
    #[must_use]
    pub fn new(conda: Conda, pypi: PyPi) -> Self {
        CondaTool {
            conda,
            pypi,
            driver: Driver::new("conda"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building both registry clients.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        CondaTool::new(Conda::new(http.clone()), PyPi::new(http))
    }

    async fn raw_releases(&self, dep: &Dependency) -> Result<Vec<RawRelease>> {
        if dep.package.registry.as_deref() == Some(CONDA) {
            self.conda.releases(&dep.package).await
        } else {
            self.pypi.releases(&dep.package).await
        }
    }
}

/// The package names the project's source manifest (`environment.yml`) declares, so the resolved
/// lock can be split direct vs. transitive — the lock itself does not say.
/// `None` when no manifest is found, so the caller treats every locked package as direct.
fn direct_names(root: &Utf8Path) -> Option<HashSet<String>> {
    let content = std::fs::read_to_string(root.join("environment.yml"))
        .or_else(|_| std::fs::read_to_string(root.join("environment.yaml")))
        .ok()?;
    Some(lock::environment_yml_direct(&content))
}

pub(crate) fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

pub(crate) fn build_releases(current: &str, raw: Vec<RawRelease>) -> Vec<Release> {
    build_registry_releases(
        current,
        raw,
        RegistryVersionClassifier {
            is_valid: |value| version::parse(value).is_some(),
            compare: version::compare,
            major_key: version::major_key,
            major_number: version::major_number,
            classify_kind: version::classify_kind,
            classify_quality,
        },
    )
}

#[async_trait]
impl ToolRead for CondaTool {
    fn version_command(&self) -> Option<cooldown_core::trace::ToolVersionCommand> {
        Some(cooldown_core::trace::ToolVersionCommand {
            program: self.driver.program().to_string(),
            arguments: vec!["--version".into()],
        })
    }

    fn evidence(&self, project: &Project) -> Result<cooldown_core::trace::Evidence> {
        use cooldown_core::trace::GraphCoverage;
        let mut evidence = cooldown_adapter_util::evidence::native(
            project,
            LOCKFILE,
            GraphCoverage::Inventory {
                reason: "native lock retains its original platform-specific package evidence"
                    .into(),
            },
            &[
                "environment.yml",
                "environment.yaml",
                "pyproject.toml",
                ".condarc",
            ],
        );
        cooldown_adapter_util::evidence::ancestors(&mut evidence, &project.root, &[]);
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &["CONDARC", "CONDA_PREFIX", "CONDA_SUBDIR"],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        CONDA_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
            // Conda and PyPI packages share this graph, so no single OSV ecosystem is safe.
            advisory_ecosystem: None,
            honors_generated_members: false,
        }
    }

    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        ProjectMarker {
            marker: LOCKFILE,
            manifest: LOCKFILE,
            alternate_manifests: &[],
            workspace_root: false,
        }
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        // conda-world locks do not mark direct vs. transitive, so cross-reference the source manifest
        // (`environment.yml`): a package named there is direct, the rest transitive. The
        // split lets `--major` rewrite direct deps across a major while leaving transitives capped.
        // When no manifest is found, fall back to treating every resolved package as direct.
        let content = std::fs::read_to_string(project.root.join(LOCKFILE))?;
        let direct = direct_names(&project.root).filter(|names| !names.is_empty());
        let ceilings = lock::conda_lock_ceilings(&content);
        let mut deps = Vec::new();
        for entry in lock::parse_conda_lock(&content) {
            let normalized = lock::normalize_name(&entry.name);
            let is_direct = direct
                .as_ref()
                .is_none_or(|names| names.contains(&normalized));
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            // A requirer's exact `dependencies:` spec caps this package at its resolved
            // version: the ceiling is honored only when some pin equals it, so a fuzzy prefix spec
            // holds only when resolution landed exactly on it. The match is by string equality, not
            // `version::compare`: conda versions are routinely not PEP 440 (calver `2022g`, R-style
            // `r4.1.2`), and `compare` treats two unparsable strings as equal — that would falsely
            // cap a dep whose pin and resolved version merely both fail to parse. The ceilings come
            // from conda match-specs, so they apply only to conda packages: a same-named PyPI package
            // (both ecosystems coexist in one lock) must not be held by a conda constraint.
            let graph_ceiling = entry
                .conda
                .then(|| ceilings.get(&normalized))
                .flatten()
                .filter(|pins| pins.iter().any(|pin| pin == &entry.version))
                .map(|_| Version::new(entry.version.clone()));
            let registry = if entry.conda { CONDA } else { PYPI };
            deps.push(Dependency {
                package: PackageId::new(CONDA_ID, entry.name, Some(registry.to_string())),
                // No advisory-database ecosystem maps conda channels (see `capabilities`), so
                // no package carries an advisory identity.
                advisory_identity: None,
                current: Version::new(entry.version.clone()),
                current_quality: classify_quality(&entry.version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling,
                declared_bound: None,
                members: Vec::new(),
                pinned: false,
                hold_edges: Vec::new(),
            });
        }
        Ok(deps)
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<LockVerifyReport> {
        Ok(verify_current_unknown(LOCKFILE))
    }
}

#[async_trait]
impl ReleaseFetcher for CondaTool {
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
        let time = if dep.package.registry.as_deref() == Some(CONDA) {
            self.conda
                .published_at(&dep.package, &dep.current, &[])
                .await?
        } else {
            self.pypi
                .published_at(&dep.package, &dep.current, &[])
                .await?
        };
        Ok(Release {
            version: dep.current.clone(),
            order: ReleaseOrder(Vec::new()),
            major: version::major_key(dep.current.as_str()),
            major_number: version::major_number(dep.current.as_str()),
            kind_from_current: None,
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

#[async_trait]
impl ToolWrite for CondaTool {
    fn mutation_tool(&self) -> ToolId {
        CONDA_ID
    }

    // `conda install` writes the active prefix.
    fn mutation_installs(&self) -> bool {
        true
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(&project.root, [Utf8Path::new(LOCKFILE)])
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, _) = mutation.parts_for(self)?;
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            let args = [
                "install".to_owned(),
                "-y".to_owned(),
                format!("{}={}", change.package.name, change.to),
            ];
            match self.driver.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.driver
            .verify(&project.root, &["list".to_owned()], "build succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// conda's apply installs into the active prefix, so it takes the environment turn.
    #[test]
    fn the_conda_apply_installs() {
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        assert!(CondaTool::from_http(http).mutation_installs());
    }

    #[test]
    fn mixed_registry_graphs_have_no_single_osv_ecosystem() {
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        assert_eq!(
            CondaTool::from_http(http).capabilities().advisory_ecosystem,
            None
        );
    }
}
