//! The generic PyPI-backed [`Tool`] for non-uv Python projects, parameterised by a [`PyLayout`]
//! (pip or Poetry). Both resolve from PyPI and share the PEP 440 version model — reused wholesale
//! from [`cooldown_uv`] — and differ only in which files they read and how their CLI re-pins a
//! dependency.

use crate::lock;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    Driver, build_registry_releases, skipped_on_apply_error, verify_current_unknown,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, DepScope, Dependency, FetchContext,
    LockVerifyReport, NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality,
    ResolveInputs, Result, SkipReason, Skipped, ToolId, ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use cooldown_uv::PyPi;
use cooldown_uv::pypi::PYPI;
use cooldown_uv::version;
use std::marker::PhantomData;

/// The per-tool knobs the generic adapter needs: identity, the files it reads, the driver binary,
/// how to split resolved deps into direct/transitive, and how to re-pin one.
pub trait PyLayout: Send + Sync + 'static {
    /// The tool's canonical [`ToolId`] (`pip` or `poetry`).
    const ID: ToolId;
    /// The file detected as the project marker and read for resolved versions.
    const LOCKFILE: &'static str;
    /// The manifest read for the direct-dependency set (the same file as the lock, for pip).
    const MANIFEST: &'static str;
    /// The driver binary, shelled out to for apply/build.
    const BIN: &'static str;

    /// Parses the lock + manifest into resolved `(name, version, is_direct)` triples.
    fn parse(lock: &str, manifest: &str) -> Vec<(String, String, bool)>;

    /// The driver args that re-pin `name` to `version`.
    fn upgrade_args(name: &str, version: &str) -> Vec<String>;

    /// The driver args for the opt-in `--build` step.
    fn build_args() -> Vec<String>;
}

/// pip: a pinned `requirements.txt` is both the manifest and the version source, and every pinned
/// line is treated as direct (a flat requirements file records no graph).
pub struct Pip;
/// Poetry: the resolved graph is `poetry.lock`, and `pyproject.toml` supplies the direct set.
pub struct Poetry;

impl PyLayout for Pip {
    const ID: ToolId = ToolId("pip");
    const LOCKFILE: &'static str = "requirements.txt";
    const MANIFEST: &'static str = "requirements.txt";
    const BIN: &'static str = "pip";

    fn parse(lock: &str, _manifest: &str) -> Vec<(String, String, bool)> {
        lock::parse_requirements(lock)
            .into_iter()
            .map(|(name, ver)| (name, ver, true))
            .collect()
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["install".into(), format!("{name}=={version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into(), "-r".into(), "requirements.txt".into()]
    }
}

impl PyLayout for Poetry {
    const ID: ToolId = ToolId("poetry");
    const LOCKFILE: &'static str = "poetry.lock";
    const MANIFEST: &'static str = "pyproject.toml";
    const BIN: &'static str = "poetry";

    fn parse(lock: &str, manifest: &str) -> Vec<(String, String, bool)> {
        let direct = lock::parse_poetry_direct(manifest);
        lock::parse_poetry_lock(lock)
            .into_iter()
            .map(|(name, ver)| {
                let is_direct = lock::is_direct(&direct, &name);
                (name, ver, is_direct)
            })
            .collect()
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["add".into(), format!("{name}@{version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into()]
    }
}

/// The PyPI-backed implementation of the [`Tool`] port, generic over a [`PyLayout`].
pub struct PyTool<L> {
    pypi: PyPi,
    driver: Driver,
    _layout: PhantomData<fn() -> L>,
}

impl<L: PyLayout> PyTool<L> {
    /// Creates the adapter from a configured [`PyPi`] client.
    #[must_use]
    pub fn new(pypi: PyPi) -> Self {
        PyTool {
            pypi,
            driver: Driver::new(L::BIN),
            _layout: PhantomData,
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`PyPi`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        PyTool::new(PyPi::new(http))
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
impl<L: PyLayout> ToolRead for PyTool<L> {
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
            manifest: L::MANIFEST,
            alternate_manifests: &[],
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let lock = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let manifest = std::fs::read_to_string(&project.manifest).unwrap_or_default();
        let mut deps = Vec::new();
        for (name, ver, is_direct) in L::parse(&lock, &manifest) {
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(L::ID, name, Some(PYPI.to_string())),
                current: Version::new(ver.clone()),
                current_quality: classify_quality(&ver),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
                members: Vec::new(),
                pinned: false,
            });
        }
        Ok(deps)
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<LockVerifyReport> {
        Ok(verify_current_unknown(L::LOCKFILE))
    }
}

#[async_trait]
impl<L: PyLayout> ReleaseFetcher for PyTool<L> {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.pypi.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self
            .pypi
            .published_at(&dep.package, &dep.current, &[])
            .await?;
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
impl<L: PyLayout> ToolWrite for PyTool<L> {
    fn resolve_inputs(&self) -> ResolveInputs {
        // `pip-compile`/`uv pip compile` EXECUTES a project's `setup.py` (and reads any version/readme
        // file it imports) to discover its dependencies, so the throwaway probe copy must carry `.py`
        // source. A purely `requirements.txt`/declarative project ignores the extra files.
        ResolveInputs {
            source_extensions: &["py"],
            ..ResolveInputs::DEFAULT
        }
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        let mut files = vec![ProjectMutationJournal::capture_file(
            &project.root,
            Utf8Path::new(L::LOCKFILE),
        )?];
        if L::MANIFEST != L::LOCKFILE {
            files.push(ProjectMutationJournal::capture_file(
                &project.root,
                Utf8Path::new(L::MANIFEST),
            )?);
        }
        Ok(ProjectMutationJournal { files })
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        _journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        if L::ID == Pip::ID {
            for change in &plan.changes {
                if rewrite_pip_requirement(project, change)? {
                    report.applied.push(change.clone());
                } else {
                    report.skipped.push(not_eligible(change));
                }
            }
            return Ok(report);
        }

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
            .verify(&project.root, &L::build_args(), "install succeeded")
            .await
    }
}

fn rewrite_pip_requirement(project: &Project, change: &Change) -> Result<bool> {
    let path = project.root.join(Pip::LOCKFILE);
    let content = std::fs::read_to_string(&path)?;
    let Some(rewritten) =
        lock::rewrite_requirement_pin(&content, &change.package.name, change.to.as_str())
    else {
        return Ok(false);
    };
    std::fs::write(path, rewritten)?;
    Ok(true)
}

fn not_eligible(change: &Change) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::NotEligible,
        offending: Some(change.package.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use indoc::indoc;

    #[tokio::test]
    async fn pip_apply_rewrites_requirements_without_invoking_pip_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("requirements.txt"),
            indoc! {"
                requests==2.28.0
                flask==2.2.0
            "},
        )
        .expect("requirements");
        let project = Project {
            root: root.clone(),
            kind: Pip::ID,
            manifest: root.join("requirements.txt"),
            exclude_newer: None,
        };
        let cache = tempfile::tempdir().expect("cache");
        let tool = PyTool::<Pip>::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );
        let change = Change {
            package: PackageId::new(Pip::ID, "requests", Some(PYPI.to_string())),
            from: Version::new("2.28.0"),
            to: Version::new("2.31.0"),
            kind: cooldown_core::UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        };
        let plan = Plan {
            changes: vec![change],
            rewrite: cooldown_core::RewriteMode::Auto,
        };

        let report = tool
            .apply(&project, &plan, &ProjectMutationJournal::default())
            .await
            .expect("apply");

        assert_eq!(report.applied.len(), 1);
        assert!(report.skipped.is_empty());
        let rewritten =
            std::fs::read_to_string(root.join("requirements.txt")).expect("requirements");
        assert!(rewritten.contains("requests==2.31.0"));
    }

    #[tokio::test]
    async fn poetry_splits_direct_from_transitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("poetry.lock"),
            "[[package]]\nname = \"requests\"\nversion = \"2.28.0\"\n\n[[package]]\nname = \"urllib3\"\nversion = \"1.26.0\"\n",
        )
        .expect("lock");
        std::fs::write(
            root.join("pyproject.toml"),
            "[tool.poetry.dependencies]\npython = \"^3.10\"\nrequests = \"^2.28\"\n",
        )
        .expect("manifest");
        let project = Project {
            root: root.clone(),
            kind: Poetry::ID,
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        };
        let cache = tempfile::tempdir().expect("cache");
        let tool = PyTool::<Poetry>::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );

        let direct = tool
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].package.name, "requests");
        assert_eq!(direct[0].package.registry.as_deref(), Some(PYPI));

        let graph = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph");
        assert_eq!(graph.len(), 2);
    }
}
