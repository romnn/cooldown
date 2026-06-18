mod detect;
mod options;
mod policy;

use super::{CliOverrides, GlobalArgs};
use crate::app::{Baseline, Workspace};
use crate::discovery;
use camino::Utf8PathBuf;
use cooldown_core::CoreError;

pub(crate) struct PreparedRun {
    pub(crate) repo_root: Utf8PathBuf,
    pub(crate) ws: Workspace,
    pub(crate) opts: crate::app::RunOpts,
}

pub(crate) async fn prepare_run(
    global: &GlobalArgs,
    overrides: &CliOverrides,
    command_key: &str,
    default_major: bool,
) -> Result<PreparedRun, CoreError> {
    let workdir = detect::workdir(global)?;
    let repo_root = discovery::find_repo_root(&workdir);
    let configs =
        discovery::ConfigSources::load(&repo_root, global.config.as_deref(), global.no_global)?;
    let scan = configs.scan_config()?;
    let cfg = scan.resolved(command_key);
    let invocation = options::resolve_invocation(global, overrides, &cfg, default_major)?;
    let adapters = detect::adapter_set(invocation.offline(), invocation.fresh())?;
    let projects = detect::detect_projects(
        &adapters,
        &workdir,
        &scan,
        command_key,
        invocation.tools(),
        invocation.respect_gitignore(),
    )?;
    let shared = policy::build_shared_layers(&configs, &invocation)?;
    let ctxs = policy::assemble_projects(
        &adapters,
        &repo_root,
        projects,
        &configs,
        &shared,
        &invocation,
        global.no_native,
    )
    .await?;

    let baseline = Baseline::load(&repo_root.join(crate::app::baseline::BASELINE_FILE))?;
    let now = jiff::Timestamp::now();
    let ws = Workspace::new(adapters, ctxs, now, baseline);
    Ok(PreparedRun {
        repo_root,
        ws,
        opts: invocation.into_run_opts(),
    })
}
