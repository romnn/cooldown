mod baseline;
mod common;
mod report;

use super::{Command, present};
use crate::app::{Exit, RunOpts, Workspace};
use camino::Utf8Path;
use cooldown_core::CoreError;

pub(crate) struct CommandContext<'a> {
    pub(crate) ws: &'a Workspace,
    pub(crate) opts: &'a RunOpts,
    pub(crate) repo_root: &'a Utf8Path,
    pub(crate) color: bool,
    pub(crate) generated_at: &'a str,
}

pub(crate) async fn dispatch(command: Command, ctx: CommandContext<'_>) -> Result<Exit, CoreError> {
    match command {
        Command::Outdated { .. } => report::run_outdated(&ctx).await,
        // The per-command `--transitive` / `--downgrade-pinned` values flow through `RunOpts` via the
        // `CliOverrides` capture, so the variant fields are not read here.
        Command::Check { .. } => report::run_check(&ctx).await,
        Command::Upgrade => report::run_upgrade(&ctx).await,
        Command::Fix { .. } => report::run_fix(&ctx).await,
        Command::Explain { package } => report::run_explain(&ctx, &package).await,
        Command::Config => report::run_config(&ctx),
        Command::Sync => report::run_sync(&ctx).await,
        Command::Baseline { prune } => baseline::run_baseline(&ctx, prune).await,
        #[allow(
            clippy::unreachable,
            reason = "schema/init are dispatched before any workspace exists"
        )]
        Command::Schema | Command::Init => unreachable!("handled earlier"),
    }
}

pub(crate) fn no_tool_json(command: &'static str) -> Result<String, CoreError> {
    present::no_tool_json(command)
}
