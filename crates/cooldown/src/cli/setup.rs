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
    let scan = discovery::scan_config(&repo_root, global.config.as_deref(), global.no_global)?;
    let cfg = scan.resolved(command_key);
    let opts = options::resolve_run_opts(global, overrides, &cfg, default_major)?;
    let adapters = detect::adapter_set(opts.offline(), opts.fresh())?;
    let projects = detect::detect_projects(
        &adapters,
        &workdir,
        &scan,
        command_key,
        opts.tools(),
        opts.respect_gitignore(),
    )?;
    let shared = policy::build_shared_layers(global)?;
    let ctxs = policy::assemble_projects(&adapters, &repo_root, projects, &shared, global).await?;

    let baseline = Baseline::load(&repo_root.join(crate::app::baseline::BASELINE_FILE))?;
    let now = jiff::Timestamp::now();
    let ws = Workspace::new(adapters, ctxs, now, baseline);
    Ok(PreparedRun {
        repo_root,
        ws,
        opts: opts.into_run_opts(),
    })
}
