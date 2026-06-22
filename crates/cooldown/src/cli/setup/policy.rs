use super::options::{ResolvedInvocation, StrictNativeMode};
use crate::app::{AdapterSet, ProjectCtx};
use crate::discovery::ConfigSources;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{builtin_default_layer, layer_from_fields};
use cooldown_core::{
    CoreError, Origin, PolicyLayer, PolicyStack, Project, ResolveKind, ResolveQuery, ToolId,
    normalize_native, resolve, window_exclude_newer,
};
use jiff::Timestamp;

pub(super) struct SharedLayers {
    global: Option<PolicyLayer>,
    explicit: Option<PolicyLayer>,
    env: Option<PolicyLayer>,
    cli: Option<PolicyLayer>,
}

pub(super) struct ProjectAssembly<'a> {
    pub(super) adapters: &'a AdapterSet,
    pub(super) repo_root: &'a Utf8Path,
    pub(super) configs: &'a ConfigSources,
    pub(super) shared: &'a SharedLayers,
    pub(super) invocation: &'a ResolvedInvocation,
    pub(super) no_native: bool,
    /// The run's single `now`, used to derive each project's resolution cutoff (`now - window`).
    pub(super) now: Timestamp,
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

/// The repo-root policy cascade, used to resolve a repo-wide window without borrowing any one
/// project's layers.
///
/// Mirrors [`assemble_ctx`]'s layer order — builtin default, global, repo cascade, explicit, env,
/// CLI — but is anchored at `repo_root` and omits the [`Origin::Native`] layer: there is no project
/// manifest at the repo root to read native config from. `sync` resolves a repo-scoped adapter's
/// single repo-level window against this once, regardless of project count.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`]/[`CoreError::Config`] if a discovered `cooldown.toml` cannot be
/// read or parsed.
pub(super) fn repo_layers(
    configs: &ConfigSources,
    shared: &SharedLayers,
    repo_root: &Utf8Path,
) -> Result<Vec<PolicyLayer>, CoreError> {
    let cascade = configs.repo_cascade_layers(repo_root, repo_root)?;
    let mut layers: Vec<PolicyLayer> = vec![builtin_default_layer()];
    if let Some(layer) = &shared.global {
        layers.push(layer.clone());
    }
    layers.extend(cascade);
    for layer in [&shared.explicit, &shared.env, &shared.cli]
        .into_iter()
        .flatten()
    {
        layers.push(layer.clone());
    }
    Ok(layers)
}

pub(super) async fn assemble_projects(
    assembly: &ProjectAssembly<'_>,
    projects: Vec<(ToolId, Project)>,
) -> Result<Vec<ProjectCtx>, CoreError> {
    let mut contexts = Vec::new();
    for (tool, project) in projects {
        contexts.push(assemble_ctx(assembly, tool, project).await?);
    }
    Ok(contexts)
}

async fn assemble_ctx(
    assembly: &ProjectAssembly<'_>,
    tool: ToolId,
    mut project: Project,
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

    // The resolution window the project's resolver should honor, as a uv `exclude-newer` value. Tools
    // that accept a publish-time cutoff (uv) pass this so the lock resolves against cooldown's own
    // window rather than the tool's or environment's default — which may be shorter, silently
    // weakening the supply-chain delay. An age window becomes a *relative* span ("14 days") so the
    // persisted value stays stable across runs (no per-run churn in the file); a freeze becomes its
    // absolute instant. The default-window query mirrors `sync` (per-package/per-kind windows are not
    // expressible as one resolver cutoff), and the effective duration (`now - cutoff`) folds in any floor.
    let query = ResolveQuery {
        tool,
        package: "",
        registry: None,
        project: &rel_path,
        kind: ResolveKind::CurrentPin,
    };
    let window = resolve(&layers, &query, assembly.now).window;
    // Fold any binding floor into a single effective [`WindowSpec`], then render it once via the
    // shared formatter — an age becomes a *relative* span (the *persisted* value stays stable across
    // runs, with no per-run churn in the file); a freeze is its absolute instant. The relative span
    // still resolves to `now - window` each run, so once a dependency matures past the window
    // `uv lock --check` correctly reports the lock needs updating — the intended signal to re-lock,
    // not a guarantee the verdict never changes. `effective_spec` folds the floor into *every* arm —
    // exactly as `ResolvedWindow::cutoff` clamps — so a floor-protected window is never handed to uv
    // weaker than cooldown's own gate enforces, even when an `allow` rule yields `Latest`/`Freeze`
    // with a residual floor still binding.
    let effective = window.effective_spec(assembly.now);
    // A real window renders to its span/instant. A true opt-out (`Latest`/zero age, no binding floor)
    // yields `None`; pass `now` so the uv command's `--exclude-newer` overrides any repo `uv.toml` or
    // ambient config (which would otherwise re-impose the repo default cutoff on a project whose policy
    // explicitly disclaimed one) and resolves to the latest releases.
    project.exclude_newer =
        window_exclude_newer(&effective).or_else(|| Some(assembly.now.to_string()));

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
