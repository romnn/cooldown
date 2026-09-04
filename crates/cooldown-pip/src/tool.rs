//! The generic PyPI-backed [`Tool`] for non-uv Python projects, parameterised by a [`PyLayout`]
//! (pip or Poetry). Both resolve from PyPI and share the PEP 440 version model — reused wholesale
//! from [`cooldown_uv`] — and differ only in which files they read and how their CLI re-pins a
//! dependency.

use crate::lock;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    Driver, RegistryVersionClassifier, build_registry_releases, skipped_on_apply_error,
    verify_current_unknown,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, Change, DepScope, Dependency, FetchContext,
    LockVerifyReport, NativePolicyLayer, PackageId, PackageRegistry, Plan, PreparedMutation,
    Project, ProjectMarker, ProjectMutationJournal, RawRelease, Release, ReleaseFetcher,
    ReleaseOrder, ReleaseQuality, ResolveInputs, Result, SkipReason, Skipped, ToolId, ToolRead,
    ToolWrite, UpdateKind, VerifyReport, Version,
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
    /// Whether the apply installs into the active virtualenv as it pins (`poetry add` does;
    /// pip's apply only rewrites `requirements.txt`).
    const INSTALLS: bool;

    /// Parses the lock + manifest into the resolved [`ResolvedDep`] set.
    fn parse(lock: &str, manifest: &str) -> Vec<ResolvedDep>;

    /// The driver args that re-pin `name` to `version`.
    fn upgrade_args(name: &str, version: &str) -> Vec<String>;

    /// The driver args for the opt-in `--build` step.
    fn build_args() -> Vec<String>;

    /// Whether the layout's ambient configuration leaves its pins' PyPI origin claims intact —
    /// a veto joined onto every pin's [`ResolvedDep::pypi`]. The in-file evidence still decides
    /// the grant; this can only withhold it.
    fn ambient_origin_intact(project: &Project) -> bool;

    /// Whether a feed-time grant must additionally be confirmed by the driver binary itself
    /// (`pip config list`). pip's site configuration lives at the *interpreter's* `sys.prefix` —
    /// behind a shim (mise, pyenv) that is a location no static walk can enumerate — so only
    /// the selected pip executable can prove its effective routing clean. Poetry's evidence is
    /// per-package lock source records plus the manifest source veto, and its resolver does not
    /// read pip configuration: its identities stand without a confirmation step.
    const CONFIRMS_WITH_DRIVER: bool;
}

/// One resolved distribution from a [`PyLayout`]'s lock + manifest.
pub struct ResolvedDep {
    /// The distribution name as recorded in the lock.
    pub name: String,
    /// The resolved (locked) version.
    pub version: String,
    /// Whether the manifest declares the distribution directly (vs. a transitive dependency).
    pub direct: bool,
    /// Whether the distribution's origin is provably PyPI (see [`lock::ResolvedPin::pypi`]).
    pub pypi: bool,
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
    const INSTALLS: bool = false;

    fn parse(lock: &str, _manifest: &str) -> Vec<ResolvedDep> {
        lock::parse_requirements(lock)
            .into_iter()
            .map(|pin| ResolvedDep {
                name: pin.name,
                version: pin.version,
                direct: true,
                pypi: pin.pypi,
            })
            .collect()
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["install".into(), format!("{name}=={version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into(), "-r".into(), "requirements.txt".into()]
    }

    /// pip's index options are install-wide and arrive from well past the requirements file
    /// itself: `-r`/`-c` includes, `PIP_*` variables, and every enumerable pip config location.
    fn ambient_origin_intact(project: &Project) -> bool {
        !crate::ambient::reroutes(&project.root.join(Self::LOCKFILE))
    }

    const CONFIRMS_WITH_DRIVER: bool = true;
}

impl PyLayout for Poetry {
    const ID: ToolId = ToolId("poetry");
    const LOCKFILE: &'static str = "poetry.lock";
    const MANIFEST: &'static str = "pyproject.toml";
    const BIN: &'static str = "poetry";
    const INSTALLS: bool = true;

    fn parse(lock: &str, manifest: &str) -> Vec<ResolvedDep> {
        let direct = lock::parse_poetry_direct(manifest);
        lock::parse_poetry_lock(lock)
            .into_iter()
            .map(|pin| {
                let is_direct = lock::is_direct(&direct, &pin.name);
                ResolvedDep {
                    name: pin.name,
                    version: pin.version,
                    direct: is_direct,
                    pypi: pin.pypi,
                }
            })
            .collect()
    }

    fn upgrade_args(name: &str, version: &str) -> Vec<String> {
        vec!["add".into(), format!("{name}@{version}")]
    }

    fn build_args() -> Vec<String> {
        vec!["install".into()]
    }

    /// Poetry takes resolution sources from the manifest alone; declaring any
    /// `[[tool.poetry.source]]` withdraws the lock's per-package origin claims (see
    /// [`crate::ambient::poetry_declares_sources`]).
    fn ambient_origin_intact(project: &Project) -> bool {
        !crate::ambient::poetry_declares_sources(&project.manifest)
    }

    const CONFIRMS_WITH_DRIVER: bool = false;
}

/// The PyPI-backed implementation of the [`Tool`] port, generic over a [`PyLayout`].
pub struct PyTool<L> {
    pypi: PyPi,
    driver: Driver,
    /// Whether the driver's effective configuration was confirmed clean, per project root — one
    /// `pip config list` per root per process, matching the port's memoization contract.
    effective_config_clean:
        tokio::sync::Mutex<std::collections::HashMap<camino::Utf8PathBuf, bool>>,
    _layout: PhantomData<fn() -> L>,
}

impl<L: PyLayout> PyTool<L> {
    /// Creates the adapter from a configured [`PyPi`] client.
    #[must_use]
    pub fn new(pypi: PyPi) -> Self {
        PyTool {
            pypi,
            driver: Driver::new(L::BIN),
            effective_config_clean: tokio::sync::Mutex::default(),
            _layout: PhantomData,
        }
    }

    /// Whether the driver binary confirms its effective configuration free of routing options,
    /// memoized per project root. A query that fails to spawn, exits non-zero, or produces
    /// output [`crate::ambient::effective_config_is_clean`] cannot vouch for is *not* clean:
    /// unknown routing must not pass as none.
    async fn confirmed_clean(&self, root: &Utf8Path) -> bool {
        let mut cache = self.effective_config_clean.lock().await;
        if let Some(&clean) = cache.get(root) {
            return clean;
        }
        let args = vec!["config".to_string(), "list".to_string()];
        let clean = match self.driver.stdout(root, &args).await {
            Ok(output) => crate::ambient::effective_config_is_clean(&output),
            Err(_) => false,
        };
        cache.insert(root.to_owned(), clean);
        clean
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
            advisory_ecosystem: Some("PyPI"),
        }
    }

    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        cooldown_core::ProjectDetection::Primary(ProjectMarker {
            lockfile: L::LOCKFILE,
            manifest: L::MANIFEST,
            alternate_manifests: &[],
            workspace_root: false,
        })
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let lock = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let manifest = std::fs::read_to_string(&project.manifest).unwrap_or_default();
        let ambient_intact = L::ambient_origin_intact(project);
        let mut deps = Vec::new();
        for resolved in L::parse(&lock, &manifest) {
            if scope == DepScope::Direct && !resolved.direct {
                continue;
            }
            // The lockfile may preserve the author's spelling (`Django`, `jupyter_server`);
            // OSV's `PyPI` ecosystem stores PEP 503-normalized names, and its documents'
            // `affected[]` entries only match the normalized form. An origin the file cannot
            // prove to be PyPI — or that ambient configuration reroutes — carries no advisory
            // identity at all.
            let advisory_identity = (resolved.pypi && ambient_intact)
                .then(|| cooldown_adapter_util::pep503_normalize(&resolved.name));
            deps.push(Dependency {
                package: PackageId::new(L::ID, resolved.name, Some(PYPI.to_string())),
                advisory_identity,
                current: Version::new(resolved.version.clone()),
                current_quality: classify_quality(&resolved.version),
                direct: resolved.direct,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
                declared_bound: None,
                members: Vec::new(),
                pinned: false,
                hold_edges: Vec::new(),
            });
        }
        Ok(deps)
    }

    /// pip's site configuration lives at the selected interpreter's `sys.prefix` — behind a
    /// shim (mise, pyenv), a location no static walk can enumerate — so a grant only survives
    /// to the feed once the same pip executable cooldown's future invocations use confirms its
    /// effective configuration (`pip config list`) free of routing options. A failed or
    /// unreadable query withholds every identity. Poetry resolves from its own source
    /// configuration, already vetoed at grant time, and passes through untouched.
    async fn confirm_advisory_identities(&self, project: &Project, deps: &mut [Dependency]) {
        if !L::CONFIRMS_WITH_DRIVER {
            return;
        }
        if deps.iter().all(|dep| dep.advisory_identity.is_none()) {
            return;
        }
        if !self.confirmed_clean(&project.root).await {
            for dep in deps {
                dep.advisory_identity = None;
            }
        }
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
impl<L: PyLayout> ToolWrite for PyTool<L> {
    fn mutation_tool(&self) -> ToolId {
        L::ID
    }

    fn mutation_installs(&self) -> bool {
        L::INSTALLS
    }

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
        let mut paths = vec![Utf8Path::new(L::LOCKFILE)];
        if L::MANIFEST != L::LOCKFILE {
            paths.push(Utf8Path::new(L::MANIFEST));
        }
        ProjectMutationJournal::capture(&project.root, paths)
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, _) = mutation.parts_for(self)?;
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
        detail: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use color_eyre::eyre;
    use indoc::indoc;

    /// Only poetry's apply installs: `poetry add` syncs the virtualenv, while pip's apply
    /// rewrites `requirements.txt` and leaves installing to `--build`.
    #[test]
    fn only_the_poetry_apply_installs() {
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");

        assert!(!PyTool::<Pip>::from_http(http.clone()).mutation_installs());
        assert!(PyTool::<Poetry>::from_http(http).mutation_installs());
    }

    #[test]
    fn advisory_ecosystem_matches_osv() {
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        assert_eq!(
            PyTool::<Pip>::from_http(http.clone())
                .capabilities()
                .advisory_ecosystem,
            Some("PyPI")
        );
        assert_eq!(
            PyTool::<Poetry>::from_http(http)
                .capabilities()
                .advisory_ecosystem,
            Some("PyPI")
        );
    }

    /// A lockfile spelling like `Django` or `jupyter_server` must query (and match) OSV's PEP
    /// 503-normalized `PyPI` names, or every advisory for the package is silently lost — and a
    /// requirements file that routes through a custom index proves no pin's origin, so none
    /// carries an identity at all.
    #[tokio::test]
    async fn advisory_identity_normalizes_per_pep503_and_needs_a_provable_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
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

        std::fs::write(
            root.join("requirements.txt"),
            "Django==5.0.0\njupyter_server==2.0.0\nruamel.yaml.clib==0.2.0\n",
        )
        .expect("write");
        let deps = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("deps");
        let identities: Vec<Option<&str>> = deps
            .iter()
            .map(|dep| dep.advisory_identity.as_deref())
            .collect();
        assert_eq!(
            identities,
            [
                Some("django"),
                Some("jupyter-server"),
                Some("ruamel-yaml-clib")
            ]
        );

        std::fs::write(
            root.join("requirements.txt"),
            "--extra-index-url https://pypi.corp.example/simple\nDjango==5.0.0\n",
        )
        .expect("write");
        let deps = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].advisory_identity, None);

        // Index options are install-wide, so a directive in an `-r` include reroutes the pins
        // of the including file just as surely as one written inline.
        std::fs::write(
            root.join("requirements.txt"),
            "Django==5.0.0\n-r extra.txt\n",
        )
        .expect("write");
        std::fs::write(
            root.join("extra.txt"),
            "--index-url https://pypi.corp.example/simple\nms==1.0.0\n",
        )
        .expect("write");
        let deps = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("deps");
        assert!(
            deps.iter().all(|dep| dep.advisory_identity.is_none()),
            "an included directive withholds every identity"
        );
    }

    /// A pin as `dependencies()` would grant it: identity present, everything else minimal —
    /// the confirmation hook's input.
    fn granted_pin(kind: ToolId, name: &str) -> Dependency {
        Dependency {
            package: PackageId::new(kind, name.to_string(), Some(PYPI.to_string())),
            advisory_identity: Some(name.to_string()),
            current: Version::new("1.0.0".to_string()),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn pip_config_script(root: &Utf8Path, body: &str) -> Utf8PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let script = root.join("fake-pip.sh");
        std::fs::write(&script, format!("#!/bin/sh\n{body}\n")).expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("chmod script");
        script
    }

    /// Confirmation asks the selected pip executable for its *effective* configuration — the
    /// merge that includes the interpreter-prefix site file no static walk can locate — and
    /// keeps identities only when it is visibly clean; a routed config or a failed query
    /// withholds them all.
    #[cfg(unix)]
    #[tokio::test]
    async fn confirmation_requires_a_clean_effective_pip_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        let project = Project {
            root: root.clone(),
            kind: Pip::ID,
            manifest: root.join("requirements.txt"),
            exclude_newer: None,
        };

        let script = pip_config_script(&root, "echo \"global.timeout='60'\"");
        let mut clean: PyTool<Pip> = PyTool::from_http(http.clone());
        clean.driver = Driver::from_program(script.as_str());
        let mut deps = vec![granted_pin(Pip::ID, "django")];
        clean.confirm_advisory_identities(&project, &mut deps).await;
        assert_eq!(
            deps[0].advisory_identity.as_deref(),
            Some("django"),
            "a visibly clean effective config keeps the grant"
        );

        let script = pip_config_script(
            &root,
            "echo \"global.index-url='https://pypi.corp.example/simple'\"",
        );
        let mut routed: PyTool<Pip> = PyTool::from_http(http.clone());
        routed.driver = Driver::from_program(script.as_str());
        let mut deps = vec![granted_pin(Pip::ID, "django")];
        routed
            .confirm_advisory_identities(&project, &mut deps)
            .await;
        assert_eq!(
            deps[0].advisory_identity, None,
            "a routed site config governs the future install and vetoes the grant"
        );

        let script = pip_config_script(&root, "exit 1");
        let mut failing: PyTool<Pip> = PyTool::from_http(http);
        failing.driver = Driver::from_program(script.as_str());
        let mut deps = vec![granted_pin(Pip::ID, "django")];
        failing
            .confirm_advisory_identities(&project, &mut deps)
            .await;
        assert_eq!(
            deps[0].advisory_identity, None,
            "unknown routing must not pass as none"
        );
    }

    /// Poetry's identities rest on per-package lock source records plus the manifest source
    /// veto, not on pip configuration — confirmation passes them through without spawning
    /// anything.
    #[tokio::test]
    async fn poetry_identities_pass_confirmation_without_spawning() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let cache = tempfile::tempdir().expect("cache");
        let mut tool: PyTool<Poetry> = PyTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );
        // A binary that must never run: were it consulted, its failure would withhold the
        // identity and fail the assertion below.
        tool.driver = Driver::from_program(root.join("absent-poetry").as_str());
        let project = Project {
            root: root.clone(),
            kind: Poetry::ID,
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        };
        let mut deps = vec![granted_pin(Poetry::ID, "django")];

        tool.confirm_advisory_identities(&project, &mut deps).await;
        assert_eq!(deps[0].advisory_identity.as_deref(), Some("django"));
    }

    #[tokio::test]
    async fn pip_apply_rewrites_requirements_without_invoking_pip_install() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            root.join("requirements.txt"),
            indoc! {"
                requests==2.28.0
                flask==2.2.0
            "},
        )?;
        let project = Project {
            root: root.clone(),
            kind: Pip::ID,
            manifest: root.join("requirements.txt"),
            exclude_newer: None,
        };
        let cache = tempfile::tempdir()?;
        let tool = PyTool::<Pip>::from_http(SharedHttp::new(
            cache.path(),
            cooldown_registry::HttpOptions::default(),
        )?);
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
            ..Plan::default()
        };

        let mutation = PreparedMutation::prepare(&tool, &project, &plan).await?;
        let report = tool.apply(&mutation).await?;

        assert_eq!(report.applied.len(), 1);
        assert!(report.skipped.is_empty());
        let rewritten = std::fs::read_to_string(root.join("requirements.txt"))?;
        assert!(rewritten.contains("requests==2.31.0"));
        Ok(())
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
