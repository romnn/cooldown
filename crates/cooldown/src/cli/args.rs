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

/// `--transitive <mode>`: how `check`/`fix`/`upgrade` handle too-fresh *transitive* dependencies.
/// Absent, each command acts on them by default (check fails, fix downgrades, upgrade reconciles);
/// these modes relax that. The same spelling across the three commands keeps the surface consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub(crate) enum TransitiveMode {
    /// Include transitive deps but don't act on them: `check` reports them non-fatally, `fix`/
    /// `upgrade` leave them in place (direct deps are still handled).
    Allow,
    /// Skip transitive deps entirely — a direct-only run.
    Hide,
}

/// `outdated --countdown <latest|soonest>`: which still-cooling upgrade the "Cooldown" column
/// counts down to when several newer versions exist. Display-only — neither value changes what is
/// adoptable, only which candidate's `age/window` the column shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
#[value(rename_all = "kebab-case")]
pub(crate) enum Countdown {
    /// Count down to the newest version maturing — the longest wait (the default).
    #[default]
    Latest,
    /// Count down to the next version to mature — the soonest unlock, which an intermediate release
    /// can reach days before the newest one does.
    Soonest,
}

impl Countdown {
    /// Map the CLI flag onto the core [`CooldownHorizon`](cooldown_core::CooldownHorizon).
    pub(in crate::cli) fn horizon(self) -> cooldown_core::CooldownHorizon {
        match self {
            Countdown::Latest => cooldown_core::CooldownHorizon::Latest,
            Countdown::Soonest => cooldown_core::CooldownHorizon::Soonest,
        }
    }
}

#[derive(Subcommand)]
pub(in crate::cli) enum Command {
    /// What could update — split into "adoptable now" vs "in cooldown".
    Outdated {
        /// Also list transitive (indirect) deps. By default the report shows only direct deps; the
        /// summary line still counts the whole resolved graph.
        #[arg(long)]
        transitive: bool,
        /// Also list up-to-date deps. Hidden by default so the report shows only deps with something
        /// to act on; the summary line still counts every dependency.
        #[arg(long)]
        all: bool,
        /// Hide held rows (exact `==`/`=` pins and commit pins) from the table, leaving only deps
        /// with an actionable update. A held row's `Latest` column still shows what is available.
        #[arg(long = "hide-pinned")]
        hide_pinned: bool,
        /// Which still-cooling upgrade the "Cooldown" column counts down to when several newer
        /// versions exist: `latest` (the newest version — the default) or `soonest` (the next
        /// version to mature, which an intermediate release can reach days earlier). Display-only.
        #[arg(long, value_name = "WHICH", value_enum)]
        countdown: Option<Countdown>,
        /// Exit with this code when adoptable updates exist, for CI gating. Bare `--exit-code` means
        /// 1, or pass `--exit-code=N`; omitting it keeps `outdated` informational (always exit 0).
        #[arg(
            long = "exit-code",
            value_name = "CODE",
            num_args = 0..=1,
            require_equals = true,
            default_missing_value = "1"
        )]
        exit_code: Option<u8>,
    },
    /// Move direct deps to the newest version older than the cooldown; always re-locks.
    Upgrade {
        /// How to treat *transitive* (indirect) deps. Default: move them too — advance each to its
        /// newest matured version, and reconcile any too-fresh one a re-lock drags in back down, so
        /// the new lock is gate-clean. `hide` is direct-only (transitives untouched); `allow` still
        /// advances the graph but leaves a floated-up too-fresh transitive in place (reported, not
        /// rolled back).
        #[arg(long, value_enum, value_name = "MODE")]
        transitive: Option<TransitiveMode>,
        /// Also compile/sync after re-locking.
        #[arg(long)]
        build: bool,
        /// Always rewrite the manifest's version constraint to the adopted version, even for in-range
        /// moves. By default the constraint is left untouched and only the lock moves, unless the
        /// target falls outside the current constraint (e.g. a cross-major bump), which always
        /// rewrites the one owning manifest entry.
        #[arg(long)]
        rewrite: bool,
        #[command(flatten)]
        mutation: MutationArgs,
    },
    /// Fix cooldown violations: downgrade too-fresh deps to a matured version (never upgrades).
    Fix {
        /// How to treat too-fresh *transitive* deps. Default: downgrade them too, to the newest
        /// matured version the graph still allows. `hide` skips them (direct-only fix); `allow`
        /// leaves them in place but still downgrades direct deps.
        #[arg(long, value_enum, value_name = "MODE")]
        transitive: Option<TransitiveMode>,
        /// Downgrade and rewrite exact-pinned deps too. By default a pinned cooldown violation is
        /// left in place with a warning, since a pin is a deliberate choice.
        #[arg(long = "downgrade-pinned")]
        downgrade_pinned: bool,
        #[command(flatten)]
        mutation: MutationArgs,
    },
    /// Exit non-zero if anything resolved is younger than the cooldown (the CI gate).
    Check {
        /// How to treat too-fresh *transitive* deps. Default: fail the gate on them. `allow` keeps
        /// them visible but non-fatal; `hide` skips evaluating transitive deps entirely (direct-only).
        #[arg(long, value_enum, value_name = "MODE")]
        transitive: Option<TransitiveMode>,
        /// Gate every artifact in a universal lock, not just env-relevant ones.
        #[arg(long = "all-artifacts")]
        all_artifacts: bool,
        /// Fail (not just warn) on deps with no publish time.
        #[arg(long = "fail-on-unknown-age")]
        fail_on_unknown_age: bool,
        #[command(flatten)]
        strict_native: StrictNativeArgs,
    },
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
    Config {
        #[command(flatten)]
        strict_native: StrictNativeArgs,
    },
    /// Scaffold a documented starter cooldown.toml (refuses to clobber).
    Init,
    /// Print the machine-readable JSON schema for `--json` output.
    Schema,
    /// Write the resolved policy down into native configs.
    Sync,
}

/// Flags shared by the mutating commands (`upgrade`, `fix`). Flattened into each so the flag is
/// scoped to where it applies (not silently accepted on every command).
#[derive(Args)]
pub(in crate::cli) struct MutationArgs {
    /// Fail (exit 1) if the mutation cannot complete cleanly.
    #[arg(long)]
    pub(in crate::cli) strict: bool,
}

/// Flags shared by the policy-introspection commands (`check`, `config`). Flattened into each so
/// the strict-native controls are scoped to where they apply.
#[derive(Args)]
pub(in crate::cli) struct StrictNativeArgs {
    /// Fail when repo policy overrides a stricter native value.
    #[arg(long = "fail-on-stricter-native")]
    pub(in crate::cli) fail_on_stricter_native: bool,
    /// Override a config-set `strict-native` (the only way to turn it off).
    #[arg(long = "no-fail-on-stricter-native")]
    pub(in crate::cli) no_fail_on_stricter_native: bool,
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
    /// OFF for `upgrade`/`check`/etc. (a major bump is usually breaking work you opt into). For
    /// `upgrade`/`fix` it applies to every eligible dependency; narrow it with `--package`.
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
    /// Directories never scanned, `.gitignore`-style — overrides the `[global]`/`[<command>]` config
    /// lists (per-tool `[tool.*]` excludes still apply). Repeatable.
    #[arg(long = "exclude-folders", global = true, value_name = "GLOB")]
    pub(in crate::cli) exclude_folders: Vec<String>,
    /// Workspace members dropped from reports by package-name glob — overrides the
    /// `[global]`/`[<command>]` config lists (per-tool `[tool.*]` excludes still apply). Repeatable.
    #[arg(long = "exclude-packages", global = true, value_name = "GLOB")]
    pub(in crate::cli) exclude_packages: Vec<String>,
    /// Don't honor `.gitignore` while detecting projects (the rare repo whose lockfiles are
    /// themselves ignored). By default detection skips gitignored paths — correct and faster.
    #[arg(long = "no-gitignore", global = true)]
    pub(in crate::cli) no_gitignore: bool,
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
    /// List every source package on its own line instead of `first (+N others)`.
    #[arg(long = "list-packages", global = true)]
    pub(in crate::cli) list_packages: bool,
    /// Show the "Used by" column as workspace paths instead of package names.
    #[arg(long = "paths", global = true)]
    pub(in crate::cli) paths: bool,
    /// Add the "Project" column attributing each row to its project. Hidden by default: the
    /// "Used by" package names usually identify a row, so the per-project path is mostly noise.
    /// No effect in a single-root repo (there is no distinct project to show).
    #[arg(long = "show-projects", global = true)]
    pub(in crate::cli) show_projects: bool,
    /// Suppress actionable tips (e.g. the `--major` command `upgrade` prints when it holds back a
    /// cross-major update). The reports and their counts are unaffected.
    #[arg(long = "no-suggestions", global = true)]
    pub(in crate::cli) no_suggestions: bool,
    /// Downgrade a stale/absent lock from failure (the default) to a warning.
    #[arg(
        long = "allow-stale-lock",
        global = true,
        env = "COOLDOWN_ALLOW_STALE_LOCK"
    )]
    pub(in crate::cli) allow_stale_lock: bool,
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
    /// How many registry requests to run concurrently — sets both the fan-out width and the
    /// per-host in-flight cap. Higher finishes a large workspace faster; too high can trip a
    /// registry's rate limit. Defaults to 16; also settable per-section in config as `concurrency`.
    #[arg(long, global = true, value_name = "N", env = "COOLDOWN_CONCURRENCY")]
    pub(in crate::cli) concurrency: Option<usize>,
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
    /// Evaluate as if the current time were this RFC3339 instant or `YYYY-MM-DD` date, instead of the
    /// system clock. Debug builds only (hidden); it exists to regenerate the README screenshots
    /// reproducibly. Releases dated after it are treated as not-yet-published.
    #[cfg(debug_assertions)]
    #[arg(long, global = true, value_name = "DATE", hide = true)]
    pub(in crate::cli) now: Option<String>,
}

impl GlobalArgs {
    /// The evaluation-clock override (`--now`), parsed to an instant. Always `None` in release
    /// builds — the flag exists only in debug builds — so production runs read the system clock.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError`](cooldown_core::CoreError) if the value is not an RFC3339 instant or a
    /// `YYYY-MM-DD` date.
    pub(in crate::cli) fn now_override(
        &self,
    ) -> Result<Option<jiff::Timestamp>, cooldown_core::CoreError> {
        #[cfg(debug_assertions)]
        let parsed = self
            .now
            .as_deref()
            .map(cooldown_core::duration::parse_freeze)
            .transpose();
        #[cfg(not(debug_assertions))]
        let parsed: Result<Option<jiff::Timestamp>, cooldown_core::CoreError> = Ok(None);
        parsed
    }
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
    pub(crate) all_artifacts: Option<bool>,
    pub(crate) allow_stale_lock: Option<bool>,
    pub(crate) fail_on_unknown_age: Option<bool>,
    pub(crate) strict: Option<bool>,
    pub(crate) build: Option<bool>,
    /// `outdated --transitive` — list indirect deps in the report (a bool; outdated-only).
    pub(crate) transitive: Option<bool>,
    pub(crate) downgrade_pinned: Option<bool>,
    /// `check`/`fix`/`upgrade --transitive <allow|hide>` — the shared transitive-handling mode, if
    /// set on the CLI. Absent, each command acts on transitives by default.
    pub(crate) transitive_mode: Option<TransitiveMode>,
    /// `outdated --exit-code [N]` — the CI gate exit code, if set on the CLI.
    pub(crate) exit_code: Option<u8>,
    /// `outdated --hide-pinned` — CLI-only display filter (not config-backed).
    pub(crate) hide_pinned: Option<bool>,
    /// `outdated --countdown <latest|soonest>` — CLI-only display control (not config-backed);
    /// `None` falls back to [`Countdown::Latest`].
    pub(crate) countdown: Option<Countdown>,
    /// `upgrade --rewrite` — CLI-only manifest-rewrite control (not config-backed).
    pub(crate) rewrite: Option<bool>,
    /// `check`/`config --fail-on-stricter-native` — CLI-only (not config-backed).
    pub(crate) fail_on_stricter_native: Option<bool>,
    /// `check`/`config --no-fail-on-stricter-native` — CLI-only override of a config `strict-native`.
    pub(crate) no_fail_on_stricter_native: Option<bool>,
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
            allow_stale_lock: on("allow_stale_lock"),
            // Per-command flags: probe the owning subcommand directly. The root never registers
            // them, so `set_on_cli`'s root `value_source` would panic on an unknown id.
            all: set_on_subcommand(matches, "outdated", "all").then_some(true),
            all_artifacts: set_on_subcommand(matches, "check", "all_artifacts").then_some(true),
            fail_on_unknown_age: set_on_subcommand(matches, "check", "fail_on_unknown_age")
                .then_some(true),
            build: set_on_subcommand(matches, "upgrade", "build").then_some(true),
            hide_pinned: set_on_subcommand(matches, "outdated", "hide_pinned").then_some(true),
            // `--countdown <latest|soonest>` carries an enum value under `outdated`; absent, the
            // report keeps its `latest` default.
            countdown: matches
                .subcommand_matches("outdated")
                .and_then(|sub| sub.get_one::<Countdown>("countdown").copied()),
            rewrite: set_on_subcommand(matches, "upgrade", "rewrite").then_some(true),
            // `--strict` is shared by the mutating commands (flattened into both `upgrade` and `fix`).
            strict: (set_on_subcommand(matches, "upgrade", "strict")
                || set_on_subcommand(matches, "fix", "strict"))
            .then_some(true),
            // The strict-native pair is shared by `check` and `config`.
            fail_on_stricter_native: (set_on_subcommand(
                matches,
                "check",
                "fail_on_stricter_native",
            ) || set_on_subcommand(
                matches,
                "config",
                "fail_on_stricter_native",
            ))
            .then_some(true),
            no_fail_on_stricter_native: (set_on_subcommand(
                matches,
                "check",
                "no_fail_on_stricter_native",
            ) || set_on_subcommand(
                matches,
                "config",
                "no_fail_on_stricter_native",
            ))
            .then_some(true),
            // `outdated --transitive` is a bool (list indirect deps in the report).
            transitive: set_on_subcommand(matches, "outdated", "transitive").then_some(true),
            downgrade_pinned: set_on_subcommand(matches, "fix", "downgrade_pinned").then_some(true),
            // `--transitive <allow|hide>` is the shared enum on `check`/`fix`/`upgrade`; only the
            // active subcommand carries a value, so take it from whichever ran.
            transitive_mode: ["check", "fix", "upgrade"].iter().find_map(|command| {
                matches
                    .subcommand_matches(command)
                    .and_then(|sub| sub.get_one::<TransitiveMode>("transitive").copied())
            }),
            // `--exit-code [N]` carries an optional value under `outdated`.
            exit_code: matches
                .subcommand_matches("outdated")
                .and_then(|sub| sub.get_one::<u8>("exit_code").copied()),
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

/// Whether `id` was explicitly set under the `command` subcommand. Used for per-command flags (e.g.
/// `fix --transitive`) that the *root* matches do not know — [`set_on_cli`]'s root `value_source`
/// would panic on an id the root command never registered.
fn set_on_subcommand(matches: &ArgMatches, command: &str, id: &str) -> bool {
    matches.subcommand_matches(command).is_some_and(|sub| {
        matches!(
            sub.value_source(id),
            Some(ValueSource::CommandLine | ValueSource::EnvVariable)
        )
    })
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
        // A genuinely global flag propagates whether it comes before or after the subcommand.
        assert_eq!(
            overrides(&["cooldown", "outdated", "--dry-run"]).dry_run,
            Some(true)
        );
        assert_eq!(
            overrides(&["cooldown", "--dry-run", "outdated"]).dry_run,
            Some(true)
        );
    }

    #[test]
    fn per_command_flags_are_captured_from_their_subcommand() {
        assert_eq!(
            overrides(&["cooldown", "outdated", "--all"]).all,
            Some(true)
        );
        assert_eq!(
            overrides(&["cooldown", "upgrade", "--strict"]).strict,
            Some(true)
        );
        assert_eq!(
            overrides(&["cooldown", "fix", "--strict"]).strict,
            Some(true)
        );
        assert_eq!(
            overrides(&["cooldown", "outdated", "--exit-code=2"]).exit_code,
            Some(2)
        );
        assert_eq!(
            overrides(&["cooldown", "check", "--fail-on-stricter-native"]).fail_on_stricter_native,
            Some(true)
        );
    }

    #[test]
    fn per_command_flags_are_rejected_on_the_wrong_command() {
        // The whole point of scoping: a flag is an error where it does not apply, not silently
        // accepted. `--hide-pinned` belongs to `outdated`, not `check`.
        let parsed = Cli::command().try_get_matches_from(["cooldown", "check", "--hide-pinned"]);
        assert!(parsed.is_err());
        // `--strict` belongs to the mutating commands, not `outdated`.
        let parsed = Cli::command().try_get_matches_from(["cooldown", "outdated", "--strict"]);
        assert!(parsed.is_err());
    }

    #[test]
    fn fix_flags_are_explicit_overrides() {
        let ov = overrides(&[
            "cooldown",
            "fix",
            "--transitive",
            "hide",
            "--downgrade-pinned",
        ]);
        assert_eq!(ov.transitive_mode, Some(super::TransitiveMode::Hide));
        assert_eq!(ov.downgrade_pinned, Some(true));
    }

    #[test]
    fn transitive_mode_is_captured_from_check_fix_or_upgrade() {
        assert_eq!(
            overrides(&["cooldown", "check", "--transitive", "allow"]).transitive_mode,
            Some(super::TransitiveMode::Allow)
        );
        assert_eq!(
            overrides(&["cooldown", "upgrade", "--transitive", "hide"]).transitive_mode,
            Some(super::TransitiveMode::Hide)
        );
        // Default (no flag) leaves it unset, so each command applies its own act-on-transitives default.
        assert_eq!(overrides(&["cooldown", "fix"]).transitive_mode, None);
    }

    #[test]
    fn outdated_transitive_is_a_bool_override() {
        assert_eq!(
            overrides(&["cooldown", "outdated", "--transitive"]).transitive,
            Some(true)
        );
        assert_eq!(overrides(&["cooldown", "outdated"]).transitive, None);
    }

    #[test]
    fn outdated_countdown_captures_the_selected_horizon() {
        assert_eq!(
            overrides(&["cooldown", "outdated", "--countdown", "soonest"]).countdown,
            Some(super::Countdown::Soonest)
        );
        assert_eq!(
            overrides(&["cooldown", "outdated", "--countdown", "latest"]).countdown,
            Some(super::Countdown::Latest)
        );
        // Absent, it is unset, so the report keeps its `latest` default.
        assert_eq!(overrides(&["cooldown", "outdated"]).countdown, None);
    }

    #[test]
    fn parser_accepts_full_command_shape() {
        let cli = Cli::parse_from(["cooldown", "check", "--json"]);
        assert!(matches!(cli.command, super::Command::Check { .. }));
        assert!(cli.global.json);
    }
}
