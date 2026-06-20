use super::{Command, commands, setup, workspace_free};
use crate::app::Exit;
use crate::cli::{Cli, CliOverrides};
use camino::Utf8PathBuf;
use cooldown_core::CoreError;

/// Install the tracing subscriber that writes to stderr.
///
/// `RUST_LOG`, when set and non-empty, is honored verbatim as a full `EnvFilter` directive (the
/// standard Rust escape hatch); otherwise the `--log-level` / `COOLDOWN_LOG` selection drives it.
/// Idempotent: a second call is a no-op (so tests that build a CLI can't double-install).
fn init_logging(level: super::LogLevel) {
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

/// Run a parsed [`Cli`], returning the process [`Exit`].
///
/// Any [`CoreError`] that escapes the dispatch is printed to stderr and mapped to an exit by its
/// kind, so this function itself is infallible.
pub async fn run(cli: Cli, overrides: CliOverrides) -> Exit {
    init_logging(cli.global.log_level);
    match run_inner(cli, overrides).await {
        Ok(exit) => exit,
        Err(error) => {
            tracing::debug!(error = %error, "run failed");
            eprintln!("error: {error}");
            exit_for_error(&error)
        }
    }
}

fn exit_for_error(error: &CoreError) -> Exit {
    match error {
        // Bad user input (config/CLI/duration/glob) is a usage error; everything else — including
        // filesystem/runtime I/O — is an environment fault.
        CoreError::Config(_) => Exit::Usage,
        _ => Exit::Environment,
    }
}

async fn run_inner(cli: Cli, overrides: CliOverrides) -> Result<Exit, CoreError> {
    let global = &cli.global;

    if let Some(result) = workspace_free::run_workspace_free(&cli.command, global) {
        return result;
    }

    // `outdated` discovers, so it defaults to cross-major; mutating/gating commands don't. The
    // config (`[<command>]`/`[global]`) and `--major`/`--no-major` refine this inside `prepare_run`.
    let default_major = matches!(cli.command, Command::Outdated { .. });
    let prepared = setup::prepare_run(
        global,
        &overrides,
        command_name(&cli.command),
        default_major,
    )
    .await?;
    let repo_root = prepared.repo_root;
    let ws = prepared.ws;
    let opts = prepared.opts;
    // `--json` is itself config-resolvable, so color and the no-tool output key off the
    // resolved value rather than the raw flag. `--color` then forces/suppresses (default: auto).
    let color = global.color.resolve(opts.json);

    if ws.is_empty() {
        let workdir = global.dir.clone().unwrap_or_else(|| Utf8PathBuf::from("."));
        if opts.json {
            println!("{}", commands::no_tool_json(command_name(&cli.command))?);
        } else {
            eprintln!("no supported tool detected under {workdir}");
        }
        return Ok(Exit::NoTool);
    }

    if requires_tool_match(&cli.command) && !tool_matches_project(&ws, &opts) {
        return Err(CoreError::Config(
            "--tool matched no detected project in scope".to_string(),
        ));
    }

    // `--sync` (opt-in) writes the policy into native config first, so the command runs against an
    // up-to-date lock and cooldown.toml stays the source of truth. Skipped under `--dry-run` (a dry
    // run must not mutate). Only the dependency commands pre-sync; `sync`/`config`/etc. do not.
    if global.sync && !opts.dry_run && pre_syncs(&cli.command) {
        let synced = ws.sync(&opts).await;
        if !synced.exit.is_ok() {
            eprintln!("sync failed before {}", command_name(&cli.command));
            return Ok(synced.exit);
        }
        if synced.summary.written > 0 {
            opts.progress.say(&format!(
                "synced policy into {} native config(s)",
                synced.summary.written
            ));
        }
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

/// Whether `--sync` pre-syncs native config before this command (the dependency commands only).
fn pre_syncs(command: &Command) -> bool {
    matches!(
        command,
        Command::Outdated { .. } | Command::Upgrade | Command::Fix { .. } | Command::Check { .. }
    )
}

fn requires_tool_match(command: &Command) -> bool {
    matches!(
        command,
        Command::Upgrade | Command::Fix { .. } | Command::Baseline { .. } | Command::Explain { .. }
    )
}

fn tool_matches_project(ws: &crate::app::Workspace, opts: &crate::app::RunOpts) -> bool {
    ws.projects()
        .iter()
        .any(|project| opts.tool.is_empty() || opts.tool.contains(&project.tool))
}

pub(in crate::cli) fn generated_at(now: jiff::Timestamp) -> String {
    jiff::Timestamp::from_second(now.as_second())
        .map_or_else(|_| now.to_string(), |timestamp| timestamp.to_string())
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Outdated { .. } => "outdated",
        Command::Upgrade => "upgrade",
        Command::Fix { .. } => "fix",
        Command::Check { .. } => "check",
        Command::Baseline { .. } => "baseline",
        Command::Explain { .. } => "explain",
        Command::Config => "config",
        Command::Init => "init",
        Command::Schema => "schema",
        Command::Sync => "sync",
    }
}
