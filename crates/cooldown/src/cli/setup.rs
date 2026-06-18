use super::{CliOverrides, GlobalArgs, LogLevel};
use crate::app::{AdapterSet, Baseline, Progress, ProjectCtx, RunOpts, Workspace};
use crate::discovery;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_cargo::{CARGO_ID, CargoEcosystem};
use cooldown_core::config::{CommandConfig, WindowFields, builtin_default_layer, layer_from_fields};
use cooldown_core::{
    CoreError, EcosystemId, Origin, PatternGlob, PolicyLayer, PolicyStack, Project, ecosystem_id,
    normalize_native,
};
use cooldown_go::GoEcosystem;
use cooldown_registry::{HttpOptions, SharedHttp};
use cooldown_uv::UvEcosystem;
use std::sync::Arc;
use std::time::Duration;

pub(crate) struct PreparedRun {
    pub(crate) repo_root: Utf8PathBuf,
    pub(crate) ws: Workspace,
    pub(crate) opts: RunOpts,
}

struct SharedLayers {
    global: Option<PolicyLayer>,
    explicit: Option<PolicyLayer>,
    env: Option<PolicyLayer>,
    cli: Option<PolicyLayer>,
}

pub(crate) async fn prepare_run(
    g: &GlobalArgs,
    overrides: &CliOverrides,
    command_key: &str,
    default_major: bool,
) -> Result<PreparedRun, CoreError> {
    let workdir = match &g.dir {
        Some(dir) => dir.clone(),
        None => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(CoreError::from)?)
            .map_err(|_| CoreError::PathEncoding("current dir is not valid UTF-8".into()))?,
    };
    let repo_root = discovery::find_repo_root(&workdir);
    let scan = discovery::scan_config(&repo_root, g.config.as_deref(), g.no_global)?;
    // The resolved flag defaults for this command ([global] + [<command>]); each RunOpts field is
    // then `explicit CLI flag ?? config ?? built-in default` (see `resolve_flag`).
    let cfg = scan.resolved(command_key);

    let json = resolve_flag(overrides.json, cfg.json, false);
    let respect_gitignore = resolve_flag(overrides.gitignore, cfg.gitignore, true);
    let lang = resolve_langs(g, &cfg)?;
    let opts = RunOpts {
        lang: lang.clone(),
        package: resolve_globs(g, &cfg)?,
        allow_major: resolve_flag(overrides.major, cfg.major, default_major),
        major_all: resolve_flag(overrides.major_all, cfg.major_all, false),
        direct_only: resolve_flag(overrides.direct_only, cfg.direct_only, false),
        include_indirect: resolve_flag(overrides.include_indirect, cfg.include_indirect, false),
        all_artifacts: resolve_flag(overrides.all_artifacts, cfg.all_artifacts, false),
        allow_stale_lock: resolve_flag(overrides.allow_stale_lock, cfg.allow_stale_lock, false),
        fail_on_unknown_age: resolve_flag(
            overrides.fail_on_unknown_age,
            cfg.fail_on_unknown_age,
            false,
        ),
        strict: resolve_flag(overrides.strict, cfg.strict, false),
        build: resolve_flag(overrides.build, cfg.build, false),
        dry_run: resolve_flag(overrides.dry_run, cfg.dry_run, false),
        outdated_exit_code: g.exit_code.or(cfg.exit_code),
        show_all: resolve_flag(overrides.all, cfg.all, false),
        json,
        progress: progress_mode(json, g.log_level),
        concurrency: cfg.concurrency.unwrap_or(8),
    };

    let offline = resolve_flag(overrides.offline, cfg.offline, false);
    let fresh = resolve_flag(overrides.fresh, cfg.fresh, false);
    let adapters = adapter_set(offline, fresh)?;
    let mut projects: Vec<(EcosystemId, Project)> = Vec::new();
    for adapter in adapters.readers() {
        let id = adapter.id();
        // `--lang`/`--cargo` restrict *detection itself*: an unselected ecosystem is never walked
        // or enumerated, so a polyglot monorepo doesn't pay for (or hang on) Go/Python discovery.
        if !lang.is_empty() && !lang.contains(&id) {
            tracing::debug!(ecosystem = id.as_str(), "skipping detection (filtered out)");
            continue;
        }
        // The orchestrator owns the scan: the adapter only declares its marker, and we apply the
        // shared gitignore/exclude policy here so a leaf crate can't diverge from it.
        let marker = adapter.project_marker();
        let exclude = scan.exclude_for(command_key, id.as_str());
        let dirs = cooldown_scan::find_marker_dirs(
            &workdir,
            marker.lockfile,
            respect_gitignore,
            &exclude,
            marker.workspace_root,
        )?;
        tracing::info!(
            ecosystem = id.as_str(),
            projects = dirs.len(),
            gitignore = respect_gitignore,
            "detected projects"
        );
        for dir in dirs {
            tracing::debug!(ecosystem = id.as_str(), root = %dir, "detected project");
            projects.push((
                id,
                Project {
                    manifest: dir.join(marker.manifest),
                    root: dir,
                    kind: id,
                },
            ));
        }
    }

    let shared = build_shared_layers(g)?;
    let mut ctxs = Vec::new();
    for (ecosystem, project) in projects {
        ctxs.push(assemble_ctx(&adapters, &repo_root, ecosystem, project, &shared, g).await?);
    }

    let baseline = Baseline::load(&repo_root.join(crate::app::baseline::BASELINE_FILE))?;
    let now = jiff::Timestamp::now();
    let ws = Workspace::new(adapters, ctxs, now, baseline);
    Ok(PreparedRun {
        repo_root,
        ws,
        opts,
    })
}

/// Resolve one flag: an explicit CLI value wins, else the config value, else the built-in default.
fn resolve_flag(cli: Option<bool>, config: Option<bool>, default: bool) -> bool {
    cli.or(config).unwrap_or(default)
}

fn adapter_set(offline: bool, fresh: bool) -> Result<AdapterSet, CoreError> {
    let http = SharedHttp::new(
        discovery::cache_dir().into_std_path_buf(),
        HttpOptions {
            offline,
            fresh,
            request_timeout: Duration::from_secs(30),
            ..Default::default()
        },
    )?;

    let mut adapters = AdapterSet::new();
    adapters.register(Arc::new(GoEcosystem::from_http(http.clone())));
    adapters.register(Arc::new(CargoEcosystem::from_http(http.clone())));
    adapters.register(Arc::new(UvEcosystem::from_http(http.clone())));
    Ok(adapters)
}

fn build_shared_layers(g: &GlobalArgs) -> Result<SharedLayers, CoreError> {
    let global = if g.no_global {
        None
    } else {
        discovery::global_layer()?
    };
    let explicit = match &g.config {
        Some(path) => Some(discovery::explicit_config_layer(path)?),
        None => None,
    };
    Ok(SharedLayers {
        global,
        explicit,
        env: layer_from_fields(Origin::Env, &env_window_fields())?,
        cli: layer_from_fields(Origin::Cli, &cli_window_fields(g))?,
    })
}

async fn assemble_ctx(
    adapters: &AdapterSet,
    repo_root: &Utf8Path,
    ecosystem: EcosystemId,
    project: Project,
    shared: &SharedLayers,
    g: &GlobalArgs,
) -> Result<ProjectCtx, CoreError> {
    let native = if g.no_native {
        None
    } else {
        match adapters.reader(ecosystem) {
            Some(adapter) => adapter.native_policy(&project).await?.map(normalize_native),
            None => None,
        }
    };
    let cascade = discovery::repo_cascade_layers(repo_root, &project.root)?;

    let mut layers: Vec<PolicyLayer> = vec![builtin_default_layer()];
    if let Some(layer) = &shared.global {
        layers.push(layer.clone());
    }
    if let Some(layer) = native {
        layers.push(layer);
    }
    layers.extend(cascade);
    for layer in [&shared.explicit, &shared.env, &shared.cli]
        .into_iter()
        .flatten()
    {
        layers.push(layer.clone());
    }

    let strict_native = compute_strict_native(&layers, g);
    let rel_path = project.root.strip_prefix(repo_root).ok().map_or_else(
        || project.root.clone(),
        |path| {
            if path.as_str().is_empty() {
                Utf8PathBuf::from(".")
            } else {
                path.to_owned()
            }
        },
    );

    Ok(ProjectCtx {
        ecosystem,
        project,
        rel_path,
        policy: PolicyStack {
            layers,
            strict_native,
        },
    })
}

/// The ecosystem set this run is restricted to (empty = all detected).
///
/// `--cargo` is exact shorthand for `--lang rust` (clap rejects passing both); otherwise an explicit
/// `--lang` is used, falling back to the config `lang` list. Values accept the common tool-name
/// aliases (`cargo` → rust, `uv`/`pip` → python, `golang` → go, `npm`/`pnpm`/`yarn` → node).
fn resolve_langs(g: &GlobalArgs, cfg: &CommandConfig) -> Result<Vec<EcosystemId>, CoreError> {
    if g.cargo {
        return Ok(vec![CARGO_ID]);
    }
    let langs = if g.lang.is_empty() { &cfg.lang } else { &g.lang };
    langs.iter().map(|lang| lang_id(lang)).collect()
}

fn lang_id(lang: &str) -> Result<EcosystemId, CoreError> {
    let canonical = match lang {
        "cargo" | "crates" => "rust",
        "uv" | "pip" | "pypi" => "python",
        "golang" => "go",
        "npm" | "pnpm" | "yarn" => "node",
        other => other,
    };
    ecosystem_id(canonical).ok_or_else(|| {
        CoreError::Config(format!(
            "unknown --lang `{lang}`; recognised: go, rust, python, node"
        ))
    })
}

/// The package globs this run is scoped to: an explicit `--package` is used, else the config
/// `package` list.
fn resolve_globs(g: &GlobalArgs, cfg: &CommandConfig) -> Result<Vec<PatternGlob>, CoreError> {
    let globs = if g.package.is_empty() {
        &cfg.package
    } else {
        &g.package
    };
    globs.iter().map(|glob| PatternGlob::new(glob)).collect()
}

/// Route coarse progress notes: silent when `--log-level` already narrates the run, to stderr under
/// `--json` (keep stdout pure), to stdout otherwise (next to the pretty report).
fn progress_mode(json: bool, log_level: LogLevel) -> Progress {
    if log_level != LogLevel::Off {
        Progress::Silent
    } else if json {
        Progress::Stderr
    } else {
        Progress::Stdout
    }
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

fn compute_strict_native(layers: &[PolicyLayer], g: &GlobalArgs) -> bool {
    if g.no_fail_on_stricter_native {
        return false;
    }
    if g.fail_on_stricter_native {
        return true;
    }
    layers.iter().any(|layer| layer.strict_native == Some(true))
}

#[cfg(test)]
mod tests {
    use super::{lang_id, resolve_flag};

    #[test]
    fn resolve_flag_follows_cli_then_config_then_default() {
        // No CLI, no config → the per-command built-in default (true for outdated --major, etc.).
        assert!(resolve_flag(None, None, true));
        assert!(!resolve_flag(None, None, false));
        // Config overrides the built-in default...
        assert!(resolve_flag(None, Some(true), false));
        assert!(!resolve_flag(None, Some(false), true));
        // ...and an explicit CLI flag overrides both.
        assert!(resolve_flag(Some(true), Some(false), false));
        assert!(!resolve_flag(Some(false), Some(true), true));
    }

    #[test]
    fn lang_aliases_map_to_canonical_ecosystems() {
        for (input, canonical) in [
            ("rust", "rust"),
            ("cargo", "rust"),
            ("crates", "rust"),
            ("python", "python"),
            ("uv", "python"),
            ("pip", "python"),
            ("go", "go"),
            ("golang", "go"),
            ("node", "node"),
            ("npm", "node"),
            ("pnpm", "node"),
        ] {
            assert_eq!(
                lang_id(input).expect("known alias").as_str(),
                canonical,
                "alias `{input}`"
            );
        }
    }

    #[test]
    fn unknown_lang_is_a_config_error() {
        assert!(lang_id("ralang").is_err());
    }
}
