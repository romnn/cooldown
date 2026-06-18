use super::baseline::Baseline;
use super::model::Window;
use camino::Utf8PathBuf;
use cooldown_core::{
    ArtifactScope, CandidateScope, DepScope, Dependency, Diagnostic, FetchContext, PatternGlob,
    PolicyStack, Project, ResolveContext, ResolvedWindow, ToolId, ToolRead, ToolWrite,
};
use jiff::Timestamp;
use std::collections::HashMap;
use std::sync::Arc;

/// Per-project context: which tool, the detected project, its path relative to the repo root
/// (for `project` selectors), and its fully-assembled policy stack.
pub struct ProjectCtx {
    /// The tool the project belongs to.
    pub tool: ToolId,
    /// The detected project (manifest, lock, root).
    pub project: Project,
    /// The project root relative to the repo root, used by `project` policy selectors.
    pub rel_path: Utf8PathBuf,
    /// The fully-assembled, project-scoped policy layers.
    pub policy: PolicyStack,
}

/// The exit-code taxonomy. `check` is the CI gate, so non-zero is its contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// clean / nothing to do
    Ok,
    /// policy violation (check) or an unmovable planned change (upgrade --strict)
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

/// Where human-facing progress notes go while a slow command runs (detection, registry fan-out).
///
/// These are coarse "still working" lines, not the structured `tracing` log: they exist so a plain
/// `cooldown outdated` isn't silent for ten seconds. They are suppressed entirely when `--log-level`
/// is on (the log already narrates progress), routed to stderr under `--json` (so stdout stays pure
/// JSON), and to stdout otherwise (alongside the pretty report).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Progress {
    /// Emit to stdout (default human/pretty mode).
    Stdout,
    /// Emit to stderr (`--json` mode, to keep stdout machine-readable).
    Stderr,
    /// Emit nothing (`--log-level` is on, so `tracing` covers it).
    #[default]
    Silent,
}

impl Progress {
    /// Print one progress note to the configured stream (a no-op when [`Progress::Silent`]).
    pub fn say(self, message: &str) {
        match self {
            Progress::Stdout => println!("{message}"),
            Progress::Stderr => eprintln!("{message}"),
            Progress::Silent => {}
        }
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
    /// `--major`: allow cross-major candidates.
    pub allow_major: bool,
    /// `--major-all`: apply cross-major to all eligible deps (else `--package` is required).
    pub major_all: bool,
    /// `--direct-only`: evaluate only direct deps.
    pub direct_only: bool,
    /// `--include-indirect` (outdated): include transitive deps in the report.
    pub include_indirect: bool,
    /// `--all-artifacts` (check): gate every recorded artifact.
    pub all_artifacts: bool,
    /// `--allow-stale-lock`: downgrade a stale/absent lock from failure to a warning.
    pub allow_stale_lock: bool,
    /// `--fail-on-unknown-age`: make `check` fail on deps with no publish time.
    pub fail_on_unknown_age: bool,
    /// `--strict` (upgrade): fail if any planned change was skipped.
    pub strict: bool,
    /// `--build` (upgrade): compile/sync after re-locking.
    pub build: bool,
    /// `--dry-run`: resolve and print the plan; never mutate.
    pub dry_run: bool,
    /// `--exit-code N` (outdated): exit with `N` when adoptable updates exist (CI gate). `None`
    /// keeps `outdated` informational (always exit 0).
    pub outdated_exit_code: Option<u8>,
    /// `--all` (outdated): also list up-to-date deps in the report.
    pub show_all: bool,
    /// `--json`: machine-readable output (never changes the exit code).
    pub json: bool,
    /// Where coarse progress notes go while the command runs.
    pub progress: Progress,
    /// Concurrency for registry fan-out.
    pub concurrency: usize,
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

/// The detected adapters, per-project policy, and the run's single `now`.
pub struct Workspace {
    adapters: AdapterSet,
    projects: Vec<ProjectCtx>,
    now: Timestamp,
    pub(crate) baseline: Baseline,
}

/// The registered tool adapters, split into read-side and mutation-side ports.
#[derive(Default)]
pub struct AdapterSet {
    readers: Vec<Arc<dyn ToolRead>>,
    writers: HashMap<ToolId, Arc<dyn ToolWrite>>,
}

impl AdapterSet {
    /// Create an empty adapter registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one concrete adapter as both a read-side and a mutation-side port.
    pub fn register<T>(&mut self, adapter: Arc<T>)
    where
        T: cooldown_core::Tool + 'static,
    {
        let id = adapter.id();
        let reader: Arc<dyn ToolRead> = adapter.clone();
        let writer: Arc<dyn ToolWrite> = adapter;
        self.readers.push(reader);
        self.writers.insert(id, writer);
    }

    /// Iterate the read-side adapters in registration order.
    pub fn readers(&self) -> impl Iterator<Item = &Arc<dyn ToolRead>> {
        self.readers.iter()
    }

    /// Look up the read-side port for one tool.
    pub fn reader(&self, id: ToolId) -> Option<&dyn ToolRead> {
        self.readers
            .iter()
            .find(|adapter| adapter.id() == id)
            .map(std::convert::AsRef::as_ref)
    }

    /// Look up the mutation-side port for one tool.
    pub fn writer(&self, id: ToolId) -> Option<&dyn ToolWrite> {
        self.writers.get(&id).map(std::convert::AsRef::as_ref)
    }
}

impl Workspace {
    /// Assemble a workspace from the detected adapters, per-project contexts, the run's single
    /// `now`, and the loaded baseline.
    #[must_use]
    pub fn new(
        adapters: AdapterSet,
        projects: Vec<ProjectCtx>,
        now: Timestamp,
        baseline: Baseline,
    ) -> Self {
        Workspace {
            adapters,
            projects,
            now,
            baseline,
        }
    }

    /// The single `now` snapshotted once for the whole run.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.now
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

    /// Projects in scope for this run (filtered by `--tool`).
    pub(crate) fn scoped_projects<'a>(
        &'a self,
        opts: &'a RunOpts,
    ) -> impl Iterator<Item = &'a ProjectCtx> {
        self.projects
            .iter()
            .filter(move |project| opts.tool.is_empty() || opts.tool.contains(&project.tool))
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

    pub(crate) async fn dependencies_in_scope(
        &self,
        adapter: &dyn ToolRead,
        pctx: &ProjectCtx,
        scope: DepScope,
        opts: &RunOpts,
    ) -> cooldown_core::Result<Vec<Dependency>> {
        let deps = adapter.dependencies(&pctx.project, scope).await?;
        Ok(deps
            .into_iter()
            .filter(|dep| Self::package_in_scope(opts, &dep.package.name))
            .collect())
    }

    pub(crate) fn resolve_ctx<'a>(pctx: &'a ProjectCtx, opts: &RunOpts) -> ResolveContext<'a> {
        ResolveContext {
            tool: pctx.tool,
            project: &pctx.rel_path,
            allow_major: opts.allow_major,
        }
    }
}

/// Map a resolved window to its JSON view at `now`.
pub(crate) fn render_window(window: &ResolvedWindow, now: Timestamp) -> Window {
    Window {
        min_age_days: round2(window.effective_min_age_days(now)),
        source: window.source(),
        clamped_by: window.clamped_by(now).map(cooldown_core::Origin::token),
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
    let mut diagnostic = Diagnostic::new(err.diagnostic_kind(), err.to_string())
        .with_tool(tool.as_str())
        .with_project(project);
    if let Some(package) = package {
        diagnostic = diagnostic.with_package(package);
    }
    diagnostic
}
