use crate::app::{Progress, RunOpts};
use crate::cli::{CliOverrides, GlobalArgs, LogLevel};
use cooldown_cargo::CARGO_ID;
use cooldown_core::config::{CommandConfig, WindowFields};
use cooldown_core::{CoreError, PatternGlob, ToolId, recognized_tool_names, tool_id};
use std::collections::BTreeMap;

pub(super) struct ResolvedInvocation {
    run: RunOpts,
    offline: bool,
    fresh: bool,
    respect_gitignore: bool,
    env_policy: WindowFields,
    cli_policy: WindowFields,
    strict_native: StrictNativeMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StrictNativeMode {
    Inherit,
    ForceOn,
    ForceOff,
}

impl ResolvedInvocation {
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

    pub(super) fn env_policy(&self) -> &WindowFields {
        &self.env_policy
    }

    pub(super) fn cli_policy(&self) -> &WindowFields {
        &self.cli_policy
    }

    pub(super) fn strict_native(&self) -> StrictNativeMode {
        self.strict_native
    }
}

pub(super) fn resolve_invocation(
    global: &GlobalArgs,
    overrides: &CliOverrides,
    cfg: &CommandConfig,
    default_major: bool,
) -> Result<ResolvedInvocation, CoreError> {
    let explicit = explicit_command_config(global, overrides);
    let merged = builtin_command_config(default_major)
        .merge_layer(cfg.clone())
        .apply_explicit(&explicit);
    let tools = resolve_tools(&merged)?;
    let package = resolve_globs(&merged)?;
    let json = merged.json.unwrap_or(false);

    Ok(ResolvedInvocation {
        run: RunOpts {
            tool: tools,
            package,
            // Populated in `setup` from the scan config, which owns the exclude globs.
            exclude_folders: Vec::new(),
            exclude_folders_by_tool: BTreeMap::default(),
            exclude_packages: Vec::new(),
            exclude_packages_by_tool: BTreeMap::default(),
            allow_major: merged.major.unwrap_or(default_major),
            // A display filter, read straight from the CLI (not config-file backed).
            hide_pinned: overrides.hide_pinned.unwrap_or(false),
            // Read straight from the CLI (not config-file backed).
            rewrite: if overrides.rewrite.unwrap_or(false) {
                cooldown_core::RewriteMode::Always
            } else {
                cooldown_core::RewriteMode::Auto
            },
            transitive: merged.transitive.unwrap_or(false),
            // A display control, read straight from the CLI (not config-file backed); absent, the
            // report counts down to the latest version.
            cooldown_horizon: overrides.countdown.unwrap_or_default().horizon(),
            downgrade_pinned: merged.downgrade_pinned.unwrap_or(false),
            // `--transitive <mode>` is read straight from the CLI (per-command, not config); absent,
            // each command acts on transitives by default (Enforce).
            transitive_mode: match overrides.transitive_mode {
                Some(crate::cli::args::TransitiveMode::Allow) => crate::app::TransitiveGate::Allow,
                Some(crate::cli::args::TransitiveMode::Hide) => crate::app::TransitiveGate::Hide,
                None => crate::app::TransitiveGate::Enforce,
            },
            major_all: merged.major_all.unwrap_or(false),
            all_artifacts: merged.all_artifacts.unwrap_or(false),
            allow_stale_lock: merged.allow_stale_lock.unwrap_or(false),
            fail_on_unknown_age: merged.fail_on_unknown_age.unwrap_or(false),
            strict: merged.strict.unwrap_or(false),
            build: merged.build.unwrap_or(false),
            dry_run: merged.dry_run.unwrap_or(false),
            outdated_exit_code: merged.exit_code,
            show_all: merged.all.unwrap_or(false),
            // Pure presentation flags, read straight from the CLI (not config-file backed).
            list_packages: global.list_packages,
            paths: global.paths,
            show_projects: global.show_projects,
            json,
            progress: progress_mode(json, global.log_level),
            concurrency: merged.concurrency.unwrap_or(8),
        },
        offline: merged.offline.unwrap_or(false),
        fresh: merged.fresh.unwrap_or(false),
        respect_gitignore: merged.gitignore.unwrap_or(true),
        env_policy: env_window_fields(),
        cli_policy: cli_window_fields(global),
        strict_native: strict_native_mode(overrides),
    })
}

fn builtin_command_config(default_major: bool) -> CommandConfig {
    CommandConfig {
        gitignore: Some(true),
        major: Some(default_major),
        major_all: Some(false),
        all: Some(false),
        all_artifacts: Some(false),
        allow_stale_lock: Some(false),
        fail_on_unknown_age: Some(false),
        strict: Some(false),
        build: Some(false),
        transitive: Some(false),
        downgrade_pinned: Some(false),
        dry_run: Some(false),
        offline: Some(false),
        fresh: Some(false),
        json: Some(false),
        concurrency: Some(8),
        ..CommandConfig::default()
    }
}

fn explicit_command_config(global: &GlobalArgs, overrides: &CliOverrides) -> CommandConfig {
    let tool = if global.cargo {
        vec![CARGO_ID.as_str().to_string()]
    } else {
        global.tool.clone()
    };
    CommandConfig {
        tool,
        package: global.package.clone(),
        gitignore: overrides.gitignore,
        major: overrides.major,
        major_all: overrides.major_all,
        all: overrides.all,
        all_artifacts: overrides.all_artifacts,
        allow_stale_lock: overrides.allow_stale_lock,
        fail_on_unknown_age: overrides.fail_on_unknown_age,
        strict: overrides.strict,
        build: overrides.build,
        transitive: overrides.transitive,
        downgrade_pinned: overrides.downgrade_pinned,
        dry_run: overrides.dry_run,
        offline: overrides.offline,
        fresh: overrides.fresh,
        json: overrides.json,
        exit_code: overrides.exit_code,
        ..CommandConfig::default()
    }
}

fn strict_native_mode(overrides: &CliOverrides) -> StrictNativeMode {
    if overrides.no_fail_on_stricter_native == Some(true) {
        StrictNativeMode::ForceOff
    } else if overrides.fail_on_stricter_native == Some(true) {
        StrictNativeMode::ForceOn
    } else {
        StrictNativeMode::Inherit
    }
}

/// The tool/tool set this run is restricted to (empty = all detected).
///
/// Values accept the language name and sibling tools as aliases (see [`tool_id`]).
fn resolve_tools(cfg: &CommandConfig) -> Result<Vec<ToolId>, CoreError> {
    cfg.tool
        .iter()
        .map(|name| {
            tool_id(name).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown --tool `{name}`; recognised: {}",
                    recognized_tool_names()
                ))
            })
        })
        .collect()
}

/// The package globs this run is scoped to.
fn resolve_globs(cfg: &CommandConfig) -> Result<Vec<PatternGlob>, CoreError> {
    cfg.package
        .iter()
        .map(|glob| PatternGlob::new(glob))
        .collect()
}

/// Route coarse progress notes: silent when `--log-level` already narrates the run, to stderr
/// under `--json` (keep stdout pure), to stdout otherwise (next to the pretty report).
fn progress_mode(json: bool, log_level: LogLevel) -> Progress {
    if log_level != LogLevel::Off {
        Progress::Silent
    } else if json {
        Progress::Stderr
    } else {
        Progress::Stdout
    }
}

fn cli_window_fields(global: &GlobalArgs) -> WindowFields {
    WindowFields {
        min_age: global.min_age.clone(),
        min_age_major: global.min_age_major.clone(),
        min_age_minor: global.min_age_minor.clone(),
        min_age_patch: global.min_age_patch.clone(),
        latest: global.latest,
        freeze: global.freeze.clone(),
        allow: global.allow.clone(),
    }
}

fn env_window_fields() -> WindowFields {
    let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let truthy = |key: &str| matches!(var(key).as_deref(), Some("1" | "true" | "yes" | "on"));
    WindowFields {
        min_age: var("COOLDOWN_MIN_AGE"),
        min_age_major: var("COOLDOWN_MIN_AGE_MAJOR"),
        min_age_minor: var("COOLDOWN_MIN_AGE_MINOR"),
        min_age_patch: var("COOLDOWN_MIN_AGE_PATCH"),
        latest: truthy("COOLDOWN_LATEST"),
        freeze: var("COOLDOWN_FREEZE"),
        allow: var("COOLDOWN_ALLOW")
            .map(|value| {
                value
                    .split(',')
                    .map(|part| part.trim().to_string())
                    .filter(|part| !part.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use super::builtin_command_config;
    use cooldown_core::config::CommandConfig;

    #[test]
    fn builtin_defaults_seed_the_config_shape() {
        let cfg = builtin_command_config(true);
        assert_eq!(cfg.major, Some(true));
        assert_eq!(cfg.gitignore, Some(true));
        assert_eq!(cfg.concurrency, Some(8));
    }

    #[test]
    fn explicit_overrides_replace_lists_and_scalars() {
        let base = CommandConfig {
            tool: vec!["go".into()],
            package: vec!["left-*".into()],
            major: Some(false),
            transitive: Some(false),
            ..CommandConfig::default()
        };
        let explicit = CommandConfig {
            tool: vec!["cargo".into()],
            package: vec!["serde".into()],
            major: Some(true),
            transitive: Some(true),
            ..CommandConfig::default()
        };
        let resolved = base.apply_explicit(&explicit);
        assert_eq!(resolved.tool, vec!["cargo"]);
        assert_eq!(resolved.package, vec!["serde"]);
        assert_eq!(resolved.major, Some(true));
        assert_eq!(resolved.transitive, Some(true));
    }
}
