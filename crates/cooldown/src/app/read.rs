use super::{ProjectCtx, RunOpts, Workspace};
use cooldown_core::{FetchContext, ResolveContext, ToolRead};

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
}
