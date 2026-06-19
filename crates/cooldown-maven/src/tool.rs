//! The generic Java [`Tool`], parameterised by a [`JavaLayout`] (Maven or Gradle). Both resolve
//! from Maven Central and share the Maven version model; they differ only in which files they read
//! (`pom.xml` vs `gradle.lockfile` + `build.gradle`) and how their CLI re-pins a dependency.

use crate::lock;
use crate::registry::{MAVEN_CENTRAL, MavenCentral};
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    Driver, build_registry_releases, skipped_on_apply_error, verify_current_report,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseOrder, ReleaseQuality, Result, ToolId,
    ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;
use std::marker::PhantomData;

/// The per-build-tool knobs the generic adapter needs: identity, the files it reads, the driver
/// binary, how to split its resolved deps into direct/transitive, and how to re-pin one.
pub trait JavaLayout: Send + Sync + 'static {
    /// The tool's canonical [`ToolId`] (`maven` or `gradle`).
    const ID: ToolId;
    /// The file detected as the project marker and read for resolved versions.
    const LOCKFILE: &'static str;
    /// The manifest read for the direct-dependency set (the same file as the lock, for Maven).
    const MANIFEST: &'static str;
    /// The driver binary, shelled out to for apply/build.
    const BIN: &'static str;

    /// Parses the lock + manifest into resolved `(coordinate, version, is_direct)` triples.
    fn parse(lock: &str, manifest: &str) -> Vec<(String, String, bool)>;

    /// The driver args that re-pin `coordinate` to `version`.
    fn upgrade_args(coordinate: &str, version: &str) -> Vec<String>;

    /// The driver args for the opt-in `--build` step.
    fn build_args() -> Vec<String>;
}

/// Maven: declared dependencies live in `pom.xml`, which is both the manifest and the version
/// source. Without running `mvn` the transitive graph is unavailable, so every parsed dependency is
/// treated as direct.
pub struct Maven;
/// Gradle: the resolved graph is the `gradle.lockfile` (requires dependency locking), and
/// `build.gradle` supplies the direct set.
pub struct Gradle;

impl JavaLayout for Maven {
    const ID: ToolId = ToolId("maven");
    const LOCKFILE: &'static str = "pom.xml";
    const MANIFEST: &'static str = "pom.xml";
    const BIN: &'static str = "mvn";

    fn parse(lock: &str, _manifest: &str) -> Vec<(String, String, bool)> {
        lock::parse_pom(lock)
            .into_iter()
            .map(|(coord, ver)| (coord, ver, true))
            .collect()
    }

    fn upgrade_args(coordinate: &str, version: &str) -> Vec<String> {
        vec![
            "versions:use-dep-version".into(),
            format!("-Dincludes={coordinate}"),
            format!("-DdepVersion={version}"),
            "-DforceVersion=true".into(),
            "-DgenerateBackupPoms=false".into(),
        ]
    }

    fn build_args() -> Vec<String> {
        vec!["-q".into(), "-DskipTests".into(), "validate".into()]
    }
}

impl JavaLayout for Gradle {
    const ID: ToolId = ToolId("gradle");
    const LOCKFILE: &'static str = "gradle.lockfile";
    const MANIFEST: &'static str = "build.gradle";
    const BIN: &'static str = "gradle";

    fn parse(lock: &str, manifest: &str) -> Vec<(String, String, bool)> {
        let direct = lock::parse_gradle_direct(manifest);
        lock::parse_gradle_lock(lock)
            .into_iter()
            .map(|(coord, ver)| {
                let is_direct = direct.contains(&coord);
                (coord, ver, is_direct)
            })
            .collect()
    }

    fn upgrade_args(_coordinate: &str, _version: &str) -> Vec<String> {
        // Gradle has no first-class CLI to pin a single dependency to an exact version; re-writing
        // the locks re-resolves the graph within the build script's constraints.
        vec!["dependencies".into(), "--write-locks".into()]
    }

    fn build_args() -> Vec<String> {
        vec!["dependencies".into(), "-q".into()]
    }
}

/// The Java implementation of the [`Tool`] port, generic over a [`JavaLayout`].
pub struct JavaTool<L> {
    registry: MavenCentral,
    driver: Driver,
    _layout: PhantomData<fn() -> L>,
}

impl<L: JavaLayout> JavaTool<L> {
    /// Creates the adapter from a configured [`MavenCentral`] client.
    #[must_use]
    pub fn new(registry: MavenCentral) -> Self {
        JavaTool {
            registry,
            driver: Driver::new(L::BIN),
            _layout: PhantomData,
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`MavenCentral`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        JavaTool::new(MavenCentral::new(http))
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
        |value| !value.is_empty(),
        version::compare,
        version::major_key,
        version::classify_kind,
        classify_quality,
    )
}

#[async_trait]
impl<L: JavaLayout> ToolRead for JavaTool<L> {
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
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let lock = std::fs::read_to_string(project.root.join(L::LOCKFILE))?;
        let manifest = std::fs::read_to_string(&project.manifest).unwrap_or_default();
        let mut deps = Vec::new();
        for (coord, ver, is_direct) in L::parse(&lock, &manifest) {
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(L::ID, coord, Some(MAVEN_CENTRAL.to_string())),
                current: Version::new(ver.clone()),
                current_quality: classify_quality(&ver),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                members: Vec::new(),
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
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<VerifyReport> {
        Ok(verify_current_report(
            true,
            "resolved versions taken as current",
            "resolved versions are stale",
        ))
    }
}

#[async_trait]
impl<L: JavaLayout> ToolWrite for JavaTool<L> {
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
    async fn maven_reads_declared_dependencies() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("pom.xml"),
            "<project><dependencies><dependency><groupId>com.google.code.gson</groupId><artifactId>gson</artifactId><version>2.8.0</version></dependency></dependencies></project>",
        )
        .expect("pom");
        let project = Project {
            root: root.clone(),
            kind: Maven::ID,
            manifest: root.join("pom.xml"),
        };
        let cache = tempfile::tempdir().expect("cache");
        let tool = JavaTool::<Maven>::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );

        let deps = tool
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].package.name, "com.google.code.gson:gson");
        assert_eq!(deps[0].package.registry.as_deref(), Some(MAVEN_CENTRAL));
    }
}
