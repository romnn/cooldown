use super::{Command, commands, setup, workspace_free};
use crate::app::Exit;
use crate::cli::{Cli, CliOverrides};
use camino::Utf8PathBuf;
use cooldown_core::CoreError;
use std::sync::Arc;
use tracing::instrument::WithSubscriber as _;

/// Builds independent console and run-recording layers.
///
/// `RUST_LOG`, when set and non-empty, is honored verbatim as a full `EnvFilter` directive (the
/// standard Rust escape hatch); otherwise the `--log-level` / `COOLDOWN_LOG` selection drives it.
/// The subscriber is scoped to one run so concurrent library callers cannot share trace files.
fn logging(
    level: super::LogLevel,
    recorder: Option<Arc<crate::trace::Recorder>>,
    detail: cooldown_core::trace::Detail,
) -> tracing::Dispatch {
    use tracing_subscriber::filter::filter_fn;
    use tracing_subscriber::{EnvFilter, fmt, prelude::*};

    let directive = match std::env::var("RUST_LOG") {
        Ok(value) if !value.is_empty() => value,
        _ => level.directive(),
    };
    let filter = EnvFilter::try_new(&directive).unwrap_or_else(|_| EnvFilter::new("off"));
    let console = fmt::layer()
        .with_writer(std::io::stderr)
        .with_target(false)
        .with_filter(filter_fn(|metadata| {
            metadata.target() != cooldown_core::trace::TARGET
        }))
        .with_filter(filter);
    let recording = recorder.map(|recorder| crate::trace::TraceLayer::recording(recorder, detail));
    tracing::Dispatch::new(tracing_subscriber::registry().with(console).with(recording))
}

/// Run a parsed [`Cli`], returning the process [`Exit`].
///
/// Any [`CoreError`] that escapes the dispatch is printed to stderr and mapped to an exit by its
/// kind, so this function itself is infallible.
pub async fn run(cli: Cli, overrides: CliOverrides) -> Exit {
    let directory = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let selected = cli
        .global
        .dir
        .as_ref()
        .map_or_else(|| directory.clone(), |selected| directory.join(selected));
    let trace_command = match &cli.command {
        Command::Upgrade { .. } => Some(crate::trace::TraceCommand::Upgrade),
        Command::Fix { .. } => Some(crate::trace::TraceCommand::Fix),
        Command::Outdated { .. } => Some(crate::trace::TraceCommand::Outdated),
        _ => None,
    }
    .filter(|_| !cli.global.no_trace);
    let detail = if cli.global.full_trace {
        cooldown_core::trace::Detail::Full
    } else {
        cooldown_core::trace::Detail::Compact
    };
    let recorder = trace_command.and_then(|command| {
        match crate::trace::Recorder::start(
            &selected,
            command,
            cli.global
                .trace_dir
                .as_deref()
                .map(camino::Utf8Path::as_std_path),
            detail,
        ) {
            Ok(recorder) => Some(recorder),
            Err(err) => {
                eprintln!("cooldown: warning: cannot record run trace: {err}");
                None
            }
        }
    });
    let subscriber = logging(cli.global.log_level, recorder.clone(), detail);
    async move {
        record_invocation(
            &cli.command,
            InvocationPaths {
                directory: &directory,
                selected: &selected,
            },
        );
        // Keep the command state out of every enclosing subscriber and runtime frame.
        let result = Box::pin(run_inner(cli, overrides)).await;
        let error = result.as_ref().err().map(ToString::to_string);
        let exit = match result {
            Ok(exit) => exit,
            Err(error) => {
                tracing::debug!(error = %error, "run failed");
                eprintln!("error: {error}");
                exit_for_error(&error)
            }
        };
        cooldown_core::trace::Event::RunFinished {
            exit_code: if cooldown_core::interrupt::requested() {
                130
            } else {
                u8::try_from(exit.code()).unwrap_or(4)
            },
            interrupted: cooldown_core::interrupt::requested(),
            error: error.as_deref(),
        }
        .emit();
        if let Some(recorder) = recorder {
            recorder.finish();
        }
        exit
    }
    .with_subscriber(subscriber)
    .await
}

#[derive(Clone, Copy)]
struct InvocationPaths<'a> {
    directory: &'a std::path::Path,
    selected: &'a std::path::Path,
}

fn record_invocation(command: &Command, paths: InvocationPaths<'_>) {
    if let Some(detail) = cooldown_core::trace::Detail::current() {
        cooldown_core::trace::Event::RunStarted {
            command: command_name(command),
            version: env!("CARGO_PKG_VERSION"),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            directory: &paths.directory.to_string_lossy(),
            detail,
        }
        .emit();
        let arguments: Vec<String> = std::env::args_os()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        let environment: Vec<_> = std::env::vars_os()
            .filter_map(|(name, value)| {
                let name = name.to_string_lossy();
                (name.starts_with("COOLDOWN_")
                    || matches!(
                        name.as_ref(),
                        "PATH"
                            | "HOME"
                            | "USERPROFILE"
                            | "TMPDIR"
                            | "TEMP"
                            | "TMP"
                            | "XDG_CONFIG_HOME"
                            | "XDG_CACHE_HOME"
                            | "APPDATA"
                            | "LOCALAPPDATA"
                    ))
                .then(|| cooldown_core::trace::EnvironmentOverride {
                    name: name.into_owned(),
                    value: Some(value.to_string_lossy().into_owned()),
                })
            })
            .collect();
        cooldown_core::trace::Event::Invocation {
            arguments: &arguments,
            environment: &environment,
            selected_directory: &paths.selected.to_string_lossy(),
        }
        .emit();
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

#[tracing::instrument(name = "command", skip_all, fields(command = command_name(&cli.command)))]
async fn run_inner(cli: Cli, overrides: CliOverrides) -> Result<Exit, CoreError> {
    let global = &cli.global;

    if let Some(result) = workspace_free::run_workspace_free(&cli.command, global) {
        return result;
    }

    if matches!(cli.command, Command::Recover) {
        let prepared = match setup::prepare_recovery(global) {
            Ok(prepared) => prepared,
            Err(error) if global.json => {
                let exit = exit_for_error(&error);
                return commands::run_recover_error(&error, exit);
            }
            Err(error) => return Err(error),
        };
        return commands::run_recover(prepared);
    }

    let setup_command = setup::SetupCommand::for_command(&cli.command).ok_or_else(|| {
        CoreError::System("workspace-free command reached workspace setup".to_string())
    })?;
    let prepared = Box::pin(setup::prepare_run(global, &overrides, setup_command)).await?;
    let repo_root = prepared.repo_root;
    cooldown_core::trace::Event::Workspace {
        repository: repo_root.as_str(),
    }
    .emit();
    let ws = prepared.ws;
    let opts = prepared.opts;
    // `--json` is itself config-resolvable, so color and the no-tool output key off the
    // resolved value rather than the raw flag. `--color` then forces/suppresses (default: auto).
    let color = global.color.resolve(opts.json);

    ws.trace_initial().await;

    if ws.is_empty() {
        opts.progress.finish_run();
        let workdir = global.dir.clone().unwrap_or_else(|| Utf8PathBuf::from("."));
        if opts.json {
            super::output::stdout_line(&commands::no_tool_json(command_name(&cli.command))?)?;
        } else {
            eprintln!("no supported tool detected under {workdir}");
        }
        return Ok(Exit::NoTool);
    }

    if requires_tool_match(&cli.command) && !tool_matches_project(&ws, &opts) {
        opts.progress.finish_run();
        return Err(CoreError::Config(
            "--tool matched no detected project in scope".to_string(),
        ));
    }

    // `--sync` (opt-in) writes the policy into native config first, so the command runs against an
    // up-to-date lock and cooldown.toml stays the source of truth. Skipped under `--dry-run` (a dry
    // run must not mutate). Only the dependency commands pre-sync; `sync`/`config`/etc. do not.
    let mut pre_sync_warnings = Vec::new();
    if global.sync && !opts.dry_run && pre_syncs(&cli.command) {
        let synced = ws.sync(&opts).await;
        if cooldown_core::trace::enabled() {
            let report = serde_json::json!({ "command": "sync", "phase": "before_command", "exitCode": synced.exit.code(), "summary": synced.summary, "items": synced.items, "warnings": synced.warnings, "errors": synced.errors });
            cooldown_core::trace::Event::Report { report: &report }.emit();
        }
        if !synced.exit.is_ok() {
            opts.progress.finish_run();
            for warning in &synced.warnings {
                eprintln!("warning [{}]: {}", warning.kind, warning.message);
            }
            eprintln!("sync failed before {}", command_name(&cli.command));
            return Ok(synced.exit);
        }
        pre_sync_warnings = synced.warnings;
        if synced.summary.written > 0 {
            opts.progress.phase(format!(
                "synced policy into {} native config(s)",
                synced.summary.written
            ));
        }
    }

    let generated_at = generated_at(ws.now());
    let mut progress_project_tools = ws.progress_project_tools(&opts);
    if matches!(cli.command, Command::Explain { .. }) {
        progress_project_tools.truncate(1);
    }
    opts.progress.start_run(&progress_project_tools);
    // Dispatch owns a large command future; store it once instead of copying it through spans.
    let result = Box::pin(commands::dispatch(
        cli.command,
        commands::CommandContext {
            ws: &ws,
            opts: &opts,
            repo_root: &repo_root,
            color,
            generated_at: &generated_at,
            pre_sync_warnings: &pre_sync_warnings,
        },
    ))
    .await;
    opts.progress.finish_run();

    result
}

/// Whether `--sync` pre-syncs native config before this command (the dependency commands only).
fn pre_syncs(command: &Command) -> bool {
    matches!(
        command,
        Command::Outdated { .. }
            | Command::Upgrade { .. }
            | Command::Fix { .. }
            | Command::Check { .. }
    )
}

fn requires_tool_match(command: &Command) -> bool {
    matches!(
        command,
        Command::Upgrade { .. }
            | Command::Fix { .. }
            | Command::Baseline { .. }
            | Command::Explain { .. }
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
        Command::Upgrade { .. } => "upgrade",
        Command::Fix { .. } => "fix",
        Command::Check { .. } => "check",
        Command::Baseline { .. } => "baseline",
        Command::Explain { .. } => "explain",
        Command::Config { .. } => "config",
        Command::Init => "init",
        Command::Schema => "schema",
        Command::Prune { .. } => "prune",
        Command::Recover => "recover",
        Command::Sync => "sync",
    }
}
