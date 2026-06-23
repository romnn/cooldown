//! The Ruby/Bundler [`Tool`]: detection by `Gemfile.lock`, the resolved graph read from that lock,
//! and `RubyGems` publish times. The core owns the verdict; `bundle` is driven only to apply.

use crate::lock;
use crate::registry::{RUBYGEMS, RubyGems};
use crate::version;
use async_trait::async_trait;
use camino::Utf8Path;
use cooldown_adapter_util::{
    Driver, build_registry_releases, skipped_on_apply_error, verify_current_report,
};
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, PackageId, PackageRegistry, Plan, Project, ProjectMarker,
    ProjectMutationJournal, RawRelease, Release, ReleaseFetcher, ReleaseOrder, ReleaseQuality,
    ResolveInputs, Result, ToolId, ToolRead, ToolWrite, VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

/// The [`ToolId`] for the Ruby/Bundler adapter (`"bundler"`).
pub const BUNDLER_ID: ToolId = ToolId("bundler");

/// The Ruby/Bundler implementation of the [`Tool`] port.
pub struct BundlerTool {
    registry: RubyGems,
    bundle: Driver,
}

impl BundlerTool {
    /// Creates the adapter from a configured [`RubyGems`] client.
    #[must_use]
    pub fn new(registry: RubyGems) -> Self {
        BundlerTool {
            registry,
            bundle: Driver::new("bundle"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`RubyGems`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        BundlerTool::new(RubyGems::new(http))
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
impl ToolRead for BundlerTool {
    fn id(&self) -> ToolId {
        BUNDLER_ID
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
            lockfile: "Gemfile.lock",
            manifest: "Gemfile",
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join("Gemfile.lock"))?;
        let direct = lock::parse_direct(&content);
        let ceilings = lock::graph_ceilings(&content);
        let mut deps = Vec::new();
        for (name, ver) in lock::parse_resolved(&content) {
            let is_direct = direct.contains(&name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            deps.push(Dependency {
                package: PackageId::new(BUNDLER_ID, name.clone(), Some(RUBYGEMS.to_string())),
                current: Version::new(ver.clone()),
                current_quality: classify_quality(&ver),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                // A requirer pinning this gem `(= X)` caps it at its resolved version. The active
                // check (pin equals the resolved version) records the canonical resolved form so it
                // matches a fetched release, mirroring the uv adapter.
                graph_ceiling: ceilings.get(&name).and_then(|pin| {
                    version::compare(pin, &ver)
                        .is_eq()
                        .then(|| Version::new(ver.clone()))
                }),
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
            "Gemfile.lock taken as current",
            "Gemfile.lock is stale",
        ))
    }
}

#[async_trait]
impl ReleaseFetcher for BundlerTool {
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
}

#[async_trait]
impl ToolWrite for BundlerTool {
    fn resolve_inputs(&self) -> ResolveInputs {
        // A `gemspec` directive in the Gemfile makes bundler load the project's `*.gemspec` (Ruby),
        // which typically `require_relative`s a `lib/**/version.rb`. The throwaway probe copy must
        // carry both, so `.gemspec` and `.rb` source are included; a plain Gemfile ignores them.
        ResolveInputs {
            source_extensions: &["rb", "gemspec"],
            ..ResolveInputs::DEFAULT
        }
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        Ok(ProjectMutationJournal {
            files: vec![
                ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("Gemfile"))?,
                ProjectMutationJournal::capture_file(&project.root, Utf8Path::new("Gemfile.lock"))?,
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
            // `bundle update --conservative <gem>` re-resolves just this gem (and its requirements)
            // without disturbing the rest of the lock.
            let args = vec![
                "update".to_string(),
                "--conservative".to_string(),
                change.package.name.clone(),
            ];
            match self.bundle.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.bundle
            .verify(
                &project.root,
                &["lock".to_string()],
                "bundle lock succeeded",
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[tokio::test]
    async fn dependencies_split_direct_from_transitive() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("Gemfile.lock"),
            "GEM\n  remote: https://rubygems.org/\n  specs:\n    rake (13.0.6)\n    racc (1.6.0)\n\nDEPENDENCIES\n  rake (~> 13.0)\n",
        )
        .expect("write lock");
        let project = Project {
            root: root.clone(),
            kind: BUNDLER_ID,
            manifest: root.join("Gemfile"),
            exclude_newer: None,
        };
        let cache = tempfile::tempdir().expect("cache");
        let tool = BundlerTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );

        let direct = tool
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].package.name, "rake");
        assert_eq!(direct[0].package.registry.as_deref(), Some(RUBYGEMS));

        let graph = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph");
        assert_eq!(graph.len(), 2);
    }
}
