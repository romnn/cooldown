use crate::app::{Progress, RunOpts};
use crate::cli::{CliOverrides, GlobalArgs, LogLevel};
use cooldown_cargo::CARGO_ID;
use cooldown_core::config::CommandConfig;
use cooldown_core::{CoreError, PatternGlob, ToolId, tool_id};

pub(super) struct ResolvedRunOpts {
    run: RunOpts,
    offline: bool,
    fresh: bool,
    respect_gitignore: bool,
}

impl ResolvedRunOpts {
    pub(super) fn into_run_opts(self) -> RunOpts {
        self.run
    }

    pub(super) fn offline(&self) -> bool {
        self.offline
    }

    pub(super) fn fresh(&self) -> bool {
        self.fresh
    }

    pub(super) fn respect_gitignore(&self) -> bool {
        self.respect_gitignore
    }

    pub(super) fn tools(&self) -> &[ToolId] {
        &self.run.tool
    }
}

pub(super) fn resolve_run_opts(
    global: &GlobalArgs,
    overrides: &CliOverrides,
    cfg: &CommandConfig,
    default_major: bool,
) -> Result<ResolvedRunOpts, CoreError> {
    let json = resolve_flag(overrides.json, cfg.json, false);
    let respect_gitignore = resolve_flag(overrides.gitignore, cfg.gitignore, true);
    let tools = resolve_tools(global, cfg)?;
    let offline = resolve_flag(overrides.offline, cfg.offline, false);
    let fresh = resolve_flag(overrides.fresh, cfg.fresh, false);
    Ok(ResolvedRunOpts {
        run: RunOpts {
            tool: tools.clone(),
            package: resolve_globs(global, cfg)?,
            allow_major: resolve_flag(overrides.major, cfg.major, default_major),
            major_all: resolve_flag(overrides.major_all, cfg.major_all, false),
            direct_only: resolve_flag(overrides.direct_only, cfg.direct_only, false),
            include_indirect: resolve_flag(overrides.include_indirect, cfg.include_indirect, false),
            all_artifacts: resolve_flag(overrides.all_artifacts, cfg.all_artifacts, false),
            allow_stale_lock: resolve_flag(overrides.allow_stale_lock, cfg.allow_stale_lock, false),
            fail_on_unknown_age: resolve_flag(
                overrides.fail_on_unknown_age,
                cfg.fail_on_unknown_age,
                false,
            ),
            strict: resolve_flag(overrides.strict, cfg.strict, false),
            build: resolve_flag(overrides.build, cfg.build, false),
            dry_run: resolve_flag(overrides.dry_run, cfg.dry_run, false),
            outdated_exit_code: global.exit_code.or(cfg.exit_code),
            show_all: resolve_flag(overrides.all, cfg.all, false),
            json,
            progress: progress_mode(json, global.log_level),
            concurrency: cfg.concurrency.unwrap_or(8),
        },
        offline,
        fresh,
        respect_gitignore,
    })
}

/// Resolve one flag: an explicit CLI value wins, else the config value, else the built-in default.
pub(super) fn resolve_flag(cli: Option<bool>, config: Option<bool>, default: bool) -> bool {
    cli.or(config).unwrap_or(default)
}

/// The tool/tool set this run is restricted to (empty = all detected).
///
/// `--cargo` is exact shorthand for `--tool cargo` (clap rejects passing both); otherwise an
/// explicit `--tool` is used, falling back to the config `tool` list. Values accept the language
/// name and sibling tools as aliases (see [`tool_id`]).
fn resolve_tools(global: &GlobalArgs, cfg: &CommandConfig) -> Result<Vec<ToolId>, CoreError> {
    if global.cargo {
        return Ok(vec![CARGO_ID]);
    }
    let tools = if global.tool.is_empty() {
        &cfg.tool
    } else {
        &global.tool
    };
    tools
        .iter()
        .map(|name| {
            tool_id(name).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown --tool `{name}`; recognised: cargo, go, uv, node"
                ))
            })
        })
        .collect()
}

/// The package globs this run is scoped to: an explicit `--package` is used, else the config
/// `package` list.
fn resolve_globs(global: &GlobalArgs, cfg: &CommandConfig) -> Result<Vec<PatternGlob>, CoreError> {
    let globs = if global.package.is_empty() {
        &cfg.package
    } else {
        &global.package
    };
    globs.iter().map(|glob| PatternGlob::new(glob)).collect()
}

/// Route coarse progress notes: silent when `--log-level` already narrates the run, to stderr under
/// `--json` (keep stdout pure), to stdout otherwise (next to the pretty report).
fn progress_mode(json: bool, log_level: LogLevel) -> Progress {
    if log_level != LogLevel::Off {
        Progress::Silent
    } else if json {
        Progress::Stderr
    } else {
        Progress::Stdout
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_flag;

    #[test]
    fn resolve_flag_follows_cli_then_config_then_default() {
        // No CLI, no config → the per-command built-in default (true for outdated --major, etc.).
        assert!(resolve_flag(None, None, true));
        assert!(!resolve_flag(None, None, false));
        // Config overrides the built-in default...
        assert!(resolve_flag(None, Some(true), false));
        assert!(!resolve_flag(None, Some(false), true));
        // ...and an explicit CLI flag overrides both.
        assert!(resolve_flag(Some(true), Some(false), false));
        assert!(!resolve_flag(Some(false), Some(true), true));
    }
}
