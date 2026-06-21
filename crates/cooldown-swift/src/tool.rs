//! The Swift Package Manager [`Tool`]: detection by `Package.resolved`, the resolved graph from
//! that pin file, and GitHub Releases publish times. The core owns the verdict; `swift` is driven
//! only to apply.

use crate::lock;
use crate::registry::{GITHUB, GitHubReleases};
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

/// The [`ToolId`] for the Swift Package Manager adapter (`"swift"`).
pub const SWIFT_ID: ToolId = ToolId("swift");

/// The Swift Package Manager implementation of the [`Tool`] port.
pub struct SwiftTool {
    registry: GitHubReleases,
    swift: Driver,
}

impl SwiftTool {
    /// Creates the adapter from a configured [`GitHubReleases`] client.
    #[must_use]
    pub fn new(registry: GitHubReleases) -> Self {
        SwiftTool {
            registry,
            swift: Driver::new("swift"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`GitHubReleases`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        SwiftTool::new(GitHubReleases::new(http))
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
impl ToolRead for SwiftTool {
    fn id(&self) -> ToolId {
        SWIFT_ID
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
            lockfile: "Package.resolved",
            manifest: "Package.swift",
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        // Package.resolved is the resolved graph and does not mark which pins are direct, so cross-
        // reference the manifest: a pin declared via `.package(url: …)` in Package.swift is direct,
        // the rest are transitive. If the manifest is absent or names no GitHub deps, fall back to
        // treating every pin as direct (the resolved graph is all we have). The direct/transitive
        // split lets `--major` rewrite direct deps across a major while leaving transitives capped.
        let content = std::fs::read_to_string(project.root.join("Package.resolved"))?;
        let direct = std::fs::read_to_string(project.root.join("Package.swift"))
            .ok()
            .map(|manifest| lock::direct_repos(&manifest))
            .filter(|repos| !repos.is_empty());
        let mut deps = Vec::new();
        for (repo, ver) in lock::parse_resolved(&content)? {
            let is_direct = direct
                .as_ref()
                .is_none_or(|repos| repos.contains(&repo.to_lowercase()));
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(SWIFT_ID, repo, Some(GITHUB.to_string())),
                current: Version::new(ver.clone()),
                current_quality: classify_quality(&ver),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                members: Vec::new(),
                pinned: false,
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
            "Package.resolved taken as current",
            "Package.resolved is stale",
        ))
    }
}

#[async_trait]
impl ToolWrite for SwiftTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal {
            files: vec![ProjectMutationJournal::capture_file(
                &project.root,
                Utf8Path::new("Package.resolved"),
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
            // `swift package update <identity>` re-resolves one dependency within Package.swift's
            // version constraints; the identity is the repository's basename.
            let identity = change
                .package
                .name
                .rsplit('/')
                .next()
                .unwrap_or(&change.package.name);
            let args = vec!["package".into(), "update".into(), identity.to_string()];
            match self.swift.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.swift
            .verify(&project.root, &["build".into()], "swift build succeeded")
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[tokio::test]
    async fn dependencies_read_github_pins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("Package.resolved"),
            r#"{ "pins": [ { "identity": "swift-log", "location": "https://github.com/apple/swift-log.git", "state": { "version": "1.4.0" } } ], "version": 3 }"#,
        )
        .expect("resolved");
        let project = Project {
            root: root.clone(),
            kind: SWIFT_ID,
            manifest: root.join("Package.swift"),
        };
        let cache = tempfile::tempdir().expect("cache");
        let tool = SwiftTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );

        let deps = tool
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("deps");
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].package.name, "apple/swift-log");
        assert_eq!(deps[0].package.registry.as_deref(), Some(GITHUB));
    }
}
