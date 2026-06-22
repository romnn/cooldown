//! The generic conda-world [`Tool`], parameterised by a [`CondaLayout`] (conda-lock or pixi). Both
//! lockfiles pin a mix of conda-channel and PyPI packages, so — like the Deno adapter — this tool
//! carries two registry clients and dispatches each dependency's publish-time lookup to the one
//! named on its [`PackageId`]. Versions use the PEP 440 model reused from [`cooldown_uv`] (a close
//! approximation of conda's own ordering for the common `X.Y.Z` shapes).

use crate::lock::{self, CondaDep};
use crate::registry::{CONDA, Conda};
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    Driver, build_registry_releases, skipped_on_apply_error, verify_current_report,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality,
    Result, ToolId, ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use cooldown_uv::PyPi;
use cooldown_uv::pypi::PYPI;
use cooldown_uv::version;
use std::collections::HashSet;
use std::marker::PhantomData;

/// The per-tool knobs the generic adapter needs: identity, the lockfile it reads, the driver
/// binary, how to parse the lock, and how to re-pin a dependency.
pub trait CondaLayout: Send + Sync + 'static {
    /// The tool's canonical [`ToolId`] (`conda` or `pixi`).
    const ID: ToolId;
    /// The lockfile detected as the project marker.
    const LOCKFILE: &'static str;
    /// The driver binary, shelled out to for apply/build.
    const BIN: &'static str;

    /// Parses the lock into its resolved conda/PyPI dependency list.
    fn parse(content: &str) -> Vec<CondaDep>;

    /// The package names declared in the project's *source* manifest (`environment.yml` /
    /// `pixi.toml`), so the resolved lock can be split direct vs. transitive — the lock itself does
    /// not say. `None` when no manifest is found, so the caller treats every locked package as direct.
    fn direct_names(root: &Utf8Path) -> Option<HashSet<String>>;

    /// The driver args that re-pin `name` to `version`.
    fn upgrade_args(name: &str, version: &str) -> Vec<String>;

    /// The driver args for the opt-in `--build` step.
    fn build_args() -> Vec<String>;
}

/// conda-lock: `conda-lock.yml`, driven by `conda`.
pub struct CondaLock;
/// pixi: `pixi.lock`, driven by `pixi`.
pub struct Pixi;

impl CondaLayout for CondaLock {
    const ID: ToolId = ToolId("conda");
    const LOCKFILE: &'static str = "conda-lock.yml";
    const BIN: &'static str = "conda";

    fn parse(content: &str) -> Vec<CondaDep> {
        lock::parse_conda_lock(content)
    }

    fn direct_names(root: &Utf8Path) -> Option<HashSet<String>> {
        let content = std::fs::read_to_string(root.join("environment.yml"))
            .or_else(|_| std::fs::read_to_string(root.join("environment.yaml")))
            .ok()?;
        Some(lock::environment_yml_direct(&content))
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["install".into(), "-y".into(), format!("{name}={version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["list".into()]
    }
}

impl CondaLayout for Pixi {
    const ID: ToolId = ToolId("pixi");
    const LOCKFILE: &'static str = "pixi.lock";
    const BIN: &'static str = "pixi";

    fn parse(content: &str) -> Vec<CondaDep> {
        lock::parse_pixi_lock(content)
    }

    fn direct_names(root: &Utf8Path) -> Option<HashSet<String>> {
        let content = std::fs::read_to_string(root.join("pixi.toml")).ok()?;
        Some(lock::pixi_toml_direct(&content))
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["add".into(), format!("{name}=={version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into()]
    }
}

/// The conda-world implementation of the [`Tool`] port, generic over a [`CondaLayout`].
pub struct CondaEnvTool<L> {
    conda: Conda,
    pypi: PyPi,
    driver: Driver,
    _layout: PhantomData<fn() -> L>,
}

impl<L: CondaLayout> CondaEnvTool<L> {
    /// Creates the adapter from the conda and PyPI registry clients.
    #[must_use]
    pub fn new(conda: Conda, pypi: PyPi) -> Self {
        CondaEnvTool {
            conda,
            pypi,
            driver: Driver::new(L::BIN),
            _layout: PhantomData,
        }
    }

    /// Creates the adapter from a shared HTTP client, building both registry clients.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        CondaEnvTool::new(Conda::new(http.clone()), PyPi::new(http))
    }

    async fn raw_releases(&self, dep: &Dependency) -> Result<Vec<RawRelease>> {
        if dep.package.registry.as_deref() == Some(CONDA) {
            self.conda.releases(&dep.package).await
        } else {
            self.pypi.releases(&dep.package).await
        }
    }
}

fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

fn build_releases(current: &str, raw: Vec<RawRelease>) -> Vec<Release> {
    build_registry_releases(
        current,
        raw,
        |value| version::parse(value).is_some(),
        version::compare,
        version::major_key,
        version::classify_kind,
        classify_quality,
    )
}

#[async_trait]
impl<L: CondaLayout> ToolRead for CondaEnvTool<L> {
    fn id(&self) -> ToolId {
        L::ID
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
            lockfile: L::LOCKFILE,
            manifest: L::LOCKFILE,
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        // conda-world locks do not mark direct vs. transitive, so cross-reference the source manifest
        // (`environment.yml` / `pixi.toml`): a package named there is direct, the rest transitive. The
        // split lets `--major` rewrite direct deps across a major while leaving transitives capped.
        // When no manifest is found, fall back to treating every resolved package as direct.
        let content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let direct = L::direct_names(&project.root).filter(|names| !names.is_empty());
        let mut deps = Vec::new();
        for entry in L::parse(&content) {
            let is_direct = direct
                .as_ref()
                .is_none_or(|names| names.contains(&lock::normalize_name(&entry.name)));
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            let registry = if entry.conda { CONDA } else { PYPI };
            deps.push(Dependency {
                package: PackageId::new(L::ID, entry.name, Some(registry.to_string())),
                current: Version::new(entry.version.clone()),
                current_quality: classify_quality(&entry.version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                members: Vec::new(),
                pinned: false,
            });
        }
        Ok(deps)
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
impl<L: CondaLayout> ReleaseFetcher for CondaEnvTool<L> {
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
            kind_from_current: None,
            published_at: time,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

#[async_trait]
impl<L: CondaLayout> ToolWrite for CondaEnvTool<L> {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(
                &project.root,
                Utf8Path::new(L::LOCKFILE),
            )?],
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
            let args = L::upgrade_args(&change.package.name, change.to.as_str());
            match self.driver.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.driver
            .verify(&project.root, &L::build_args(), "build succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[tokio::test]
    async fn pixi_routes_conda_and_pypi_registries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("pixi.lock"),
            "version: 6\npackages:\n- conda: https://conda.anaconda.org/conda-forge/linux-64/numpy-1.24.0-py311h_0.conda\n- pypi: https://files.pythonhosted.org/x/requests-2.28.0-py3-none-any.whl\n  name: requests\n  version: 2.28.0\n",
        )
        .expect("lock");
        let project = Project {
            root: root.clone(),
            kind: Pixi::ID,
            manifest: root.join("pixi.lock"),
        };
        let cache = tempfile::tempdir().expect("cache");
        let tool = CondaEnvTool::<Pixi>::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );

        let mut deps = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("deps");
        deps.sort_by(|a, b| a.package.name.cmp(&b.package.name));
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].package.name, "numpy");
        assert_eq!(deps[0].package.registry.as_deref(), Some(CONDA));
        assert_eq!(deps[1].package.name, "requests");
        assert_eq!(deps[1].package.registry.as_deref(), Some(PYPI));
    }
}
