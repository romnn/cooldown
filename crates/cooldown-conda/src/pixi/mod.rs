//! The pixi [`Tool`](cooldown_core::Tool): the graph of every environment and platform from
//! `pixi.lock`, the direct split from the manifest, and anaconda.org or PyPI publish times, with a
//! conda package's current age read from the builds the lock holds.
//!
//! An apply locks the project once per batch in a scratch copy whose manifest pins each target
//! and cooldown's cutoff, checks the result against the lock it started from and publishes only
//! `pixi.lock`.

mod land;
mod lock;
mod manifest;

use crate::registry::{CONDA, Conda};
use crate::tool::build_releases;
use async_trait::async_trait;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_adapter_util::{Driver, canonical_path, single_lock_journal, verify_current_unknown};
use cooldown_core::{
    ApplyAttempt, ApplyObserver, ApplyReport, ArtifactId, CandidateScope, Capabilities, Change,
    CoreError, DepScope, Dependency, FetchContext, LockVerifyReport, NativePolicyLayer, PackageId,
    PackageRegistry, Plan, PreparedMutation, Project, ProjectMarker, ProjectMutationJournal,
    RawRelease, Release, ReleaseFetcher, ReleaseOrder, Result, ToolId, ToolRead, ToolWrite,
    UpdateKind, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use cooldown_uv::PyPi;
use cooldown_uv::pypi::PYPI;
use cooldown_uv::version;
use jiff::Timestamp;
use lock::Kind;
use manifest::Manifest;
use std::collections::{BTreeMap, BTreeSet};

/// The [`ToolId`] for the pixi adapter (`"pixi"`).
const PIXI_ID: ToolId = ToolId("pixi");

/// The lock pixi writes, which is the project marker and the resolved graph.
const LOCKFILE: &str = "pixi.lock";

/// The pixi implementation of the [`Tool`](cooldown_core::Tool) port.
pub struct PixiTool {
    conda: Conda,
    pypi: PyPi,
    driver: Driver,
}

/// One version of a package the lock holds somewhere.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct LockedVersion {
    kind: Kind,
    name: String,
    version: String,
}

/// What every binding of one [`LockedVersion`] says of it.
#[derive(Default)]
struct Bound {
    /// Whether another package pins any binding exactly.
    capped: bool,
    /// Each of its conda builds, as [`build_id`] observes it.
    builds: BTreeSet<String>,
}

/// One conda build as a graph read observes it: its digest and its upload time in milliseconds,
/// or `undated`, which a dependency carries as one of its artifacts.
///
/// The age travels with the observation, so a rebuild the lock takes at the same version is
/// another artifact, and the answers cached for the dependency are keyed apart.
fn build_id(binding: &lock::Binding) -> String {
    let uploaded = binding.uploaded.map_or_else(
        || "undated".to_owned(),
        |at| at.as_millisecond().to_string(),
    );
    format!("{}@{uploaded}", binding.identity)
}

/// When the newest of the conda `builds` a dependency is locked at was uploaded, or `None` when
/// any of them records no time, since the others cannot tell how young that one is.
///
/// A conda version is rebuilt under its own number, so a build the lock holds can be far younger
/// than the version's first upload.
fn locked_age(builds: &[ArtifactId]) -> Option<Timestamp> {
    let ages: Option<Vec<Timestamp>> = builds
        .iter()
        .map(|build| {
            let (_, millis) = build.0.rsplit_once('@')?;
            Timestamp::from_millisecond(millis.parse().ok()?).ok()
        })
        .collect();
    ages?.into_iter().max()
}

/// The ecosystem a change moves a package in.
fn kind_of(change: &Change) -> Kind {
    if change.package.registry.as_deref() == Some(CONDA) {
        Kind::Conda
    } else {
        Kind::Pypi
    }
}

/// The registry name a package of `kind` is tagged with.
fn registry_of(kind: Kind) -> &'static str {
    match kind {
        Kind::Conda => CONDA,
        Kind::Pypi => PYPI,
    }
}

impl PixiTool {
    /// Creates the adapter from the conda and PyPI registry clients.
    #[must_use]
    pub fn new(conda: Conda, pypi: PyPi) -> Self {
        PixiTool {
            conda,
            pypi,
            driver: Driver::new("pixi"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building both registry clients.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        PixiTool::new(Conda::new(http.clone()), PyPi::new(http))
    }

    fn registry(&self, dep: &Dependency) -> &dyn PackageRegistry {
        if dep.package.registry.as_deref() == Some(CONDA) {
            &self.conda
        } else {
            &self.pypi
        }
    }
}

#[async_trait]
impl ToolRead for PixiTool {
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
            &["pixi.toml", "pyproject.toml", ".pixi/config.toml"],
        );
        cooldown_adapter_util::evidence::ancestors(&mut evidence, &project.root, &[]);
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &["PIXI_HOME", "PIXI_CONFIG_FILE", "PIXI_NO_CONFIG"],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        PIXI_ID
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

    fn project_marker(&self) -> ProjectMarker {
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
        let lock = lock::read(&std::fs::read_to_string(project.root.join(LOCKFILE))?)?;
        let manifest = Manifest::read(&project.root)?;
        let mut versions: BTreeMap<LockedVersion, Bound> = BTreeMap::new();
        for (slot, binding) in &lock.bindings {
            let bound = versions
                .entry(LockedVersion {
                    kind: slot.kind,
                    name: slot.name.clone(),
                    version: binding.version.clone(),
                })
                .or_default();
            bound.capped |= binding.capped;
            if slot.kind == Kind::Conda {
                bound.builds.insert(build_id(binding));
            }
        }
        let mut deps = Vec::new();
        for (locked, bound) in versions {
            let LockedVersion {
                kind,
                name,
                version: current,
            } = locked;
            // A package is direct wherever the manifest declares it, though cooldown lands only
            // what the top-level tables pin.
            let direct = manifest
                .as_ref()
                .is_some_and(|manifest| !manifest.declarations(kind, &name).is_empty());
            if scope == DepScope::Direct && !direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(PIXI_ID, name, Some(registry_of(kind).to_owned())),
                // No advisory-database ecosystem maps conda channels (see `capabilities`), so no
                // package carries an advisory identity.
                advisory_identity: None,
                current_quality: crate::tool::classify_quality(&current),
                graph_ceiling: bound.capped.then(|| Version::new(current.clone())),
                current: Version::new(current),
                direct,
                artifacts: bound.builds.into_iter().map(ArtifactId).collect(),
                graph_floor: None,
                declared_bound: None,
                members: Vec::new(),
                pinned: false,
                hold_edges: Vec::new(),
            });
        }
        Ok(deps)
    }

    async fn native_policy(&self, project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(Manifest::read(&project.root)?.and_then(|manifest| manifest.native_policy()))
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<LockVerifyReport> {
        Ok(verify_current_unknown(LOCKFILE))
    }
}

#[async_trait]
impl ReleaseFetcher for PixiTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw: Vec<RawRelease> = self.registry(dep).releases(&dep.package).await?;
        let mut releases = build_releases(dep.current.as_str(), raw);
        if dep.package.registry.as_deref() == Some(CONDA) {
            let age = locked_age(&dep.artifacts);
            for release in &mut releases {
                if release.version == dep.current {
                    release.published_at = age;
                }
            }
        }
        Ok(releases)
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let published_at = if dep.package.registry.as_deref() == Some(CONDA) {
            locked_age(&dep.artifacts)
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
            published_at,
            yanked: false,
            quality: dep.current_quality,
        })
    }
}

#[async_trait]
impl ToolWrite for PixiTool {
    fn mutation_tool(&self) -> ToolId {
        PIXI_ID
    }

    // A preview copies what an apply's scratch copy holds, so pixi locks the same project in
    // both, and fails where the apply would hold the batch for what no copy can reproduce.
    fn external_resolve_roots(&self, project: &Project) -> Result<Vec<Utf8PathBuf>> {
        let Some(manifest) = Manifest::read(&project.root)? else {
            return Ok(Vec::new());
        };
        let root = canonical_path(&project.root)?;
        let links = land::find_links(&root, &manifest).map_err(CoreError::Config)?;
        let mut roots = land::inputs(&root, &manifest, &links)?;
        // A file root brings its directory along, so only an empty flat index needs its own.
        let empty: Vec<Utf8PathBuf> = links
            .into_iter()
            .filter(|link| {
                !roots
                    .iter()
                    .any(|file| file.parent() == Some(link.as_path()))
            })
            .collect();
        roots.extend(empty);
        Ok(roots)
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        single_lock_journal(&project.root, Utf8Path::new(LOCKFILE))
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, journal) = mutation.parts_for(self)?;
        land::relock(
            &self.driver,
            project,
            plan,
            &mut journal.state_for(journal)?,
        )
        .await
    }

    async fn apply_with_observer(
        &self,
        mutation: &PreparedMutation,
        _observer: &dyn ApplyObserver,
    ) -> Result<ApplyAttempt> {
        let (project, plan, journal) = mutation.parts_for(self)?;
        // The apply starts from the prepared state and hands back that state, advanced only by its
        // own write, never a capture of the disk, so a lock someone else changed after preparing
        // is neither published over nor taken for cooldown's own, and a rollback refuses to
        // overwrite it.
        let mut owned = journal.state_for(journal)?;
        let report = land::relock(&self.driver, project, plan, &mut owned).await;
        mutation.finished_attempt(report, &owned)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        let args = ["install", "--locked", "--all"].map(ToString::to_string);
        self.driver
            .verify(&project.root, &args, "pixi install succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A preview copies the files of each flat index, a directory only for one that holds none,
    /// and fails, as the apply holds, once a link sits in one.
    #[cfg(unix)]
    #[test]
    fn a_preview_copies_flat_indexes_and_refuses_a_link() -> Result<()> {
        let cache = tempfile::tempdir()?;
        let http = SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default())?;
        let tool = PixiTool::from_http(http);
        let dir = tempfile::tempdir()?;
        let root = canonical_path(Utf8Path::from_path(dir.path()).ok_or_else(|| {
            CoreError::PathEncoding("the temporary directory is not UTF-8".to_owned())
        })?)?;
        for index in ["wheels", "empty"] {
            std::fs::create_dir_all(root.join(index))?;
        }
        std::fs::write(root.join("wheels").join("a-1-py3-none-any.whl"), "wheel")?;
        std::fs::write(
            root.join("pixi.toml"),
            indoc::indoc! {r#"
                [pypi-options]
                find-links = [{ path = "./wheels" }, { path = "./empty" }, { path = "." }]
            "#},
        )?;
        let project = Project::detected(root.clone(), PIXI_ID, root.join(LOCKFILE));

        let roots = tool.external_resolve_roots(&project)?;

        assert!(roots.contains(&root.join("wheels").join("a-1-py3-none-any.whl")));
        assert!(roots.contains(&root.join("empty")));
        assert!(
            !roots.contains(&root) && !roots.contains(&root.join("wheels")),
            "{roots:?}"
        );

        std::os::unix::fs::symlink(
            root.join("wheels").join("a-1-py3-none-any.whl"),
            root.join("wheels").join("b-1-py3-none-any.whl"),
        )?;
        let refused = tool.external_resolve_roots(&project);
        assert!(
            matches!(&refused, Err(CoreError::Config(detail)) if detail.contains("holds the link")),
            "{refused:?}"
        );
        Ok(())
    }

    /// A conda package is as young as the newest build the graph read saw it locked at, and of
    /// unknown age once one of them records no time, rather than taking the age of the others.
    #[test]
    fn an_undated_build_leaves_the_age_unknown() -> std::result::Result<(), jiff::Error> {
        let binding = |identity: &str, uploaded: Option<Timestamp>| lock::Binding {
            version: "1.3.1".to_owned(),
            url: String::new(),
            identity: identity.to_owned(),
            source: String::new(),
            uploaded,
            capped: false,
        };
        let older = Timestamp::from_second(1_700_000_000)?;
        let newer = Timestamp::from_second(1_710_000_000)?;
        let observed = |builds: &[lock::Binding]| -> Vec<ArtifactId> {
            builds
                .iter()
                .map(|build| ArtifactId(build_id(build)))
                .collect()
        };

        let dated = observed(&[binding("a", Some(older)), binding("b", Some(newer))]);
        assert_eq!(locked_age(&dated), Some(newer));
        let undated = observed(&[binding("a", Some(newer)), binding("b", None)]);
        assert_eq!(locked_age(&undated), None);
        Ok(())
    }
}
