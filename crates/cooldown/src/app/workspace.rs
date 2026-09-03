use super::Window;
use super::baseline::Baseline;
use super::lock::{ProjectAccessReadGuard, ProjectAccessWriteGuard};
use super::progress::Progress;
use super::release_cache::{ReleaseCache, ReleaseResolver};
use crate::scan::{FolderExcludeSet, PackageExcludeSet};
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    ArtifactScope, CandidateScope, DepScope, Dependency, Diagnostic, DiagnosticKind, FetchContext,
    LockStatus, LockVerifyReport, MutationRecovery, PatternGlob, PolicyLayer, PolicyStack, Project,
    RecoveryDisposition, Release, ReleaseFetcher, ResolveContext, ResolvedWindow, ToolId, ToolRead,
    ToolWrite,
};
use futures::stream::{self, StreamExt};
use jiff::Timestamp;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// Per-project context: which tool, the detected project, its path relative to the repo root
/// (for `project` selectors), and its fully-assembled policy stack.
pub struct ProjectCtx {
    /// The tool the project belongs to.
    pub tool: ToolId,
    /// The detected project (manifest, lock, root).
    pub project: Project,
    /// The project root relative to the scan root (the repo root in an anchored checkout): the
    /// project's label in reports, the key `project` policy selectors match, and the base of its
    /// members' locations.
    /// A staged copy of the project keeps it, so a selection scopes the copy like the original.
    pub rel_path: Utf8PathBuf,
    /// The fully-assembled, project-scoped policy layers.
    pub policy: PolicyStack,
    /// The Cargo edge policy resolved for this project's config cascade.
    pub edge_policy: cooldown_core::EdgePolicy,
}

pub(crate) struct LockRefresh {
    pub(crate) report: cooldown_core::Result<Option<LockVerifyReport>>,
    /// What the caller should surface as warnings: recovery notes, and the note that the tool
    /// could not honor `--lock`.
    pub(crate) warnings: Vec<Diagnostic>,
    pub(crate) guard: Option<ProjectAccessWriteGuard>,
}

pub(crate) fn recovery_diagnostics(
    recovery: MutationRecovery,
    tool: ToolId,
    project: &str,
) -> Vec<Diagnostic> {
    let mut diagnostics = recovery
        .warnings
        .into_iter()
        .map(|warning| warning.with_tool(tool.as_str()).with_project(project))
        .collect::<Vec<_>>();
    let message = match recovery.disposition {
        RecoveryDisposition::Unchanged => None,
        RecoveryDisposition::Accepted => {
            Some("completed an interrupted accepted publication before continuing")
        }
        RecoveryDisposition::Restored => {
            Some("restored an interrupted mutation to its preimage before continuing")
        }
        RecoveryDisposition::CleanupOnly => {
            Some("removed recovery artifacts for an already settled mutation before continuing")
        }
    };
    if let Some(message) = message {
        diagnostics.push(
            Diagnostic::new(DiagnosticKind::Recovery, message)
                .with_tool(tool.as_str())
                .with_project(project),
        );
    }
    diagnostics
}

/// The exit-code taxonomy. `check` is the CI gate, so non-zero is its contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// clean / nothing to do
    Ok,
    /// policy violation (`check`) or an incomplete mutation under `--strict`
    Policy,
    /// usage / config error
    Usage,
    /// no tool detected
    NoTool,
    /// stale/absent lock, registry unreachable, tool failed, or unknown-age under the flag
    Environment,
    /// `outdated --exit-code N` gate tripped (adoptable updates exist); the process exits with the
    /// caller-supplied code `N`. Distinct from the fixed taxonomy so CI can pick its own code.
    Gated(u8),
}

/// Whether a lock-refresh diagnostic lets the command continue or should skip the project.
pub(crate) enum LockReportAction {
    /// Continue evaluating the project.
    Continue,
    /// Skip this project after recording the diagnostic as an error.
    Skip,
}

/// The shared classification of a lock-refresh report for read commands.
pub(crate) struct LockReportOutcome {
    /// The diagnostic to surface, absent only when the lock is current.
    pub(crate) diagnostic: Option<Diagnostic>,
    /// Whether the caller can keep evaluating this project.
    pub(crate) action: LockReportAction,
}

impl Exit {
    /// The process exit code for this variant (`0`–`4`).
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown::Exit;
    ///
    /// assert_eq!(Exit::Ok.code(), 0);
    /// assert_eq!(Exit::Policy.code(), 1);
    /// ```
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            Exit::Ok => 0,
            Exit::Policy => 1,
            Exit::Usage => 2,
            Exit::NoTool => 3,
            Exit::Environment => 4,
            Exit::Gated(code) => i32::from(code),
        }
    }

    /// Whether this is the clean exit ([`Exit::Ok`]).
    #[must_use]
    pub fn is_ok(self) -> bool {
        self == Exit::Ok
    }
}

/// How `check`/`fix`/`upgrade` handle too-fresh *transitive* dependencies (`--transitive <mode>`).
/// The full graph is in scope by default; the modes relax that consistently across the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransitiveGate {
    /// Act on transitive deps (the default, full graph): `check` fails on too-fresh ones, `fix`
    /// downgrades them, `upgrade` advances matured in-range ones (where the tool's engine can pin
    /// an undeclared package) and reconciles any too-fresh one a re-lock drags in.
    #[default]
    Enforce,
    /// Relax only the too-fresh handling: `check` reports violations non-fatally, `fix` leaves
    /// too-fresh transitives in place, and `upgrade` still advances matured ones but keeps a
    /// floated-up too-fresh transitive instead of reconciling it (reported, not rolled back).
    Allow,
    /// Don't plan or evaluate transitive deps (direct-only). A direct re-resolve can still move
    /// them; such moves stay visible as collateral rows.
    Hide,
}

/// How an unusable enabled advisory feed affects a command's diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AdvisoryFailureMode {
    /// Keep the ordinary, stricter window and surface a warning.
    #[default]
    Warn,
    /// Refuse to certify the run without usable advisory evidence.
    Error,
}

impl AdvisoryFailureMode {
    pub(crate) const fn is_error(self) -> bool {
        matches!(self, Self::Error)
    }
}

/// Per-run invocation controls (the non-policy flags). Policy lives in each project's
/// [`PolicyStack`].
#[derive(Debug, Clone, Default)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent run-option flags; grouping into enums would obscure them"
)]
pub struct RunOpts {
    /// Restrict to these tools (empty = all detected).
    pub tool: Vec<ToolId>,
    /// Scope to packages matching any of these globs (empty = all).
    pub package: Vec<PatternGlob>,
    /// Where the run looks: the scan root and the directory selected below it, if any (see
    /// [`RunScope`]).
    ///
    /// The scope selects by location, which is deliberately separate from
    /// [`package`](Self::package)'s scoping by dependency name.
    pub scope: RunScope,
    /// The `exclude-folders`/`exclude-packages` globs, compiled once for filtering workspace
    /// members.
    pub excludes: MemberExcludes,
    /// `--major`: allow cross-major candidates.
    pub allow_major: bool,
    /// `--no-respect-dist-tags` (config `respect-dist-tags = false`): admit npm-family releases
    /// ordered above the registry's mutable `latest` dist-tag as candidates. Off by default: the
    /// tag is the maintainer's own "this is current" pointer (`npm install` resolves to it), so a
    /// stable release above it — a premature or abandoned major the maintainer kept releasing
    /// below — is held rather than proposed.
    pub ignore_dist_tags: bool,
    /// `--hide-pinned` (outdated): omit exact `==`/`=` and commit-pin holds from the table. Bound,
    /// graph, and `max-major` holds remain because they require a different action. This is purely a
    /// display filter; JSON and summary counts retain every dependency.
    pub hide_pinned: bool,
    /// `--rewrite` (upgrade): how to treat the manifest's version constraint. By default,
    /// [`RewriteMode::Auto`] preserves an in-range constraint where the adapter supports lock-only
    /// updates, widens implicit ceilings when required, and holds explicit upper bounds.
    /// `--rewrite` selects [`RewriteMode::Always`].
    pub rewrite: cooldown_core::RewriteMode,
    /// `outdated --transitive`: include transitive (indirect) deps in the report.
    pub transitive: bool,
    /// `--countdown <latest|soonest>` (outdated): which still-cooling upgrade the Cooldown column
    /// counts down to. [`Soonest`](cooldown_core::CooldownHorizon::Soonest) tracks the next version
    /// to mature (the default); [`Latest`](cooldown_core::CooldownHorizon::Latest) tracks the newest
    /// version. Display-only — it changes which candidate's `age/window` the report shows, never what
    /// is adoptable.
    pub cooldown_horizon: cooldown_core::CooldownHorizon,
    /// `--downgrade-pinned` (fix): downgrade and rewrite exact-pinned deps too; otherwise a pinned
    /// violation is left in place with a warning.
    pub downgrade_pinned: bool,
    /// `--transitive <mode>` (check/fix/upgrade): how the operation handles too-fresh transitive
    /// deps. Defaults to [`TransitiveGate::Enforce`] — act on the full graph.
    pub transitive_mode: TransitiveGate,
    /// `--all-artifacts` (check): gate every recorded artifact.
    pub all_artifacts: bool,
    /// `--allow-stale-lock`: downgrade a stale/absent lock from failure to a warning.
    pub allow_stale_lock: bool,
    /// `--fail-on-unknown-age`: make `check` fail on deps with no publish time.
    pub fail_on_unknown_age: bool,
    /// How an unusable enabled advisory feed is surfaced.
    ///
    /// Only `check` selects [`AdvisoryFailureMode::Error`]; other commands fail open because an
    /// outage can only fail to shorten a window, never loosen a verdict.
    pub advisory_failure: AdvisoryFailureMode,
    /// `--lock` (check/outdated): refresh the lock before reading it. No-op under `--dry-run`.
    pub lock: bool,
    /// `--strict` (upgrade/fix): fail if the mutation could not complete cleanly.
    pub strict: bool,
    /// `--build` (upgrade): compile/sync after re-locking.
    pub build: bool,
    /// `--dry-run`: resolve and print the plan; never mutate.
    pub dry_run: bool,
    /// `--offline`: cache-only mode. Registry fetches are already constrained by the HTTP client;
    /// app-layer probes that would spawn native resolvers also consult this.
    pub offline: bool,
    /// `--exit-code N` (outdated): exit with `N` when adoptable updates exist (CI gate). `None`
    /// keeps `outdated` informational (always exit 0).
    pub outdated_exit_code: Option<u8>,
    /// `--all` (outdated): also list up-to-date deps in the report.
    pub show_all: bool,
    /// `--list-packages`: list every source package on its own line instead of
    /// `first (+N others)`.
    pub list_packages: bool,
    /// `--paths`: render the "Used by" column as workspace paths instead of package names.
    pub paths: bool,
    /// `--show-projects`: add the per-project "Project" column to the dependency tables. Without
    /// the flag the renderer still adds it when identical rows need project paths to distinguish
    /// them.
    pub show_projects: bool,
    /// `--no-suggestions`: suppress actionable tips (e.g. the `--major` command after `upgrade`).
    pub no_suggestions: bool,
    /// `--json`: machine-readable output (never changes the exit code).
    pub json: bool,
    /// Human-facing progress while the command runs; defaults to silent for embedded callers.
    pub progress: Progress,
    /// Concurrency for registry fan-out.
    pub concurrency: usize,
    /// Round budget for the `fix`/reconcile fixpoint loops; `None` uses the built-in default.
    /// No CLI flag sets this — it exists so tests (and embedders) can exercise the budget-
    /// exhaustion path without driving a dozen productive rounds.
    pub fix_round_budget: Option<usize>,
}

impl RunOpts {
    pub(crate) fn fanout(&self) -> usize {
        self.concurrency.max(1)
    }

    pub(crate) fn artifact_scope(&self) -> ArtifactScope {
        if self.all_artifacts {
            ArtifactScope::All
        } else {
            ArtifactScope::Environment
        }
    }

    pub(crate) fn candidate_scope(&self) -> CandidateScope {
        if self.allow_major {
            CandidateScope::AllowCrossMajor
        } else {
            CandidateScope::CurrentMajorOnly
        }
    }
}

/// The `exclude-folders`/`exclude-packages` globs compiled for filtering workspace members: the
/// resolved `[global]`/`[<command>]` (or CLI) lists, plus each tool's `[tool.<name>]` lists kept
/// apart so one tool's excludes never over-filter another in a polyglot run.
///
/// Folder globs match a member's repository-relative location (`.gitignore` semantics, the same
/// coordinates the scan uses); package globs match its package name.
#[derive(Debug, Clone, Default)]
pub struct MemberExcludes {
    folders: FolderExcludeSet,
    packages: PackageExcludeSet,
    folders_by_tool: BTreeMap<String, FolderExcludeSet>,
    packages_by_tool: BTreeMap<String, PackageExcludeSet>,
}

impl MemberExcludes {
    /// Compiles the base lists and the per-tool lists (keyed by canonical tool name).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`](cooldown_core::CoreError::Config) if a pattern is not a
    /// valid glob.
    pub fn compile(
        folders: &[String],
        packages: &[String],
        folders_by_tool: &BTreeMap<String, Vec<String>>,
        packages_by_tool: &BTreeMap<String, Vec<String>>,
    ) -> cooldown_core::Result<Self> {
        Ok(Self {
            folders: FolderExcludeSet::compile(folders)?,
            packages: PackageExcludeSet::compile(packages)?,
            folders_by_tool: folders_by_tool
                .iter()
                .map(|(tool, patterns)| Ok((tool.clone(), FolderExcludeSet::compile(patterns)?)))
                .collect::<cooldown_core::Result<_>>()?,
            packages_by_tool: packages_by_tool
                .iter()
                .map(|(tool, patterns)| Ok((tool.clone(), PackageExcludeSet::compile(patterns)?)))
                .collect::<cooldown_core::Result<_>>()?,
        })
    }

    /// Whether a member of a `tool` project at the repository-relative `location`, named `name`,
    /// is excluded.
    /// Folder globs matching at or above `lifted` (the selection and the directories leading to
    /// it) do not count, as in the scan walk; see [`FolderExcludeSet::excludes_path`].
    fn excludes(
        &self,
        tool: &str,
        location: &Utf8Path,
        name: &str,
        lifted: Option<&Utf8Path>,
    ) -> bool {
        self.folders.excludes_path(location, lifted)
            || self
                .folders_by_tool
                .get(tool)
                .is_some_and(|set| set.excludes_path(location, lifted))
            || self.packages.excludes_name(name)
            || self
                .packages_by_tool
                .get(tool)
                .is_some_and(|set| set.excludes_name(name))
    }
}

/// Where a run looks: the scan root every location is measured from, and the directory the
/// invocation selected below it (`-C/--dir`, or its own working directory), if any.
///
/// A project at or below the selection is in scope, as is the nearest enclosing project of each
/// tool.
/// Within a project, the member owning the selection (the deepest one containing it) stays whole
/// and exempt from the exclude lists, since naming a directory outranks a glob that would drop
/// it; a member below the selection stays unless a glob matches it below the selection; any other
/// member is out of scope.
/// A row no member claims stays under any selection, since no member can vouch for it either
/// way and dropping it would let a transitive dependency slip past the gate.
/// Project detection applies the same rule to the walk (see [`WalkPolicy`]).
///
/// [`WalkPolicy`]: crate::scan::WalkPolicy
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunScope {
    scan_root: Utf8PathBuf,
    selected: Option<Utf8PathBuf>,
}

impl RunScope {
    /// The scope of a run anchored at `scan_root` and pointed at `workdir`: a selection when the
    /// workdir lies strictly below the scan root, none when it is the scan root itself.
    #[must_use]
    pub fn new(scan_root: &Utf8Path, workdir: &Utf8Path) -> Self {
        let selected = workdir
            .strip_prefix(scan_root)
            .ok()
            .filter(|rel| !rel.as_str().is_empty())
            .map(Utf8Path::to_owned);
        Self {
            scan_root: scan_root.to_owned(),
            selected,
        }
    }

    /// The selected directory relative to the scan root, if any: the coordinates member
    /// locations and the folder globs are written in.
    #[must_use]
    pub fn selected(&self) -> Option<&Utf8Path> {
        self.selected.as_deref()
    }

    /// The selected directory as an absolute path, if any.
    #[must_use]
    pub fn selected_dir(&self) -> Option<Utf8PathBuf> {
        self.selected.as_ref().map(|rel| self.scan_root.join(rel))
    }
}

/// How a directory relates to the selected one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionRelation {
    /// At or below the selection.
    Within,
    /// Strictly above it.
    Encloses,
    /// Neither.
    Unrelated,
}

impl SelectionRelation {
    fn of(dir: &Utf8Path, selected: &Utf8Path) -> Self {
        if dir.starts_with(selected) {
            SelectionRelation::Within
        } else if selected.starts_with(dir) {
            SelectionRelation::Encloses
        } else {
            SelectionRelation::Unrelated
        }
    }
}

/// How the selection scopes one project's dependency rows.
enum MemberScope {
    /// Every row not excluded stays: there is no selection.
    Whole,
    /// Rows stay by where their members sit relative to `sel`, the selection in the scan-root
    /// coordinates member locations use (see [`RunScope`]): `owner` whole, a member below the
    /// selection unless excluded below it, no other.
    Selected {
        sel: Utf8PathBuf,
        owner: Option<Utf8PathBuf>,
    },
}

impl MemberScope {
    /// Resolves the scope from every row the adapter reported, before any other filter runs, so
    /// the owner is the member that really contains the selection rather than whichever member
    /// a `--package` filter happened to leave behind.
    /// Each row list resolves its own scope; the resolved and manifest-constraint lists agree
    /// because every adapter attributes both alike.
    fn resolve(pctx: &ProjectCtx, opts: &RunOpts, deps: &[Dependency]) -> Self {
        let Some(sel) = opts.scope.selected() else {
            return MemberScope::Whole;
        };
        let owner = deps
            .iter()
            .flat_map(|dep| &dep.members)
            .map(|member| member_location(pctx, member))
            .filter(|location| sel.starts_with(location))
            .max_by_key(|location| location.components().count());
        MemberScope::Selected {
            sel: sel.to_owned(),
            owner,
        }
    }

    /// Whether the member of a `tool` project at the scan-root-relative `location`, named
    /// `name`, stays.
    fn keeps(
        &self,
        excludes: &MemberExcludes,
        tool: &str,
        location: &Utf8Path,
        name: &str,
    ) -> bool {
        match self {
            MemberScope::Whole => !excludes.excludes(tool, location, name, None),
            MemberScope::Selected { sel, owner, .. } => {
                if owner.as_deref() == Some(location) {
                    return true;
                }
                // A member containing the selection that a nearer member owns (the root package
                // above a selected member) is out of scope, like anything beside the selection.
                if sel.starts_with(location) {
                    return false;
                }
                location.starts_with(sel) && !excludes.excludes(tool, location, name, Some(sel))
            }
        }
    }
}

fn is_root_member(path: &Utf8Path) -> bool {
    path.as_str().is_empty() || path == Utf8Path::new(".")
}

/// A member's location relative to the scan root: the coordinates the folder globs were written
/// in, so an anchored or interior-slash pattern means the same thing at scan time and here.
/// The root itself is the empty path, so that it is a prefix of every other location.
/// The project's own location is [`ProjectCtx::rel_path`] rather than its root, because the
/// mutating commands scope a staged copy of the project, which keeps the former.
fn member_location(pctx: &ProjectCtx, member: &cooldown_core::MemberRef) -> Utf8PathBuf {
    let project = project_location(pctx);
    let path = Utf8Path::new(&member.path);
    if is_root_member(path) {
        project.to_owned()
    } else {
        project.join(path)
    }
}

/// A project's location relative to the scan root, the empty path for the root itself (see
/// [`ProjectCtx::rel_path`]).
fn project_location(pctx: &ProjectCtx) -> &Utf8Path {
    if is_root_member(&pctx.rel_path) {
        Utf8Path::new("")
    } else {
        pctx.rel_path.as_path()
    }
}

/// The note a report ends with when the selection left nothing to evaluate, so an empty result
/// under `-C` is never mistaken for a clean one.
/// A project skipped for a stale lock already explains its empty result with its own warning, so
/// the note is withheld then rather than contradicting it.
pub(crate) fn empty_selection_diagnostic(
    opts: &RunOpts,
    evaluated: usize,
    skipped_stale_projects: usize,
) -> Option<Diagnostic> {
    let selected = opts.scope.selected()?;
    (evaluated == 0 && skipped_stale_projects == 0).then(|| {
        Diagnostic::new(
            DiagnosticKind::Config,
            format!(
                "nothing to evaluate under {selected}: no detected project or workspace member \
                 covers it, those have no dependencies, or the package and exclude filters \
                 removed them; run from the repository root to evaluate everything"
            ),
        )
    })
}

/// The note for a project that encloses the selection yet contributed no row to it: none of its
/// members covers the selection, the members that do have no dependencies, or the filters
/// removed the rows that did.
/// Another project's rows would otherwise hide that, since the run-level count is not zero.
/// A project at or below the selection contributes nothing only when it has no dependencies,
/// which needs no note.
/// The relation is taken from the project's location rather than its root, so a staged copy of
/// the project would resolve like the original.
fn empty_project_selection_diagnostic(
    opts: &RunOpts,
    pctx: &ProjectCtx,
    evaluated: usize,
) -> Option<Diagnostic> {
    if evaluated != 0 {
        return None;
    }
    let selected = opts.scope.selected()?;
    if SelectionRelation::of(project_location(pctx), selected) != SelectionRelation::Encloses {
        return None;
    }
    Some(
        Diagnostic::new(
            DiagnosticKind::Config,
            format!(
                "nothing to evaluate under {selected} in {}: no workspace member covers it, the \
                 members that do have no dependencies, or the package and exclude filters removed \
                 their rows; run from the workspace root to evaluate every member",
                pctx.rel_path
            ),
        )
        .with_tool(pctx.tool.as_str())
        .with_project(pctx.rel_path.as_str()),
    )
}

/// The detected adapters, per-project policy, and the run's single `now`.
pub struct Workspace {
    adapters: AdapterSet,
    projects: Vec<ProjectCtx>,
    now: Timestamp,
    /// The repo root the run was anchored at, used as the write target for repo-scoped native config
    /// (a single `uv.toml`) and to label its `sync` item with the repo-relative path (".").
    repo_root: Utf8PathBuf,
    /// The repo-root policy cascade (no native layer), used to resolve a repo-wide window once for
    /// [`cooldown_core::SyncScope::Repo`] adapters without borrowing any project's layers.
    repo_layers: Vec<PolicyLayer>,
    pub(crate) baseline: Baseline,
    /// The run-scoped release resolver every fetch routes through, so a package shared across
    /// workspace members or re-resolved across `upgrade` fixpoint rounds hits the registry once.
    /// Held as the [`ReleaseResolver`] port (not the concrete cache) so it is swappable and
    /// mockable. See [`release_cache`](super::release_cache).
    release_cache: Box<dyn ReleaseResolver>,
    /// The advisory feed, when one is wired (see [`Workspace::with_advisory_source`]).
    ///
    /// `None` keeps the feature inert — tests and embedded callers need not care.
    pub(crate) advisory_source: Option<Arc<dyn cooldown_core::AdvisorySource>>,
}

struct RegisteredAdapter {
    reader: Arc<dyn ToolRead>,
    fetcher: Arc<dyn ReleaseFetcher>,
    writer: Option<Arc<dyn ToolWrite>>,
}

/// The registered tool adapters, with one coherent port family per tool identifier.
#[derive(Default)]
pub struct AdapterSet {
    order: Vec<ToolId>,
    adapters: HashMap<ToolId, RegisteredAdapter>,
}

impl AdapterSet {
    /// Create an empty adapter registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one concrete adapter as read-side and registry-fetch ports only.
    ///
    /// # Errors
    ///
    /// Returns [`cooldown_core::CoreError::System`] when that tool family is already registered.
    pub fn register_read<T>(&mut self, adapter: Arc<T>) -> cooldown_core::Result<()>
    where
        T: ToolRead + ReleaseFetcher + 'static,
    {
        let id = adapter.id();
        let reader: Arc<dyn ToolRead> = adapter.clone();
        let fetcher: Arc<dyn ReleaseFetcher> = adapter;
        self.register(
            id,
            RegisteredAdapter {
                reader,
                fetcher,
                writer: None,
            },
        )
    }

    /// Register one concrete adapter as read/fetch ports plus a mutator whose writes are verified
    /// by the application layer's post-apply graph proof before they are committed.
    ///
    /// # Errors
    ///
    /// Returns [`cooldown_core::CoreError::System`] when the adapter's read and write sides declare
    /// different tool-family identifiers or when that tool family is already registered.
    pub fn register_target_verified_mutator<T>(
        &mut self,
        adapter: Arc<T>,
    ) -> cooldown_core::Result<()>
    where
        T: cooldown_core::Tool + 'static,
    {
        let read_id = adapter.id();
        let write_id = adapter.mutation_tool();
        if read_id != write_id {
            return Err(cooldown_core::CoreError::System(format!(
                "adapter registration mismatched read tool {} and write tool {}",
                read_id.as_str(),
                write_id.as_str()
            )));
        }
        let reader: Arc<dyn ToolRead> = adapter.clone();
        let fetcher: Arc<dyn ReleaseFetcher> = adapter.clone();
        let writer: Arc<dyn ToolWrite> = adapter;
        self.register(
            read_id,
            RegisteredAdapter {
                reader,
                fetcher,
                writer: Some(writer),
            },
        )
    }

    fn register(&mut self, id: ToolId, adapter: RegisteredAdapter) -> cooldown_core::Result<()> {
        if self.adapters.contains_key(&id) {
            return Err(cooldown_core::CoreError::System(format!(
                "adapter tool family {} is already registered",
                id.as_str()
            )));
        }
        self.order.push(id);
        self.adapters.insert(id, adapter);
        Ok(())
    }

    /// Iterate the read-side adapters in registration order.
    pub fn readers(&self) -> impl Iterator<Item = &Arc<dyn ToolRead>> {
        self.order
            .iter()
            .filter_map(|id| self.adapters.get(id).map(|adapter| &adapter.reader))
    }

    /// Look up the read-side port for one tool.
    #[must_use]
    pub fn reader(&self, id: ToolId) -> Option<&dyn ToolRead> {
        self.adapters
            .get(&id)
            .map(|adapter| adapter.reader.as_ref())
    }

    /// Look up the mutation-side port for one tool.
    #[must_use]
    pub fn writer(&self, id: ToolId) -> Option<&dyn ToolWrite> {
        self.adapters
            .get(&id)
            .and_then(|adapter| adapter.writer.as_deref())
    }

    /// The registry-fetch port for one tool. Intentionally private to this module: it is reached
    /// only by [`Workspace`]'s cache-backed fetch methods, so no caller elsewhere can fetch releases
    /// without going through the release cache — the [`ReleaseFetcher`] never leaves this module.
    fn release_fetcher(&self, id: ToolId) -> Option<&dyn ReleaseFetcher> {
        self.adapters
            .get(&id)
            .map(|adapter| adapter.fetcher.as_ref())
    }
}

impl Workspace {
    /// Assemble a workspace from the detected adapters, per-project contexts, the run's single
    /// `now`, the loaded baseline, and the repo root with its native-free policy cascade.
    #[must_use]
    pub fn new(
        adapters: AdapterSet,
        projects: Vec<ProjectCtx>,
        now: Timestamp,
        baseline: Baseline,
        repo_root: Utf8PathBuf,
        repo_layers: Vec<PolicyLayer>,
    ) -> Self {
        Workspace {
            adapters,
            projects,
            now,
            repo_root,
            repo_layers,
            baseline,
            release_cache: Box::new(ReleaseCache::new()),
            advisory_source: None,
        }
    }

    /// Wires an advisory feed into the run.
    ///
    /// Without one nothing is fetched, no row is flagged, and no window is shortened — but a
    /// policy that *enabled* the feed still reports that no source implements it rather than
    /// certifying silently, so the absence is inert only when the policy asked for nothing.
    #[must_use]
    pub fn with_advisory_source(mut self, source: Arc<dyn cooldown_core::AdvisorySource>) -> Self {
        self.advisory_source = Some(source);
        self
    }

    /// The single `now` snapshotted once for the whole run.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.now
    }

    /// The repo root the run was anchored at.
    pub(crate) fn repo_root(&self) -> &camino::Utf8Path {
        &self.repo_root
    }

    /// The repo-root policy cascade (no native layer) for resolving a repo-wide window.
    pub(crate) fn repo_layers(&self) -> &[PolicyLayer] {
        &self.repo_layers
    }

    /// The per-project contexts in this workspace.
    #[must_use]
    pub fn projects(&self) -> &[ProjectCtx] {
        &self.projects
    }

    /// Whether no projects were detected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    pub(crate) fn adapter(&self, id: ToolId) -> Option<&dyn ToolRead> {
        self.adapters.reader(id)
    }

    pub(crate) fn mutator(&self, id: ToolId) -> Option<&dyn ToolWrite> {
        self.adapters.writer(id)
    }

    /// Refreshes one project's lock under the same per-project guard used by mutating commands.
    ///
    /// Returns `None` when the current run did not request `--lock`, when it is a dry-run, or when
    /// the tool has no standalone lock refresh operation.
    pub(crate) async fn refresh_project_lock(
        &self,
        pctx: &ProjectCtx,
        opts: &RunOpts,
    ) -> cooldown_core::Result<LockRefresh> {
        if !opts.lock || opts.dry_run {
            return Ok(LockRefresh {
                report: Ok(None),
                warnings: Vec::new(),
                guard: None,
            });
        }
        let Some(writer) = self.mutator(pctx.tool) else {
            return Ok(LockRefresh {
                report: Ok(None),
                warnings: Vec::new(),
                guard: None,
            });
        };
        if !writer.supports_lock_refresh() {
            // The flag was requested and cannot be honored here; say so where a script can see
            // it (the warnings reach JSON and survive `--no-progress`), as a usage note rather
            // than a new diagnostic kind, which would be a JSON schema bump.
            let note = Diagnostic::new(
                DiagnosticKind::Config,
                format!(
                    "--lock: {} has no standalone lock refresh; the existing lock was read",
                    pctx.tool.as_str()
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(pctx.rel_path.as_str())
            .with_path(pctx.project.manifest.as_str());
            return Ok(LockRefresh {
                report: Ok(None),
                warnings: vec![note],
                guard: None,
            });
        }
        opts.progress.phase("refreshing lock state");
        let guard = ProjectAccessWriteGuard::acquire(
            self.repo_root(),
            &pctx.project.root,
            pctx.tool,
            writer.sync_scope() == cooldown_core::SyncScope::Repo,
        )?;
        let recovery = writer
            .recover_pending_mutation(&pctx.project, guard.coordination())
            .await?;
        let recovery = recovery_diagnostics(recovery, pctx.tool, pctx.rel_path.as_str());
        let report = writer.refresh_lock(&pctx.project).await;
        Ok(LockRefresh {
            report,
            warnings: recovery,
            guard: Some(guard),
        })
    }

    /// Starts a read session and checks adapter-owned pending state without mutating it.
    pub(crate) async fn project_read_guard(
        &self,
        pctx: &ProjectCtx,
    ) -> cooldown_core::Result<ProjectAccessReadGuard> {
        let writer = self.mutator(pctx.tool);
        let guard = ProjectAccessReadGuard::acquire(
            self.repo_root(),
            &pctx.project.root,
            pctx.tool,
            writer.is_some_and(|writer| writer.sync_scope() == cooldown_core::SyncScope::Repo),
        )?;
        if let Some(writer) = writer {
            writer.ensure_no_pending_mutation(&pctx.project).await?;
        }
        Ok(guard)
    }

    /// The note for a project that encloses the selection yet contributed no row to it (see
    /// [`empty_project_selection_diagnostic`]), withheld when a project of another tool sits
    /// nearer to the selection: that project accounts for the directory, and the enclosing one
    /// merely has no member there, as a Cargo workspace above a Go module has none.
    pub(crate) fn empty_project_note(
        &self,
        pctx: &ProjectCtx,
        opts: &RunOpts,
        evaluated: usize,
    ) -> Option<Diagnostic> {
        let note = empty_project_selection_diagnostic(opts, pctx, evaluated)?;
        let selected = opts.scope.selected()?;
        let location = project_location(pctx);
        let nearer_other_tool = self.scoped_projects(opts).any(|other| {
            let other_location = project_location(other);
            other.tool != pctx.tool
                && other_location != location
                && other_location.starts_with(location)
                && SelectionRelation::of(other_location, selected) != SelectionRelation::Unrelated
        });
        (!nearer_other_tool).then_some(note)
    }

    /// Projects in scope for this run: filtered by `--tool`, then by the selection
    /// ([`RunOpts::scope`]).
    pub(crate) fn scoped_projects<'a>(
        &'a self,
        opts: &'a RunOpts,
    ) -> impl Iterator<Item = &'a ProjectCtx> {
        self.projects
            .iter()
            .filter(move |project| opts.tool.is_empty() || opts.tool.contains(&project.tool))
            .filter(move |project| self.project_in_selection(project, opts))
    }

    /// Whether `pctx` is in the selection: at or below the selected directory, or the nearest
    /// project of its tool containing it.
    /// A farther containing project of the same tool cannot own the selection — a nested
    /// workspace root is never a member of the one above it — so it stays out, and neither a
    /// read nor a `--lock` refresh touches it.
    /// A same-tool project below the selection evicts nothing: it cannot own the selection
    /// either, and the enclosing project's members below the selection are still due.
    fn project_in_selection(&self, pctx: &ProjectCtx, opts: &RunOpts) -> bool {
        let Some(selected) = opts.scope.selected_dir() else {
            return true;
        };
        match SelectionRelation::of(&pctx.project.root, &selected) {
            SelectionRelation::Unrelated => false,
            SelectionRelation::Within => true,
            SelectionRelation::Encloses => !self.projects.iter().any(|nearer| {
                nearer.tool == pctx.tool
                    && nearer.project.root != pctx.project.root
                    && nearer.project.root.starts_with(&pctx.project.root)
                    && selected.starts_with(&nearer.project.root)
            }),
        }
    }

    /// The tool of each project the run's scope covers, one entry per project — what the
    /// progress display needs to size its per-tool counters.
    pub(crate) fn progress_project_tools(&self, opts: &RunOpts) -> Vec<ToolId> {
        self.scoped_projects(opts)
            .map(|project| project.tool)
            .collect()
    }

    pub(crate) fn package_in_scope(opts: &RunOpts, name: &str) -> bool {
        opts.package.is_empty() || opts.package.iter().any(|glob| glob.is_match(name))
    }

    pub(crate) fn fetch_context<'a>(pctx: &'a ProjectCtx, opts: &RunOpts) -> FetchContext<'a> {
        FetchContext {
            project: &pctx.project,
            artifacts: opts.artifact_scope(),
        }
    }

    /// The scoped dependency list for reporting: the adapter's raw deps with `--package` scoping and
    /// the `exclude` policy applied (excluded members dropped, then deps with no member left removed).
    /// This is the single chokepoint every list/report command (`outdated`/`check`/`upgrade`/
    /// `baseline`) reads through, so excluded packages never reach a report. Whole-graph reads that
    /// must see everything (the upgrade graph-violation check, `explain`) call the adapter directly.
    pub(crate) async fn dependencies_in_scope(
        &self,
        adapter: &dyn ToolRead,
        pctx: &ProjectCtx,
        scope: DepScope,
        opts: &RunOpts,
    ) -> cooldown_core::Result<Vec<Dependency>> {
        let deps = adapter.dependencies(&pctx.project, scope).await?;
        Ok(Self::scope_dependencies(pctx, opts, deps))
    }

    /// The build-backend requirements (`[build-system].requires`) the lockfile never records, scoped
    /// the same way as the resolved deps. Merged into `outdated`/`upgrade` so the local flow surfaces
    /// and raises the build-backend floor exactly as Dependabot does. The lock-based `check`/`fix`
    /// gate never calls this — there is no locked version to gate.
    pub(crate) async fn manifest_constraints_in_scope(
        &self,
        adapter: &dyn ToolRead,
        pctx: &ProjectCtx,
        opts: &RunOpts,
    ) -> cooldown_core::Result<Vec<Dependency>> {
        let deps = adapter.manifest_constraints(&pctx.project).await?;
        Ok(Self::scope_dependencies(pctx, opts, deps))
    }

    /// Apply `--package` scoping and the `exclude` policy (excluded members dropped, then deps with no
    /// member left removed), then sort deterministically. Shared by the lock-driven dependency list
    /// and the manifest-constraint list so both reach reports through the same scoping chokepoint.
    fn scope_dependencies(
        pctx: &ProjectCtx,
        opts: &RunOpts,
        deps: Vec<Dependency>,
    ) -> Vec<Dependency> {
        let scope = MemberScope::resolve(pctx, opts, &deps);
        let mut deps: Vec<Dependency> = deps
            .into_iter()
            .filter(|dep| Self::package_in_scope(opts, &dep.package.name))
            .collect();
        // Drop out-of-scope and excluded members from each dependency first, then drop a
        // dependency with no member left.
        // Pruning the members before anything reads them means a kept dep is attributed only to
        // in-scope, non-excluded packages, so its "used by" representative is never one of those.
        // A row no member claims (a Go module's rows, or the transitive rows of an adapter that
        // attributes only direct dependencies) stays under any selection: dropping it would let a
        // too-fresh transitive slip past the gate, and no member can vouch for it either way.
        let tool = pctx.tool.as_str();
        deps.retain_mut(|dep| {
            if dep.members.is_empty() {
                return true;
            }
            dep.members.retain(|member| {
                scope.keeps(
                    &opts.excludes,
                    tool,
                    &member_location(pctx, member),
                    &member.name,
                )
            });
            !dep.members.is_empty()
        });
        // Adapters yield deps in registry/HashMap order; sort so every command — most importantly
        // `upgrade`, which applies one change at a time — is deterministic when re-run back to back.
        deps.sort_by(|a, b| {
            a.package
                .name
                .cmp(&b.package.name)
                .then_with(|| a.current.to_string().cmp(&b.current.to_string()))
        });
        deps
    }

    pub(crate) fn resolve_ctx<'a>(pctx: &'a ProjectCtx, opts: &RunOpts) -> ResolveContext<'a> {
        ResolveContext {
            tool: pctx.tool,
            project: &pctx.rel_path,
            allow_major: opts.allow_major,
            honor_declared_bounds: true,
            honor_latest_tag: !opts.ignore_dist_tags,
        }
    }

    /// Fetch the locked release for each dep through the run's release cache, concurrently.
    ///
    /// The cache (a [`ReleaseResolver`]) is the only thing handed the tool's [`ReleaseFetcher`], so
    /// every locked-release read is single-flight-deduplicated and rate-limited by construction —
    /// there is no API to fetch one any other way.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(tool = adapter.id().as_str(), deps = deps.len(), fanout)
    )]
    pub(crate) async fn fetch_locked_releases(
        &self,
        adapter: &dyn ToolRead,
        deps: Vec<Dependency>,
        fetch: &FetchContext<'_>,
        progress: &Progress,
        fanout: usize,
    ) -> Vec<FetchedRelease<Release>> {
        let started = std::time::Instant::now();
        let Some(fetcher) = self.adapters.release_fetcher(adapter.id()) else {
            return no_fetcher_results(adapter.id(), deps);
        };
        progress.packages(deps.len(), "fetching locked release metadata");
        let results = stream::iter(deps)
            .map(|dep| {
                let progress = progress.clone();
                async move {
                    progress.package_started(&dep.package.name);
                    let result = self
                        .release_cache
                        .locked_release(fetcher, &dep, fetch)
                        .await;
                    progress.package_finished(&dep.package.name);
                    FetchedRelease {
                        dependency: dep,
                        result,
                    }
                }
            })
            .buffer_unordered(fanout)
            .collect()
            .await;
        self.log_release_fetch(started);
        results
    }

    /// Fetch the candidate releases for each dep through the run's release cache, concurrently. See
    /// [`fetch_locked_releases`](Self::fetch_locked_releases) for why this is the only fetch path.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(tool = adapter.id().as_str(), deps = deps.len(), fanout)
    )]
    pub(crate) async fn fetch_candidate_releases(
        &self,
        adapter: &dyn ToolRead,
        deps: Vec<Dependency>,
        fetch: &FetchContext<'_>,
        candidate_scope: CandidateScope,
        progress: &Progress,
        fanout: usize,
    ) -> Vec<FetchedRelease<Vec<Release>>> {
        let started = std::time::Instant::now();
        let Some(fetcher) = self.adapters.release_fetcher(adapter.id()) else {
            return no_fetcher_results(adapter.id(), deps);
        };
        progress.packages(deps.len(), "fetching release metadata");
        let results = stream::iter(deps)
            .map(|dep| {
                let progress = progress.clone();
                async move {
                    progress.package_started(&dep.package.name);
                    let result = self
                        .release_cache
                        .candidate_releases(fetcher, &dep, fetch, candidate_scope)
                        .await;
                    progress.package_finished(&dep.package.name);
                    FetchedRelease {
                        dependency: dep,
                        result,
                    }
                }
            })
            .buffer_unordered(fanout)
            .collect()
            .await;
        self.log_release_fetch(started);
        results
    }

    /// Emit per-fetch timing plus cumulative cache effectiveness, nested under the fetch span so the
    /// tool and dep count are already in scope.
    fn log_release_fetch(&self, started: std::time::Instant) {
        let stats = self.release_cache.stats();
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis(),
            cache_lookups = stats.lookups,
            cache_resolved = stats.resolved,
            cache_saved = stats.saved(),
            "release fetch complete"
        );
    }
}

/// One dependency's release-fetch outcome, keeping the input beside its result so per-dep
/// reporting never loses attribution.
pub(crate) struct FetchedRelease<T> {
    /// The dependency the fetch was for.
    pub(crate) dependency: Dependency,
    /// The fetch outcome: a locked release, the candidate list, or the per-dep error.
    pub(crate) result: cooldown_core::Result<T>,
}

/// The fallback result when a tool somehow has no registered [`ReleaseFetcher`] (every registered
/// adapter has one, so this is unreachable in practice) — one typed error per dep, never a panic.
fn no_fetcher_results<T>(tool: ToolId, deps: Vec<Dependency>) -> Vec<FetchedRelease<T>> {
    deps.into_iter()
        .map(|dep| {
            let err = cooldown_core::CoreError::System(format!(
                "no release fetcher registered for tool {}",
                tool.as_str()
            ));
            FetchedRelease {
                dependency: dep,
                result: Err(err),
            }
        })
        .collect()
}

/// Map a resolved window to its JSON view at `now`.
pub(crate) fn render_window(window: &ResolvedWindow, now: Timestamp) -> Window {
    Window {
        min_age_days: round2(window.effective_min_age_days(now)),
        source: window.source(),
        clamped_by: window.clamped_by(now).map(cooldown_core::Origin::token),
        shortened_by: window
            .shortened_by
            .as_ref()
            .map(|advisory| advisory.as_str().to_string()),
    }
}

/// Map a core [`SecurityRelevance`](cooldown_core::SecurityRelevance) to its JSON/TTY view.
///
/// `version` is the release the relevance describes — the locked pin on a `check` row, the
/// security-relevant candidate on an `outdated` row — so the block is self-describing even when
/// it belongs to a different candidate than the row's displayed cooldown.
pub(crate) fn security_info(
    security: &cooldown_core::SecurityRelevance,
    version: &cooldown_core::Version,
) -> super::SecurityInfo {
    super::SecurityInfo {
        version: version.to_string(),
        fixes: security
            .fixes
            .iter()
            .map(|advisory| advisory.as_str().to_string())
            .collect(),
        severity: security.severity,
        source: security.source,
        applied: security.applied,
    }
}

/// Days between two instants, rounded to 2 places for display.
pub(crate) fn age_days(published: Timestamp, now: Timestamp) -> f64 {
    round2(cooldown_core::duration::duration_as_days(
        cooldown_core::duration::since(now, published),
    ))
}

pub(crate) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

/// A diagnostic built from a `CoreError`, scoped to a package where possible.
pub(crate) fn diag_from_error(
    err: &cooldown_core::CoreError,
    tool: ToolId,
    project: &str,
    package: Option<&str>,
) -> Diagnostic {
    // `CoreError::Tool` embeds the tool's verbatim stderr, which can quote credentialed registry
    // URLs (`https://user:secret@host/`). Every error that becomes a report diagnostic funnels
    // through here, so redact at this choke point — the message reaches the TTY and the JSON
    // envelope unmodified downstream.
    let message = cooldown_core::redact::url_secrets(&err.to_string());
    let mut diagnostic = Diagnostic::new(err.diagnostic_kind(), message)
        .with_tool(tool.as_str())
        .with_project(project);
    if let Some(package) = package {
        diagnostic = diagnostic.with_package(package);
    }
    diagnostic
}

pub(crate) fn lock_report_outcome(
    report: LockVerifyReport,
    tool: ToolId,
    project_label: &str,
    manifest: &camino::Utf8Path,
    allow_stale_lock: bool,
) -> LockReportOutcome {
    let kind = match report.status {
        LockStatus::Current => {
            return LockReportOutcome {
                diagnostic: None,
                action: LockReportAction::Continue,
            };
        }
        LockStatus::Stale => DiagnosticKind::StaleLock,
        LockStatus::Unknown => DiagnosticKind::LockUnknown,
    };
    // The detail is tool output (a verifier's stderr can quote registry URLs), so it gets the same
    // secret redaction as `diag_from_error` messages. Moving it out keeps `report` consumed for
    // the by-value signature.
    let detail = report.detail;
    let diagnostic = Diagnostic::new(kind, cooldown_core::redact::url_secrets(&detail))
        .with_tool(tool.as_str())
        .with_project(project_label)
        .with_path(manifest.as_str());
    let action = if allow_stale_lock && report.status == LockStatus::Stale {
        LockReportAction::Continue
    } else {
        LockReportAction::Skip
    };
    LockReportOutcome {
        diagnostic: Some(diagnostic),
        action,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use color_eyre::eyre;
    use cooldown_core::config::builtin_default_layer;
    use cooldown_core::{
        Capabilities, DepScope, LockStatus, LockVerifyReport, MemberRef, NativePolicyLayer,
        PackageId, ProjectMarker, ReleaseQuality, Version,
    };
    const CARGO: ToolId = ToolId("cargo");
    const PNPM: ToolId = ToolId("pnpm");
    const UV: ToolId = ToolId("uv");
    const GO: ToolId = ToolId("go");

    struct FakeReader {
        id: ToolId,
        deps: Vec<Dependency>,
    }

    struct TestAdapter {
        write_id: ToolId,
        refresh: bool,
    }

    #[async_trait]
    impl ToolRead for TestAdapter {
        fn id(&self) -> ToolId {
            CARGO
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn project_detection(&self) -> cooldown_core::ProjectDetection {
            cooldown_core::ProjectDetection::Primary(ProjectMarker {
                lockfile: "lock",
                manifest: "manifest",
                alternate_manifests: &[],
                workspace_root: true,
            })
        }

        async fn dependencies(
            &self,
            _project: &Project,
            _scope: DepScope,
        ) -> cooldown_core::Result<Vec<Dependency>> {
            Ok(Vec::new())
        }

        async fn native_policy(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<Option<NativePolicyLayer>> {
            Ok(None)
        }

        async fn verify_lock_current(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<LockVerifyReport> {
            Ok(LockVerifyReport {
                status: LockStatus::Current,
                detail: "current".to_string(),
            })
        }
    }

    #[async_trait]
    impl cooldown_core::ReleaseFetcher for TestAdapter {
        async fn releases(
            &self,
            _dep: &Dependency,
            _fetch: &cooldown_core::FetchContext<'_>,
            _candidates: cooldown_core::CandidateScope,
        ) -> cooldown_core::Result<Vec<cooldown_core::Release>> {
            Ok(Vec::new())
        }

        async fn locked_release(
            &self,
            dep: &Dependency,
            _fetch: &cooldown_core::FetchContext<'_>,
        ) -> cooldown_core::Result<cooldown_core::Release> {
            Err(cooldown_core::CoreError::NotFound(dep.package.name.clone()))
        }
    }

    #[async_trait]
    impl ToolWrite for TestAdapter {
        fn mutation_tool(&self) -> ToolId {
            self.write_id
        }

        async fn mutation_journal(
            &self,
            project: &Project,
            _plan: &cooldown_core::Plan,
        ) -> cooldown_core::Result<cooldown_core::ProjectMutationJournal> {
            cooldown_core::ProjectMutationJournal::capture(
                &project.root,
                std::iter::empty::<&camino::Utf8Path>(),
            )
        }

        async fn apply(
            &self,
            mutation: &cooldown_core::PreparedMutation,
        ) -> cooldown_core::Result<cooldown_core::ApplyReport> {
            mutation.parts_for(self)?;
            Ok(cooldown_core::ApplyReport::default())
        }

        async fn build(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<cooldown_core::VerifyReport> {
            Ok(cooldown_core::VerifyReport {
                ok: true,
                detail: String::new(),
            })
        }

        async fn refresh_lock(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<Option<LockVerifyReport>> {
            Ok(Some(LockVerifyReport {
                status: LockStatus::Current,
                detail: "refreshed".to_string(),
            }))
        }

        fn supports_lock_refresh(&self) -> bool {
            self.refresh
        }
    }

    #[test]
    fn adapter_registration_rejects_mismatched_read_and_write_tool_families() {
        let mut adapters = AdapterSet::new();

        let result = adapters.register_target_verified_mutator(Arc::new(TestAdapter {
            write_id: PNPM,
            refresh: false,
        }));

        std::assert_matches!(result, Err(cooldown_core::CoreError::System(_)));
        assert_eq!(adapters.readers().count(), 0);
        assert!(adapters.writer(CARGO).is_none());
        assert!(adapters.writer(PNPM).is_none());
    }

    #[test]
    fn adapter_registration_rejects_a_duplicate_tool_family_atomically() {
        let mut adapters = AdapterSet::new();
        std::assert_matches!(
            adapters.register_target_verified_mutator(Arc::new(TestAdapter {
                write_id: CARGO,
                refresh: false,
            })),
            Ok(())
        );

        let result = adapters.register_read(Arc::new(TestAdapter {
            write_id: CARGO,
            refresh: false,
        }));

        std::assert_matches!(result, Err(cooldown_core::CoreError::System(_)));
        assert_eq!(adapters.readers().count(), 1);
        assert_eq!(adapters.reader(CARGO).map(ToolRead::id), Some(CARGO));
        assert!(adapters.writer(CARGO).is_some());
    }

    #[tokio::test]
    async fn lock_refresh_retains_exclusive_access_for_the_following_read() -> eyre::Result<()> {
        let directory = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(directory.path().to_owned())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let pctx = project_ctx(CARGO, root.as_str());
        let mut adapters = AdapterSet::new();
        adapters.register_target_verified_mutator(Arc::new(TestAdapter {
            write_id: CARGO,
            refresh: true,
        }))?;
        let ws = Workspace::new(
            adapters,
            vec![pctx],
            "2026-06-17T00:00:00Z".parse()?,
            Baseline::default(),
            root.clone(),
            vec![builtin_default_layer()],
        );
        let opts = RunOpts {
            lock: true,
            ..RunOpts::default()
        };

        let refresh = ws.refresh_project_lock(&ws.projects()[0], &opts).await?;

        assert!(refresh.guard.is_some());
        std::assert_matches!(
            ProjectAccessReadGuard::acquire(&root, &root, CARGO, false),
            Err(cooldown_core::CoreError::LockConflict(_))
        );
        drop(refresh);
        ProjectAccessReadGuard::acquire(&root, &root, CARGO, false)?;
        Ok(())
    }

    #[async_trait]
    impl ToolRead for FakeReader {
        fn id(&self) -> ToolId {
            self.id
        }

        fn capabilities(&self) -> Capabilities {
            Capabilities::default()
        }

        fn project_detection(&self) -> cooldown_core::ProjectDetection {
            cooldown_core::ProjectDetection::Primary(ProjectMarker {
                lockfile: "lock",
                manifest: "manifest",
                alternate_manifests: &[],
                workspace_root: true,
            })
        }

        async fn dependencies(
            &self,
            _project: &Project,
            scope: DepScope,
        ) -> cooldown_core::Result<Vec<Dependency>> {
            Ok(self
                .deps
                .iter()
                .filter(|dep| scope == DepScope::Graph || dep.direct)
                .cloned()
                .collect())
        }

        async fn native_policy(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<Option<NativePolicyLayer>> {
            Ok(None)
        }

        async fn verify_lock_current(
            &self,
            _project: &Project,
        ) -> cooldown_core::Result<LockVerifyReport> {
            Ok(LockVerifyReport {
                status: LockStatus::Current,
                detail: "current".to_string(),
            })
        }
    }

    fn test_workspace(root: Utf8PathBuf, ctx: ProjectCtx) -> Workspace {
        Workspace::new(
            AdapterSet::new(),
            vec![ctx],
            "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            Baseline::default(),
            root,
            vec![builtin_default_layer()],
        )
    }

    /// A project at `root`, located relative to the `/repo` scan root the tests scan from.
    fn project_ctx(tool: ToolId, root: &str) -> ProjectCtx {
        let root = Utf8PathBuf::from(root);
        let rel_path = root
            .strip_prefix("/repo")
            .ok()
            .filter(|rel| !rel.as_str().is_empty())
            .map_or_else(|| Utf8PathBuf::from("."), Utf8Path::to_owned);
        ProjectCtx {
            tool,
            rel_path,
            project: Project {
                root: root.clone(),
                kind: tool,
                manifest: root.join("manifest"),
                exclude_newer: None,
            },
            policy: PolicyStack {
                layers: vec![builtin_default_layer()],
                strict_native: false,
            },
            edge_policy: cooldown_core::EdgePolicy::default(),
        }
    }

    /// Options for a run scanned from `/repo`, pointed at `selected_dir` (the root when `None`).
    fn opts_for(selected_dir: Option<&str>) -> RunOpts {
        RunOpts {
            scope: RunScope::new(
                Utf8Path::new("/repo"),
                Utf8Path::new(selected_dir.unwrap_or("/repo")),
            ),
            ..RunOpts::default()
        }
    }

    fn excludes(folders: &[&str], packages: &[&str]) -> MemberExcludes {
        let strings = |items: &[&str]| items.iter().map(ToString::to_string).collect::<Vec<_>>();
        MemberExcludes::compile(
            &strings(folders),
            &strings(packages),
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .expect("valid globs")
    }

    fn dep(name: &str, member_name: &str, member_path: &str) -> Dependency {
        Dependency {
            package: PackageId::new(PNPM, name.to_string(), Some("registry.example".to_string())),
            advisory_identity: Some(name.to_string()),
            current: Version::new("1.0.0"),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: vec![MemberRef {
                name: member_name.to_string(),
                path: member_path.to_string(),
            }],
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    async fn scoped_names(tool: ToolId, source_dir: Option<&str>) -> Vec<String> {
        let pctx = project_ctx(tool, "/repo");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let reader = FakeReader {
            id: tool,
            deps: vec![
                dep("left-dep", "left", "packages/left"),
                dep("right-dep", "right", "packages/right"),
            ],
        };
        ws.dependencies_in_scope(
            &reader,
            &ws.projects()[0],
            DepScope::Direct,
            &opts_for(source_dir),
        )
        .await
        .expect("dependencies")
        .into_iter()
        .map(|dep| dep.package.name)
        .collect()
    }

    #[tokio::test]
    async fn selected_dir_scopes_cargo_workspace_members() {
        assert_eq!(
            scoped_names(CARGO, Some("/repo/packages/left")).await,
            vec!["left-dep"]
        );
    }

    #[tokio::test]
    async fn selected_dir_scopes_pnpm_workspace_members() {
        assert_eq!(
            scoped_names(PNPM, Some("/repo/packages/right")).await,
            vec!["right-dep"]
        );
    }

    #[tokio::test]
    async fn selected_dir_scopes_uv_projects_by_project_root() {
        let left = project_ctx(UV, "/repo/packages/left");
        let right = project_ctx(UV, "/repo/packages/right");
        let ws = Workspace::new(
            AdapterSet::new(),
            vec![left, right],
            "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            Baseline::default(),
            Utf8PathBuf::from("/repo"),
            vec![builtin_default_layer()],
        );
        let opts = opts_for(Some("/repo/packages/right"));

        let projects: Vec<&Utf8PathBuf> = ws
            .scoped_projects(&opts)
            .map(|project| &project.project.root)
            .collect();

        assert_eq!(projects, vec![&Utf8PathBuf::from("/repo/packages/right")]);
    }

    #[tokio::test]
    async fn selected_dir_includes_nested_independent_projects() {
        let left = project_ctx(UV, "/repo/services/left");
        let right = project_ctx(UV, "/repo/services/right");
        let outside = project_ctx(UV, "/repo/packages/outside");
        let ws = Workspace::new(
            AdapterSet::new(),
            vec![left, right, outside],
            "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            Baseline::default(),
            Utf8PathBuf::from("/repo"),
            vec![builtin_default_layer()],
        );
        let opts = opts_for(Some("/repo/services"));

        let projects: Vec<&Utf8PathBuf> = ws
            .scoped_projects(&opts)
            .map(|project| &project.project.root)
            .collect();

        assert_eq!(
            projects,
            vec![
                &Utf8PathBuf::from("/repo/services/left"),
                &Utf8PathBuf::from("/repo/services/right")
            ]
        );
    }

    #[tokio::test]
    async fn nested_independent_project_keeps_all_dependencies() {
        let pctx = project_ctx(UV, "/repo/services/left");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let reader = FakeReader {
            id: UV,
            deps: vec![dep("left-dep", "left", "."), dep("right-dep", "right", ".")],
        };

        let deps = ws
            .dependencies_in_scope(
                &reader,
                &ws.projects()[0],
                DepScope::Direct,
                &opts_for(Some("/repo/services")),
            )
            .await
            .expect("dependencies");

        let names: Vec<String> = deps.into_iter().map(|dep| dep.package.name).collect();
        assert_eq!(names, vec!["left-dep", "right-dep"]);
    }

    #[tokio::test]
    async fn selected_dir_inside_member_matches_that_member() {
        assert_eq!(
            scoped_names(PNPM, Some("/repo/packages/left/src")).await,
            vec!["left-dep"]
        );
    }

    #[tokio::test]
    async fn repo_root_source_does_not_filter_members() {
        assert_eq!(
            scoped_names(CARGO, Some("/repo")).await,
            vec!["left-dep", "right-dep"]
        );
    }

    async fn scoped_names_with_excludes(
        source_dir: Option<&str>,
        exclude_folders: &[&str],
        exclude_packages: &[&str],
    ) -> Vec<String> {
        let pctx = project_ctx(CARGO, "/repo");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let reader = FakeReader {
            id: CARGO,
            deps: vec![
                dep("root-dep", "root", "."),
                dep("left-dep", "left", "packages/left"),
                dep("right-dep", "right", "packages/right"),
            ],
        };
        let opts = RunOpts {
            excludes: excludes(exclude_folders, exclude_packages),
            ..opts_for(source_dir)
        };
        ws.dependencies_in_scope(&reader, &ws.projects()[0], DepScope::Direct, &opts)
            .await
            .expect("dependencies")
            .into_iter()
            .map(|dep| dep.package.name)
            .collect()
    }

    /// `-C packages/left` with `left` excluded by an ancestor config: the selected member is
    /// reported anyway, by folder and by name alike — the selection outranks the exclude, so the
    /// run cannot silently scope itself to nothing.
    #[tokio::test]
    async fn selected_member_is_exempt_from_excludes() {
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/packages/left"), &["packages/left"], &[]).await,
            vec!["left-dep"]
        );
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/packages/left/src"), &["packages"], &[]).await,
            vec!["left-dep"]
        );
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/packages/left"), &[], &["left"]).await,
            vec!["left-dep"]
        );
    }

    /// Without a selection, or with one that does not land inside an excluded member, the
    /// excludes apply as usual.
    #[tokio::test]
    async fn excludes_still_apply_outside_the_selection() {
        assert_eq!(
            scoped_names_with_excludes(None, &["packages/left"], &[]).await,
            vec!["right-dep", "root-dep"]
        );
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/packages/right"), &["packages/left"], &[]).await,
            vec!["right-dep"]
        );
    }

    /// Selecting the project root itself puts the whole project in scope with its excludes
    /// applying as usual: nothing below the selection is an explicit selection.
    #[tokio::test]
    async fn selecting_the_project_root_keeps_the_excludes() {
        assert_eq!(
            scoped_names_with_excludes(Some("/repo"), &["packages/left"], &["root"]).await,
            vec!["right-dep"]
        );
    }

    /// `cd src && cooldown check` in a single-package project: the root member `.` owns every
    /// directory the project contains, so its rows are the report — not an empty pass.
    #[tokio::test]
    async fn the_root_member_owns_a_selection_no_other_member_contains() {
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/src"), &[], &[]).await,
            vec!["root-dep"]
        );
        // The root member owns the selection even when a config excludes it by name.
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/src"), &[], &["root"]).await,
            vec!["root-dep"]
        );
    }

    /// The deepest member containing the selection owns it, so the root package's own rows do
    /// not leak into a member's report.
    #[tokio::test]
    async fn the_deepest_containing_member_owns_the_selection() {
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/packages/left/src"), &[], &[]).await,
            vec!["left-dep"]
        );
    }

    /// Selecting a directory above several members scopes the run to the members below it, with
    /// the excludes applying to them as usual, plus the member that owns the selection.
    #[tokio::test]
    async fn a_container_selection_scopes_to_the_members_below_it() {
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/packages"), &[], &[]).await,
            vec!["left-dep", "right-dep", "root-dep"]
        );
        assert_eq!(
            scoped_names_with_excludes(Some("/repo/packages"), &["packages/left"], &[]).await,
            vec!["right-dep", "root-dep"]
        );
    }

    /// Rows no member claims stay under a selection inside the project, whether the adapter
    /// attributes nothing (a Go module) or only its direct dependencies (uv, the npm family):
    /// dropping the unattributed transitive rows would let a too-fresh one past the gate.
    #[tokio::test]
    async fn unattributed_rows_stay_under_a_selection() {
        let pctx = project_ctx(CARGO, "/repo");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let unattributed = |name: &str| {
            let mut dep = dep(name, "unused", "unused");
            dep.members.clear();
            dep
        };
        let names = |deps: Vec<Dependency>| async {
            let reader = FakeReader { id: CARGO, deps };
            ws.dependencies_in_scope(
                &reader,
                &ws.projects()[0],
                DepScope::Direct,
                &opts_for(Some("/repo/cmd/tool")),
            )
            .await
            .expect("dependencies")
            .into_iter()
            .map(|dep| dep.package.name)
            .collect::<Vec<_>>()
        };
        assert_eq!(
            names(vec![unattributed("module-a"), unattributed("module-b")]).await,
            vec!["module-a", "module-b"]
        );
        assert_eq!(
            names(vec![unattributed("module-a"), dep("root-dep", "root", ".")]).await,
            vec!["module-a", "root-dep"]
        );
    }

    /// Folder excludes are written in repository coordinates, so a nested project's members are
    /// matched at their repository-relative location, not at their path within the project.
    #[tokio::test]
    async fn folder_excludes_match_the_repository_relative_member_location() {
        let mut pctx = project_ctx(CARGO, "/repo/vendor/ws");
        pctx.rel_path = Utf8PathBuf::from("vendor/ws");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let ws = &ws;
        let names = |folders: &[&str]| {
            let opts = RunOpts {
                excludes: excludes(folders, &[]),
                ..opts_for(None)
            };
            async move {
                let reader = FakeReader {
                    id: CARGO,
                    deps: vec![
                        dep("lab-dep", "lab", "lab"),
                        dep("root-dep", "ws-root", "."),
                    ],
                };
                ws.dependencies_in_scope(&reader, &ws.projects()[0], DepScope::Direct, &opts)
                    .await
                    .expect("dependencies")
                    .into_iter()
                    .map(|dep| dep.package.name)
                    .collect::<Vec<_>>()
            }
        };
        // An interior-slash pattern is repository-anchored and reaches the nested member.
        assert_eq!(names(&["vendor/ws/lab"]).await, vec!["root-dep"]);
        // A bare name still matches at any depth.
        assert_eq!(names(&["lab"]).await, vec!["root-dep"]);
        // The nested project's root member sits at the project's own location.
        assert_eq!(names(&["vendor/ws"]).await, Vec::<String>::new());
        // A pattern that would only match the path within the project does not apply.
        assert_eq!(names(&["/lab"]).await, vec!["lab-dep", "root-dep"]);
    }

    /// Selecting an excluded nested workspace lifts the globs that match it or the directories
    /// above it for its members too, since the selection outranks them; a glob matching below
    /// the selection still drops the member it names.
    #[tokio::test]
    async fn a_selected_excluded_workspace_keeps_its_members_below_the_lifted_globs() {
        let mut pctx = project_ctx(CARGO, "/repo/incubator");
        pctx.rel_path = Utf8PathBuf::from("incubator");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let ws = &ws;
        let names = |folders: &[&str]| {
            let opts = RunOpts {
                excludes: excludes(folders, &[]),
                ..opts_for(Some("/repo/incubator"))
            };
            async move {
                let reader = FakeReader {
                    id: CARGO,
                    deps: vec![
                        dep("lab-dep", "lab", "lab"),
                        dep("tools-dep", "tools", "tools"),
                    ],
                };
                ws.dependencies_in_scope(&reader, &ws.projects()[0], DepScope::Direct, &opts)
                    .await
                    .expect("dependencies")
                    .into_iter()
                    .map(|dep| dep.package.name)
                    .collect::<Vec<_>>()
            }
        };
        // The glob that excludes the whole selected subtree is lifted for every member in it.
        assert_eq!(names(&["incubator"]).await, vec!["lab-dep", "tools-dep"]);
        assert_eq!(names(&["/incubator"]).await, vec!["lab-dep", "tools-dep"]);
        // A glob matching a member below the selection applies as usual.
        assert_eq!(names(&["lab"]).await, vec!["tools-dep"]);
        assert_eq!(names(&["incubator/lab"]).await, vec!["tools-dep"]);
    }

    /// An enclosing project that contributed no row to the selection is called out by name, so
    /// another project's rows cannot hide it; a project at or below the selection with nothing to
    /// evaluate is merely dependency-free.
    #[test]
    fn an_enclosing_project_with_no_rows_under_the_selection_is_noted() {
        let opts = opts_for(Some("/repo/docs"));
        let enclosing = project_ctx(CARGO, "/repo");
        let note = empty_project_selection_diagnostic(&opts, &enclosing, 0)
            .expect("an enclosing project with no rows is noted");
        assert_eq!(note.kind, DiagnosticKind::Config);
        assert!(note.message.contains("docs"), "{}", note.message);
        // A dependency-free member covering the selection is one of the causes the note names,
        // since the rows alone cannot tell it apart from an uncovered selection.
        assert!(note.message.contains("no dependencies"), "{}", note.message);
        assert!(empty_project_selection_diagnostic(&opts, &enclosing, 1).is_none());
        let below = project_ctx(CARGO, "/repo/docs/tool");
        assert!(empty_project_selection_diagnostic(&opts, &below, 0).is_none());
        assert!(empty_project_selection_diagnostic(&opts_for(None), &enclosing, 0).is_none());
    }

    /// Inside another tool's project, the enclosing workspace of a different tool contributing
    /// nothing is the expected shape, not a silent empty run, so it earns no note; beside such a
    /// project, or above one, it still does.
    #[test]
    fn an_enclosing_project_is_not_noted_when_another_tool_owns_the_selection() {
        let ws = Workspace::new(
            AdapterSet::new(),
            vec![
                project_ctx(CARGO, "/repo"),
                project_ctx(GO, "/repo/go-svc"),
                project_ctx(PNPM, "/repo"),
            ],
            "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            Baseline::default(),
            Utf8PathBuf::from("/repo"),
            vec![builtin_default_layer()],
        );
        let cargo = &ws.projects()[0];
        // Selecting the Go module, or a directory inside it, is covered by the Go module.
        assert!(
            ws.empty_project_note(cargo, &opts_for(Some("/repo/go-svc")), 0)
                .is_none()
        );
        assert!(
            ws.empty_project_note(cargo, &opts_for(Some("/repo/go-svc/internal")), 0)
                .is_none()
        );
        // A project of another tool at the same root is no nearer, so the note stands.
        assert!(
            ws.empty_project_note(cargo, &opts_for(Some("/repo/crates/lab")), 0)
                .is_some()
        );
        // A same-tool project below the selection never covers the enclosing one's members.
        let nested = Workspace::new(
            AdapterSet::new(),
            vec![
                project_ctx(CARGO, "/repo"),
                project_ctx(CARGO, "/repo/apps/x"),
            ],
            "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            Baseline::default(),
            Utf8PathBuf::from("/repo"),
            vec![builtin_default_layer()],
        );
        assert!(
            nested
                .empty_project_note(&nested.projects()[0], &opts_for(Some("/repo/apps")), 0)
                .is_some()
        );
    }

    /// With a nested workspace of the same tool at the selection, the enclosing workspace is
    /// out of scope — it cannot own the selection, and `--lock` must not touch its lock — while an
    /// enclosing project of another tool stays in, since it may well own the directory.
    /// A same-tool project below the selection evicts nothing: the enclosing workspace still owns
    /// the selection and its members below it.
    #[test]
    fn only_the_nearest_enclosing_project_of_a_tool_is_in_scope() {
        let ws = Workspace::new(
            AdapterSet::new(),
            vec![
                project_ctx(CARGO, "/repo"),
                project_ctx(CARGO, "/repo/incubator"),
                project_ctx(PNPM, "/repo"),
                project_ctx(CARGO, "/repo/other"),
                project_ctx(CARGO, "/repo/apps/x"),
            ],
            "2026-06-17T00:00:00Z".parse().expect("timestamp"),
            Baseline::default(),
            Utf8PathBuf::from("/repo"),
            vec![builtin_default_layer()],
        );
        let roots = |selected: &str| {
            ws.scoped_projects(&opts_for(Some(selected)))
                .map(|pctx| (pctx.tool, pctx.project.root.as_str().to_owned()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            roots("/repo/incubator/lab"),
            vec![
                (CARGO, "/repo/incubator".to_string()),
                (PNPM, "/repo".to_string())
            ]
        );
        assert_eq!(
            roots("/repo/crates/app"),
            vec![(CARGO, "/repo".to_string()), (PNPM, "/repo".to_string())]
        );
        // Selecting above a nested workspace keeps the enclosing one and the nested one alike.
        assert_eq!(
            roots("/repo/apps"),
            vec![
                (CARGO, "/repo".to_string()),
                (PNPM, "/repo".to_string()),
                (CARGO, "/repo/apps/x".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn selected_dir_retains_only_matching_members() {
        let pctx = project_ctx(CARGO, "/repo");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let reader = FakeReader {
            id: CARGO,
            deps: vec![Dependency {
                package: PackageId::new(CARGO, "shared-dep".to_string(), None),
                advisory_identity: Some("shared-dep".to_string()),
                current: Version::new("1.0.0"),
                current_quality: ReleaseQuality::Stable,
                direct: true,
                artifacts: Vec::new(),
                graph_floor: None,
                graph_ceiling: None,
                declared_bound: None,
                members: vec![
                    MemberRef {
                        name: "left".to_string(),
                        path: "packages/left".to_string(),
                    },
                    MemberRef {
                        name: "right".to_string(),
                        path: "packages/right".to_string(),
                    },
                ],
                pinned: false,
                hold_edges: Vec::new(),
            }],
        };

        let deps = ws
            .dependencies_in_scope(
                &reader,
                &ws.projects()[0],
                DepScope::Direct,
                &opts_for(Some("/repo/packages/right")),
            )
            .await
            .expect("dependencies");

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].members.len(), 1);
        assert_eq!(deps[0].members[0].name, "right");
    }

    #[tokio::test]
    async fn package_filter_still_filters_dependency_names() {
        let pctx = project_ctx(PNPM, "/repo");
        let ws = test_workspace(Utf8PathBuf::from("/repo"), pctx);
        let reader = FakeReader {
            id: PNPM,
            deps: vec![
                dep("left-dep", "left", "packages/left"),
                dep("right-dep", "right", "packages/right"),
            ],
        };
        let mut opts = opts_for(Some("/repo/packages/left"));
        opts.package = vec![PatternGlob::new("right-*").expect("glob")];

        let deps = ws
            .dependencies_in_scope(&reader, &ws.projects()[0], DepScope::Direct, &opts)
            .await
            .expect("dependencies");

        assert!(deps.is_empty());

        // Ownership is resolved before the filter: with the selected member's own rows filtered
        // away, the root member does not become the owner and leak its rows into the report.
        let reader = FakeReader {
            id: PNPM,
            deps: vec![
                dep("left-dep", "left", "packages/left"),
                dep("shared", "root", "."),
            ],
        };
        opts.package = vec![PatternGlob::new("shared").expect("glob")];
        let deps = ws
            .dependencies_in_scope(&reader, &ws.projects()[0], DepScope::Direct, &opts)
            .await
            .expect("dependencies");
        assert!(deps.is_empty());
    }

    #[test]
    fn diag_from_error_redacts_url_secrets_in_tool_stderr() {
        // A registry configured with inline credentials leaks them through the tool's stderr; the
        // diagnostic message reaches the TTY and JSON envelope verbatim, so the choke point must
        // mask the secret while keeping the host for debuggability.
        let error = cooldown_core::CoreError::Tool {
            tool: "cargo".to_string(),
            termination: cooldown_core::ToolTermination::ExitCode(101),
            stderr: "failed to fetch https://user:hunter2@registry.example/index/config.json"
                .to_string(),
        };

        let diag = diag_from_error(&error, CARGO, ".", Some("serde"));

        assert_eq!(diag.kind, cooldown_core::DiagnosticKind::ToolFailed);
        assert!(
            diag.message.contains("registry.example"),
            "the host survives redaction, got: {}",
            diag.message
        );
        assert!(
            !diag.message.contains("hunter2"),
            "the credential must be redacted, got: {}",
            diag.message
        );
    }
}
