//! The application use cases. A [`Workspace`] bundles the detected adapters, per-project layered
//! policy, and a single `now` snapshotted once for the whole run (consistency over freshness — two
//! deps evaluated 30s apart must use the same boundary).
//!
//! Policy is **per project**: the shared layers (default, global, explicit `--config`, env, CLI)
//! are common, but the native layer and the repo cascade (root → this project's dir) are scoped to
//! each project, so sibling projects never leak policy into one another.

pub mod baseline;
mod check;
mod explain;
mod lock;
mod outdated;
mod upgrade;

pub use baseline::Baseline;

use camino::Utf8PathBuf;
use cooldown_core::{
    Diagnostic, Ecosystem, EcosystemId, PatternGlob, PolicyStack, Project, ResolveContext,
    ResolvedWindow,
};
use cooldown_render as render;
use jiff::Timestamp;

/// Per-project context: which ecosystem, the detected project, its path relative to the repo root
/// (for `project` selectors), and its fully-assembled policy stack.
pub struct ProjectCtx {
    /// The ecosystem the project belongs to.
    pub ecosystem: EcosystemId,
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
    /// no ecosystem detected
    NoEcosystem,
    /// stale/absent lock, registry unreachable, tool failed, or unknown-age under the flag
    Environment,
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
            Exit::NoEcosystem => 3,
            Exit::Environment => 4,
        }
    }

    /// Whether this is the clean exit ([`Exit::Ok`]).
    #[must_use]
    pub fn is_ok(self) -> bool {
        self == Exit::Ok
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
    /// Restrict to these ecosystems (empty = all detected).
    pub lang: Vec<EcosystemId>,
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
    /// Concurrency for registry fan-out.
    pub concurrency: usize,
}

impl RunOpts {
    fn fanout(&self) -> usize {
        self.concurrency.max(1)
    }
}

/// The detected adapters, per-project policy, and the run's single `now`.
pub struct Workspace {
    ecosystems: Vec<Box<dyn Ecosystem>>,
    projects: Vec<ProjectCtx>,
    now: Timestamp,
    baseline: Baseline,
}

impl Workspace {
    /// Assemble a workspace from the detected adapters, per-project contexts, the run's single
    /// `now`, and the loaded baseline.
    #[must_use]
    pub fn new(
        ecosystems: Vec<Box<dyn Ecosystem>>,
        projects: Vec<ProjectCtx>,
        now: Timestamp,
        baseline: Baseline,
    ) -> Self {
        Workspace {
            ecosystems,
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

    fn adapter(&self, id: EcosystemId) -> Option<&dyn Ecosystem> {
        self.ecosystems
            .iter()
            .find(|e| e.id() == id)
            .map(std::convert::AsRef::as_ref)
    }

    /// Projects in scope for this run (filtered by `--lang`).
    fn scoped_projects<'a>(&'a self, opts: &'a RunOpts) -> impl Iterator<Item = &'a ProjectCtx> {
        self.projects
            .iter()
            .filter(move |p| opts.lang.is_empty() || opts.lang.contains(&p.ecosystem))
    }

    fn package_in_scope(opts: &RunOpts, name: &str) -> bool {
        opts.package.is_empty() || opts.package.iter().any(|g| g.is_match(name))
    }

    fn resolve_ctx<'a>(pctx: &'a ProjectCtx, opts: &RunOpts) -> ResolveContext<'a> {
        ResolveContext {
            ecosystem: pctx.ecosystem,
            project: &pctx.rel_path,
            allow_major: opts.allow_major,
        }
    }
}

/// Map a resolved window to its JSON view at `now`.
pub(crate) fn render_window(w: &ResolvedWindow, now: Timestamp) -> render::Window {
    render::Window {
        min_age_days: round2(w.effective_min_age_days(now)),
        source: w.source(),
        clamped_by: w.clamped_by(now).map(cooldown_core::Origin::token),
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
    ecosystem: EcosystemId,
    project: &str,
    package: Option<&str>,
) -> Diagnostic {
    let mut d = Diagnostic::new(err.diagnostic_kind(), err.to_string())
        .with_ecosystem(ecosystem.as_str())
        .with_project(project);
    if let Some(p) = package {
        d = d.with_package(p);
    }
    d
}
