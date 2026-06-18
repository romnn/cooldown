//! The CLI composition root: clap parsing, config discovery, adapter wiring, and dispatch. This is
//! the only place that knows the full cast of ecosystems.

mod commands;
mod present;
mod setup;

use crate::app::Exit;
use crate::discovery;
use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand};
use cooldown_core::CoreError;
use cooldown_render as render;
use std::io::IsTerminal;

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
    /// Allow major version changes (default: within the current major).
    #[arg(long, global = true)]
    major: bool,
    /// With --major, apply cross-major to ALL eligible deps (else --package is required).
    #[arg(long = "major-all", global = true)]
    major_all: bool,

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
    match run_inner(cli).await {
        Ok(exit) => exit,
        Err(e) => {
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

    let prepared = setup::prepare_run(g).await?;
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
"#;
