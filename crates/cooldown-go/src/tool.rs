//! The Go [`Tool`]: a thin adapter shell over Go-specific graph reading, release discovery, and
//! `go`-driven apply/build helpers.

mod apply;
mod graph;
mod releases;
#[cfg(test)]
mod tests;

use crate::gocmd::Go;
use crate::mutation;
use crate::proxy::GoProxy;
use async_trait::async_trait;
use camino::Utf8PathBuf;
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, Plan, Project, ProjectMarker, ProjectMutationJournal, Release,
    ReleaseFetcher, Result, ToolId, ToolRead, ToolWrite, VerifyReport,
};
use cooldown_registry::SharedHttp;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The [`ToolId`] for the Go adapter.
pub const GO_ID: ToolId = ToolId("go");

/// Go's authoritative available-version lists for one project: module path → the versions Go
/// itself would consider (`go list -m -versions`).
type GoVersions = HashMap<String, Vec<String>>;

/// The Go adapter, constructed from a [`GoProxy`] (itself built over the shared HTTP layer).
pub struct GoTool {
    proxy: GoProxy,
    go: Go,
    /// Go's authoritative per-module version lists (`go list -m -versions`), cached per project
    /// root. A module's version list does not depend on which project queries it, but the `all`
    /// query that seeds the cache is per-project — so the cache is keyed by project root and
    /// populated once, on the first release lookup for that project.
    version_lists: Mutex<HashMap<Utf8PathBuf, Arc<GoVersions>>>,
}

impl GoTool {
    /// Creates the adapter over `proxy`, using a default [`Go`] driver for resolution/apply.
    #[must_use]
    pub fn new(proxy: GoProxy) -> Self {
        GoTool {
            proxy,
            go: Go::new(),
            version_lists: Mutex::new(HashMap::new()),
        }
    }

    /// Convenience: build the proxy from `GOPROXY` over the shared HTTP client.
    #[must_use]
    pub fn from_http(http: SharedHttp) -> Self {
        GoTool::new(GoProxy::from_env(http))
    }

    fn registry(&self) -> Option<String> {
        self.proxy.registry_name()
    }

    /// Go's authoritative version list for every module in `project`, fetched once via
    /// `go list -m -versions` and cached. If that probe fails (e.g. an incomplete module context),
    /// returns an empty map so release discovery degrades to the proxy's own `@v/list` rather than
    /// failing the command; the core's no-major guard still prevents major jumps in that case.
    async fn project_version_lists(&self, project: &Project) -> Arc<GoVersions> {
        let mut cache = self.version_lists.lock().await;
        if let Some(map) = cache.get(&project.root) {
            return Arc::clone(map);
        }
        let map = Arc::new(
            self.go
                .list_versions(&project.root)
                .await
                .unwrap_or_default(),
        );
        cache.insert(project.root.clone(), Arc::clone(&map));
        map
    }
}

#[async_trait]
impl ToolRead for GoTool {
    fn id(&self) -> ToolId {
        GO_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            has_pseudo: true,
            has_incompatible: true,
            has_dist_tags: false,
            can_sync: false,
            artifact_granular: false,
        }
    }

    fn project_marker(&self) -> ProjectMarker {
        // Go multi-module repos nest independent modules, so every `go.mod` is its own project
        // (not a workspace root).
        ProjectMarker {
            lockfile: "go.mod",
            manifest: "go.mod",
            workspace_root: false,
        }
    }

    async fn dependencies(&self, project: &Project, scope: DepScope) -> Result<Vec<Dependency>> {
        graph::dependencies(&self.go, self.registry(), project, scope).await
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None) // Go has no native cooldown config.
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport> {
        graph::verify_lock_current(&self.go, project).await
    }
}

#[async_trait]
impl ReleaseFetcher for GoTool {
    async fn releases(
        &self,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        let version_lists = self.project_version_lists(fetch.project).await;
        let go_versions = version_lists.get(&dep.package.name).map(Vec::as_slice);
        releases::releases(&self.proxy, dep, candidates, self.registry(), go_versions).await
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        graph::locked_release(&self.proxy, dep).await
    }

    fn releases_are_project_scoped(&self) -> bool {
        // `releases` resolves candidates from `go list -m -versions` run in the asking module's
        // root (and uses `dep.graph_floor`), so the answer differs per Go module — each `go.mod` is
        // its own project. The cache must key by project, not share across modules.
        true
    }
}

#[async_trait]
impl ToolWrite for GoTool {
    async fn mutation_journal(
        &self,
        project: &Project,
        plan: &Plan,
    ) -> Result<ProjectMutationJournal> {
        mutation::mutation_journal(&project.root, plan)
    }

    async fn apply(
        &self,
        project: &Project,
        plan: &Plan,
        journal: &ProjectMutationJournal,
    ) -> Result<ApplyReport> {
        apply::apply(&self.go, project, plan, journal).await
    }

    async fn build(&self, project: &Project) -> Result<VerifyReport> {
        self.go.build(&project.root).await
    }
}
