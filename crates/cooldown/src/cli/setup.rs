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
    let mut cfg = scan.resolved(command_key);
    // CLI `--exclude-folders`/`--exclude-packages` are the highest-precedence layer: they replace the
    // resolved `[global]`/`[<command>]` lists (per-tool `[tool.*]` excludes, carried on `scan`, still
    // apply). Applied to the resolved `cfg` — not the shared `scan` — so it cannot leak across other
    // commands' resolution; both detection and member-filtering read the override from `cfg`.
    cfg.override_excludes(&global.exclude_folders, &global.exclude_packages)?;
    let invocation = options::resolve_invocation(global, overrides, &cfg, default_major)?;
    let adapters = detect::adapter_set(invocation.offline(), invocation.fresh())?;
    let projects = detect::detect_projects(
        &adapters,
        &workdir,
        &scan,
        &cfg.exclude_folders,
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
    let mut opts = invocation.into_run_opts();
    // The scan-exclude globs also filter workspace-member dependencies (folders by member path,
    // packages by member name), so carry both global/command and per-tool excludes into the run.
    opts.exclude_folders = cfg.exclude_folders;
    opts.exclude_packages = cfg.exclude_packages;
    opts.exclude_folders_by_tool = scan.tool_exclude_folders;
    opts.exclude_packages_by_tool = scan.tool_exclude_packages;
    Ok(PreparedRun {
        repo_root,
        ws,
        opts,
    })
}
