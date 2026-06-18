use super::GlobalArgs;
use crate::app::{AdapterSet, Baseline, ProjectCtx, RunOpts, Workspace};
use crate::discovery;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_cargo::CargoEcosystem;
use cooldown_core::config::{WindowFields, builtin_default_layer, layer_from_fields};
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

pub(crate) async fn prepare_run(g: &GlobalArgs) -> Result<PreparedRun, CoreError> {
    let workdir = match &g.dir {
        Some(dir) => dir.clone(),
        None => Utf8PathBuf::from_path_buf(std::env::current_dir().map_err(CoreError::from)?)
            .map_err(|_| CoreError::PathEncoding("current dir is not valid UTF-8".into()))?,
    };
    let repo_root = discovery::find_repo_root(&workdir);
    let opts = RunOpts {
        lang: parse_langs(&g.lang)?,
        package: parse_globs(&g.package)?,
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

    let adapters = adapter_set(g)?;
    let mut projects: Vec<(EcosystemId, Project)> = Vec::new();
    for adapter in adapters.readers() {
        for project in adapter.detect(&workdir).await? {
            projects.push((adapter.id(), project));
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

fn adapter_set(g: &GlobalArgs) -> Result<AdapterSet, CoreError> {
    let http = SharedHttp::new(
        discovery::cache_dir().into_std_path_buf(),
        HttpOptions {
            offline: g.offline,
            fresh: g.fresh,
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

fn parse_langs(langs: &[String]) -> Result<Vec<EcosystemId>, CoreError> {
    langs
        .iter()
        .map(|lang| {
            ecosystem_id(lang).ok_or_else(|| {
                CoreError::Config(format!(
                    "unknown --lang `{lang}`; recognised: go, rust, python, node"
                ))
            })
        })
        .collect()
}

fn parse_globs(globs: &[String]) -> Result<Vec<PatternGlob>, CoreError> {
    globs.iter().map(|glob| PatternGlob::new(glob)).collect()
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
