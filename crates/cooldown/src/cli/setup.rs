mod detect;
mod options;
mod policy;

use super::{CliOverrides, GlobalArgs};
use crate::app::{Baseline, Clock, FixedClock, SystemClock, Workspace};
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
    let scan_root = scan_root_for(&workdir, &repo_root);
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
    options::reject_offline_dry_run(command_key, invocation.dry_run(), invocation.offline())?;
    let adapters = detect::adapter_set(
        invocation.offline(),
        invocation.fresh(),
        invocation.concurrency(),
    )?;
    let projects = detect::detect_projects(
        &adapters,
        &scan_root,
        &scan,
        &cfg.exclude_folders,
        invocation.tools(),
        invocation.respect_gitignore(),
    )?;
    let shared = policy::build_shared_layers(&configs, &invocation)?;
    // The evaluation clock is a port (`Clock`): real runs read the system clock, while `--now`
    // (debug builds only) injects a fixed instant so the README screenshots regenerate reproducibly.
    // Sample it once here so every dependency in the run is judged against the same "now" — and so
    // each project's resolution cutoff (`now - window`) is computed against the same instant.
    let now = match global.now_override()? {
        Some(instant) => FixedClock::new(instant).now(),
        None => SystemClock.now(),
    };
    let assembly = policy::ProjectAssembly {
        adapters: &adapters,
        repo_root: &repo_root,
        configs: &configs,
        shared: &shared,
        invocation: &invocation,
        no_native: global.no_native,
        now,
    };
    let ctxs = policy::assemble_projects(&assembly, projects).await?;
    // The repo-root cascade (no native layer) lets `sync` resolve a repo-wide window once for
    // repo-scoped adapters (uv's root `uv.toml`) without borrowing any project's layers.
    let repo_layers = policy::repo_layers(&configs, &shared, &repo_root)?;

    let baseline = Baseline::load(&repo_root.join(crate::app::baseline::BASELINE_FILE))?;
    let ws = Workspace::new(
        adapters,
        ctxs,
        now,
        baseline,
        repo_root.clone(),
        repo_layers,
    );
    let mut opts = invocation.into_run_opts();
    if workdir != scan_root {
        opts.source_dir = Some(workdir);
    }
    // The scan-exclude globs also filter workspace-member dependencies (folders by member path,
    // packages by member name), so carry both global/command and per-tool excludes into the run.
    opts.exclude_folders = cfg.exclude_folders;
    opts.exclude_packages = cfg.exclude_packages;
    opts.exclude_folders_by_tool = scan.tool_exclude_folders;
    opts.exclude_packages_by_tool = scan.tool_exclude_packages;
    opts.compile_excludes()?;
    Ok(PreparedRun {
        repo_root,
        ws,
        opts,
    })
}

fn repo_root_is_anchored(repo_root: &camino::Utf8Path) -> bool {
    repo_root.join(".git").exists() || repo_root.join(discovery::CONFIG_FILE).is_file()
}

fn scan_root_for(workdir: &camino::Utf8Path, repo_root: &camino::Utf8Path) -> Utf8PathBuf {
    if repo_root_is_anchored(repo_root) && workdir.starts_with(repo_root) {
        repo_root.to_owned()
    } else {
        workdir.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;

    fn utf8(path: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(path.to_path_buf()).expect("utf8 path")
    }

    #[test]
    fn package_dir_inside_repo_scans_from_repo_root() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = utf8(tempdir.path());
        std::fs::create_dir(root.join(".git")).expect("git dir");
        let workdir = root.join("packages/a");
        std::fs::create_dir_all(&workdir).expect("workdir");

        assert_eq!(scan_root_for(&workdir, &root), root);
    }

    #[test]
    fn unanchored_package_dir_keeps_scanning_from_workdir() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let root = utf8(tempdir.path());
        let workdir = root.join("packages/a");
        std::fs::create_dir_all(&workdir).expect("workdir");

        assert_eq!(scan_root_for(&workdir, Utf8Path::new("/")), workdir);
    }
}
