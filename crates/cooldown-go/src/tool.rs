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
use cooldown_core::{
    ApplyReport, CandidateScope, Capabilities, DepScope, Dependency, FetchContext,
    NativePolicyLayer, Plan, Project, ProjectMarker, ProjectMutationJournal, Release, Result,
    ToolId, ToolRead, ToolWrite, VerifyReport,
};
use cooldown_registry::SharedHttp;

/// The [`ToolId`] for the Go adapter.
pub const GO_ID: ToolId = ToolId("go");

/// The Go adapter, constructed from a [`GoProxy`] (itself built over the shared HTTP layer).
pub struct GoTool {
    proxy: GoProxy,
    go: Go,
}

impl GoTool {
    /// Creates the adapter over `proxy`, using a default [`Go`] driver for resolution/apply.
    #[must_use]
    pub fn new(proxy: GoProxy) -> Self {
        GoTool {
            proxy,
            go: Go::new(),
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

    async fn releases(
        &self,
        dep: &Dependency,
        _fetch: &FetchContext<'_>,
        candidates: CandidateScope,
    ) -> Result<Vec<Release>> {
        releases::releases(&self.proxy, dep, candidates, self.registry()).await
    }

    async fn locked_release(&self, dep: &Dependency, _fetch: &FetchContext<'_>) -> Result<Release> {
        graph::locked_release(&self.proxy, dep).await
    }

    async fn native_policy(&self, _project: &Project) -> Result<Option<NativePolicyLayer>> {
        Ok(None) // Go has no native cooldown config.
    }

    async fn verify_lock_current(&self, project: &Project) -> Result<VerifyReport> {
        graph::verify_lock_current(&self.go, project).await
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
