//! The CLI composition root: clap parsing, config discovery, adapter wiring, and dispatch. This is
//! the only place that knows the full cast of tools.

mod commands;
mod present;
mod setup;

use crate::app::Exit;
use crate::discovery;
use camino::Utf8PathBuf;
use clap::parser::ValueSource;
use clap::{ArgMatches, Args, Parser, Subcommand, ValueEnum};
use cooldown_core::CoreError;
use cooldown_render as render;
use std::io::IsTerminal;

/// Verbosity for the diagnostic log written to stderr (independent of `--json`/report output).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Default)]
enum LogLevel {
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
    fn directive(self) -> String {
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

/// Install the tracing subscriber that writes to stderr.
///
/// `RUST_LOG`, when set and non-empty, is honored verbatim as a full `EnvFilter` directive (the
/// standard Rust escape hatch); otherwise the `--log-level` / `COOLDOWN_LOG` selection drives it.
/// Idempotent: a second call is a no-op (so tests that build a CLI can't double-install).
fn init_logging(level: LogLevel) {
    use tracing_subscriber::{EnvFilter, fmt};

    let directive = match std::env::var("RUST_LOG") {
        Ok(value) if !value.is_empty() => value,
        _ => level.directive(),
    };
    let filter = EnvFilter::try_new(&directive).unwrap_or_else(|_| EnvFilter::new("off"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

/// The parsed `cooldown` command line: a subcommand plus the global, mostly-policy flags.
///
/// Construct it with clap's [`Parser`] (`Cli::parse()`) and hand it to [`run`].
#[derive(Parser)]
#[command(
    name = "cooldown",
    version,
    about = "A unified, language-agnostic dependency-cooldown CLI",
    long_about = "Refuse to adopt any dependency version younger than a minimum release age, across tools, from one policy core."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
    #[command(flatten)]
    global: GlobalArgs,
}

#[derive(Subcommand)]
enum Command {
    /// What could update — split into "adoptable now" vs "in cooldown".
    Outdated,
    /// Move direct deps to the newest version older than the cooldown; always re-locks.
    Upgrade,
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
struct GlobalArgs {
    /// Window: "7d", "2 weeks", "36h", ISO-8601 "P7D" (default 7d).
    #[arg(long, global = true, value_name = "DUR", conflicts_with_all = ["latest", "freeze"])]
    min_age: Option<String>,
    /// Per-kind window for major jumps.
    #[arg(long = "min-age-major", global = true, value_name = "DUR")]
    min_age_major: Option<String>,
    /// Per-kind window for minor jumps.
    #[arg(long = "min-age-minor", global = true, value_name = "DUR")]
    min_age_minor: Option<String>,
    /// Per-kind window for patch jumps.
    #[arg(long = "min-age-patch", global = true, value_name = "DUR")]
    min_age_patch: Option<String>,
    /// Opt OUT (window = 0) — the explicit, audited escape hatch.
    #[arg(long, global = true, alias = "no-min-age", conflicts_with_all = ["min_age", "freeze"])]
    latest: bool,
    /// An absolute cutoff instead of a rolling window (reproducible).
    #[arg(long, global = true, value_name = "DATE", conflicts_with_all = ["min_age", "latest"])]
    freeze: Option<String>,
    /// Exempt matching packages from the cooldown (repeatable, audited).
    #[arg(long, global = true, value_name = "GLOB")]
    allow: Vec<String>,
    /// Allow major version changes. Default: ON for `outdated` (so a new major is discoverable),
    /// OFF for `upgrade`/`check`/etc. (a major bump is usually breaking work you opt into).
    #[arg(long, global = true)]
    major: bool,
    /// Stay within the current major (the inverse of `--major`; alias `--minor`). Useful for
    /// clean `outdated` output in CI, where `outdated` otherwise shows cross-major candidates.
    #[arg(
        long = "no-major",
        visible_alias = "minor",
        global = true,
        conflicts_with = "major"
    )]
    no_major: bool,
    /// With --major, apply cross-major to ALL eligible deps (else --package is required).
    #[arg(long = "major-all", global = true)]
    major_all: bool,
    /// (outdated) Also list up-to-date deps. Hidden by default so the report shows only deps with
    /// something to act on; the summary line still counts every dependency.
    #[arg(long, global = true)]
    all: bool,

    /// Scope the command to matching packages (repeatable).
    #[arg(long, short = 'p', global = true, value_name = "GLOB")]
    package: Vec<String>,
    /// Restrict to tool(s) — `cargo`, `go`, `uv`, … (aliases like `rust`/`pnpm` accepted);
    /// repeatable / comma-separated (default: all detected).
    #[arg(
        long,
        global = true,
        value_name = "TOOL",
        value_delimiter = ',',
        env = "COOLDOWN_TOOL"
    )]
    tool: Vec<String>,
    /// Only the Rust/Cargo tool — skip detecting/enumerating Go, Python, and Node entirely
    /// (shorthand for `--tool cargo`; the right default for a Cargo workspace in a polyglot monorepo).
    #[arg(long, global = true, conflicts_with = "tool")]
    cargo: bool,
    /// Don't honor `.gitignore` while detecting projects (the rare repo whose lockfiles are
    /// themselves ignored). By default detection skips gitignored paths — correct and faster.
    #[arg(long = "no-gitignore", global = true)]
    no_gitignore: bool,
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
    exit_code: Option<u8>,
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
    log_level: LogLevel,
    /// Evaluate only direct deps.
    #[arg(long = "direct-only", global = true)]
    direct_only: bool,
    /// (outdated) include transitive deps in the report.
    #[arg(long = "include-indirect", global = true)]
    include_indirect: bool,
    /// (check) gate every artifact in a universal lock, not just env-relevant ones.
    #[arg(long = "all-artifacts", global = true)]
    all_artifacts: bool,
    /// Downgrade a stale/absent lock from failure (the default) to a warning.
    #[arg(
        long = "allow-stale-lock",
        global = true,
        env = "COOLDOWN_ALLOW_STALE_LOCK"
    )]
    allow_stale_lock: bool,
    /// Make `check` fail (not just warn) on deps with no publish time.
    #[arg(long = "fail-on-unknown-age", global = true)]
    fail_on_unknown_age: bool,
    /// Make `check`/`config` fail when repo policy overrides a stricter native value.
    #[arg(long = "fail-on-stricter-native", global = true)]
    fail_on_stricter_native: bool,
    /// Override a config-set `strict-native` (the only way to turn it off).
    #[arg(long = "no-fail-on-stricter-native", global = true)]
    no_fail_on_stricter_native: bool,
    /// (upgrade) fail (exit 1) if any planned change was skipped.
    #[arg(long, global = true)]
    strict: bool,
    /// (upgrade) also compile/sync after re-locking.
    #[arg(long, global = true)]
    build: bool,
    /// Resolve and print the plan; never mutate.
    #[arg(long = "dry-run", short = 'n', global = true, env = "COOLDOWN_DRY_RUN")]
    dry_run: bool,
    /// Cache only; cache misses become `UnknownAge` (never a false "ok").
    #[arg(long, global = true, env = "COOLDOWN_OFFLINE")]
    offline: bool,
    /// Ignore the local cache; always hit the registry (use in CI gates).
    #[arg(long, global = true, visible_alias = "no-cache")]
    fresh: bool,
    /// Ignore the native config layer (reproducibility / debugging).
    #[arg(long = "no-native", global = true)]
    no_native: bool,
    /// Ignore the global config layer.
    #[arg(long = "no-global", global = true)]
    no_global: bool,
    /// Load one extra, highest-precedence file layer (still below env/flags).
    #[arg(long, global = true, value_name = "PATH", env = "COOLDOWN_CONFIG")]
    config: Option<Utf8PathBuf>,
    /// Run as if from <path>.
    #[arg(long = "dir", short = 'C', global = true, value_name = "PATH")]
    dir: Option<Utf8PathBuf>,
    /// Machine-readable output (never changes the exit code).
    #[arg(long, global = true)]
    json: bool,
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

/// Run a parsed [`Cli`], returning the process [`Exit`].
///
/// Any [`CoreError`] that escapes the dispatch is printed to stderr and mapped to an exit by its
/// kind, so this function itself is infallible.
pub async fn run(cli: Cli, overrides: CliOverrides) -> Exit {
    init_logging(cli.global.log_level);
    match run_inner(cli, overrides).await {
        Ok(exit) => exit,
        Err(e) => {
            tracing::debug!(error = %e, "run failed");
            eprintln!("error: {e}");
            exit_for_error(&e)
        }
    }
}

fn exit_for_error(e: &CoreError) -> Exit {
    match e {
        // Bad user input (config/CLI/duration/glob) is a usage error; everything else — including
        // filesystem/runtime I/O — is an environment fault.
        CoreError::Config(_) => Exit::Usage,
        _ => Exit::Environment,
    }
}

/// Handle the commands that need neither a workspace nor the network (`schema`, `init`, `sync`).
///
/// Returns `Some` with the command's result when `command` is one of those; `None` otherwise, so
/// the caller proceeds to build a workspace.
fn run_workspace_free(command: &Command, g: &GlobalArgs) -> Option<Result<Exit, CoreError>> {
    match command {
        Command::Schema => Some(
            render::json_schema_string()
                .map_err(|e| CoreError::Serialization(format!("serialize schema: {e}")))
                .map(|schema| {
                    println!("{schema}");
                    Exit::Ok
                }),
        ),
        Command::Init => Some(cmd_init(g)),
        Command::Sync => {
            eprintln!("`cooldown sync` is not implemented in this build (a later phase).");
            Some(Ok(Exit::Usage))
        }
        _ => None,
    }
}

async fn run_inner(cli: Cli, overrides: CliOverrides) -> Result<Exit, CoreError> {
    let g = &cli.global;

    if let Some(res) = run_workspace_free(&cli.command, g) {
        return res;
    }

    // `outdated` discovers, so it defaults to cross-major; mutating/gating commands don't. The
    // config (`[<command>]`/`[global]`) and `--major`/`--no-major` refine this inside `prepare_run`.
    let default_major = matches!(cli.command, Command::Outdated);
    let prepared =
        setup::prepare_run(g, &overrides, command_name(&cli.command), default_major).await?;
    let repo_root = prepared.repo_root;
    let ws = prepared.ws;
    let opts = prepared.opts;
    // `--json` is itself config-resolvable, so color and the no-tool output key off the
    // resolved value rather than the raw flag.
    let color = std::io::stdout().is_terminal() && !opts.json;

    if ws.is_empty() {
        let workdir = g.dir.clone().unwrap_or_else(|| Utf8PathBuf::from("."));
        if opts.json {
            println!("{}", commands::no_tool_json(command_name(&cli.command))?);
        } else {
            eprintln!("no supported tool detected under {workdir}");
        }
        return Ok(Exit::NoTool);
    }

    // A no-match `--tool` on a mutating or `explain` command is a usage error (exit 2), distinct
    // from "no tool detected" (exit 3, handled above where `projects` was non-empty).
    let mutating_or_explain = matches!(
        cli.command,
        Command::Upgrade | Command::Baseline { .. } | Command::Explain { .. }
    );
    let tool_matches_a_project = ws
        .projects()
        .iter()
        .any(|p| opts.tool.is_empty() || opts.tool.contains(&p.tool));
    if mutating_or_explain && !tool_matches_a_project {
        return Err(CoreError::Config(
            "--tool matched no detected project in scope".to_string(),
        ));
    }

    let generated_at = generated_at(ws.now());
    commands::dispatch(
        cli.command,
        commands::CommandContext {
            ws: &ws,
            opts: &opts,
            repo_root: &repo_root,
            color,
            generated_at: &generated_at,
        },
    )
    .await
}

fn generated_at(now: jiff::Timestamp) -> String {
    jiff::Timestamp::from_second(now.as_second())
        .map_or_else(|_| now.to_string(), |t| t.to_string())
}

fn command_name(c: &Command) -> &'static str {
    match c {
        Command::Outdated => "outdated",
        Command::Upgrade => "upgrade",
        Command::Check => "check",
        Command::Baseline { .. } => "baseline",
        Command::Explain { .. } => "explain",
        Command::Config => "config",
        Command::Init => "init",
        Command::Schema => "schema",
        Command::Sync => "sync",
    }
}

fn cmd_init(g: &GlobalArgs) -> Result<Exit, CoreError> {
    let dir = g.dir.clone().unwrap_or_else(|| Utf8PathBuf::from("."));
    let path = dir.join(discovery::CONFIG_FILE);
    if path.exists() {
        eprintln!("refusing to clobber existing {path}");
        return Ok(Exit::Usage);
    }
    std::fs::write(&path, STARTER_CONFIG)?;
    println!("wrote {path}");
    Ok(Exit::Ok)
}

const STARTER_CONFIG: &str = r#"# cooldown.toml — refuse to adopt dependency versions younger than a minimum release age.
# Docs: https://github.com/romnn/cooldown

# The one knob most repos ever set. Durations accept "7d", "2 weeks", ISO-8601 "P7D".
min-age = "7d"

# Risk-tiered windows (use INSTEAD of the scalar above):
# min-age = { default = "7d", patch = "3d", minor = "7d", major = "30d" }

# Per tool (npm is the most-attacked registry):
# [tool.node]
# min-age = "21d"

# First-party packages are trusted:
# [package."github.com/acme/*"]
# min-age = "0d"

# Exemptions (audited; shown in `cooldown explain`):
# allow = ["github.com/acme/*"]

# A hard minimum no nearer config can weaken:
# floor = "3d"

# Flag defaults: [global] applies to every subcommand; a [<command>] section overrides it; an
# explicit CLI flag overrides both. Keys are the kebab-case flag names. A few examples:
# [global]
# exclude = ["third_party"]   # directories never scanned (gitignore is honored by default)
# gitignore = true            # set false to scan gitignored paths too
# offline = false             # cache-only; concurrency = 8 tunes the registry fan-out
#
# [tool.cargo]
# exclude = ["vendor"]        # extra excludes for one tool
#
# [outdated]
# major = true                # outdated shows cross-major by default; set false for minor-only
# all = false                 # also list up-to-date deps; exit-code = 1 gates CI
#
# [upgrade]
# strict = true               # fail if any planned change was skipped; build = true to compile
"#;

#[cfg(test)]
mod tests {
    use super::{Cli, CliOverrides};
    use clap::CommandFactory;

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
}
