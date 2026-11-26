//! The Gradle [`Tool`](cooldown_core::Tool): the resolved graph from `gradle.lockfile`, the direct
//! split from `build.gradle`, and Maven Central publish times under the Maven version model.

use crate::lock;
use crate::registry::{MAVEN_CENTRAL, MavenCentral};
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{Driver, skipped_on_apply_error, verify_current_unknown};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    LockVerifyReport, NativePolicyLayer, PackageId, PackageRegistry, Plan, PreparedMutation,
    Project, ProjectMarker, ProjectMutationJournal, Release, ReleaseFetcher, ReleaseOrder, Result,
    ToolId, ToolRead, ToolWrite, UpdateKind, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

/// The [`ToolId`] for the Gradle adapter (`"gradle"`).
pub const GRADLE_ID: ToolId = ToolId("gradle");

/// The lock Gradle's dependency locking writes, which is the project marker and the resolved graph.
const LOCKFILE: &str = "gradle.lockfile";

/// The build script whose dependency literals name the direct dependencies.
const MANIFEST: &str = "build.gradle";

/// The Gradle implementation of the [`Tool`](cooldown_core::Tool) port.
pub struct GradleTool {
    registry: MavenCentral,
    driver: Driver,
}

impl GradleTool {
    /// Creates the adapter from a configured [`MavenCentral`] client.
    #[must_use]
    pub fn new(registry: MavenCentral) -> Self {
        GradleTool {
            registry,
            driver: Driver::new("gradle"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`MavenCentral`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        GradleTool::new(MavenCentral::new(http))
    }
}

#[async_trait]
impl ToolRead for GradleTool {
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
                reason: "native declarations or pins do not contain the full resolved graph".into(),
            },
            &[
                "settings.gradle",
                "settings.gradle.kts",
                "build.gradle",
                "build.gradle.kts",
                "gradle.properties",
                ".mvn/maven.config",
                ".mvn/jvm.config",
            ],
        );
        cooldown_adapter_util::evidence::ancestors(&mut evidence, &project.root, &[]);
        cooldown_adapter_util::evidence::environment(
            &mut evidence,
            &[
                "JAVA_HOME",
                "MAVEN_OPTS",
                "MAVEN_ARGS",
                "GRADLE_USER_HOME",
                "GRADLE_OPTS",
            ],
        );
        Ok(evidence)
    }

    fn id(&self) -> ToolId {
        GRADLE_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
            advisory_ecosystem: Some("Maven"),
            honors_generated_members: false,
        }
    }

    fn project_marker(&self) -> cooldown_core::ProjectMarker {
        ProjectMarker {
            marker: LOCKFILE,
            manifest: MANIFEST,
            alternate_manifests: &[],
            workspace_root: false,
        }
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join(LOCKFILE))?;
        let manifest = std::fs::read_to_string(&project.manifest).unwrap_or_default();
        let direct = lock::parse_gradle_direct(&manifest);
        let mut deps = Vec::new();
        for pin in lock::parse_gradle_lock(&content) {
            let is_direct = direct.contains(&pin.name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            // No advisory identity, ever: the lock records no per-artifact repository, and the
            // routing that decides real origin lives outside the files this adapter reads —
            // `settings.gradle` `dependencyResolutionManagement` blocks and init scripts.
            // Origin evidence that cannot be positive must decline: OSV's `Maven` advisories
            // describe Maven Central artifacts, and a same-named coordinate from a private
            // repository must not inherit their shortened windows.
            // Declining only fails to shorten — the ordinary window stands, and the app layer
            // says so once per run.
            let advisory_identity = None;
            deps.push(Dependency {
                package: PackageId::new(GRADLE_ID, pin.name, Some(MAVEN_CENTRAL.to_string())),
                advisory_identity,
                current: Version::new(pin.version.clone()),
                current_quality: version::classify_quality(&pin.version),
                direct: is_direct,
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

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None)
    }

    async fn verify_lock_current(&self, _project: &Project) -> Result<LockVerifyReport> {
        Ok(verify_current_unknown(LOCKFILE))
    }
}

#[async_trait]
impl ReleaseFetcher for GradleTool {
    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        _candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let raw = self.registry.releases(&dep.package).await?;
        Ok(version::build_releases(dep.current.as_str(), raw))
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
impl ToolWrite for GradleTool {
    fn mutation_tool(&self) -> ToolId {
        GRADLE_ID
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(
            &project.root,
            [Utf8Path::new(LOCKFILE), Utf8Path::new(MANIFEST)],
        )
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, _) = mutation.parts_for(self)?;
        let mut report = ApplyReport::default();
        // Gradle has no first-class CLI to pin a single dependency to an exact version; re-writing
        // the locks re-resolves the graph within the build script's constraints.
        let args = ["dependencies", "--write-locks"].map(str::to_owned);
        for change in &plan.changes {
            match self.driver.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        let args = ["dependencies", "-q"].map(str::to_owned);
        self.driver
            .verify(&project.root, &args, "build succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advisory_ecosystem_matches_osv() {
        let cache = tempfile::tempdir().expect("cache");
        let http =
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http");
        assert_eq!(
            GradleTool::from_http(http)
                .capabilities()
                .advisory_ecosystem,
            Some("Maven")
        );
    }
}
