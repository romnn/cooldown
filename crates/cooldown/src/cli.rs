//! The CLI composition root: clap parsing, config discovery, adapter wiring, and dispatch. This is
//! the only place that knows the full cast of ecosystems.

use crate::app::{Baseline, Exit, ProjectCtx, RunOpts, Workspace};
use crate::discovery;
use camino::{Utf8Path, Utf8PathBuf};
use clap::{Args, Parser, Subcommand};
use cooldown_cargo::CargoEcosystem;
use cooldown_core::config::{WindowFields, builtin_default_layer, layer_from_fields};
use cooldown_core::{
    CoreError, Ecosystem, EcosystemId, Origin, PatternGlob, PolicyLayer, PolicyStack, Project,
    ecosystem_id, normalize_native,
};
use cooldown_go::GoEcosystem;
use cooldown_registry::{HttpOptions, SharedHttp};
use cooldown_render as render;
use cooldown_uv::UvEcosystem;
use std::io::IsTerminal;
use std::time::Duration;

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
        // Bad user input (config/CLI/duration/glob) is a usage error; everything else — a stale or
        // unreadable lock, a malformed registry payload, a tool failure — is an environment fault.
        CoreError::Config(_) | CoreError::Io(_) => Exit::Usage,
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
                .map_err(|e| CoreError::Io(format!("serialize schema: {e}")))
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

    let workdir = match &g.dir {
        Some(d) => d.clone(),
        None => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(CoreError::from)?)
            .map_err(|_| CoreError::Io("current dir is not valid UTF-8".into()))?,
    };
    let repo_root = discovery::find_repo_root(&workdir);

    // Resolve the requested ecosystems (a typo is exit 2).
    let lang_ids = parse_langs(&g.lang)?;
    let package_globs = parse_globs(&g.package)?;

    let adapters = adapter_set(g)?;

    // Detect projects under the working dir.
    let mut projects: Vec<(EcosystemId, Project)> = Vec::new();
    for adapter in &adapters {
        for p in adapter.detect(&workdir).await? {
            projects.push((adapter.id(), p));
        }
    }
    if projects.is_empty() {
        if g.json {
            println!("{}", no_ecosystem_json(command_name(&cli.command)));
        } else {
            eprintln!("no supported ecosystem detected under {workdir}");
        }
        return Ok(Exit::NoEcosystem);
    }

    let shared = build_shared_layers(g)?;
    let mut ctxs: Vec<ProjectCtx> = Vec::new();
    for (eco, project) in projects {
        ctxs.push(assemble_ctx(&adapters, &repo_root, eco, project, &shared, g).await?);
    }

    let baseline = Baseline::load(&repo_root.join(crate::app::baseline::BASELINE_FILE))?;
    let now = jiff::Timestamp::now();
    let ws = Workspace::new(adapters, ctxs, now, baseline);

    let opts = RunOpts {
        lang: lang_ids,
        package: package_globs,
        allow_major: g.major,
        major_all: g.major_all,
        direct_only: g.direct_only,
        include_indirect: g.include_indirect,
        all_artifacts: g.all_artifacts,
        allow_stale_lock: g.allow_stale_lock,
        fail_on_unknown_age: g.fail_on_unknown_age,
        strict: g.strict,
        build: g.build,
        dry_run: g.dry_run,
        concurrency: 8,
    };

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

    let generated_at = generated_at(now);
    dispatch(cli.command, &ws, &opts, &repo_root, g, color, &generated_at).await
}

/// The shared, project-independent policy layers, assembled once per run.
struct SharedLayers {
    global: Option<PolicyLayer>,
    explicit: Option<PolicyLayer>,
    env: Option<PolicyLayer>,
    cli: Option<PolicyLayer>,
}

/// Build the ecosystem adapter set (one per supported ecosystem) over a shared HTTP cache.
fn adapter_set(g: &GlobalArgs) -> Result<Vec<Box<dyn Ecosystem>>, CoreError> {
    let http = SharedHttp::new(
        discovery::cache_dir().into_std_path_buf(),
        HttpOptions {
            offline: g.offline,
            fresh: g.fresh,
            request_timeout: Duration::from_secs(30),
            ..Default::default()
        },
    )?;
    Ok(vec![
        Box::new(GoEcosystem::from_http(http.clone())),
        Box::new(CargoEcosystem::from_http(http.clone())),
        Box::new(UvEcosystem::from_http(http.clone())),
    ])
}

/// Build the project-independent layers (global file, explicit `--config`, env, CLI) once per run.
fn build_shared_layers(g: &GlobalArgs) -> Result<SharedLayers, CoreError> {
    let global = if g.no_global {
        None
    } else {
        discovery::global_layer()?
    };
    let explicit = match &g.config {
        Some(p) => Some(discovery::explicit_config_layer(p)?),
        None => None,
    };
    Ok(SharedLayers {
        global,
        explicit,
        env: layer_from_fields(Origin::Env, &env_window_fields())?,
        cli: layer_from_fields(Origin::Cli, &cli_window_fields(g))?,
    })
}

/// Assemble one project's [`ProjectCtx`]: its native layer, the repo cascade, the shared layers,
/// and the relative path used by `project` selectors.
async fn assemble_ctx(
    adapters: &[Box<dyn Ecosystem>],
    repo_root: &Utf8Path,
    eco: EcosystemId,
    project: Project,
    shared: &SharedLayers,
    g: &GlobalArgs,
) -> Result<ProjectCtx, CoreError> {
    let native = if g.no_native {
        None
    } else {
        match adapters.iter().find(|a| a.id() == eco) {
            Some(a) => a.native_policy(&project).await?.map(normalize_native),
            None => None,
        }
    };
    let cascade = discovery::repo_cascade_layers(repo_root, &project.root)?;

    let mut layers: Vec<PolicyLayer> = vec![builtin_default_layer()];
    if let Some(l) = &shared.global {
        layers.push(l.clone());
    }
    if let Some(n) = native {
        layers.push(n);
    }
    layers.extend(cascade);
    for l in [&shared.explicit, &shared.env, &shared.cli]
        .into_iter()
        .flatten()
    {
        layers.push(l.clone());
    }

    let strict_native = compute_strict_native(&layers, g);
    let rel_path = project.root.strip_prefix(repo_root).ok().map_or_else(
        || project.root.clone(),
        |p| {
            if p.as_str().is_empty() {
                Utf8PathBuf::from(".")
            } else {
                p.to_owned()
            }
        },
    );

    Ok(ProjectCtx {
        ecosystem: eco,
        project,
        rel_path,
        policy: PolicyStack {
            layers,
            strict_native,
        },
    })
}

/// Run the chosen workspace command and emit its output (JSON or TTY), returning its [`Exit`].
///
/// The workspace-free commands (`schema`/`init`/`sync`) are handled before a workspace exists, so
/// they never reach here.
async fn dispatch(
    command: Command,
    ws: &Workspace,
    opts: &RunOpts,
    repo_root: &Utf8Path,
    g: &GlobalArgs,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let exit = match command {
        Command::Outdated => run_outdated(ws, opts, g, color, generated_at).await?,
        Command::Check => run_check(ws, opts, g, color, generated_at).await?,
        Command::Upgrade => run_upgrade(ws, opts, g, color, generated_at).await?,
        Command::Explain { package } => {
            run_explain(ws, opts, &package, g, color, generated_at).await?
        }
        Command::Config => run_config(ws, opts, g, generated_at),
        Command::Baseline { prune } => {
            cmd_baseline(ws, opts, repo_root, prune, g.json, generated_at).await?
        }
        // `run_workspace_free` returns `Some` for exactly these, so `run_inner` returns before a
        // workspace is built; they can never reach `dispatch`.
        #[allow(
            clippy::unreachable,
            reason = "schema/init/sync are dispatched by run_workspace_free before any workspace exists"
        )]
        Command::Schema | Command::Init | Command::Sync => unreachable!("handled earlier"),
    };

    Ok(exit)
}

async fn run_outdated(
    ws: &Workspace,
    opts: &RunOpts,
    g: &GlobalArgs,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let out = ws.outdated(opts).await;
    let env = render::Envelope::new(
        "outdated",
        out.exit.is_ok(),
        generated_at.to_owned(),
        render::OutdatedMeta {},
        out.summary.clone(),
        out.items.clone(),
    );
    let env = with_diags(env, out.warnings.clone(), out.errors.clone());
    if g.json {
        let json = render::to_json(&env)
            .map_err(|e| CoreError::Io(format!("serialize JSON output: {e}")))?;
        println!("{json}");
    } else {
        print!(
            "{}",
            render::tty::render_outdated(
                &out.summary,
                &out.items,
                &out.warnings,
                &out.errors,
                color
            )
        );
    }
    Ok(out.exit)
}

async fn run_check(
    ws: &Workspace,
    opts: &RunOpts,
    g: &GlobalArgs,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let out = ws.check(opts).await;
    let env = render::Envelope::new(
        "check",
        out.exit.is_ok(),
        generated_at.to_owned(),
        out.meta.clone(),
        out.summary.clone(),
        out.items.clone(),
    );
    let env = with_diags(env, out.warnings.clone(), out.errors.clone());
    if g.json {
        let json = render::to_json(&env)
            .map_err(|e| CoreError::Io(format!("serialize JSON output: {e}")))?;
        println!("{json}");
    } else {
        print!(
            "{}",
            render::tty::render_check(
                &out.meta,
                &out.summary,
                &out.items,
                &out.warnings,
                &out.errors,
                color
            )
        );
    }
    Ok(out.exit)
}

/// Validate the `upgrade`-specific flag combinations before running, then render the result.
///
/// # Errors
///
/// Returns [`CoreError::Config`] for `--include-indirect` (a non-goal) or for `--major` without a
/// scoping `--package`/`--major-all`.
async fn run_upgrade(
    ws: &Workspace,
    opts: &RunOpts,
    g: &GlobalArgs,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    if g.include_indirect {
        return Err(CoreError::Config(
            "`upgrade --include-indirect` is not allowed: acting on transitive deps is a non-goal"
                .into(),
        ));
    }
    if g.major && g.package.is_empty() && !g.major_all {
        return Err(CoreError::Config(
            "`upgrade --major` rewrites import paths repo-wide; pass --package or --major-all"
                .into(),
        ));
    }
    let out = ws.upgrade(opts).await;
    let env = render::Envelope::new(
        "upgrade",
        out.exit.is_ok(),
        generated_at.to_owned(),
        out.meta.clone(),
        out.summary.clone(),
        out.items.clone(),
    );
    let env = with_diags(env, out.warnings.clone(), out.errors.clone());
    if g.json {
        let json = render::to_json(&env)
            .map_err(|e| CoreError::Io(format!("serialize JSON output: {e}")))?;
        println!("{json}");
    } else {
        print!(
            "{}",
            render::tty::render_upgrade(
                &out.meta,
                &out.summary,
                &out.items,
                &out.warnings,
                &out.errors,
                color
            )
        );
    }
    Ok(out.exit)
}

async fn run_explain(
    ws: &Workspace,
    opts: &RunOpts,
    package: &str,
    g: &GlobalArgs,
    color: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let out = ws.explain(package, opts).await;
    let env = render::Envelope::new(
        "explain",
        out.exit.is_ok(),
        generated_at.to_owned(),
        out.meta.clone(),
        render::ExplainSummary {},
        out.steps.clone(),
    );
    if g.json {
        let json = render::to_json(&env)
            .map_err(|e| CoreError::Io(format!("serialize JSON output: {e}")))?;
        println!("{json}");
    } else {
        print!(
            "{}",
            render::tty::render_explain(&out.meta, &out.steps, color)
        );
    }
    Ok(out.exit)
}

fn run_config(ws: &Workspace, opts: &RunOpts, g: &GlobalArgs, generated_at: &str) -> Exit {
    let out = ws.config(opts, generated_at);
    if g.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&out.json).unwrap_or_default()
        );
    } else {
        print!("{}", out.text);
    }
    out.exit
}

fn with_diags<M: serde::Serialize, S: serde::Serialize, I: serde::Serialize>(
    mut env: render::Envelope<M, S, I>,
    warnings: Vec<cooldown_core::Diagnostic>,
    errors: Vec<cooldown_core::Diagnostic>,
) -> render::Envelope<M, S, I> {
    env.warnings = warnings;
    env.errors = errors;
    env
}

fn generated_at(now: jiff::Timestamp) -> String {
    jiff::Timestamp::from_second(now.as_second())
        .map_or_else(|_| now.to_string(), |t| t.to_string())
}

fn parse_langs(langs: &[String]) -> Result<Vec<EcosystemId>, CoreError> {
    langs
        .iter()
        .map(|l| {
            ecosystem_id(l).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown --lang `{l}`; recognised: go, rust, python, node"
                ))
            })
        })
        .collect()
}

fn parse_globs(globs: &[String]) -> Result<Vec<PatternGlob>, CoreError> {
    globs.iter().map(|g| PatternGlob::new(g)).collect()
}

fn cli_window_fields(g: &GlobalArgs) -> WindowFields {
    WindowFields {
        min_age: g.min_age.clone(),
        min_age_major: g.min_age_major.clone(),
        min_age_minor: g.min_age_minor.clone(),
        min_age_patch: g.min_age_patch.clone(),
        latest: g.latest,
        freeze: g.freeze.clone(),
        allow: g.allow.clone(),
    }
}

fn env_window_fields() -> WindowFields {
    let var = |k: &str| std::env::var(k).ok().filter(|s| !s.is_empty());
    let truthy = |k: &str| matches!(var(k).as_deref(), Some("1" | "true" | "yes" | "on"));
    WindowFields {
        min_age: var("COOLDOWN_MIN_AGE"),
        min_age_major: var("COOLDOWN_MIN_AGE_MAJOR"),
        min_age_minor: var("COOLDOWN_MIN_AGE_MINOR"),
        min_age_patch: var("COOLDOWN_MIN_AGE_PATCH"),
        latest: truthy("COOLDOWN_LATEST"),
        freeze: var("COOLDOWN_FREEZE"),
        allow: var("COOLDOWN_ALLOW")
            .map(|s| {
                s.split(',')
                    .map(|x| x.trim().to_string())
                    .filter(|x| !x.is_empty())
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// `strict-native` is security-monotone (any config layer that sets it turns it on); the CLI flags
/// force it on/off.
fn compute_strict_native(layers: &[PolicyLayer], g: &GlobalArgs) -> bool {
    if g.no_fail_on_stricter_native {
        return false;
    }
    if g.fail_on_stricter_native {
        return true;
    }
    layers.iter().any(|l| l.strict_native == Some(true))
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

fn no_ecosystem_json(command: &str) -> String {
    serde_json::json!({
        "schemaVersion": render::SCHEMA_VERSION,
        "command": command,
        "ok": false,
        "generatedAt": generated_at(jiff::Timestamp::now()),
        "summary": {},
        "items": [],
        "warnings": [],
        "errors": [{ "kind": "not_found", "message": "no supported ecosystem detected" }],
    })
    .to_string()
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

async fn cmd_baseline(
    ws: &Workspace,
    opts: &RunOpts,
    repo_root: &Utf8Path,
    prune: bool,
    json: bool,
    generated_at: &str,
) -> Result<Exit, CoreError> {
    let path = repo_root.join(crate::app::baseline::BASELINE_FILE);
    let existing = Baseline::load(&path)?;
    let young = ws.baseline_entries(opts).await?;

    let key = |e: &crate::app::baseline::AckEntry| {
        (
            e.ecosystem.clone(),
            e.project.clone(),
            e.package.clone(),
            e.version.clone(),
            e.registry.clone(),
        )
    };
    let young_keys: std::collections::HashSet<_> = young.iter().map(key).collect();

    let merged = if prune {
        // Keep only entries that are still young; preserve existing metadata (reason/until).
        young
            .into_iter()
            .map(|y| {
                existing
                    .entries
                    .iter()
                    .find(|e| key(e) == key(&y))
                    .map(|e| crate::app::baseline::AckEntry {
                        reason: e.reason.clone(),
                        until: e.until.clone(),
                        ..y.clone()
                    })
                    .unwrap_or(y)
            })
            .collect::<Vec<_>>()
    } else {
        // Additive: keep all existing, add newly-young entries not already present.
        let mut out = existing.entries.clone();
        for y in young {
            if !out.iter().any(|e| key(e) == key(&y)) {
                out.push(y);
            }
        }
        out
    };

    let count = merged.len();
    let new_baseline = Baseline { entries: merged };
    new_baseline.save(&path)?;

    let removed = existing.entries.len().saturating_sub(
        existing
            .entries
            .iter()
            .filter(|e| young_keys.contains(&key(e)) || !prune)
            .count(),
    );

    if json {
        let items: Vec<_> = new_baseline
            .entries
            .iter()
            .map(|e| {
                serde_json::json!({
                    "ecosystem": e.ecosystem,
                    "project": e.project,
                    "package": e.package,
                    "version": e.version,
                    "registry": e.registry,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "schemaVersion": render::SCHEMA_VERSION,
                "command": "baseline",
                "ok": true,
                "generatedAt": generated_at,
                "path": path.to_string(),
                "summary": { "acknowledged": count, "pruned": removed },
                "items": items,
                "warnings": [],
                "errors": [],
            })
        );
    } else {
        println!(
            "wrote {path}: {count} acknowledged entr{}",
            if count == 1 { "y" } else { "ies" }
        );
        if prune && removed > 0 {
            println!(
                "pruned {removed} stale entr{}",
                if removed == 1 { "y" } else { "ies" }
            );
        }
    }
    Ok(Exit::Ok)
}
