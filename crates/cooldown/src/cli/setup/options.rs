use crate::app::{MemberExcludes, Progress, RunOpts, RunScope};
use crate::cli::setup::SetupCommand;
use crate::cli::{CliOverrides, GlobalArgs, LogLevel};
use cooldown_cargo::CARGO_ID;
use cooldown_core::config::{CommandConfig, ExcludeList, WindowFields};
use cooldown_core::{CoreError, PatternGlob, ToolId, recognized_tool_names, tool_id};

pub(super) struct ResolvedInvocation {
    run: RunOpts,
    offline: bool,
    fresh: bool,
    respect_gitignore: bool,
    env_policy: WindowFields,
    cli_policy: WindowFields,
    strict_native: StrictNativeMode,
    edge_policy_override: Option<cooldown_core::EdgePolicy>,
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

    pub(super) fn dry_run(&self) -> bool {
        self.run.dry_run
    }

    pub(super) fn lock(&self) -> bool {
        self.run.lock
    }

    pub(super) fn fresh(&self) -> bool {
        self.fresh
    }

    pub(super) fn concurrency(&self) -> usize {
        self.run.concurrency
    }

    pub(super) fn respect_dist_tags(&self) -> bool {
        !self.run.ignore_dist_tags
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

    pub(super) fn edge_policy_override(&self) -> Option<cooldown_core::EdgePolicy> {
        self.edge_policy_override
    }

    pub(super) fn progress(&self) -> &Progress {
        &self.run.progress
    }
}

/// `--dry-run` stages the plan by running the real resolver against a project copy, which
/// necessarily resolves online; `--offline` promises no network. Reject the combination for the
/// mutating commands instead of letting the native tool quietly violate that promise.
pub(super) fn reject_offline_dry_run(
    command: SetupCommand,
    dry_run: bool,
    offline: bool,
) -> Result<(), CoreError> {
    if command.adopts_versions() && dry_run && offline {
        return Err(CoreError::Config(
            "--dry-run previews the plan with the real resolver, which needs the network; \
             it cannot be combined with --offline"
                .to_string(),
        ));
    }
    Ok(())
}

/// `--lock` hands the lock to the package manager's resolver, which reads the registry, while
/// `--offline` promises no network.
/// Reject the pair instead of refreshing anyway or silently skipping the refresh the user asked
/// for.
pub(super) fn reject_offline_lock(lock: bool, offline: bool) -> Result<(), CoreError> {
    if lock && offline {
        return Err(CoreError::Config(
            "--lock refreshes the lock with the package manager's resolver, which needs the \
             network; it cannot be combined with --offline"
                .to_string(),
        ));
    }
    Ok(())
}

pub(super) fn resolve_invocation(
    global: &GlobalArgs,
    overrides: &CliOverrides,
    cfg: &CommandConfig,
    command: SetupCommand,
) -> Result<ResolvedInvocation, CoreError> {
    let default_major = command.defaults_to_major();
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
            scope: RunScope::default(),
            // Populated in `setup` from the scan config, which owns the exclude globs.
            excludes: MemberExcludes::default(),
            allow_major: merged.major.unwrap_or(default_major),
            ignore_dist_tags: !merged.respect_dist_tags.unwrap_or(true),
            // A display filter, read straight from the CLI (not config-file backed).
            hide_pinned: overrides.hide_pinned.unwrap_or(false),
            // A display control, read straight from the CLI (not config-file backed).
            why: overrides.why.unwrap_or(false),
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
            all_artifacts: merged.all_artifacts.unwrap_or(false),
            allow_stale_lock: merged.allow_stale_lock.unwrap_or(false),
            fail_on_unknown_age: merged.fail_on_unknown_age.unwrap_or(false),
            advisory_failure: advisory_failure_mode(command, merged.fail_on_advisory_source),
            // A CLI-only mutating convenience for read-only commands; intentionally not config-backed.
            lock: overrides.lock.unwrap_or(false),
            fail_on_new_duplicate: merged.fail_on_new_duplicate.unwrap_or(false),
            strict: merged.strict.unwrap_or(false),
            build: merged.build.unwrap_or(false),
            dry_run: merged.dry_run.unwrap_or(false),
            offline: merged.offline.unwrap_or(false),
            outdated_exit_code: merged.exit_code,
            show_all: merged.all.unwrap_or(false),
            // Pure presentation flags, read straight from the CLI (not config-file backed).
            list_packages: global.list_packages,
            paths: global.paths,
            show_projects: global.show_projects,
            no_suggestions: global.no_suggestions,
            json,
            progress: progress_mode(global),
            // `--concurrency` (CLI/env) wins over a `[<command>]`/`[global]` config value, then the
            // built-in default. Sets both the fan-out width and the per-host HTTP cap downstream.
            concurrency: global
                .concurrency
                .or(merged.concurrency)
                .unwrap_or(16)
                .max(1),
            // No CLI flag: the built-in fix-round budget always applies to real invocations.
            fix_round_budget: None,
        },
        offline: merged.offline.unwrap_or(false),
        fresh: merged.fresh.unwrap_or(false),
        respect_gitignore: merged.gitignore.unwrap_or(true),
        env_policy: env_window_fields()?,
        cli_policy: cli_window_fields(global),
        strict_native: strict_native_mode(overrides),
        edge_policy_override: overrides.edge_policy,
    })
}

fn advisory_failure_mode(
    command: SetupCommand,
    configured: Option<bool>,
) -> crate::app::AdvisoryFailureMode {
    if command.is_check() && configured.unwrap_or(false) {
        crate::app::AdvisoryFailureMode::Error
    } else {
        crate::app::AdvisoryFailureMode::Warn
    }
}

fn builtin_command_config(default_major: bool) -> CommandConfig {
    CommandConfig {
        exclude_folders: ExcludeList::default(),
        exclude_packages: ExcludeList::default(),
        tool: Vec::new(),
        package: Vec::new(),
        gitignore: Some(true),
        major: Some(default_major),
        respect_dist_tags: Some(true),
        all: Some(false),
        all_artifacts: Some(false),
        allow_stale_lock: Some(false),
        fail_on_unknown_age: Some(false),
        fail_on_advisory_source: Some(false),
        fail_on_new_duplicate: Some(false),
        strict: Some(false),
        build: Some(false),
        transitive: Some(false),
        downgrade_pinned: Some(false),
        dry_run: Some(false),
        offline: Some(false),
        fresh: Some(false),
        json: Some(false),
        exit_code: None,
        concurrency: Some(16),
    }
}

fn explicit_command_config(global: &GlobalArgs, overrides: &CliOverrides) -> CommandConfig {
    let tool = if global.cargo {
        vec![CARGO_ID.as_str().to_string()]
    } else {
        global.tool.clone()
    };
    CommandConfig {
        exclude_folders: ExcludeList::default(),
        exclude_packages: ExcludeList::default(),
        tool,
        package: global.package.clone(),
        gitignore: overrides.gitignore,
        major: overrides.major,
        respect_dist_tags: overrides.respect_dist_tags,
        all: overrides.all,
        all_artifacts: overrides.all_artifacts,
        allow_stale_lock: overrides.allow_stale_lock,
        fail_on_unknown_age: overrides.fail_on_unknown_age,
        fail_on_advisory_source: overrides.fail_on_advisory_source,
        fail_on_new_duplicate: overrides.fail_on_new_duplicate,
        strict: overrides.strict,
        build: overrides.build,
        transitive: overrides.transitive,
        downgrade_pinned: overrides.downgrade_pinned,
        dry_run: overrides.dry_run,
        offline: overrides.offline,
        fresh: overrides.fresh,
        json: overrides.json,
        exit_code: overrides.exit_code,
        concurrency: None,
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

/// Select an interactive terminal display or a plain automation-friendly transcript. Diagnostic
/// logging uses the plain form because tracing writes directly to stderr between progress events.
pub(super) fn progress_mode(global: &GlobalArgs) -> Progress {
    use std::io::IsTerminal;

    if global.no_progress {
        return Progress::default();
    }
    if std::io::stderr().is_terminal() && global.log_level == LogLevel::Off {
        Progress::interactive(global.color.progress_colors())
    } else {
        Progress::plain()
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
        advisories: if global.advisories {
            Some(true)
        } else if global.no_advisories {
            Some(false)
        } else {
            None
        },
        advisory_min_age: global.advisory_min_age.clone(),
        advisory_severity: global.advisory_severity.clone(),
    }
}

fn env_window_fields() -> Result<WindowFields, CoreError> {
    let var = |key: &str| std::env::var(key).ok().filter(|value| !value.is_empty());
    let truthy = |key: &str| matches!(var(key).as_deref(), Some("1" | "true" | "yes" | "on"));
    // Unlike the presence-only truthy flags, `COOLDOWN_ADVISORIES=0` must *disable* the feed a
    // config layer enabled, so an explicit falsy value maps to `Some(false)`, not `None`.
    // A value that is neither is rejected rather than guessed: silently ignoring it leaves the
    // feed in whatever state the config chose while the operator believes the variable took
    // effect, and treating it as falsy would let a typo switch off a feed the org's config
    // turned on.
    // The other advisory tokens (`min-age`, `severity`) are validated the same way.
    let advisories = match var("COOLDOWN_ADVISORIES") {
        None => None,
        Some(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            other => {
                return Err(CoreError::Config(format!(
                    "COOLDOWN_ADVISORIES: expected one of 1/true/yes/on or 0/false/no/off, got {other:?}"
                )));
            }
        },
    };
    Ok(WindowFields {
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
        advisories,
        advisory_min_age: var("COOLDOWN_ADVISORY_MIN_AGE"),
        advisory_severity: var("COOLDOWN_ADVISORY_SEVERITY"),
    })
}

#[cfg(test)]
mod tests {
    use super::{
        advisory_failure_mode, builtin_command_config, reject_offline_dry_run, reject_offline_lock,
    };
    use crate::app::AdvisoryFailureMode;
    use crate::cli::setup::SetupCommand;
    use cooldown_core::CoreError;
    use cooldown_core::config::CommandConfig;

    #[test]
    fn builtin_defaults_seed_the_config_shape() {
        let cfg = builtin_command_config(true);
        assert_eq!(cfg.major, Some(true));
        assert_eq!(cfg.gitignore, Some(true));
        assert_eq!(cfg.concurrency, Some(16));
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

    #[test]
    fn offline_lock_refresh_is_rejected() {
        let err =
            reject_offline_lock(true, true).expect_err("offline refresh must be a usage error");
        std::assert_matches!(err, CoreError::Config(_));
        assert!(reject_offline_lock(true, false).is_ok());
        assert!(reject_offline_lock(false, true).is_ok());
    }

    #[test]
    fn offline_dry_run_is_rejected_for_mutating_commands_only() {
        for command in [SetupCommand::Upgrade, SetupCommand::Fix] {
            let err = reject_offline_dry_run(command, true, true)
                .expect_err("offline dry-run must be a usage error");
            std::assert_matches!(err, CoreError::Config(_));
        }
        // Non-mutating commands and non-conflicting flag combinations pass through.
        assert!(reject_offline_dry_run(SetupCommand::Outdated, true, true).is_ok());
        assert!(reject_offline_dry_run(SetupCommand::Upgrade, true, false).is_ok());
        assert!(reject_offline_dry_run(SetupCommand::Upgrade, false, true).is_ok());
        assert!(reject_offline_dry_run(SetupCommand::Fix, false, false).is_ok());
    }

    /// `fail-on-advisory-source` is a `check` gate even when a `[global]` table sets it: no other
    /// command certifies, so none of them may turn an advisory warning into an error.
    #[test]
    fn the_advisory_source_gate_is_scoped_to_check() {
        assert_eq!(
            advisory_failure_mode(SetupCommand::Check, Some(true)),
            AdvisoryFailureMode::Error
        );
        for command in [
            SetupCommand::Outdated,
            SetupCommand::Upgrade,
            SetupCommand::Fix,
            SetupCommand::Baseline,
            SetupCommand::Sync,
            SetupCommand::Explain,
            SetupCommand::Config,
        ] {
            assert_eq!(
                advisory_failure_mode(command, Some(true)),
                AdvisoryFailureMode::Warn
            );
        }
        assert_eq!(
            advisory_failure_mode(SetupCommand::Check, None),
            AdvisoryFailureMode::Warn
        );
        assert_eq!(
            advisory_failure_mode(SetupCommand::Check, Some(false)),
            AdvisoryFailureMode::Warn
        );
    }
}
