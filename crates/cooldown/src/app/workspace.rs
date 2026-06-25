use super::baseline::Baseline;
use super::model::Window;
use super::release_cache::{ReleaseCache, ReleaseResolver};
use camino::Utf8PathBuf;
use cooldown_core::{
    ArtifactScope, CandidateScope, DepScope, Dependency, Diagnostic, FetchContext, PatternGlob,
    PolicyLayer, PolicyStack, Project, Release, ReleaseFetcher, ResolveContext, ResolvedWindow,
    ToolId, ToolRead, ToolWrite,
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

/// How `check`/`fix`/`upgrade` handle too-fresh *transitive* dependencies (`--transitive <mode>`).
/// The full graph is in scope by default; the modes relax that consistently across the three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransitiveGate {
    /// Act on too-fresh transitive deps (the default, full graph): `check` fails on them, `fix`
    /// downgrades them, `upgrade` reconciles them to a matured version.
    #[default]
    Enforce,
    /// Evaluate transitive deps but don't act on them: `check` reports them non-fatally, `fix`/
    /// `upgrade` leave them in place while still handling direct deps.
    Allow,
    /// Don't evaluate transitive deps at all (direct-only).
    Hide,
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
    /// `exclude-folders` globs (`[global]`/`[<command>]`), `.gitignore` semantics. Beyond pruning
    /// project detection, these also drop a dependency whose declaring workspace members all sit
    /// under an excluded path — so a pnpm/cargo workspace can exclude a member's deps even though one
    /// root lock covers the whole workspace.
    pub exclude_folders: Vec<String>,
    /// Additional `[tool.<name>].exclude-folders` globs, keyed by canonical tool name. Kept separate
    /// from [`exclude_folders`](Self::exclude_folders) so one tool's excludes do not over-filter
    /// another tool in a polyglot run.
    pub exclude_folders_by_tool: BTreeMap<String, Vec<String>>,
    /// `exclude-packages` globs (`[global]`/`[<command>]`): drop a dependency whose declaring
    /// workspace members all match one of these package-name globs (`@scope/*`).
    pub exclude_packages: Vec<String>,
    /// Additional `[tool.<name>].exclude-packages` globs, keyed by canonical tool name — where the
    /// ecosystem's package-name format is known (`my-pkg` vs `@scope/my-pkg`).
    pub exclude_packages_by_tool: BTreeMap<String, Vec<String>>,
    /// `--major`: allow cross-major candidates.
    pub allow_major: bool,
    /// `--hide-pinned` (outdated): omit held rows (exact `==`/`=` pins and commit pins) from the
    /// table, leaving only deps with an actionable update. The `latest` column on a held row still
    /// shows what is available, so this is purely a display filter.
    pub hide_pinned: bool,
    /// `--rewrite` (upgrade): how to treat the manifest's version constraint. Defaults to
    /// [`RewriteMode::Auto`] (lock-only when the target is in range, rewrite only when forced);
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
    /// `--strict` (upgrade/fix): fail if the mutation could not complete cleanly.
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
    /// `--list-packages`: list every source package on its own line instead of
    /// `first (+N others)`.
    pub list_packages: bool,
    /// `--paths`: render the "Used by" column as workspace paths instead of package names.
    pub paths: bool,
    /// `--show-projects`: add the per-project "Project" column to the dependency tables. Hidden by
    /// default, since the "Used by" names usually suffice and the path is mostly noise.
    pub show_projects: bool,
    /// `--no-suggestions`: suppress actionable tips (e.g. the `--major` command after `upgrade`).
    pub no_suggestions: bool,
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
}

/// The registered tool adapters, split into read-side and mutation-side ports.
#[derive(Default)]
pub struct AdapterSet {
    readers: Vec<Arc<dyn ToolRead>>,
    fetchers: HashMap<ToolId, Arc<dyn ReleaseFetcher>>,
    writers: HashMap<ToolId, Arc<dyn ToolWrite>>,
}

impl AdapterSet {
    /// Create an empty adapter registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Register one concrete adapter as read-side and registry-fetch ports only.
    pub fn register_read<T>(&mut self, adapter: Arc<T>)
    where
        T: ToolRead + ReleaseFetcher + 'static,
    {
        let id = adapter.id();
        let reader: Arc<dyn ToolRead> = adapter.clone();
        let fetcher: Arc<dyn ReleaseFetcher> = adapter;
        self.readers.push(reader);
        self.fetchers.insert(id, fetcher);
    }

    /// Register one concrete adapter as read/fetch ports plus a mutator whose writes are verified
    /// by the application layer's post-apply graph proof before they are committed.
    pub fn register_target_verified_mutator<T>(&mut self, adapter: Arc<T>)
    where
        T: cooldown_core::Tool + 'static,
    {
        let id = adapter.id();
        self.register_read(adapter.clone());
        let writer: Arc<dyn ToolWrite> = adapter;
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

    /// The registry-fetch port for one tool. Intentionally private to this module: it is reached
    /// only by [`Workspace`]'s cache-backed fetch methods, so no caller elsewhere can fetch releases
    /// without going through the release cache — the [`ReleaseFetcher`] never leaves this module.
    fn release_fetcher(&self, id: ToolId) -> Option<&dyn ReleaseFetcher> {
        self.fetchers.get(&id).map(std::convert::AsRef::as_ref)
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
        }
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
        let mut deps: Vec<Dependency> = deps
            .into_iter()
            .filter(|dep| Self::package_in_scope(opts, &dep.package.name))
            .collect();
        // Drop excluded members from each dependency first, then drop a dependency whose *every*
        // declaring member was excluded. Pruning the members before anything reads them means a kept
        // dep is attributed only to non-excluded packages — so its "used by" representative is never
        // an excluded package. A dep with no attributable members is left untouched.
        let mut folders = opts.exclude_folders.clone();
        let mut packages = opts.exclude_packages.clone();
        if let Some(per_tool) = opts.exclude_folders_by_tool.get(pctx.tool.as_str()) {
            folders.extend(per_tool.iter().cloned());
        }
        if let Some(per_tool) = opts.exclude_packages_by_tool.get(pctx.tool.as_str()) {
            packages.extend(per_tool.iter().cloned());
        }
        if !folders.is_empty() || !packages.is_empty() {
            let folder_excludes = crate::scan::FolderExcludeSet::compile(&folders)?;
            let package_excludes = crate::scan::PackageExcludeSet::compile(&packages)?;
            deps.retain_mut(|dep| {
                if dep.members.is_empty() {
                    return true;
                }
                dep.members.retain(|member| {
                    !folder_excludes.excludes_path(camino::Utf8Path::new(&member.path))
                        && !package_excludes.excludes_name(&member.name)
                });
                !dep.members.is_empty()
            });
        }
        // Adapters yield deps in registry/HashMap order; sort so every command — most importantly
        // `upgrade`, which applies one change at a time — is deterministic when re-run back to back.
        deps.sort_by(|a, b| {
            a.package
                .name
                .cmp(&b.package.name)
                .then_with(|| a.current.to_string().cmp(&b.current.to_string()))
        });
        Ok(deps)
    }

    pub(crate) fn resolve_ctx<'a>(pctx: &'a ProjectCtx, opts: &RunOpts) -> ResolveContext<'a> {
        ResolveContext {
            tool: pctx.tool,
            project: &pctx.rel_path,
            allow_major: opts.allow_major,
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
        fanout: usize,
    ) -> Vec<(Dependency, cooldown_core::Result<Release>)> {
        let started = std::time::Instant::now();
        let Some(fetcher) = self.adapters.release_fetcher(adapter.id()) else {
            return no_fetcher_results(adapter.id(), deps);
        };
        let results = stream::iter(deps)
            .map(|dep| async move {
                let result = self
                    .release_cache
                    .locked_release(fetcher, &dep, fetch)
                    .await;
                (dep, result)
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
        fanout: usize,
    ) -> Vec<(Dependency, cooldown_core::Result<Vec<Release>>)> {
        let started = std::time::Instant::now();
        let Some(fetcher) = self.adapters.release_fetcher(adapter.id()) else {
            return no_fetcher_results(adapter.id(), deps);
        };
        let results = stream::iter(deps)
            .map(|dep| async move {
                let result = self
                    .release_cache
                    .candidate_releases(fetcher, &dep, fetch, candidate_scope)
                    .await;
                (dep, result)
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

/// The fallback result when a tool somehow has no registered [`ReleaseFetcher`] (every registered
/// adapter has one, so this is unreachable in practice) — one typed error per dep, never a panic.
fn no_fetcher_results<T>(
    tool: ToolId,
    deps: Vec<Dependency>,
) -> Vec<(Dependency, cooldown_core::Result<T>)> {
    deps.into_iter()
        .map(|dep| {
            let err = cooldown_core::CoreError::System(format!(
                "no release fetcher registered for tool {}",
                tool.as_str()
            ));
            (dep, Err(err))
        })
        .collect()
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
