use camino::Utf8PathBuf;
use clap::parser::ValueSource;
use clap::{ArgMatches, Args, Parser, Subcommand, ValueEnum};

/// Verbosity for the diagnostic log written to stderr (independent of `--json`/report output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub(in crate::cli) enum LogLevel {
    /// No logging (the default).
    #[default]
    Off,
    /// Fatal-ish conditions only.
    Error,
    /// Recoverable problems and fallbacks.
    Warn,
    /// High-level progress (detection, per-project evaluation).
    Info,
    /// Per-dependency and per-request detail.
    Debug,
    /// Everything, including cache hits and subprocess argv.
    Trace,
}

impl LogLevel {
    /// The `EnvFilter` directive for this level: verbose levels are scoped to cooldown's own crates
    /// so noisy transitive deps (`reqwest`, `hyper`, `rustls`) stay at `warn`.
    pub(in crate::cli) fn directive(self) -> String {
        let level = match self {
            LogLevel::Off => return "off".to_string(),
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
            LogLevel::Trace => "trace",
        };
        format!(
            "warn,cooldown={level},cooldown_core={level},cooldown_cargo={level},\
cooldown_go={level},cooldown_uv={level},cooldown_registry={level},cooldown_toml_util={level}"
        )
    }
}

/// Whether the pretty (non-`--json`) report is colorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
pub(in crate::cli) enum ColorMode {
    /// Colorize only when stdout is a terminal and `NO_COLOR` is unset (the default).
    #[default]
    Auto,
    /// Always emit ANSI color — e.g. when piping into an image/screenshot tool.
    Always,
    /// Never emit color.
    Never,
}

impl ColorMode {
    /// Resolve to a concrete on/off. `--json` output is never colorized (the value is moot there,
    /// since the colored TTY table isn't rendered); `Auto` honors the terminal and `NO_COLOR`.
    pub(in crate::cli) fn resolve(self, json: bool) -> bool {
        match self {
            ColorMode::Always => true,
            ColorMode::Never => false,
            ColorMode::Auto => {
                use std::io::IsTerminal;
                let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
                std::io::stdout().is_terminal() && !json && !no_color
            }
        }
    }
}

/// The parsed `cooldown` command line: a subcommand plus the global, mostly-policy flags.
///
/// Construct it with clap's [`Parser`] (`Cli::parse()`) and hand it to [`run`](crate::cli::run).
#[derive(Parser)]
#[command(
    name = "cooldown",
    version,
    about = "A unified, language-agnostic dependency-cooldown CLI",
    long_about = "Refuse to adopt any dependency version younger than a minimum release age, across tools, from one policy core."
)]
pub struct Cli {
    #[command(subcommand)]
    pub(in crate::cli) command: Command,
    #[command(flatten)]
    pub(in crate::cli) global: GlobalArgs,
}

#[derive(Subcommand)]
pub(in crate::cli) enum Command {
    /// What could update — split into "adoptable now" vs "in cooldown".
    Outdated,
    /// Move direct deps to the newest version older than the cooldown; always re-locks.
    Upgrade,
    /// Fix cooldown violations: downgrade too-fresh deps to a matured version (never upgrades).
    Fix,
    /// Exit non-zero if anything resolved is younger than the cooldown (the CI gate).
    Check,
    /// Record currently-young deps as acknowledged, so `check` can be adopted cleanly.
    Baseline {
        /// Drop entries whose version has aged past the resolved window or is no longer present.
        #[arg(long)]
        prune: bool,
    },
    /// Why a package has the window it has — every layer and rule that applied.
    #[command(visible_alias = "why")]
    Explain {
        /// The package to explain.
        package: String,
    },
    /// The fully-resolved config, with the origin of each value.
    Config,
    /// Scaffold a documented starter cooldown.toml (refuses to clobber).
    Init,
    /// Print the machine-readable JSON schema for `--json` output.
    Schema,
    /// Write the resolved policy down into native configs (opt-in; later phase).
    Sync,
}

#[derive(Args)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent CLI flags; clap maps each bool to a --flag"
)]
pub(in crate::cli) struct GlobalArgs {
    /// Window: "7d", "2 weeks", "36h", ISO-8601 "P7D" (default 7d).
    #[arg(long, global = true, value_name = "DUR", conflicts_with_all = ["latest", "freeze"])]
    pub(in crate::cli) min_age: Option<String>,
    /// Per-kind window for major jumps.
    #[arg(long = "min-age-major", global = true, value_name = "DUR")]
    pub(in crate::cli) min_age_major: Option<String>,
    /// Per-kind window for minor jumps.
    #[arg(long = "min-age-minor", global = true, value_name = "DUR")]
    pub(in crate::cli) min_age_minor: Option<String>,
    /// Per-kind window for patch jumps.
    #[arg(long = "min-age-patch", global = true, value_name = "DUR")]
    pub(in crate::cli) min_age_patch: Option<String>,
    /// Opt OUT (window = 0) — the explicit, audited escape hatch.
    #[arg(long, global = true, alias = "no-min-age", conflicts_with_all = ["min_age", "freeze"])]
    pub(in crate::cli) latest: bool,
    /// An absolute cutoff instead of a rolling window (reproducible).
    #[arg(long, global = true, value_name = "DATE", conflicts_with_all = ["min_age", "latest"])]
    pub(in crate::cli) freeze: Option<String>,
    /// Exempt matching packages from the cooldown (repeatable, audited).
    #[arg(long, global = true, value_name = "GLOB")]
    pub(in crate::cli) allow: Vec<String>,
    /// Allow major version changes. Default: ON for `outdated` (so a new major is discoverable),
    /// OFF for `upgrade`/`check`/etc. (a major bump is usually breaking work you opt into).
    #[arg(long, global = true)]
    pub(in crate::cli) major: bool,
    /// Stay within the current major (the inverse of `--major`; alias `--minor`). Useful for
    /// clean `outdated` output in CI, where `outdated` otherwise shows cross-major candidates.
    #[arg(
        long = "no-major",
        visible_alias = "minor",
        global = true,
        conflicts_with = "major"
    )]
    pub(in crate::cli) no_major: bool,
    /// With --major, apply cross-major to ALL eligible deps (else --package is required).
    #[arg(long = "major-all", global = true)]
    pub(in crate::cli) major_all: bool,
    /// (outdated) Also list up-to-date deps. Hidden by default so the report shows only deps with
    /// something to act on; the summary line still counts every dependency.
    #[arg(long, global = true)]
    pub(in crate::cli) all: bool,

    /// Scope the command to matching packages (repeatable).
    #[arg(long, short = 'p', global = true, value_name = "GLOB")]
    pub(in crate::cli) package: Vec<String>,
    /// Restrict to tool(s) — `cargo`, `go`, `uv`, … (aliases like `rust`/`pnpm` accepted);
    /// repeatable / comma-separated (default: all detected).
    #[arg(
        long,
        global = true,
        value_name = "TOOL",
        value_delimiter = ',',
        env = "COOLDOWN_TOOL"
    )]
    pub(in crate::cli) tool: Vec<String>,
    /// Only the Rust/Cargo tool — skip detecting/enumerating Go, Python, and Node entirely
    /// (shorthand for `--tool cargo`; the right default for a Cargo workspace in a polyglot monorepo).
    #[arg(long, global = true, conflicts_with = "tool")]
    pub(in crate::cli) cargo: bool,
    /// Don't honor `.gitignore` while detecting projects (the rare repo whose lockfiles are
    /// themselves ignored). By default detection skips gitignored paths — correct and faster.
    #[arg(long = "no-gitignore", global = true)]
    pub(in crate::cli) no_gitignore: bool,
    /// (outdated) Exit with this code when adoptable updates exist, for CI gating. Bare `--exit-code`
    /// means 1, or pass `--exit-code=N`; omitting it keeps `outdated` informational (always exit 0).
    #[arg(
        long = "exit-code",
        global = true,
        value_name = "CODE",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "1"
    )]
    pub(in crate::cli) exit_code: Option<u8>,
    /// Diagnostic log verbosity on stderr (`RUST_LOG` overrides). One of: off, error, warn, info,
    /// debug, trace.
    #[arg(
        long = "log-level",
        global = true,
        value_name = "LEVEL",
        value_enum,
        default_value_t = LogLevel::Off,
        env = "COOLDOWN_LOG"
    )]
    pub(in crate::cli) log_level: LogLevel,
    /// Evaluate only direct deps.
    #[arg(long = "direct-only", global = true)]
    pub(in crate::cli) direct_only: bool,
    /// (outdated) include transitive deps in the report.
    #[arg(long = "include-indirect", global = true)]
    pub(in crate::cli) include_indirect: bool,
    /// (outdated) Hide held rows (exact `==`/`=` pins and commit pins) from the table, leaving only
    /// deps with an actionable update. A held row's `Latest` column still shows what is available.
    #[arg(long = "hide-pinned", global = true)]
    pub(in crate::cli) hide_pinned: bool,
    /// List every source package on its own line instead of `first (+N others)`.
    #[arg(long = "list-packages", global = true)]
    pub(in crate::cli) list_packages: bool,
    /// Show the "Used by" column as workspace paths instead of package names.
    #[arg(long = "paths", global = true)]
    pub(in crate::cli) paths: bool,
    /// (check) gate every artifact in a universal lock, not just env-relevant ones.
    #[arg(long = "all-artifacts", global = true)]
    pub(in crate::cli) all_artifacts: bool,
    /// Downgrade a stale/absent lock from failure (the default) to a warning.
    #[arg(
        long = "allow-stale-lock",
        global = true,
        env = "COOLDOWN_ALLOW_STALE_LOCK"
    )]
    pub(in crate::cli) allow_stale_lock: bool,
    /// Make `check` fail (not just warn) on deps with no publish time.
    #[arg(long = "fail-on-unknown-age", global = true)]
    pub(in crate::cli) fail_on_unknown_age: bool,
    /// Make `check`/`config` fail when repo policy overrides a stricter native value.
    #[arg(long = "fail-on-stricter-native", global = true)]
    pub(in crate::cli) fail_on_stricter_native: bool,
    /// Override a config-set `strict-native` (the only way to turn it off).
    #[arg(long = "no-fail-on-stricter-native", global = true)]
    pub(in crate::cli) no_fail_on_stricter_native: bool,
    /// (upgrade) fail (exit 1) if any planned change was skipped.
    #[arg(long, global = true)]
    pub(in crate::cli) strict: bool,
    /// (upgrade) also compile/sync after re-locking.
    #[arg(long, global = true)]
    pub(in crate::cli) build: bool,
    /// (upgrade) Always rewrite the manifest's version constraint to the adopted version, even for
    /// in-range moves. By default the constraint is left untouched and only the lock moves, unless
    /// the target falls outside the current constraint (e.g. a cross-major bump), which always
    /// rewrites the one owning manifest entry.
    #[arg(long, global = true)]
    pub(in crate::cli) rewrite: bool,
    /// (fix) Also downgrade too-fresh transitive deps, not just direct ones. Dangerous: downgrading
    /// a transitive can break a direct dependency that relies on the newer version.
    #[arg(long, global = true)]
    pub(in crate::cli) transitive: bool,
    /// (fix) Downgrade and rewrite exact-pinned deps too. By default a pinned cooldown violation is
    /// left in place with a warning, since a pin is a deliberate choice.
    #[arg(long = "downgrade-pinned", global = true)]
    pub(in crate::cli) downgrade_pinned: bool,
    /// Sync the policy into native config (e.g. uv `exclude-newer`) before running this command, so
    /// cooldown.toml stays the source of truth. No-op under `--dry-run`.
    #[arg(long, global = true)]
    pub(in crate::cli) sync: bool,
    /// Resolve and print the plan; never mutate.
    #[arg(long = "dry-run", short = 'n', global = true, env = "COOLDOWN_DRY_RUN")]
    pub(in crate::cli) dry_run: bool,
    /// Cache only; cache misses become `UnknownAge` (never a false "ok").
    #[arg(long, global = true, env = "COOLDOWN_OFFLINE")]
    pub(in crate::cli) offline: bool,
    /// Ignore the local cache; always hit the registry (use in CI gates).
    #[arg(long, global = true, visible_alias = "no-cache")]
    pub(in crate::cli) fresh: bool,
    /// Ignore the native config layer (reproducibility / debugging).
    #[arg(long = "no-native", global = true)]
    pub(in crate::cli) no_native: bool,
    /// Ignore the global config layer.
    #[arg(long = "no-global", global = true)]
    pub(in crate::cli) no_global: bool,
    /// Load one extra, highest-precedence file layer (still below env/flags).
    #[arg(long, global = true, value_name = "PATH", env = "COOLDOWN_CONFIG")]
    pub(in crate::cli) config: Option<Utf8PathBuf>,
    /// Run as if from <path>.
    #[arg(long = "dir", short = 'C', global = true, value_name = "PATH")]
    pub(in crate::cli) dir: Option<Utf8PathBuf>,
    /// Machine-readable output (never changes the exit code).
    #[arg(long, global = true)]
    pub(in crate::cli) json: bool,
    /// When to colorize the pretty report: auto (TTY + `NO_COLOR` unset), always, or never.
    /// `--color always` forces ANSI even when piped (e.g. into a screenshot tool).
    #[arg(
        long,
        global = true,
        value_name = "WHEN",
        value_enum,
        default_value_t = ColorMode::Auto
    )]
    pub(in crate::cli) color: ColorMode,
}

/// The flags the user set *explicitly* (on the command line or via an env var), captured once from
/// the parsed [`ArgMatches`].
///
/// Resolution precedence is `explicit CLI flag > [<command>] config > [global] config > built-in
/// default`; a `None` field here means "not set on the CLI", so the config or default is used. This
/// is the one place that needs `ArgMatches`, because a clap `bool` flag can't otherwise distinguish
/// "passed `--flag`" from "defaulted to false".
#[derive(Debug, Clone, Copy, Default)]
pub struct CliOverrides {
    pub(crate) major: Option<bool>,
    pub(crate) gitignore: Option<bool>,
    pub(crate) all: Option<bool>,
    pub(crate) major_all: Option<bool>,
    pub(crate) direct_only: Option<bool>,
    pub(crate) include_indirect: Option<bool>,
    pub(crate) all_artifacts: Option<bool>,
    pub(crate) allow_stale_lock: Option<bool>,
    pub(crate) fail_on_unknown_age: Option<bool>,
    pub(crate) strict: Option<bool>,
    pub(crate) build: Option<bool>,
    pub(crate) dry_run: Option<bool>,
    pub(crate) offline: Option<bool>,
    pub(crate) fresh: Option<bool>,
    pub(crate) json: Option<bool>,
}

impl CliOverrides {
    /// Capture which flags were given explicitly (on the CLI or via their env var).
    #[must_use]
    pub fn from_matches(matches: &ArgMatches) -> Self {
        let on = |id: &str| set_on_cli(matches, id).then_some(true);
        CliOverrides {
            // `--major` forces cross-major on; `--no-major` (alias `--minor`) forces it off.
            major: if set_on_cli(matches, "major") {
                Some(true)
            } else if set_on_cli(matches, "no_major") {
                Some(false)
            } else {
                None
            },
            // `--no-gitignore` is the only CLI control; the default (on) lives in config/built-in.
            gitignore: set_on_cli(matches, "no_gitignore").then_some(false),
            all: on("all"),
            major_all: on("major_all"),
            direct_only: on("direct_only"),
            include_indirect: on("include_indirect"),
            all_artifacts: on("all_artifacts"),
            allow_stale_lock: on("allow_stale_lock"),
            fail_on_unknown_age: on("fail_on_unknown_age"),
            strict: on("strict"),
            build: on("build"),
            dry_run: on("dry_run"),
            offline: on("offline"),
            fresh: on("fresh"),
            json: on("json"),
        }
    }
}

/// Whether `id` was set on the command line or via its env var (not by a default). A global arg can
/// land in either the root or the subcommand matches, so check both.
fn set_on_cli(matches: &ArgMatches, id: &str) -> bool {
    fn explicit(source: Option<ValueSource>) -> bool {
        matches!(
            source,
            Some(ValueSource::CommandLine | ValueSource::EnvVariable)
        )
    }
    explicit(matches.value_source(id))
        || matches
            .subcommand()
            .is_some_and(|(_, sub)| explicit(sub.value_source(id)))
}

#[cfg(test)]
mod tests {
    use super::{Cli, CliOverrides};
    use clap::{CommandFactory, Parser};

    fn overrides(args: &[&str]) -> CliOverrides {
        let matches = Cli::command().get_matches_from(args);
        CliOverrides::from_matches(&matches)
    }

    #[test]
    fn unset_flags_have_no_override() {
        let ov = overrides(&["cooldown", "outdated"]);
        assert_eq!(ov.major, None);
        assert_eq!(ov.gitignore, None);
        assert_eq!(ov.all, None);
        assert_eq!(ov.strict, None);
    }

    #[test]
    fn major_and_no_major_map_to_explicit_true_and_false() {
        assert_eq!(
            overrides(&["cooldown", "outdated", "--major"]).major,
            Some(true)
        );
        assert_eq!(
            overrides(&["cooldown", "outdated", "--no-major"]).major,
            Some(false)
        );
        assert_eq!(
            overrides(&["cooldown", "outdated", "--minor"]).major,
            Some(false)
        );
        assert_eq!(
            overrides(&["cooldown", "outdated", "--no-gitignore"]).gitignore,
            Some(false)
        );
    }

    #[test]
    fn global_flags_are_detected_before_or_after_the_subcommand() {
        // After the subcommand (the common form)...
        assert_eq!(
            overrides(&["cooldown", "outdated", "--all"]).all,
            Some(true)
        );
        assert_eq!(
            overrides(&["cooldown", "upgrade", "--strict"]).strict,
            Some(true)
        );
        // ...and before it (global args propagate either way).
        assert_eq!(
            overrides(&["cooldown", "--strict", "upgrade"]).strict,
            Some(true)
        );
    }

    #[test]
    fn parser_accepts_full_command_shape() {
        let cli = Cli::parse_from(["cooldown", "check", "--json"]);
        assert!(matches!(cli.command, super::Command::Check));
        assert!(cli.global.json);
    }
}
