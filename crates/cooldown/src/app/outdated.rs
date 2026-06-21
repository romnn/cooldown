//! `outdated` — what could update, split into "adoptable now" vs "in cooldown". Reasons over the
//! candidate set; informational, so per-dep failures never change the exit code.

use super::{Exit, RunOpts, Workspace, age_days, diag_from_error, render_window};
use super::{LatestInfo, OutdatedItem, OutdatedStatus, OutdatedSummary, Window};
use cooldown_core::{
    DepScope, Dependency, Diagnostic, Release, ResolveKind, ResolveQuery, evaluate, resolve,
};

/// The result of `outdated`: every reported dependency split by status, plus diagnostics.
///
/// `outdated` is informational, so per-dependency failures become `Error` items rather than
/// changing the exit code; [`exit`](Self::exit) is therefore always [`Exit::Ok`].
pub struct OutdatedOutcome {
    /// Per-status counts across all reported items.
    pub summary: OutdatedSummary,
    /// One entry per in-scope dependency (including any that failed to evaluate).
    pub items: Vec<OutdatedItem>,
    /// Non-fatal diagnostics not attributable to a single dependency.
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors (e.g. a dependency graph that could not be enumerated).
    pub errors: Vec<Diagnostic>,
    /// Always [`Exit::Ok`]: this command never gates.
    pub exit: Exit,
}

struct OutdatedRunner<'a> {
    ws: &'a Workspace,
    opts: &'a RunOpts,
    scope: DepScope,
    items: Vec<OutdatedItem>,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
}

impl Workspace {
    /// Report what could update, split into "adoptable now" vs "in cooldown".
    ///
    /// Scopes to direct deps unless `--transitive` is set, and to packages matching `--package`.
    /// Surfaces a yanked locked version as a warning.
    pub async fn outdated(&self, opts: &RunOpts) -> OutdatedOutcome {
        OutdatedRunner::new(self, opts).run().await
    }
}

impl<'a> OutdatedRunner<'a> {
    fn new(ws: &'a Workspace, opts: &'a RunOpts) -> Self {
        let scope = if opts.transitive {
            DepScope::Graph
        } else {
            DepScope::Direct
        };
        OutdatedRunner {
            ws,
            opts,
            scope,
            items: Vec::new(),
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    async fn run(mut self) -> OutdatedOutcome {
        for pctx in self.ws.scoped_projects(self.opts) {
            self.run_project(pctx).await;
        }

        // Releases are fetched concurrently (`buffer_unordered`), so the items arrive in a
        // non-deterministic order. Sort by (project, status, name, version) for a stable report
        // (and stable `--json`); the status rank puts ready-to-adopt updates last.
        self.items.sort_by(|a, b| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.status.sort_rank().cmp(&b.status.sort_rank()))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.current.cmp(&b.current))
        });
        let summary = summarize(&self.items);
        OutdatedOutcome {
            summary,
            items: self.items,
            warnings: self.warnings,
            errors: self.errors,
            exit: Exit::Ok,
        }
    }

    async fn run_project(&mut self, pctx: &'a super::ProjectCtx) {
        let Some(read) = self.ws.read_project_ctx(pctx, self.opts) else {
            return;
        };

        self.opts.progress.say(&format!(
            "Resolving {} dependencies ({})…",
            read.project_label, pctx.tool
        ));
        let deps = match self
            .ws
            .dependencies_in_scope(read.adapter, pctx, self.scope, self.opts)
            .await
        {
            Ok(deps) => deps,
            Err(error) => {
                tracing::warn!(
                    project = read.project_label,
                    tool = pctx.tool.as_str(),
                    error = %error,
                    "could not enumerate dependencies"
                );
                self.errors.push(diag_from_error(
                    &error,
                    pctx.tool,
                    &read.project_label,
                    None,
                ));
                return;
            }
        };
        let fetched = self
            .fetch_releases(read.adapter, pctx, &read.project_label, deps, &read.fetch)
            .await;

        for (dep, result) in fetched {
            match result {
                Ok(releases) => {
                    if is_yanked_locked(&releases, &dep) {
                        self.warnings
                            .push(yanked_warning(pctx, &read.project_label, &dep));
                    }
                    self.items.push(self.outdated_item(
                        pctx,
                        &read.project_label,
                        &dep,
                        &releases,
                        &read.resolve,
                    ));
                }
                Err(error) => {
                    let diag = diag_from_error(
                        &error,
                        pctx.tool,
                        &read.project_label,
                        Some(&dep.package.name),
                    );
                    self.items
                        .push(error_item(pctx, &read.project_label, &dep, diag));
                }
            }
        }
    }

    /// Fetch release metadata for every in-scope dependency of one project, bounded by the
    /// registry fan-out. Each release fetch is independent, so a single slow or failing dependency
    /// never blocks the others. The surrounding `info!`/per-dependency `debug!`/`trace!` spans make
    /// a stalled network call visible under `--log-level debug`.
    async fn fetch_releases(
        &self,
        adapter: &dyn cooldown_core::ToolRead,
        pctx: &super::ProjectCtx,
        project_label: &str,
        deps: Vec<Dependency>,
        fctx: &cooldown_core::FetchContext<'_>,
    ) -> Vec<(Dependency, cooldown_core::Result<Vec<Release>>)> {
        tracing::info!(
            project = project_label,
            tool = pctx.tool.as_str(),
            deps = deps.len(),
            fanout = self.opts.fanout(),
            "fetching release metadata"
        );
        self.opts.progress.say(&format!(
            "Fetching release metadata for {} dependencies…",
            deps.len()
        ));
        let started = std::time::Instant::now();
        let fetched = self
            .ws
            .fetch_candidate_releases(
                adapter,
                deps,
                fctx,
                self.opts.candidate_scope(),
                self.opts.fanout(),
            )
            .await;
        for (dep, result) in &fetched {
            match result {
                Ok(releases) => tracing::trace!(
                    package = dep.package.name.as_str(),
                    releases = releases.len(),
                    "fetched releases"
                ),
                Err(error) => tracing::debug!(
                    package = dep.package.name.as_str(),
                    error = %error,
                    "release fetch failed"
                ),
            }
        }
        tracing::info!(
            project = project_label,
            tool = pctx.tool.as_str(),
            fetched = fetched.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "fetched release metadata"
        );
        fetched
    }

    /// Build the report item for a dependency whose releases were fetched successfully.
    fn outdated_item(
        &self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        dep: &Dependency,
        releases: &[Release],
        rctx: &cooldown_core::ResolveContext<'_>,
    ) -> OutdatedItem {
        let verdict = evaluate(dep, releases, &pctx.policy.layers, rctx, self.ws.now());

        // The candidate whose cooldown the report displays: the newest by default, or the soonest to
        // mature under `--countdown soonest`. Both the `window` and `candidate_age_days` below read
        // from this one candidate so the cooldown cell stays internally consistent (`age/window`).
        let shown = verdict.cooldown_candidate(self.opts.cooldown_horizon, self.ws.now());

        // Prefer the shown candidate's window; fall back to a current-pin resolution when there is
        // no candidate at all (up to date / commit pin).
        let window = if let Some(candidate) = shown {
            render_window(&candidate.window, self.ws.now())
        } else {
            let q = ResolveQuery {
                tool: pctx.tool,
                package: &dep.package.name,
                registry: dep.package.registry.as_deref(),
                project: &pctx.rel_path,
                kind: ResolveKind::CurrentPin,
            };
            render_window(
                &resolve(&pctx.policy.layers, &q, self.ws.now()).window,
                self.ws.now(),
            )
        };

        // The age of the shown candidate — the version whose `window` is shown above — so the
        // report can read its cooldown position as `age/window` (e.g. `13d/14d`, almost adoptable).
        let candidate_age_days = shown
            .and_then(|candidate| candidate.published_at)
            .map(|published| age_days(published, self.ws.now()));

        // Under `--countdown soonest` the cooldown can count down to a version no other column names
        // (an intermediate that matures before the newest candidate). Label that version so the cell
        // is unambiguous; in the default `latest` view the shown candidate *is* the newest one, so
        // there is nothing to add. Compare against the newest candidate (not `verdict.latest`, which
        // also counts an unclassifiable newest release that never became a candidate) so the default
        // view stays byte-identical: under `Latest`, `shown` is exactly `candidates.last()`.
        let newest = verdict
            .candidates
            .last()
            .map(|candidate| &candidate.version);
        let cooldown_version = shown
            .map(|candidate| &candidate.version)
            .filter(|&version| Some(version) != newest)
            .map(ToString::to_string);

        let latest = verdict.latest.as_ref().map(|lv| {
            let published = releases
                .iter()
                .find(|r| &r.version == lv)
                .and_then(|r| r.published_at);
            LatestInfo {
                version: lv.to_string(),
                published_at: published.map(|p| p.to_string()),
                age_days: published.map(|p| age_days(p, self.ws.now())),
            }
        });

        OutdatedItem {
            name: dep.package.name.clone(),
            tool: pctx.tool.as_str().to_string(),
            project: project_label.to_string(),
            registry: dep.package.registry.clone(),
            direct: dep.direct,
            current: dep.current.to_string(),
            members: dep.members.clone(),
            window,
            candidate_age_days,
            cooldown_version,
            status: verdict.status.into(),
            adoptable_target: verdict.adoptable_target.map(|v| v.to_string()),
            latest,
            error: None,
        }
    }
}

/// Whether the dependency's currently-locked version is marked yanked among `releases`.
fn is_yanked_locked(releases: &[Release], dep: &Dependency) -> bool {
    releases
        .iter()
        .any(|r| r.version == dep.current && r.yanked)
}

fn yanked_warning(pctx: &super::ProjectCtx, project_label: &str, dep: &Dependency) -> Diagnostic {
    Diagnostic::new(
        cooldown_core::DiagnosticKind::Yanked,
        "locked version is yanked",
    )
    .with_tool(pctx.tool.as_str())
    .with_project(project_label)
    .with_package(&dep.package.name)
    .with_version(dep.current.as_str())
}

fn error_item(
    pctx: &super::ProjectCtx,
    project_label: &str,
    dep: &Dependency,
    diag: Diagnostic,
) -> OutdatedItem {
    OutdatedItem {
        name: dep.package.name.clone(),
        tool: pctx.tool.as_str().to_string(),
        project: project_label.to_string(),
        registry: dep.package.registry.clone(),
        direct: dep.direct,
        current: dep.current.to_string(),
        members: dep.members.clone(),
        window: Window {
            min_age_days: 0.0,
            source: "n/a".into(),
            clamped_by: None,
        },
        candidate_age_days: None,
        cooldown_version: None,
        status: OutdatedStatus::Error,
        adoptable_target: None,
        latest: None,
        error: Some(diag),
    }
}

fn summarize(items: &[OutdatedItem]) -> OutdatedSummary {
    let mut s = OutdatedSummary {
        total: items.len(),
        adoptable: 0,
        in_cooldown: 0,
        up_to_date: 0,
        exempt: 0,
        held: 0,
        unknown_age: 0,
        errors: 0,
    };
    for it in items {
        match it.status {
            OutdatedStatus::Adoptable => s.adoptable += 1,
            OutdatedStatus::InCooldown | OutdatedStatus::CurrentInCooldown => {
                s.in_cooldown += 1;
            }
            OutdatedStatus::UpToDate => s.up_to_date += 1,
            OutdatedStatus::Exempt => s.exempt += 1,
            OutdatedStatus::Held => s.held += 1,
            OutdatedStatus::UnknownAge => s.unknown_age += 1,
            OutdatedStatus::Error => s.errors += 1,
        }
    }
    s
}
