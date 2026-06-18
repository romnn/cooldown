//! The CLI composition root: clap parsing, config discovery, adapter wiring, and dispatch. This is
//! the only place that knows the full cast of ecosystems.

mod commands;
mod present;
mod setup;

use crate::app::Exit;
use crate::discovery;
use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
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
    long_about = "Refuse to adopt any dependency version younger than a minimum release age, across ecosystems, from one policy core."
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
    /// Restrict to ecosystem(s); repeatable / comma-separated (default: all detected).
    #[arg(
        long,
        global = true,
        value_name = "LANG",
        value_delimiter = ',',
        env = "COOLDOWN_LANG"
    )]
    lang: Vec<String>,
    /// Only the Rust/Cargo ecosystem — skip detecting/enumerating Go, Python, and Node entirely
    /// (shorthand for `--lang rust`; the right default for a Cargo workspace in a polyglot monorepo).
    #[arg(long, global = true, conflicts_with = "lang")]
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

/// Run a parsed [`Cli`], returning the process [`Exit`].
///
/// Any [`CoreError`] that escapes the dispatch is printed to stderr and mapped to an exit by its
/// kind, so this function itself is infallible.
pub async fn run(cli: Cli) -> Exit {
    init_logging(cli.global.log_level);
    match run_inner(cli).await {
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

async fn run_inner(cli: Cli) -> Result<Exit, CoreError> {
    let g = &cli.global;
    let color = std::io::stdout().is_terminal() && !g.json;

    if let Some(res) = run_workspace_free(&cli.command, g) {
        return res;
    }

    // `outdated` discovers, so it defaults to cross-major; mutating/gating commands don't. The
    // config (`[<command>]`/`[global]`) and `--major`/`--no-major` refine this inside `prepare_run`.
    let default_major = matches!(cli.command, Command::Outdated);
    let prepared = setup::prepare_run(g, command_name(&cli.command), default_major).await?;
    if prepared.ws.is_empty() {
        let workdir = g.dir.clone().unwrap_or_else(|| Utf8PathBuf::from("."));
        if g.json {
            println!(
                "{}",
                commands::no_ecosystem_json(command_name(&cli.command))?
            );
        } else {
            eprintln!("no supported ecosystem detected under {workdir}");
        }
        return Ok(Exit::NoEcosystem);
    }

    let repo_root = prepared.repo_root;
    let ws = prepared.ws;
    let opts = prepared.opts;

    // A no-match `--lang` on a mutating or `explain` command is a usage error (exit 2), distinct
    // from "no ecosystem detected" (exit 3, handled above where `projects` was non-empty).
    let mutating_or_explain = matches!(
        cli.command,
        Command::Upgrade | Command::Baseline { .. } | Command::Explain { .. }
    );
    let lang_matches_a_project = ws
        .projects()
        .iter()
        .any(|p| opts.lang.is_empty() || opts.lang.contains(&p.ecosystem));
    if mutating_or_explain && !lang_matches_a_project {
        return Err(CoreError::Config(
            "--lang matched no detected project in scope".to_string(),
        ));
    }

    let generated_at = generated_at(ws.now());
    commands::dispatch(
        cli.command,
        commands::CommandContext {
            ws: &ws,
            opts: &opts,
            repo_root: &repo_root,
            global: g,
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

# Per ecosystem (npm is the most-attacked registry):
# [lang.node]
# min-age = "21d"

# First-party packages are trusted:
# [package."github.com/acme/*"]
# min-age = "0d"

# Exemptions (audited; shown in `cooldown explain`):
# allow = ["github.com/acme/*"]

# A hard minimum no nearer config can weaken:
# floor = "3d"

# Scan & per-command flag defaults (a CLI flag always overrides these):
# [global]
# exclude = ["third_party"]   # directories never scanned (gitignore is honored by default)
# gitignore = true            # set false to scan gitignored paths too
#
# [lang.rust]
# exclude = ["vendor"]        # extra excludes for one ecosystem
#
# [outdated]
# major = true                # outdated shows cross-major by default; set false for minor-only
"#;
