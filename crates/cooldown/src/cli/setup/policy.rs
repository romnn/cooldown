use super::options::{ResolvedInvocation, StrictNativeMode};
use crate::app::{AdapterSet, ProjectCtx};
use crate::discovery::ConfigSources;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{builtin_default_layer, layer_from_fields};
use cooldown_core::{
    CoreError, Origin, PolicyLayer, PolicyStack, Project, ToolId, normalize_native,
};

pub(super) struct SharedLayers {
    global: Option<PolicyLayer>,
    explicit: Option<PolicyLayer>,
    env: Option<PolicyLayer>,
    cli: Option<PolicyLayer>,
}

struct ProjectAssembly<'a> {
    adapters: &'a AdapterSet,
    repo_root: &'a Utf8Path,
    configs: &'a ConfigSources,
    shared: &'a SharedLayers,
    invocation: &'a ResolvedInvocation,
    no_native: bool,
}

pub(super) fn build_shared_layers(
    configs: &ConfigSources,
    invocation: &ResolvedInvocation,
) -> Result<SharedLayers, CoreError> {
    Ok(SharedLayers {
        global: configs.global_policy_layer()?,
        explicit: configs.explicit_policy_layer()?,
        env: layer_from_fields(Origin::Env, invocation.env_policy())?,
        cli: layer_from_fields(Origin::Cli, invocation.cli_policy())?,
    })
}

pub(super) async fn assemble_projects(
    adapters: &AdapterSet,
    repo_root: &Utf8Path,
    projects: Vec<(ToolId, Project)>,
    configs: &ConfigSources,
    shared: &SharedLayers,
    invocation: &ResolvedInvocation,
    no_native: bool,
) -> Result<Vec<ProjectCtx>, CoreError> {
    let assembly = ProjectAssembly {
        adapters,
        repo_root,
        configs,
        shared,
        invocation,
        no_native,
    };
    let mut contexts = Vec::new();
    for (tool, project) in projects {
        contexts.push(assemble_ctx(&assembly, tool, project).await?);
    }
    Ok(contexts)
}

async fn assemble_ctx(
    assembly: &ProjectAssembly<'_>,
    tool: ToolId,
    project: Project,
) -> Result<ProjectCtx, CoreError> {
    let native = if assembly.no_native {
        None
    } else {
        match assembly.adapters.reader(tool) {
            Some(adapter) => adapter.native_policy(&project).await?.map(normalize_native),
            None => None,
        }
    };
    let cascade = assembly
        .configs
        .repo_cascade_layers(assembly.repo_root, &project.root)?;

    let mut layers: Vec<PolicyLayer> = vec![builtin_default_layer()];
    if let Some(layer) = &assembly.shared.global {
        layers.push(layer.clone());
    }
    if let Some(layer) = native {
        layers.push(layer);
    }
    layers.extend(cascade);
    for layer in [
        &assembly.shared.explicit,
        &assembly.shared.env,
        &assembly.shared.cli,
    ]
    .into_iter()
    .flatten()
    {
        layers.push(layer.clone());
    }

    let strict_native = compute_strict_native(&layers, assembly.invocation);
    let rel_path = project
        .root
        .strip_prefix(assembly.repo_root)
        .ok()
        .map_or_else(
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

fn compute_strict_native(layers: &[PolicyLayer], invocation: &ResolvedInvocation) -> bool {
    match invocation.strict_native() {
        StrictNativeMode::ForceOff => false,
        StrictNativeMode::ForceOn => true,
        StrictNativeMode::Inherit => layers.iter().any(|layer| layer.strict_native == Some(true)),
    }
}
