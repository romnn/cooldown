use super::{ProjectCtx, ProjectProgress, RunOpts, Workspace};
use cooldown_core::{FetchContext, ResolveContext, ToolRead};

/// The common read-side context for one scoped project: adapter, label, fetch context, resolve
/// context, and the project's progress block.
pub(crate) struct ReadProjectCtx<'a> {
    pub(crate) adapter: &'a dyn ToolRead,
    pub(crate) project_label: String,
    pub(crate) fetch: FetchContext<'a>,
    pub(crate) resolve: ResolveContext<'a>,
    pub(crate) progress: ProjectProgress,
}

impl Workspace {
    /// `progress` is the project's open block: the caller opens it, so a caller that already
    /// has one (`explain` reusing `outdated`'s runner) does not open a second.
    pub(crate) fn read_project_ctx<'a>(
        &'a self,
        pctx: &'a ProjectCtx,
        opts: &'a RunOpts,
        progress: ProjectProgress,
    ) -> Option<ReadProjectCtx<'a>> {
        let adapter = self.adapter(pctx.tool)?;
        Some(ReadProjectCtx {
            adapter,
            project_label: pctx.rel_path.to_string(),
            fetch: Workspace::fetch_context(pctx, opts),
            resolve: Workspace::resolve_ctx(pctx, opts),
            progress,
        })
    }
}
