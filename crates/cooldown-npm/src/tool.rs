//! The generic JavaScript/TypeScript [`Tool`]: detection, the resolved graph from a lockfile, npm
//! registry publish times, and driver-backed re-resolution/apply. The lockfile format and driver
//! binary are supplied by a [`NodeLock`] type parameter, so npm, pnpm, yarn, and bun are all the
//! same adapter specialised over their lock format — they share the npm registry and version model
//! and differ only in how their lock is parsed and how their CLI re-pins a dependency.

use crate::lock::NodeLock;
use crate::manifest;
use crate::nodecmd::NodeCmd;
use crate::registry::{NPM, NpmRegistry};
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    build_registry_releases, skipped_on_apply_error, verify_current_report,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseOrder, ReleaseQuality, Result, ToolId,
    ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use std::collections::HashSet;
use std::marker::PhantomData;

/// The JavaScript/TypeScript implementation of the [`Tool`] port, generic over a [`NodeLock`].
///
/// It detects projects by their lockfile, reads the resolved graph from that lock, intersects it
/// with `package.json` to recover the direct/transitive split, and resolves publish times from the
/// shared [`NpmRegistry`]. npm has no native cooldown config, so [`native_policy`] is always empty.
///
/// [`native_policy`]: ToolRead::native_policy
pub struct NpmTool<L> {
    registry: NpmRegistry,
    cmd: NodeCmd,
    _lock: PhantomData<fn() -> L>,
}

impl<L: NodeLock> NpmTool<L> {
    /// Creates the adapter from a configured [`NpmRegistry`].
    #[must_use]
    pub fn new(registry: NpmRegistry) -> Self {
        NpmTool {
            registry,
            cmd: NodeCmd::new(L::BIN),
            _lock: PhantomData,
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`NpmRegistry`].
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        NpmTool::new(NpmRegistry::new(http))
    }
}

pub(crate) fn classify_quality(v: &str) -> ReleaseQuality {
    if version::is_prerelease(v) {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// Builds the sorted, deduplicated [`Release`] list the core consumes from the registry's raw
/// releases. npm and JSR both serve one artifact per version with no per-artifact split, so (unlike
/// PyPI) there is no artifact-scope handling here.
pub(crate) fn build_releases(current: &str, raw: Vec<RawRelease>) -> Vec<Release> {
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

/// Captures `package.json` and the lockfile as the mutation journal: an apply re-pins the manifest
/// *and* re-resolves the lock, so both must be restorable to undo a skipped change.
fn journal<L: NodeLock>(project: &Project) -> Result<ProjectMutationJournal> {
    Ok(ProjectMutationJournal {
        files: vec![
            ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("package.json"))?,
            ProjectMutationJournal::capture_file(&project.root, Utf8Path::new(L::LOCKFILE))?,
        ],
    })
}

#[async_trait]
impl<L: NodeLock> ToolRead for NpmTool<L> {
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
        // The lockfile sits at the workspace root; nested `package.json`s share it (no nested lock).
        ProjectMarker {
            lockfile: L::LOCKFILE,
            manifest: "package.json",
            workspace_root: true,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let resolved = L::parse(&content)?;
        let direct = manifest::direct_names(&project.manifest)?;

        let mut seen = HashSet::new();
        let mut deps = Vec::new();
        for (name, version) in resolved {
            let is_direct = direct.contains(&name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            if !seen.insert((name.clone(), version.clone())) {
                continue; // a name can resolve to the same version via several paths
            }
            deps.push(Dependency {
                package: PackageId::new(L::ID, name, Some(NPM.to_string())),
                current: Version::new(version.clone()),
                current_quality: classify_quality(&version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
            });
        }
        Ok(deps)
    }

    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.registry.releases(&dep.package).await?;
        Ok(build_releases(dep.current.as_str(), raw))
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        let time = self
            .registry
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

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        // npm has no standard in-manifest cooldown/freeze field, so there is no native layer.
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<VerifyReport> {
        // The npm-family CLIs lack a cheap, uniform "is the lock current?" probe, so cooldown
        // trusts the committed lock as the source of truth rather than re-resolving on every read.
        Ok(verify_current_report(
            true,
            "lockfile taken as current",
            "lockfile is stale",
        ))
    }
}

#[async_trait]
impl<L: NodeLock> ToolWrite for NpmTool<L> {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        journal::<L>(project)
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
            match self.cmd.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.cmd
            .verify(&project.root, &L::build_args(), "install succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::Npm;
    use camino::Utf8PathBuf;

    fn tool() -> NpmTool<Npm> {
        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        NpmTool::from_http(
            SharedHttp::new(cache_dir.path(), cooldown_registry::HttpOptions::default())
                .expect("http"),
        )
    }

    #[tokio::test]
    async fn dependencies_split_direct_from_transitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            r#"{ "dependencies": { "lodash": "4.17.15" } }"#,
        )
        .expect("write manifest");
        std::fs::write(
            root.join("package-lock.json"),
            r#"{
                "lockfileVersion": 3,
                "packages": {
                    "": { "version": "0.1.0" },
                    "node_modules/lodash": { "version": "4.17.15" },
                    "node_modules/ms": { "version": "2.1.3" }
                }
            }"#,
        )
        .expect("write lock");
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
        };

        let direct = tool()
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct deps");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].package.name, "lodash");
        assert!(direct[0].direct);
        assert_eq!(direct[0].package.registry.as_deref(), Some(NPM));

        let graph = tool()
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph deps");
        assert_eq!(graph.len(), 2); // lodash (direct) + ms (transitive)
        assert!(graph.iter().any(|d| d.package.name == "ms" && !d.direct));
    }

    #[tokio::test]
    async fn mutation_journal_restores_manifest_and_lock() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(root.join("package.json"), "{\"name\":\"demo\"}").expect("manifest");
        std::fs::write(root.join("package-lock.json"), "{\"original\":true}").expect("lock");
        let project = Project {
            root: root.clone(),
            kind: Npm::ID,
            manifest: root.join("package.json"),
        };

        let captured = tool()
            .mutation_journal(&project, &Plan::default())
            .await
            .expect("journal");
        std::fs::write(root.join("package-lock.json"), "{\"mutated\":true}").expect("mutate lock");
        captured.restore(&project.root).expect("restore");

        let restored = std::fs::read_to_string(root.join("package-lock.json")).expect("read lock");
        assert_eq!(restored, "{\"original\":true}");
    }
}
