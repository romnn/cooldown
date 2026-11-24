//! `outdated` — what could update, split into "adoptable now" vs "in cooldown". Reasons over the
//! candidate set; informational, so per-dep failures never change the exit code.

use super::advisories::AdvisoryPackages;
use super::lanes::LaneAccess;
use super::lock::ProjectAccessWriteGuard;
use super::read::ReadProjectCtx;
use super::upgrade::{CopyOutcome, Judged, PlannedCopy, UpgradeAccum, upgrade_verdict};
use super::{
    Exit, FetchedRelease, LockReportAction, ProjectProgress, RunOpts, Workspace, age_days,
    diag_from_error, lock_report_outcome, render_window, stale_evaluation_skipped,
};
use super::{LatestInfo, OutdatedItem, OutdatedStatus, OutdatedSummary, Window};
use cooldown_core::{
    CooldownHorizon, DepScope, Dependency, Diagnostic, DiagnosticKind, LockVerifyReport, PackageId,
    Release, ResolveKind, ResolveQuery, Status, Verdict, Version, newest_eligible, resolve,
};
use std::collections::HashMap;

/// Why a row with a matured target is blocked when `upgrade` decided no move of its copy: it
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
        let (planned, unsettled) = take_settlement(dry_run.as_mut());
        let judged = self.judge(pctx, &read, deps, planned).await;

        let project_label = read.project_label.as_str();
        for PlannedCopy {
            fact:
                FetchedRelease {
                    dependency: dep,
                    result,
                },
            outcome,
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
                    if dry_run.is_some() {
                        settle(&mut item, &dep, &judged.verdict, outcome, &unsettled);
                    }
                    item
                }
                Err(error) => {
                    let diag =
                        diag_from_error(&error, pctx.tool, project_label, Some(&dep.package.name));
                    error_item(pctx, project_label, &dep, diag)
                }
            };
            self.acc.items.push(item);
        }

        match &dry_run {
            Some(pass) => self.note_dry_run(pass),
            None if self.opts.offline => self.note_offline(pctx),
            None => {}
        }
    }

    /// Notes the dry run's warnings, and each of its failures no row carries, such as one for a
    /// package the report leaves out: a failure reaches the report through the rows it settled.
    fn note_dry_run(&mut self, pass: &UpgradeAccum) {
        let carried: Vec<&Diagnostic> = self
            .acc
            .items
            .iter()
            .filter_map(|item| item.error.as_ref())
            .collect();
        let failures = pass
            .errors
            .iter()
            .chain(pass.items.iter().filter_map(|row| row.error.as_ref()))
            .filter(|error| !carried.contains(error));
        let notes: Vec<Diagnostic> = pass.warnings.iter().chain(failures).cloned().collect();
        for diagnostic in notes {
            push_new(&mut self.acc.warnings, diagnostic);
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

    /// Every dependency with the releases and upgrade verdict its row reports, and what the dry
    /// run settled for its copy.
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
        planned: Vec<PlannedCopy>,
    ) -> Vec<PlannedCopy> {
        let mut plan = PlannedCopies::new();
        for copy in planned {
            let dep = &copy.fact.dependency;
            plan.entry((dep.package.clone(), dep.current.clone(), dep.direct))
                .or_default()
                .push(copy);
        }
        let mut judged = Vec::with_capacity(deps.len());
        let mut unjudged = Vec::new();
        for dep in deps {
            match take_copy(&mut plan, &dep, |copy| copy.fact.result.is_ok()) {
                Some(PlannedCopy { fact, outcome }) => judged.push(PlannedCopy {
                    fact: FetchedRelease {
                        dependency: dep,
                        result: fact.result,
                    },
                    outcome,
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
        for FetchedRelease { dependency, result } in fetched {
            // A copy the plan could not judge keeps what the pass settled for it.
            let outcome = take_copy(&mut plan, &dependency, |_| true).and_then(|copy| copy.outcome);
            let result = result.map(|releases| Judged {
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
            });
            judged.push(PlannedCopy {
                fact: FetchedRelease { dependency, result },
                outcome,
            });
        }
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

        // The planner judges a transitive row within its major, as `upgrade` moves it, while
        // Latest names the newest version under the run's own major setting.
        let newest = if dep.direct || !self.opts.allow_major {
            verdict.latest.clone()
        } else {
            newest_eligible(dep, releases, true, self.ws.now()).or_else(|| verdict.latest.clone())
        };

        // Under `--countdown soonest` the cooldown can count down to a version no other column
        // names: an intermediate that matures first, or a candidate below a Latest no candidate
        // reaches, such as a transitive's next major.
        // Label that version so the cell is unambiguous.
        // The `latest` view tracks the newest candidate and stays bare.
        let cooldown_version = shown
            .map(|candidate| &candidate.version)
            .filter(|&version| {
                self.opts.cooldown_horizon == CooldownHorizon::Soonest
                    && Some(version) != newest.as_ref()
            })
            .map(ToString::to_string);

        let latest = newest.as_ref().map(|lv| {
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

/// The copies the `upgrade` dry run's first plan read, by package, version, and whether a member
/// declares them.
type PlannedCopies = HashMap<(PackageId, Version, bool), Vec<PlannedCopy>>;

/// Takes the copies the dry run's first plan read, each with what the pass settled for it, and
/// the failure that stopped a pass which settled none, such as one that failed before planning,
/// for the rows it left unconfirmed.
fn take_settlement(dry_run: Option<&mut UpgradeAccum>) -> (Vec<PlannedCopy>, CopyOutcome) {
    let Some(pass) = dry_run else {
        return (Vec::new(), CopyOutcome::Undecided);
    };
    let copies = pass
        .facts
        .take()
        .and_then(|facts| facts.copies)
        .unwrap_or_default();
    let unsettled = if copies.iter().any(|copy| copy.outcome.is_some()) {
        CopyOutcome::Undecided
    } else {
        pass.errors
            .iter()
            .rev()
            .find(|error| error.package.is_none())
            .map_or(CopyOutcome::Undecided, |failure| {
                CopyOutcome::Failed(failure.clone())
            })
    };
    (copies, unsettled)
}

/// Takes the copy with `dep`'s own declaration that `wanted` accepts, so a row matches one copy
/// at most and never another copy's outcome.
fn take_copy(
    plan: &mut PlannedCopies,
    dep: &Dependency,
    wanted: impl Fn(&PlannedCopy) -> bool,
) -> Option<PlannedCopy> {
    let copies = plan.get_mut(&(dep.package.clone(), dep.current.clone(), dep.direct))?;
    let at = copies
        .iter()
        .position(|copy| copy.fact.dependency.members == dep.members && wanted(copy))?;
    Some(copies.swap_remove(at))
}

/// Settles `item`, judged `verdict`, on what the `upgrade` dry run did with its copy
/// (`outcome`), so the row's verdict and target are `upgrade`'s.
///
/// A landing is the row's target, whatever its own verdict, and an exempt row that lands stays
/// `exempt`; a refused move blocks the row at that move's target, and a failure for the copy
/// makes the row an `error`.
/// A row with a matured target is `blocked` when the dry run decided nothing about its copy,
/// because `upgrade` never moves it, and an `error` when the pass settled no copy at all
/// (`unsettled` is then the failure that stopped it).
/// Every other row keeps its own verdict.
fn settle(
    item: &mut OutdatedItem,
    dep: &Dependency,
    verdict: &Verdict,
    outcome: Option<CopyOutcome>,
    unsettled: &CopyOutcome,
) {
    let actionable = matches!(verdict.status, Status::Adoptable | Status::Exempt)
        && verdict
            .adoptable_target
            .as_ref()
            .is_some_and(|target| *target != dep.current);
    let outcome = match outcome {
        Some(outcome) => outcome,
        None if actionable => unsettled.clone(),
        None => return,
    };
    let (to, security) = match outcome {
        CopyOutcome::Landed { to, security } => {
            if item.status != OutdatedStatus::Exempt {
                item.status = OutdatedStatus::Adoptable;
                item.held_by = None;
            }
            (to, security)
        }
        CopyOutcome::Refused { to, skip, security } => {
            item.status = OutdatedStatus::Blocked;
            item.held_by = None;
            item.blocked_by = skip.offending.filter(|offender| *offender != item.name);
            item.blocked_reason = Some(skip.message);
            (to, security)
        }
        CopyOutcome::Failed(error) => {
            item.status = OutdatedStatus::Error;
            item.held_by = None;
            item.adoptable_target = None;
            item.error = Some(error);
            return;
        }
        CopyOutcome::Undecided if actionable => {
            item.status = OutdatedStatus::Blocked;
            item.blocked_reason = Some(FOLLOWS_REQUIRERS.to_string());
            return;
        }
        CopyOutcome::Undecided => return,
    };
    item.adoptable_target = Some(to);
    // The security note describes the version the row now reports.
    if security.is_some() {
        item.security = security;
    }
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
    use super::{CopyOutcome, FOLLOWS_REQUIRERS, settle};
    use crate::app::{OutdatedItem, OutdatedStatus, SecurityInfo, SkippedInfo, Window};
    use cooldown_core::{
        Dependency, Diagnostic, DiagnosticKind, PackageId, ReleaseQuality, SkipReason, Status,
        ToolId, Verdict, Version,
    };

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

    fn security(version: &str) -> SecurityInfo {
        SecurityInfo {
            version: version.to_string(),
            fixes: vec!["GHSA-inline".to_string()],
            severity: cooldown_core::AdvisorySeverity::High,
            source: cooldown_core::AdvisorySourceId("osv"),
            applied: false,
        }
    }

    /// Settles a row of `status` with the matured target `1.4.0` on `outcome`, where a pass that
    /// settled no copy failed, and reads back its status, target, reason, and security version.
    fn settled(
        status: OutdatedStatus,
        outcome: impl Into<Option<CopyOutcome>>,
    ) -> (
        OutdatedStatus,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        let mut item = adoptable("parent", "1.0.0", "1.4.0");
        item.status = status;
        item.security = Some(security("1.4.0"));
        let verdict = Verdict {
            status: match status {
                OutdatedStatus::Exempt => Status::Exempt,
                OutdatedStatus::Held => Status::Held,
                _ => Status::Adoptable,
            },
            adoptable_target: Some(Version::new("1.4.0")),
            latest: None,
            candidates: Vec::new(),
            held_reason: None,
        };
        let dep = Dependency {
            package: PackageId::new(ToolId("uv"), "parent", item.registry.clone()),
            advisory_identity: None,
            current: Version::new("1.0.0"),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
            hold_edges: Vec::new(),
        };
        let unsettled = CopyOutcome::Failed(Diagnostic::new(DiagnosticKind::ToolFailed, "crashed"));
        settle(&mut item, &dep, &verdict, outcome.into(), &unsettled);
        (
            item.status,
            item.adoptable_target,
            item.blocked_reason
                .or(item.error.map(|error| error.message)),
            item.security.map(|security| security.version),
        )
    }

    /// Each outcome the dry run settled maps onto the row: a landing or a refusal reports its own
    /// version and security evidence, a failure makes any row an error, and an undecided copy
    /// changes only a row with a matured target.
    #[test]
    fn settle_maps_each_outcome_onto_the_row() {
        let landed = || CopyOutcome::Landed {
            to: "1.3.0".to_string(),
            security: Some(security("1.3.0")),
        };
        let refused = || CopyOutcome::Refused {
            to: "1.4.0".to_string(),
            skip: SkippedInfo {
                reason: SkipReason::MultiVersionHeld,
                message: "a workspace split".to_string(),
                offending: Some("parent".to_string()),
            },
            security: None,
        };
        let failed = || CopyOutcome::Failed(Diagnostic::new(DiagnosticKind::ToolFailed, "crashed"));
        let some = |text: &str| Some(text.to_string());
        let kept = |status| (status, some("1.4.0"), None, some("1.4.0"));
        let cases = [
            (
                OutdatedStatus::Adoptable,
                landed(),
                (
                    OutdatedStatus::Adoptable,
                    some("1.3.0"),
                    None,
                    some("1.3.0"),
                ),
            ),
            (
                OutdatedStatus::Exempt,
                landed(),
                (OutdatedStatus::Exempt, some("1.3.0"), None, some("1.3.0")),
            ),
            (
                OutdatedStatus::Held,
                landed(),
                (
                    OutdatedStatus::Adoptable,
                    some("1.3.0"),
                    None,
                    some("1.3.0"),
                ),
            ),
            (
                OutdatedStatus::Adoptable,
                refused(),
                (
                    OutdatedStatus::Blocked,
                    some("1.4.0"),
                    some("a workspace split"),
                    some("1.4.0"),
                ),
            ),
            (
                OutdatedStatus::Held,
                refused(),
                (
                    OutdatedStatus::Blocked,
                    some("1.4.0"),
                    some("a workspace split"),
                    some("1.4.0"),
                ),
            ),
            (
                OutdatedStatus::Adoptable,
                failed(),
                (OutdatedStatus::Error, None, some("crashed"), some("1.4.0")),
            ),
            (
                OutdatedStatus::Held,
                failed(),
                (OutdatedStatus::Error, None, some("crashed"), some("1.4.0")),
            ),
            (
                OutdatedStatus::Adoptable,
                CopyOutcome::Undecided,
                (
                    OutdatedStatus::Blocked,
                    some("1.4.0"),
                    some(FOLLOWS_REQUIRERS),
                    some("1.4.0"),
                ),
            ),
            (
                OutdatedStatus::Held,
                CopyOutcome::Undecided,
                kept(OutdatedStatus::Held),
            ),
        ];
        for (status, outcome, expected) in cases {
            let case = format!("{status:?} {outcome:?}");
            assert_eq!(settled(status, outcome), expected, "{case}");
        }
    }

    /// A pass that settled no copy fails only the rows whose matured target it was to confirm.
    #[test]
    fn an_unsettled_pass_fails_only_the_targets_it_left_unconfirmed() {
        assert_eq!(
            settled(OutdatedStatus::Adoptable, None).0,
            OutdatedStatus::Error
        );
        assert_eq!(settled(OutdatedStatus::Held, None).0, OutdatedStatus::Held);
    }
}
