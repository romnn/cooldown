//! The Elixir/Hex [`Tool`]: detection by `mix.lock`, the resolved graph from that lock, the direct
//! split from `mix.exs`, and hex.pm publish times. The core owns the verdict; `mix` is driven only
//! to apply.

use crate::lock;
use crate::registry::{HEXPM, Hex};
use crate::version;
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
    ReleaseOrder, ReleaseQuality, ResolveInputs, Result, ToolId, ToolRead, ToolWrite, UpdateKind,
    VerifyReport, Version,
};
use cooldown_registry::SharedHttp;

/// The [`ToolId`] for the Elixir/Hex adapter (`"hex"`).
pub const HEX_ID: ToolId = ToolId("hex");

/// The Elixir/Hex implementation of the [`Tool`] port.
pub struct HexTool {
    registry: Hex,
    mix: Driver,
}

impl HexTool {
    /// Creates the adapter from a configured [`Hex`] client.
    #[must_use]
    pub fn new(registry: Hex) -> Self {
        HexTool {
            registry,
            mix: Driver::new("mix"),
        }
    }

    /// Creates the adapter from a shared HTTP client, building the [`Hex`] client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        HexTool::new(Hex::new(http))
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
impl ToolRead for HexTool {
    fn id(&self) -> ToolId {
        HEX_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: false,
            has_incompatible: false,
            has_dist_tags: false,
            can_sync: true,
            artifact_granular: false,
            advisory_ecosystem: Some("Hex"),
        }
    }

    fn project_detection(&self) -> cooldown_core::ProjectDetection {
        cooldown_core::ProjectDetection::Primary(ProjectMarker {
            lockfile: "mix.lock",
            manifest: "mix.exs",
            alternate_manifests: &[],
            workspace_root: false,
        })
    }

    fn classify_update_kind(&self, from: &str, to: &str) -> Option<UpdateKind> {
        version::classify_kind(from, to)
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        let content = std::fs::read_to_string(project.root.join("mix.lock"))?;
        let manifest = std::fs::read_to_string(&project.manifest).unwrap_or_default();
        let direct = lock::parse_direct(&manifest);
        let ceilings = lock::graph_ceilings(&content);

        let mut deps = Vec::new();
        for resolved in lock::parse_resolved(&content) {
            let is_direct = direct.contains(&resolved.name);
            if scope == DepScope::Direct && !is_direct {
                continue;
            }
            // Only a hexpm-repo entry is the package OSV's `Hex` ecosystem describes; a private
            // Hex repo's package merely shares the name.
            let advisory_identity = resolved.hexpm.then(|| resolved.name.clone());
            deps.push(Dependency {
                package: PackageId::new(HEX_ID, resolved.name.clone(), Some(HEXPM.to_string())),
                advisory_identity,
                current: Version::new(resolved.version.clone()),
                current_quality: classify_quality(&resolved.version),
                direct: is_direct,
                artifacts: Vec::new(),
                graph_floor: None,
                // A requirer pinning this dep `== X` caps it at its resolved version. The active
                // check (pin equals the resolved version) records the canonical resolved form so it
                // matches a fetched release, mirroring the uv adapter.
                graph_ceiling: ceilings.get(&resolved.name).and_then(|pin| {
                    version::compare(pin, &resolved.version)
                        .is_eq()
                        .then(|| Version::new(resolved.version.clone()))
                }),
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
        Ok(verify_current_unknown("mix.lock"))
    }
}

#[async_trait]
impl ReleaseFetcher for HexTool {
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
impl ToolWrite for HexTool {
    fn mutation_tool(&self) -> ToolId {
        HEX_ID
    }

    // `mix deps.update` fetches and compiles the dependency into `deps/` and `_build/`, which
    // `MIX_DEPS_PATH` and `MIX_BUILD_PATH` can point at one shared place.
    fn mutation_installs(&self) -> bool {
        true
    }

    fn resolve_inputs(&self) -> ResolveInputs {
        // `mix deps.get`/`deps.update` COMPILES `mix.exs` (Elixir) to resolve, and a project's
        // `mix.exs` frequently reads sibling source (a `@version` from a module, `Code.require_file`,
        // umbrella `apps/*/mix.exs`), so the throwaway probe copy must carry `.ex`/`.exs` source.
        ResolveInputs {
            source_extensions: &["ex", "exs"],
            ..ResolveInputs::DEFAULT
        }
    }

    async fn mutation_journal(
        &self,
        project: &Project,
        _plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        ProjectMutationJournal::capture(&project.root, [Utf8Path::new("mix.lock")])
    }

    async fn apply(&self, mutation: &PreparedMutation) -> Result<ApplyReport> {
        let (project, plan, _) = mutation.parts_for(self)?;
        let mut report = ApplyReport::default();
        for change in &plan.changes {
            // `mix deps.update <dep>` re-resolves that dependency within the manifest's constraints.
            let args = vec!["deps.update".to_string(), change.package.name.clone()];
            match self.mix.run(&project.root, &args).await {
                Ok(()) => report.applied.push(change.clone()),
                Err(e) => report.skipped.push(skipped_on_apply_error(change, e)?),
            }
        }
        Ok(report)
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.mix
            .verify(
                &project.root,
                &["deps.get".to_string()],
                "mix deps.get succeeded",
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[test]
    fn advisory_ecosystem_matches_osv() {
        let cache = tempfile::tempdir().expect("cache");
        let tool = HexTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );
        // The apply fetches and compiles into `deps/` and `_build/`, so it takes the environment
        // turn.
        assert!(tool.mutation_installs());
        assert_eq!(tool.capabilities().advisory_ecosystem, Some("Hex"));
    }

    #[tokio::test]
    async fn dependencies_split_uses_mix_exs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8");
        std::fs::write(
            root.join("mix.lock"),
            "%{\n  \"jason\": {:hex, :jason, \"1.4.0\", \"a\", [:mix], [], \"hexpm\", \"b\"},\n  \"mime\": {:hex, :mime, \"1.6.0\", \"c\", [:mix], [], \"hexpm\", \"d\"},\n}\n",
        )
        .expect("lock");
        std::fs::write(
            root.join("mix.exs"),
            "defp deps do\n  [\n    {:jason, \"~> 1.4\"}\n  ]\nend\n",
        )
        .expect("mix.exs");
        let project = Project {
            root: root.clone(),
            kind: HEX_ID,
            manifest: root.join("mix.exs"),
            exclude_newer: None,
        };
        let cache = tempfile::tempdir().expect("cache");
        let tool = HexTool::from_http(
            SharedHttp::new(cache.path(), cooldown_registry::HttpOptions::default()).expect("http"),
        );

        let direct = tool
            .dependencies(&project, DepScope::Direct)
            .await
            .expect("direct");
        assert_eq!(direct.len(), 1);
        assert_eq!(direct[0].package.name, "jason");

        let graph = tool
            .dependencies(&project, DepScope::Graph)
            .await
            .expect("graph");
        assert_eq!(graph.len(), 2);
    }
}
