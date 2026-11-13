use super::options::{ResolvedInvocation, StrictNativeMode};
use crate::app::lock::{ProjectReadGuard, RepoToolReadGuard};
use crate::app::{AdapterSet, ProjectCtx};
use crate::discovery::ConfigSources;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{builtin_default_layer, layer_from_fields};
use cooldown_core::{
    CoreError, GeneratedMembers, Origin, PolicyLayer, PolicyStack, Project, ResolveKind,
    ResolveQuery, SyncScope, ToolId, normalize_native, package_windows, resolve,
    window_exclude_newer,
};
use jiff::Timestamp;
use std::collections::{HashMap, HashSet};

pub(super) struct SharedLayers {
    global: Option<PolicyLayer>,
    explicit: Option<PolicyLayer>,
    env: Option<PolicyLayer>,
    cli: Option<PolicyLayer>,
}

pub(super) struct ProjectAssembly<'a> {
    pub(super) adapters: &'a AdapterSet,
    pub(super) repo_root: &'a Utf8Path,
    /// The root every project location is measured from: the repo root in an anchored checkout,
    /// the working directory otherwise.
    pub(super) scan_root: &'a Utf8Path,
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
/// Mirrors [`assemble_ctx`]'s layer order — builtin default, global, native, repo cascade,
/// explicit, env, CLI — but is anchored at `repo_root`.
/// `native` is the repo root's own [`Origin::Native`] layer for one tool (see
/// [`RepoCascades::per_tool`]); without one, the cascade serves every repo-scoped tool alike.
/// `sync` resolves a repo-scoped adapter's single repo-level window against this once, regardless
/// of project count.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`]/[`CoreError::Config`] if a discovered `cooldown.toml` cannot be
/// read or parsed.
fn repo_layers(
    configs: &ConfigSources,
    shared: &SharedLayers,
    repo_root: &Utf8Path,
    native: Option<PolicyLayer>,
) -> Result<Vec<PolicyLayer>, CoreError> {
    let project_config = configs.project_config(repo_root)?;
    let mut layers: Vec<PolicyLayer> = vec![builtin_default_layer()];
    if let Some(layer) = &shared.global {
        layers.push(layer.clone());
    }
    layers.extend(native);
    layers.extend(project_config.policy_layers);
    for layer in [&shared.explicit, &shared.env, &shared.cli]
        .into_iter()
        .flatten()
    {
        layers.push(layer.clone());
    }
    Ok(layers)
}

/// The repo-root policy cascades a run resolves repo-wide windows against.
pub(super) struct RepoCascades {
    /// The native-free cascade every repo-scoped tool without its own entry uses.
    pub(super) shared: Vec<PolicyLayer>,
    /// The cascade, repo-root native layer included, of each repo-scoped tool whose repo-level
    /// native file is also native policy.
    ///
    /// Without it, `sync` would overwrite a stricter native value with a weaker default resolved
    /// over a cascade that never saw it.
    pub(super) per_tool: HashMap<ToolId, Vec<PolicyLayer>>,
}

/// Builds the [`RepoCascades`] for the tools of the detected `projects`.
///
/// Only tools that took part in detection are asked for repo-root native policy, so an unselected
/// tool's native config can neither fail the run nor be read at all.
///
/// # Errors
///
/// Returns [`CoreError`] if the repo lease cannot be taken, a native config is invalid, or a
/// `cooldown.toml` cannot be read.
pub(super) async fn repo_cascades(
    assembly: &ProjectAssembly<'_>,
    projects: &[ProjectCtx],
) -> Result<RepoCascades, CoreError> {
    let shared = repo_layers(assembly.configs, assembly.shared, assembly.repo_root, None)?;
    let per_tool = repo_native_layers(assembly, projects.iter().map(|ctx| ctx.tool)).await?;
    Ok(RepoCascades { shared, per_tool })
}

/// The repo-root cascade of every given repo-scoped tool whose repo root carries native policy.
async fn repo_native_layers(
    assembly: &ProjectAssembly<'_>,
    tools: impl IntoIterator<Item = ToolId>,
) -> Result<HashMap<ToolId, Vec<PolicyLayer>>, CoreError> {
    let mut cascades = HashMap::new();
    if assembly.no_native {
        return Ok(cascades);
    }
    for tool in tools.into_iter().collect::<HashSet<_>>() {
        let Some(writer) = assembly
            .adapters
            .writer(tool)
            .filter(|writer| writer.sync_scope() == SyncScope::Repo)
        else {
            continue;
        };
        let native = {
            let _repo_guard = RepoToolReadGuard::acquire(assembly.repo_root, tool)?;
            writer.repo_native_policy(assembly.repo_root).await?
        };
        if let Some(native) = native {
            cascades.insert(
                tool,
                repo_layers(
                    assembly.configs,
                    assembly.shared,
                    assembly.repo_root,
                    Some(normalize_native(native)),
                )?,
            );
        }
    }
    Ok(cascades)
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
    let native = {
        let _repo_guard = assembly
            .adapters
            .writer(tool)
            .filter(|writer| writer.sync_scope() == SyncScope::Repo)
            .map(|_| RepoToolReadGuard::acquire(assembly.repo_root, tool))
            .transpose()?;
        let _project_guard = ProjectReadGuard::acquire(
            &project.root,
            &assembly.adapters.lease_family(tool, &project),
        )?;
        if let Some(writer) = assembly.adapters.writer(tool) {
            writer.ensure_no_pending_mutation(&project).await?;
        }
        if assembly.no_native {
            None
        } else {
            match assembly.adapters.reader(tool) {
                Some(adapter) => adapter.native_policy(&project).await?.map(normalize_native),
                None => None,
            }
        }
    };
    let project_config = assembly.configs.project_config(&project.root)?;
    let edge_policy = assembly
        .invocation
        .edge_policy_override()
        .or(project_config.cargo_edge_policy)
        .unwrap_or_default();
    let single_copy = project_config.pnpm_single_copy;
    let generated_members = project_config.cargo_generated_members;
    project.generated_members = generated_members
        .as_ref()
        .map_or_else(GeneratedMembers::undeclared, |declared| {
            declared.generated_members()
        });

    let mut layers: Vec<PolicyLayer> = vec![builtin_default_layer()];
    if let Some(layer) = &assembly.shared.global {
        layers.push(layer.clone());
    }
    if let Some(layer) = native {
        layers.push(layer);
    }
    layers.extend(project_config.policy_layers);
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
    let rel_path = scan_relative_project(&project.root, assembly.scan_root);

    // The resolution window the project's resolver should honor, as a uv `exclude-newer` value. Tools
    // that accept a publish-time cutoff (uv) pass this so the lock resolves against cooldown's own
    // window rather than the tool's or environment's default — which may be shorter, silently
    // weakening the supply-chain delay. An age window becomes a *relative* span ("14 days") so the
    // persisted value stays stable across runs (no per-run churn in the file); a freeze becomes its
    // absolute instant. The default-window query resolves a locked pin no package selector names,
    // except a glob matching the empty name such as `*` (per-kind windows are not
    // expressible as one resolver cutoff; per-package windows travel separately, below), and the
    // effective duration (`now - cutoff`) folds in any floor.
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
    // The per-package windows travel beside the default, so a package-level exemption or stricter
    // window reaches a per-package resolver cutoff (uv's `--exclude-newer-package`) too.
    project.exclude_newer_package = package_windows(&layers, tool, &rel_path, assembly.now);

    Ok(ProjectCtx {
        tool,
        project,
        rel_path,
        policy: PolicyStack {
            layers,
            strict_native,
        },
        edge_policy,
        single_copy,
        generated_members,
    })
}

/// A project's identity within the run: its root relative to the scan root, `.` for the scan root
/// itself, and the absolute root when it lies outside the scan (a selection above it).
///
/// The segments are joined with `/` rather than `Utf8Path`'s native separator, because this value
/// leaves the process as data — the `project` field of every diagnostic and JSON report row — and
/// is matched against `/`-separated folder globs. A Windows run must spell it the same way.
fn scan_relative_project(root: &Utf8Path, scan_root: &Utf8Path) -> Utf8PathBuf {
    crate::report_path::relative_report_path(root, scan_root).unwrap_or_else(|| root.to_owned())
}

fn compute_strict_native(layers: &[PolicyLayer], invocation: &ResolvedInvocation) -> bool {
    match invocation.strict_native() {
        StrictNativeMode::ForceOff => false,
        StrictNativeMode::ForceOn => true,
        StrictNativeMode::Inherit => layers.iter().any(|layer| layer.strict_native == Some(true)),
    }
}

#[cfg(test)]
mod tests {
    use super::scan_relative_project;
    use camino::{Utf8Path, Utf8PathBuf};

    #[test]
    fn project_identity_is_relative_to_the_scan_root() {
        let scan_root = Utf8Path::new("/repo");
        assert_eq!(
            scan_relative_project(Utf8Path::new("/repo"), scan_root),
            Utf8PathBuf::from(".")
        );
        assert_eq!(
            scan_relative_project(Utf8Path::new("/repo/tools/x"), scan_root),
            Utf8PathBuf::from("tools/x")
        );
        // A project outside the scan has no relative identity; its own root is the honest answer.
        assert_eq!(
            scan_relative_project(Utf8Path::new("/elsewhere/app"), scan_root),
            Utf8PathBuf::from("/elsewhere/app")
        );
    }

    /// The identity is reported and glob-matched, so it spells `/` even where the platform's own
    /// separator is `\`.
    #[cfg(windows)]
    #[test]
    fn project_identity_is_slash_separated_on_windows() {
        assert_eq!(
            scan_relative_project(
                Utf8Path::new("C:\\repo\\tools\\x"),
                Utf8Path::new("C:\\repo")
            ),
            Utf8PathBuf::from("tools/x")
        );
    }
}
