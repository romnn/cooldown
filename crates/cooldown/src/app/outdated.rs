//! `outdated` — what could update, split into "adoptable now" vs "in cooldown". Reasons over the
//! candidate set; informational, so per-dep failures never change the exit code.

use super::advisories::AdvisoryPackages;
use super::change_key::ChangeKey;
use super::lanes::LaneAccess;
use super::lock::ProjectAccessWriteGuard;
use super::read::ReadProjectCtx;
use super::upgrade::{Judged, UpgradeAccum, target_package_for, upgrade_verdict};
use super::{
    Exit, FetchedRelease, LockReportAction, ProjectProgress, RunOpts, Workspace, age_days,
    diag_from_error, lock_report_outcome, render_window, stale_evaluation_skipped,
};
use super::{LatestInfo, OutdatedItem, OutdatedStatus, OutdatedSummary, UpgradeItem, Window};
use cooldown_core::{
    Change, DepScope, Dependency, Diagnostic, DiagnosticKind, LockVerifyReport, PackageId, Release,
    ResolveKind, ResolveQuery, Status, UpdateKind, Version, resolve,
};

/// Why a row with a matured target is blocked when `upgrade` neither moved nor held its copy: it
/// plans no move for it, as for an indirect dependency its tool cannot advance on its own or one
/// the packages requiring it drop.
const FOLLOWS_REQUIRERS: &str =
    "upgrade does not move it on its own: it follows the packages that require it";

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

/// Evaluates one project, accumulating into its own [`OutdatedAccum`].
struct OutdatedRunner<'a> {
    ws: &'a Workspace,
    opts: &'a RunOpts,
    scope: DepScope,
    /// The one package `explain` reports, whose rows are the only ones built; `None` reports
    /// every dependency in scope.
    ///
    /// A project that contributes no row under a `-C` selection is noted only without it: the
    /// note's advice ("run from the workspace root") would be wrong for `explain`'s verdict.
    only: Option<&'a str>,
    acc: OutdatedAccum,
}

/// The rows and diagnostics of one project's evaluation, folded into the run's accumulator in project
/// order.
#[derive(Default)]
struct OutdatedAccum {
    items: Vec<OutdatedItem>,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
    skipped_stale_projects: usize,
    /// Whether a project already explained an empty selection, so the run-level note is redundant.
    empty_selection_noted: bool,
    /// Dependencies evaluated, whether or not they became an item.
    evaluated: usize,
}

impl OutdatedAccum {
    /// Folds one project's rows and diagnostics into the run's.
    fn merge(&mut self, mut project: OutdatedAccum) {
        self.items.append(&mut project.items);
        self.warnings.append(&mut project.warnings);
        self.errors.append(&mut project.errors);
        self.skipped_stale_projects += project.skipped_stale_projects;
        self.empty_selection_noted |= project.empty_selection_noted;
        self.evaluated += project.evaluated;
    }
}

/// The rows `outdated` would show for one package in one project, with the diagnostics the
/// evaluation raised — `explain`'s verdict.
pub(crate) struct PackageVerdict {
    /// One row per resolved version of the package, ascending by current version.
    pub(crate) items: Vec<OutdatedItem>,
    pub(crate) warnings: Vec<Diagnostic>,
    pub(crate) errors: Vec<Diagnostic>,
}

impl Workspace {
    /// Report what could update, split into "adoptable now" vs "in cooldown".
    ///
    /// Scopes to direct deps unless `--transitive` is set, and to packages matching `--package`.
    /// Surfaces a yanked locked version as a warning.
    pub async fn outdated(&self, opts: &RunOpts) -> OutdatedOutcome {
        // `--lock` refreshes each project under an exclusive lease, which decides how far the
        // tools' lanes may overlap (see `LaneAccess`).
        let lanes = self.lanes(opts, LaneAccess::for_lock_refresh(opts));
        let projects = lanes
            .run(|pctx| async move {
                let progress = opts.progress.project(pctx.tool, pctx.rel_path.as_str());
                let mut runner = OutdatedRunner::new(self, opts);
                runner.run_project(pctx, progress).await;
                runner.acc
            })
            .await;
        let mut acc = OutdatedAccum::default();
        for project in projects {
            acc.merge(project);
        }
        finalize_outdated(opts, acc)
    }

    /// The `outdated` verdict for `package` alone in `pctx`: the rows `outdated --transitive`
    /// reports for it, from the same evaluation and the same `upgrade` dry run over the caller's
    /// whole package scope, since what lands for one package can depend on every other planned
    /// move.
    /// Transitive rows are in scope, so a package no member declares directly is still judged.
    /// `progress` is the caller's open block for the project.
    pub(crate) async fn package_verdict(
        &self,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
        package: &str,
        progress: &ProjectProgress,
    ) -> PackageVerdict {
        let opts = RunOpts {
            transitive: true,
            ..opts.clone()
        };
        let mut runner = OutdatedRunner::new(self, &opts);
        runner.only = Some(package);
        runner.run_project(pctx, progress.clone()).await;
        let OutdatedAccum {
            mut items,
            warnings,
            errors,
            ..
        } = runner.acc;
        items.sort_by(|a, b| a.current.cmp(&b.current));
        PackageVerdict {
            items,
            warnings,
            errors,
        }
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
            only: None,
            acc: OutdatedAccum::default(),
        }
    }

    /// Records a project that encloses the selection yet contributed no row, which also makes
    /// the run-level empty-selection note redundant.
    fn note_empty_project(&mut self, pctx: &super::ProjectCtx, evaluated: usize) {
        if let Some(note) = self.ws.empty_project_note(pctx, self.opts, evaluated) {
            self.acc.warnings.push(note);
            self.acc.empty_selection_noted = true;
        }
    }

    async fn run_project(&mut self, pctx: &'a super::ProjectCtx, progress: ProjectProgress) {
        let Some(read) = self.ws.read_project_ctx(pctx, self.opts, progress) else {
            return;
        };

        let (continue_run, refresh_guard) = self.refresh_lock(pctx, &read).await;
        if !continue_run {
            return;
        }

        let read_guard = if refresh_guard.is_some() {
            None
        } else {
            match self.ws.project_read_guard(pctx).await {
                Ok(guard) => Some(guard),
                Err(error) => {
                    self.acc.errors.push(diag_from_error(
                        &error,
                        pctx.tool,
                        &read.project_label,
                        None,
                    ));
                    return;
                }
            }
        };

        let Some(mut deps) = self.scoped_dependencies(pctx, &read).await else {
            return;
        };
        self.merge_manifest_constraints(read.adapter, pctx, &read.project_label, &mut deps)
            .await;
        if let Some(package) = self.only {
            deps.retain(|dep| dep.package.name == package);
        }
        self.acc.evaluated += deps.len();
        drop(read_guard);
        drop(refresh_guard);
        if self.only.is_none() {
            self.note_empty_project(pctx, deps.len());
        }
        if deps.is_empty() {
            return;
        }

        // `upgrade --dry-run` decides what moves, so the rows report its outcomes.
        // It needs the registry, which `--offline` rules out, so offline rows keep their own
        // per-package verdicts.
        let mut dry_run = if self.ws.mutator(pctx.tool).is_some() && !self.opts.offline {
            Some(
                self.ws
                    .dry_run_upgrade(pctx, self.opts, &read.progress)
                    .await,
            )
        } else {
            None
        };
        let planned = dry_run
            .as_mut()
            .map(|pass| std::mem::take(&mut pass.judged))
            .unwrap_or_default();
        let judged = self.judge(pctx, &read, deps, planned).await;

        let project_label = read.project_label.as_str();
        let mut fetch_failures = Vec::new();
        for FetchedRelease {
            dependency: dep,
            result,
        } in judged
        {
            let item = match result {
                Ok(judged) => {
                    if is_yanked_locked(&judged.releases, &dep) {
                        self.acc
                            .warnings
                            .push(yanked_warning(pctx, project_label, &dep));
                    }
                    let mut item = self.outdated_item(pctx, project_label, &dep, &judged);
                    if let Some(pass) = &dry_run {
                        settle(&mut item, &dep, &judged, pass);
                    }
                    item
                }
                Err(error) => {
                    let diag =
                        diag_from_error(&error, pctx.tool, project_label, Some(&dep.package.name));
                    fetch_failures.push(diag.clone());
                    error_item(pctx, project_label, &dep, diag)
                }
            };
            self.acc.items.push(item);
        }

        match dry_run {
            Some(pass) => {
                // A dry-run failure reaches the report through the rows it left unsettled, which
                // carry it as their error; beside them it is a note, since it may concern no row.
                // A fetch the dry run failed as well is already the row's own error.
                let failures = pass
                    .errors
                    .into_iter()
                    .filter(|error| !fetch_failures.contains(error));
                for diagnostic in pass.warnings.into_iter().chain(failures) {
                    push_new(&mut self.acc.warnings, diagnostic);
                }
            }
            None if self.opts.offline => self.note_offline(pctx),
            None => {}
        }
    }

    /// Notes that the project's adoptable rows are the per-package verdicts, which `upgrade` may
    /// not land, since `--offline` ruled out the dry run.
    fn note_offline(&mut self, pctx: &super::ProjectCtx) {
        let unverified = self
            .acc
            .items
            .iter()
            .filter(|item| item.status == OutdatedStatus::Adoptable)
            .count();
        if unverified == 0 {
            return;
        }
        self.acc.warnings.push(
            Diagnostic::new(
                DiagnosticKind::Config,
                format!(
                    "--offline: {unverified} adoptable row(s) were not verified against the upgrade resolve, which needs the registry; a row `upgrade` would hold can read adoptable here"
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(pctx.rel_path.as_str()),
        );
    }

    /// The project's scoped dependencies, or `None` after recording why the graph could not be
    /// read: a stale lock is a skip under `--allow-stale-lock` and an error otherwise.
    async fn scoped_dependencies(
        &mut self,
        pctx: &super::ProjectCtx,
        read: &ReadProjectCtx<'_>,
    ) -> Option<Vec<Dependency>> {
        read.progress.phase("resolving dependency graph");
        match self
            .ws
            .dependencies_in_scope(read.adapter, pctx, self.scope, self.opts)
            .await
        {
            Ok(scoped) => {
                self.acc.warnings.extend(scoped.notices);
                Some(scoped.deps)
            }
            Err(error) => {
                tracing::warn!(
                    project = read.project_label,
                    tool = pctx.tool.as_str(),
                    error = %error,
                    "could not enumerate dependencies"
                );
                let diagnostic = diag_from_error(&error, pctx.tool, &read.project_label, None)
                    .with_path(pctx.manifest_report_path());
                if self.opts.allow_stale_lock
                    && matches!(error, cooldown_core::CoreError::StaleLock(_))
                {
                    self.acc.skipped_stale_projects += 1;
                    self.acc.warnings.push(stale_evaluation_skipped(diagnostic));
                } else {
                    self.acc.errors.push(diagnostic);
                }
                None
            }
        }
    }

    /// Merges the build-backend requirements (`[build-system].requires`, e.g. hatchling) the
    /// lockfile never records into `deps`, so `outdated` surfaces a build-backend update the
    /// same way Dependabot does.
    ///
    /// They are direct, so they belong in both the direct and the transitive view.
    /// A failure to read them is a non-fatal warning — the resolved deps still report.
    async fn merge_manifest_constraints(
        &mut self,
        adapter: &dyn cooldown_core::ToolRead,
        pctx: &super::ProjectCtx,
        project_label: &str,
        deps: &mut Vec<Dependency>,
    ) {
        match self
            .ws
            .manifest_constraints_in_scope(adapter, pctx, self.opts)
            .await
        {
            Ok(constraints) => deps.extend(constraints),
            Err(error) => {
                tracing::warn!(
                    project = project_label,
                    tool = pctx.tool.as_str(),
                    error = %error,
                    "could not read build-system requirements"
                );
                self.acc
                    .warnings
                    .push(diag_from_error(&error, pctx.tool, project_label, None));
            }
        }
    }

    /// Every dependency with the releases and upgrade verdict its row reports.
    ///
    /// A dependency the dry run's plan read keeps the plan's own evaluation (`planned`), so the
    /// row and the plan judge one fetch with one advisory snapshot.
    /// The rest are fetched and judged here the same way: every row when no dry run ran, and the
    /// indirect rows of a tool that never plans them.
    async fn judge(
        &mut self,
        pctx: &super::ProjectCtx,
        read: &ReadProjectCtx<'_>,
        deps: Vec<Dependency>,
        mut planned: Vec<FetchedRelease<Judged>>,
    ) -> Vec<FetchedRelease<Judged>> {
        let mut judged = Vec::with_capacity(deps.len());
        let mut unjudged = Vec::new();
        for dep in deps {
            let found = planned.iter().position(|fact| {
                fact.result.is_ok()
                    && fact.dependency.package == dep.package
                    && fact.dependency.current == dep.current
                    && fact.dependency.direct == dep.direct
            });
            match found {
                Some(index) => judged.push(FetchedRelease {
                    dependency: dep,
                    result: planned.swap_remove(index).result,
                }),
                None => unjudged.push(dep),
            }
        }
        if unjudged.is_empty() {
            return judged;
        }
        // Identities must be adapter-confirmed before they are queried, matched, or counted
        // (see `ToolRead::confirm_advisory_identities`) — and only when the feed will run.
        if super::advisories::advisory_fetch_policy(pctx).is_some() {
            read.adapter
                .confirm_advisory_identities(&pctx.project, &mut unjudged)
                .await;
        }
        let advisory_fetch = self.ws.fetch_project_advisories(
            read.adapter,
            pctx,
            &read.project_label,
            AdvisoryPackages::from_deps(&unjudged),
            self.opts,
            &read.progress,
        );
        let release_fetch = self.fetch_releases(read, pctx, unjudged);
        let (advisory_fetch, fetched) = tokio::join!(advisory_fetch, release_fetch);
        let advisories = advisory_fetch.record(&mut self.acc.warnings, &mut self.acc.errors);
        judged.extend(
            fetched
                .into_iter()
                .map(|FetchedRelease { dependency, result }| FetchedRelease {
                    result: result.map(|releases| Judged {
                        verdict: upgrade_verdict(
                            read.adapter,
                            pctx,
                            self.opts,
                            self.ws.now(),
                            advisories.as_ref(),
                            &dependency,
                            &releases,
                        ),
                        releases,
                    }),
                    dependency,
                }),
        );
        judged
    }

    async fn refresh_lock(
        &mut self,
        pctx: &'a super::ProjectCtx,
        read: &ReadProjectCtx<'_>,
    ) -> (bool, Option<ProjectAccessWriteGuard>) {
        let project_label = read.project_label.as_str();
        match self
            .ws
            .refresh_project_lock(pctx, self.opts, &read.progress)
            .await
        {
            Ok(refresh) => {
                self.acc.warnings.extend(refresh.warnings);
                let continue_run = match refresh.report {
                    Ok(report) => report
                        .is_none_or(|report| self.handle_lock_report(report, pctx, project_label)),
                    Err(error) => {
                        self.acc.errors.push(
                            diag_from_error(&error, pctx.tool, project_label, None)
                                .with_path(pctx.manifest_report_path()),
                        );
                        false
                    }
                };
                (continue_run, refresh.guard)
            }
            Err(error) => {
                self.acc.errors.push(
                    diag_from_error(&error, pctx.tool, project_label, None)
                        .with_path(pctx.manifest_report_path()),
                );
                (false, None)
            }
        }
    }

    fn handle_lock_report(
        &mut self,
        report: LockVerifyReport,
        pctx: &'a super::ProjectCtx,
        project_label: &str,
    ) -> bool {
        let stale = report.status == cooldown_core::LockStatus::Stale;
        let outcome = lock_report_outcome(
            report,
            pctx.tool,
            project_label,
            &pctx.manifest_report_path(),
            self.opts.allow_stale_lock,
        );
        match outcome.action {
            LockReportAction::Continue => {
                if let Some(diagnostic) = outcome.diagnostic {
                    self.acc.warnings.push(if stale {
                        stale_evaluation_skipped(diagnostic)
                    } else {
                        diagnostic
                    });
                }
                if stale {
                    self.acc.skipped_stale_projects += 1;
                    false
                } else {
                    true
                }
            }
            LockReportAction::Skip => {
                if let Some(diagnostic) = outcome.diagnostic {
                    self.acc.errors.push(diagnostic);
                }
                false
            }
        }
    }

    /// Fetch release metadata for the given dependencies of one project, bounded by the
    /// registry fan-out. Each release fetch is independent, so a single slow or failing dependency
    /// never blocks the others. The surrounding `info!`/per-dependency `debug!`/`trace!` spans make
    /// a stalled network call visible under `--log-level debug`.
    async fn fetch_releases(
        &self,
        read: &ReadProjectCtx<'_>,
        pctx: &super::ProjectCtx,
        deps: Vec<Dependency>,
    ) -> Vec<FetchedRelease<Vec<Release>>> {
        let project_label = read.project_label.as_str();
        tracing::info!(
            project = project_label,
            tool = pctx.tool.as_str(),
            deps = deps.len(),
            fanout = self.opts.fanout(),
            "fetching release metadata"
        );
        read.progress
            .phase(format!("fetching metadata for {} dependencies", deps.len()));
        let started = std::time::Instant::now();
        let fetched = self
            .ws
            .fetch_candidate_releases(
                read.adapter,
                deps,
                &read.fetch,
                self.opts.candidate_scope(),
                &read.progress,
                self.opts.fanout(),
            )
            .await;
        for FetchedRelease {
            dependency: dep,
            result,
        } in &fetched
        {
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

    /// Build the report item for a dependency from its judged releases.
    fn outdated_item(
        &self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        dep: &Dependency,
        judged: &Judged,
    ) -> OutdatedItem {
        let Judged { releases, verdict } = judged;

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
        // The row's security note.
        // A candidate the security window actually fast-tracked wins: only exact fix versions
        // earn the window, so the newest one that did is the actionable "adoptable now BECAUSE
        // it fixes GHSA-…" evidence, even when a newer candidate is merely annotated.
        // With none applied, prefer the shown candidate's relevance (its window is the one the
        // row renders), else the newest security-relevant candidate.
        // The block carries the candidate's version so JSON stays self-describing when that
        // candidate is not `shown`.
        let with_version = |candidate: &'_ cooldown_core::Candidate| {
            candidate
                .security
                .as_ref()
                .map(|security| (security.clone(), candidate.version.clone()))
        };
        let applied =
            verdict.candidates.iter().rev().find_map(|candidate| {
                with_version(candidate).filter(|(security, _)| security.applied)
            });
        let security = applied
            .or_else(|| shown.and_then(with_version))
            .or_else(|| verdict.candidates.iter().rev().find_map(with_version))
            .map(|(security, version)| super::security_info(&security, &version));
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
            adoptable_target: verdict.adoptable_target.as_ref().map(ToString::to_string),
            blocked_by: None,
            blocked_reason: None,
            held_by: verdict.held_reason.clone().map(Into::into),
            latest,
            security,
            error: None,
        }
    }
}

/// Sorts the merged rows and folds the tallies into the run's report.
fn finalize_outdated(opts: &RunOpts, mut acc: OutdatedAccum) -> OutdatedOutcome {
    // Releases are fetched concurrently (`buffer_unordered`), so the items arrive in a
    // non-deterministic order.
    // Sort by (project, status, name, version) for a stable report (and stable `--json`); the
    // status rank puts ready-to-adopt updates last.
    acc.items.sort_by(|a, b| {
        a.project
            .cmp(&b.project)
            .then_with(|| a.status.sort_rank().cmp(&b.status.sort_rank()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.current.cmp(&b.current))
    });
    let mut summary = summarize(&acc.items);
    summary.skipped_stale_projects = acc.skipped_stale_projects;
    // An error already explains an empty run; otherwise a selection that evaluated nothing
    // must say so, or the report is indistinguishable from "everything is current".
    if acc.errors.is_empty()
        && !acc.empty_selection_noted
        && let Some(note) =
            super::empty_selection_diagnostic(opts, acc.evaluated, acc.skipped_stale_projects)
    {
        acc.warnings.push(note);
    }
    OutdatedOutcome {
        summary,
        items: acc.items,
        warnings: acc.warnings,
        errors: acc.errors,
        exit: Exit::Ok,
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
            shortened_by: None,
        },
        candidate_age_days: None,
        cooldown_version: None,
        status: OutdatedStatus::Error,
        adoptable_target: None,
        blocked_by: None,
        blocked_reason: None,
        held_by: None,
        latest: None,
        security: None,
        error: Some(diag),
    }
}

fn summarize(items: &[OutdatedItem]) -> OutdatedSummary {
    let mut s = OutdatedSummary {
        total: items.len(),
        skipped_stale_projects: 0,
        adoptable: 0,
        blocked: 0,
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
            OutdatedStatus::Blocked => s.blocked += 1,
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

/// Settles `item` on what the `upgrade` dry run `pass` did with its dependency's copy, so the
/// row's verdict and target are `upgrade`'s.
///
/// A forward landing of the copy is the row's target, whatever its own verdict; an exempt row
/// stays `exempt`.
/// Otherwise a row with a matured target is `blocked` by the hold `upgrade` recorded at that
/// target, an `error` when the dry run failed before settling it, and else `blocked` because
/// `upgrade` plans no move for it.
/// Every other row keeps its own verdict.
fn settle(item: &mut OutdatedItem, dep: &Dependency, judged: &Judged, pass: &UpgradeAccum) {
    let Judged { releases, verdict } = judged;
    let rows = || {
        pass.items
            .iter()
            .filter(|row| row.edge.is_none() && row.from == dep.current.as_str())
    };
    let forward = |row: &&UpgradeItem| row.applied && !row.downgrade && row.from != row.to;
    let target = verdict.adoptable_target.as_ref().filter(|target| {
        matches!(verdict.status, Status::Adoptable | Status::Exempt) && **target != dep.current
    });
    // The target identifies the hold: a `--major` notice or a ceiling can share the source
    // version with an in-line move.
    let held = target.and_then(|target| {
        rows().find_map(|row| {
            let skipped = row.skipped.as_ref()?;
            (row.to == target.as_str() && reports_copy(row, dep, releases, false))
                .then_some((row, skipped))
        })
    });
    // The copy's own move, or else a sibling's that carried it onto its target, which `upgrade`
    // credits only once the target holds in every declaring member: short of one, it keeps the
    // target's hold.
    let landed = rows()
        .filter(forward)
        .find(|row| reports_copy(row, dep, releases, false))
        .or_else(|| {
            let target = target.filter(|_| held.is_none())?;
            rows()
                .filter(forward)
                .find(|row| row.to == target.as_str() && reports_copy(row, dep, releases, true))
        });
    if let Some(landed) = landed {
        if item.status != OutdatedStatus::Exempt {
            item.status = OutdatedStatus::Adoptable;
            item.held_by = None;
        }
        item.adoptable_target = Some(landed.to.clone());
        return;
    }
    if target.is_none() {
        return;
    }
    if let Some((row, skipped)) = held {
        item.status = OutdatedStatus::Blocked;
        item.blocked_by = skipped
            .offending
            .clone()
            .filter(|offender| *offender != row.name);
        item.blocked_reason = Some(skipped.message.clone());
        return;
    }
    let failure = rows()
        .filter(|row| reports_copy(row, dep, releases, false))
        .find_map(|row| row.error.clone())
        .or_else(|| {
            pass.errors
                .iter()
                .find(|error| error.package.as_ref() == Some(&dep.package.name))
                .or_else(|| pass.errors.first())
                .cloned()
        });
    if let Some(error) = failure {
        item.status = OutdatedStatus::Error;
        item.adoptable_target = None;
        item.error = Some(error);
    } else {
        item.status = OutdatedStatus::Blocked;
        item.blocked_reason = Some(FOLLOWS_REQUIRERS.to_string());
    }
}

/// Whether `row` reports a move of `dep`'s own copy: from its current version, under its own name
/// or the path-major name a Go move to the row's target takes, and from the same declaring
/// members unless `member_blind`.
///
/// A held Go cross-path move keeps the name the tree still requires, while a landed one is named
/// for its target, so either spelling is the copy's.
fn reports_copy(
    row: &UpgradeItem,
    dep: &Dependency,
    releases: &[Release],
    member_blind: bool,
) -> bool {
    let to = Version::new(row.to.clone());
    let key = |package: PackageId| {
        let key = ChangeKey::of(&Change {
            package,
            from: dep.current.clone(),
            to: to.clone(),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: dep.direct,
            members: dep.members.clone(),
        });
        if member_blind {
            key.member_blind()
        } else {
            key
        }
    };
    let row_key = if member_blind {
        ChangeKey::of_item(row).member_blind()
    } else {
        ChangeKey::of_item(row)
    };
    row_key == key(dep.package.clone()) || row_key == key(target_package_for(releases, dep, &to))
}

/// Appends `diagnostic` unless `diagnostics` already holds it: the dry run reads the same project
/// and can repeat a notice the report raised.
fn push_new(diagnostics: &mut Vec<Diagnostic>, diagnostic: Diagnostic) {
    if !diagnostics.contains(&diagnostic) {
        diagnostics.push(diagnostic);
    }
}

#[cfg(test)]
mod tests {
    use super::{FOLLOWS_REQUIRERS, Judged, settle};
    use crate::app::upgrade::UpgradeAccum;
    use crate::app::{OutdatedItem, OutdatedStatus, SkippedInfo, UpgradeItem, Window};
    use cooldown_core::{
        Dependency, Diagnostic, DiagnosticKind, MajorKey, MemberRef, PackageId, Release,
        ReleaseOrder, ReleaseQuality, SkipReason, Status, ToolId, UpdateKind, Verdict, Version,
    };

    const UV: ToolId = ToolId("uv");
    const GO: ToolId = ToolId("go");

    /// Folding two projects keeps every row and diagnostic in fold order and sums the tallies,
    /// so the run reads as the sequential fold would.
    #[test]
    fn merged_projects_fold_every_row_tally_and_diagnostic() {
        let mut first = super::OutdatedAccum {
            skipped_stale_projects: 1,
            empty_selection_noted: true,
            evaluated: 3,
            ..super::OutdatedAccum::default()
        };
        first.items.push(adoptable("a", "1.0.0", "1.1.0"));
        first.warnings.push(cooldown_core::Diagnostic::new(
            cooldown_core::DiagnosticKind::Config,
            "first",
        ));
        let mut second = super::OutdatedAccum {
            evaluated: 2,
            ..super::OutdatedAccum::default()
        };
        second.items.push(adoptable("b", "1.0.0", "1.1.0"));
        second.warnings.push(cooldown_core::Diagnostic::new(
            cooldown_core::DiagnosticKind::Config,
            "second",
        ));
        second.errors.push(cooldown_core::Diagnostic::new(
            cooldown_core::DiagnosticKind::Config,
            "error",
        ));

        let mut run = super::OutdatedAccum::default();
        run.merge(first);
        run.merge(second);

        assert_eq!(run.skipped_stale_projects, 1);
        assert_eq!(run.evaluated, 5);
        assert!(run.empty_selection_noted);
        let names: Vec<&str> = run.items.iter().map(|item| item.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        let warnings: Vec<&str> = run
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert_eq!(warnings, ["first", "second"]);
        assert_eq!(run.errors.len(), 1);
    }

    /// A project that already explained an empty selection keeps the run-level note away.
    #[test]
    fn a_noted_empty_selection_suppresses_the_run_level_note() {
        let opts = crate::app::RunOpts {
            scope: crate::app::RunScope::new(
                camino::Utf8Path::new("/repo"),
                camino::Utf8Path::new("/repo/sub"),
            ),
            ..crate::app::RunOpts::default()
        };
        let noted = super::OutdatedAccum {
            empty_selection_noted: true,
            ..super::OutdatedAccum::default()
        };

        let with_note = super::finalize_outdated(&opts, super::OutdatedAccum::default());
        let without_note = super::finalize_outdated(&opts, noted);

        assert_eq!(with_note.warnings.len(), 1, "{:?}", with_note.warnings);
        assert!(
            without_note.warnings.is_empty(),
            "{:?}",
            without_note.warnings
        );
    }

    fn adoptable(name: &str, current: &str, target: &str) -> OutdatedItem {
        OutdatedItem {
            name: name.to_string(),
            tool: "uv".to_string(),
            project: ".".to_string(),
            registry: Some("https://pypi.org/simple".to_string()),
            direct: true,
            current: current.to_string(),
            members: Vec::new(),
            window: Window {
                min_age_days: 14.0,
                source: "default".into(),
                clamped_by: None,
                shortened_by: None,
            },
            candidate_age_days: Some(40.0),
            cooldown_version: None,
            status: OutdatedStatus::Adoptable,
            adoptable_target: Some(target.to_string()),
            blocked_by: None,
            blocked_reason: None,
            held_by: None,
            latest: None,
            security: None,
            error: None,
        }
    }

    fn dependency(item: &OutdatedItem) -> Dependency {
        let tool = if item.tool == "go" { GO } else { UV };
        Dependency {
            package: PackageId::new(tool, item.name.clone(), item.registry.clone()),
            advisory_identity: Some(item.name.clone()),
            current: Version::new(item.current.clone()),
            current_quality: ReleaseQuality::Stable,
            direct: item.direct,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: item.members.clone(),
            pinned: false,
            hold_edges: Vec::new(),
        }
    }

    /// The upgrade row for `item`'s copy moving to `to`, applied unless `skipped` says why not.
    fn row(item: &OutdatedItem, to: &str, skipped: Option<SkippedInfo>) -> UpgradeItem {
        UpgradeItem {
            name: item.name.clone(),
            tool: item.tool.clone(),
            project: ".".to_string(),
            direct: item.direct,
            downgrade: false,
            members: item.members.clone(),
            registry: item.registry.clone(),
            from: item.current.clone(),
            to: to.to_string(),
            kind: UpdateKind::Minor,
            applied: skipped.is_none(),
            skipped,
            error: None,
            edge: None,
            security: None,
        }
    }

    fn hold(reason: SkipReason, blocker: Option<&str>) -> SkippedInfo {
        SkippedInfo {
            reason,
            message: blocker.map_or_else(
                || reason.message().to_string(),
                |blocker| format!("conflicts with {blocker}"),
            ),
            offending: blocker.map(str::to_string),
        }
    }

    fn release(version: &str, order: u8, major: &str) -> Release {
        Release {
            version: Version::new(version),
            order: ReleaseOrder(vec![order]),
            major: MajorKey(major.to_string()),
            major_number: major.trim_start_matches("/v").parse().ok(),
            kind_from_current: Some(UpdateKind::Major),
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: None,
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
    }

    /// Settles `item`, judged `status` with its own target, against a dry run that reported
    /// `rows` and project `errors`.
    fn settled(
        mut item: OutdatedItem,
        status: Status,
        releases: Vec<Release>,
        rows: Vec<UpgradeItem>,
        errors: Vec<Diagnostic>,
    ) -> OutdatedItem {
        let judged = Judged {
            releases,
            verdict: Verdict {
                status,
                adoptable_target: item.adoptable_target.as_deref().map(Version::new),
                latest: None,
                candidates: Vec::new(),
                held_reason: None,
            },
        };
        let pass = UpgradeAccum {
            items: rows,
            errors,
            ..UpgradeAccum::default()
        };
        let dep = dependency(&item);
        settle(&mut item, &dep, &judged, &pass);
        item
    }

    fn verdict(item: &OutdatedItem) -> (OutdatedStatus, Option<&str>, Option<&str>) {
        (
            item.status,
            item.adoptable_target.as_deref(),
            item.blocked_reason.as_deref(),
        )
    }

    /// The version `upgrade` lands is the row's target, even when it is a fallback below the
    /// row's own target or an in-line move beside a held major.
    #[test]
    fn a_landing_is_the_rows_target() {
        let item = adoptable("parent", "1.0.0", "1.4.0");
        let rows = vec![row(&item, "1.3.0", None)];
        let fallback = settled(item, Status::Adoptable, Vec::new(), rows, Vec::new());
        assert_eq!(
            verdict(&fallback),
            (OutdatedStatus::Adoptable, Some("1.3.0"), None)
        );

        let item = adoptable("eslint", "8.56.0", "9.0.0");
        let rows = vec![
            row(&item, "8.57.0", None),
            row(
                &item,
                "9.0.0",
                Some(hold(SkipReason::PeerHeld, Some("plugin"))),
            ),
        ];
        let in_line = settled(item, Status::Adoptable, Vec::new(), rows, Vec::new());
        assert_eq!(
            verdict(&in_line),
            (OutdatedStatus::Adoptable, Some("8.57.0"), None)
        );
        assert_eq!(in_line.blocked_by, None);
    }

    /// A hold at the row's target blocks it with the blocker and the reason `upgrade` gives; a
    /// hold at another target, such as a `--major` notice, says nothing about it.
    #[test]
    fn only_a_hold_at_the_target_blocks_the_row() {
        let item = adoptable("typer", "0.25.1", "0.26.7");
        let rows = vec![row(
            &item,
            "0.26.7",
            Some(hold(SkipReason::ResolverConflict, Some("huggingface-hub"))),
        )];
        let typer = settled(item, Status::Adoptable, Vec::new(), rows, Vec::new());
        assert_eq!(
            verdict(&typer),
            (
                OutdatedStatus::Blocked,
                Some("0.26.7"),
                Some("conflicts with huggingface-hub")
            )
        );
        assert_eq!(typer.blocked_by.as_deref(), Some("huggingface-hub"));

        let item = adoptable("serde", "1.0.188", "1.0.190");
        let rows = vec![row(
            &item,
            "2.0.0",
            Some(hold(SkipReason::NeedsMajor, None)),
        )];
        let serde = settled(item, Status::Adoptable, Vec::new(), rows, Vec::new());
        assert_eq!(
            verdict(&serde),
            (
                OutdatedStatus::Blocked,
                Some("1.0.190"),
                Some(FOLLOWS_REQUIRERS)
            )
        );
    }

    /// Two coexisting copies of one package, and two members declaring it, settle apart: the
    /// hold on one never blocks the other, which landed.
    #[test]
    fn copies_and_members_settle_apart() {
        let older = adoptable("shared", "1.2.0", "1.4.0");
        let newer = adoptable("shared", "1.3.0", "1.4.0");
        let rows = vec![
            row(
                &older,
                "1.4.0",
                Some(hold(SkipReason::ResolverConflict, Some("keeper"))),
            ),
            row(&newer, "1.4.0", None),
        ];
        let older = settled(
            older,
            Status::Adoptable,
            Vec::new(),
            rows.clone(),
            Vec::new(),
        );
        let newer = settled(newer, Status::Adoptable, Vec::new(), rows, Vec::new());
        assert_eq!(older.status, OutdatedStatus::Blocked);
        assert_eq!(newer.status, OutdatedStatus::Adoptable);

        let member = |name: &str| MemberRef {
            name: name.to_string(),
            path: format!("packages/{name}"),
        };
        let mut left = adoptable("serde", "1.0.188", "1.0.190");
        left.members = vec![member("left")];
        let mut right = left.clone();
        right.members = vec![member("right")];
        let rows = vec![
            row(
                &left,
                "1.0.190",
                Some(hold(SkipReason::ResolverConflict, Some("legacy"))),
            ),
            row(&right, "1.0.190", None),
        ];
        let left = settled(
            left,
            Status::Adoptable,
            Vec::new(),
            rows.clone(),
            Vec::new(),
        );
        let right = settled(right, Status::Adoptable, Vec::new(), rows, Vec::new());
        assert_eq!(left.status, OutdatedStatus::Blocked);
        assert_eq!(right.status, OutdatedStatus::Adoptable);
    }

    /// A sibling's move that carries a declared row onto its target lands the row only when
    /// `upgrade` dropped the target's hold, which it keeps while a declaring member stays behind.
    #[test]
    fn a_collateral_landing_counts_only_in_every_declaring_member() {
        let mut item = adoptable("c", "1.0.0", "2.0.0");
        item.members = vec![
            MemberRef {
                name: "web".to_string(),
                path: "web".to_string(),
            },
            MemberRef {
                name: "api".to_string(),
                path: "api".to_string(),
            },
        ];
        let mut collateral = row(&item, "2.0.0", None);
        collateral.direct = false;
        collateral.members = Vec::new();

        let everywhere = settled(
            item.clone(),
            Status::Adoptable,
            Vec::new(),
            vec![collateral.clone()],
            Vec::new(),
        );
        assert_eq!(
            verdict(&everywhere),
            (OutdatedStatus::Adoptable, Some("2.0.0"), None)
        );

        let kept = row(
            &item,
            "2.0.0",
            Some(hold(SkipReason::ResolverConflict, None)),
        );
        let short = settled(
            item,
            Status::Adoptable,
            Vec::new(),
            vec![collateral, kept],
            Vec::new(),
        );
        assert_eq!(short.status, OutdatedStatus::Blocked);
    }

    /// A held Go cross-path move keeps the path the tree still requires while a landed one is
    /// named for its target, and either settles the row; a hold at another target does not.
    #[test]
    fn go_cross_path_moves_settle_under_either_spelling() {
        let mut item = adoptable("github.com/foo/bar", "v2.9.0+incompatible", "v3.0.2");
        item.tool = "go".to_string();
        item.registry = None;
        let releases = || {
            vec![
                release("v2.9.0+incompatible", 1, ""),
                release("v3.0.1", 2, "/v3"),
                release("v3.0.2", 3, "/v3"),
            ]
        };
        let mut landed = row(&item, "v3.0.2", None);
        landed.name = "github.com/foo/bar/v3".to_string();
        let moved = settled(
            item.clone(),
            Status::Adoptable,
            releases(),
            vec![landed],
            Vec::new(),
        );
        assert_eq!(moved.status, OutdatedStatus::Adoptable);

        let self_blamed = Some(hold(
            SkipReason::ResolverConflict,
            Some("github.com/foo/bar"),
        ));
        let blocked = settled(
            item.clone(),
            Status::Adoptable,
            releases(),
            vec![row(&item, "v3.0.2", self_blamed.clone())],
            Vec::new(),
        );
        assert_eq!(blocked.status, OutdatedStatus::Blocked);
        // A row that blames itself names no blocker, yet still carries its reason.
        assert_eq!(blocked.blocked_by, None);
        assert!(blocked.blocked_reason.is_some());

        let elsewhere = settled(
            item.clone(),
            Status::Adoptable,
            releases(),
            vec![row(&item, "v3.0.1", self_blamed)],
            Vec::new(),
        );
        assert_eq!(elsewhere.blocked_reason.as_deref(), Some(FOLLOWS_REQUIRERS));
    }

    /// Without a forward landing a matured target is no promise: a copy `upgrade` never moves,
    /// a reconciliation downgrade, and a lock-edge rebind all leave the row blocked.
    #[test]
    fn a_row_without_a_forward_landing_is_blocked() {
        let item = adoptable("follower", "0.2.4", "0.2.5");
        let mut downgrade = row(&item, "0.2.3", None);
        downgrade.downgrade = true;
        let mut edge = row(&item, "0.2.5", None);
        edge.edge = Some(crate::app::UpgradeEdgeInfo {
            dependent: "parent".to_string(),
            dependent_version: "1.0.0".to_string(),
            dependent_source: None,
            action: cooldown_core::EdgeBindingAction::Rebound,
            detail: None,
        });
        for rows in [Vec::new(), vec![downgrade], vec![edge]] {
            let follower = settled(
                item.clone(),
                Status::Adoptable,
                Vec::new(),
                rows,
                Vec::new(),
            );
            assert_eq!(
                verdict(&follower),
                (
                    OutdatedStatus::Blocked,
                    Some("0.2.5"),
                    Some(FOLLOWS_REQUIRERS)
                )
            );
        }
    }

    /// A dry run that failed before settling a row turns it into an error carrying the failure,
    /// the one naming the package when there is one.
    #[test]
    fn an_unsettled_row_carries_the_dry_runs_failure() {
        let unrelated = Diagnostic::new(DiagnosticKind::ToolFailed, "the resolver crashed");
        let named =
            Diagnostic::new(DiagnosticKind::ToolFailed, "no metadata").with_package("requests");
        let item = adoptable("requests", "2.34.1", "2.34.2");
        let failed = settled(
            item,
            Status::Adoptable,
            Vec::new(),
            Vec::new(),
            vec![unrelated, named.clone()],
        );
        assert_eq!(failed.status, OutdatedStatus::Error);
        assert_eq!(failed.adoptable_target, None);
        assert_eq!(failed.error, Some(named));
    }

    /// An exempt row stays `exempt` at the target `upgrade` lands, and a held row keeps its own
    /// verdict beside the notice `upgrade` records for its pin.
    #[test]
    fn exempt_and_held_rows_keep_their_verdicts() {
        let mut item = adoptable("rayon-core", "1.11.0", "1.13.0");
        item.status = OutdatedStatus::Exempt;
        let rows = vec![row(&item, "1.12.0", None)];
        let exempt = settled(item, Status::Exempt, Vec::new(), rows, Vec::new());
        assert_eq!(
            verdict(&exempt),
            (OutdatedStatus::Exempt, Some("1.12.0"), None)
        );

        let mut item = adoptable("pinned", "1.0.0", "1.2.0");
        item.status = OutdatedStatus::Held;
        let rows = vec![row(&item, "1.2.0", Some(hold(SkipReason::PinHeld, None)))];
        let pinned = settled(item, Status::Held, Vec::new(), rows, Vec::new());
        assert_eq!(
            verdict(&pinned),
            (OutdatedStatus::Held, Some("1.2.0"), None)
        );
    }
}
