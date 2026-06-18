use crate::app::{AdapterSet, ProjectCtx};
use crate::cli::GlobalArgs;
use crate::discovery;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{WindowFields, builtin_default_layer, layer_from_fields};
use cooldown_core::{
    CoreError, Origin, PolicyLayer, PolicyStack, Project, ToolId, normalize_native,
};

pub(super) struct SharedLayers {
    global: Option<PolicyLayer>,
    explicit: Option<PolicyLayer>,
    env: Option<PolicyLayer>,
    cli: Option<PolicyLayer>,
}

pub(super) fn build_shared_layers(global: &GlobalArgs) -> Result<SharedLayers, CoreError> {
    let global_layer = if global.no_global {
        None
    } else {
        discovery::global_layer()?
    };
    let explicit = match &global.config {
        Some(path) => Some(discovery::explicit_config_layer(path)?),
        None => None,
    };
    Ok(SharedLayers {
        global: global_layer,
        explicit,
        env: layer_from_fields(Origin::Env, &env_window_fields())?,
        cli: layer_from_fields(Origin::Cli, &cli_window_fields(global))?,
    })
}

pub(super) async fn assemble_projects(
    adapters: &AdapterSet,
    repo_root: &Utf8Path,
    projects: Vec<(ToolId, Project)>,
    shared: &SharedLayers,
    global: &GlobalArgs,
) -> Result<Vec<ProjectCtx>, CoreError> {
    let mut contexts = Vec::new();
    for (tool, project) in projects {
        contexts.push(assemble_ctx(adapters, repo_root, tool, project, shared, global).await?);
    }
    Ok(contexts)
}

async fn assemble_ctx(
    adapters: &AdapterSet,
    repo_root: &Utf8Path,
    tool: ToolId,
    project: Project,
    shared: &SharedLayers,
    global: &GlobalArgs,
) -> Result<ProjectCtx, CoreError> {
    let native = if global.no_native {
        None
    } else {
        match adapters.reader(tool) {
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

    let strict_native = compute_strict_native(&layers, global);
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
        tool,
        project,
        rel_path,
        policy: PolicyStack {
            layers,
            strict_native,
        },
    })
}

fn cli_window_fields(global: &GlobalArgs) -> WindowFields {
    WindowFields {
        min_age: global.min_age.clone(),
        min_age_major: global.min_age_major.clone(),
        min_age_minor: global.min_age_minor.clone(),
        min_age_patch: global.min_age_patch.clone(),
        latest: global.latest,
        freeze: global.freeze.clone(),
        allow: global.allow.clone(),
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

fn compute_strict_native(layers: &[PolicyLayer], global: &GlobalArgs) -> bool {
    if global.no_fail_on_stricter_native {
        return false;
    }
    if global.fail_on_stricter_native {
        return true;
    }
    layers.iter().any(|layer| layer.strict_native == Some(true))
}
