use super::{ProjectCtx, RunOpts, Workspace};
use cooldown_core::{CandidateScope, Dependency, FetchContext, Release, ResolveContext, ToolRead};
use futures::stream::{self, StreamExt};

/// The common read-side context for one scoped project: adapter, label, fetch context, and
/// resolve context.
pub(crate) struct ReadProjectCtx<'a> {
    pub(crate) adapter: &'a dyn ToolRead,
    pub(crate) project_label: String,
    pub(crate) fetch: FetchContext<'a>,
    pub(crate) resolve: ResolveContext<'a>,
}

impl Workspace {
    pub(crate) fn read_project_ctx<'a>(
        &'a self,
        pctx: &'a ProjectCtx,
        opts: &'a RunOpts,
    ) -> Option<ReadProjectCtx<'a>> {
        let adapter = self.adapter(pctx.tool)?;
        Some(ReadProjectCtx {
            adapter,
            project_label: pctx.rel_path.to_string(),
            fetch: Workspace::fetch_context(pctx, opts),
            resolve: Workspace::resolve_ctx(pctx, opts),
        })
    }

    pub(crate) async fn fetch_locked_releases(
        &self,
        adapter: &dyn ToolRead,
        deps: Vec<Dependency>,
        fetch: &FetchContext<'_>,
        fanout: usize,
    ) -> Vec<(Dependency, cooldown_core::Result<Release>)> {
        stream::iter(deps)
            .map(|dep| async {
                let result = adapter.locked_release(&dep, fetch).await;
                (dep, result)
            })
            .buffer_unordered(fanout)
            .collect()
            .await
    }

    pub(crate) async fn fetch_candidate_releases(
        &self,
        adapter: &dyn ToolRead,
        deps: Vec<Dependency>,
        fetch: &FetchContext<'_>,
        candidate_scope: CandidateScope,
        fanout: usize,
    ) -> Vec<(Dependency, cooldown_core::Result<Vec<Release>>)> {
        stream::iter(deps)
            .map(|dep| async {
                let result = adapter.releases(&dep, fetch, candidate_scope).await;
                (dep, result)
            })
            .buffer_unordered(fanout)
            .collect()
            .await
    }
}
