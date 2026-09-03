//! `check` — the CI gate. Fail-closed: a stale lock, a too-fresh non-baselined pin, or any error
//! attributable to a dependency you couldn't evaluate forces a non-zero exit. Evaluates the
//! resolved graph (direct + transitive) by default.

use super::advisories::{ClassifiedAdvisories, ProjectAdvisories};
use super::lock::ProjectAccessWriteGuard;
use super::{
    CheckItem, CheckMeta, CheckStatus, CheckSummary, Exit, FetchedRelease, LockReportAction,
    RunOpts, TransitiveGate, Window, Workspace, age_days, diag_from_error, lock_report_outcome,
    render_window,
};
use cooldown_core::{
    DepScope, Dependency, Diagnostic, DiagnosticKind, LockVerifyReport, Origin, Resolution,
    ResolveKind, ResolveQuery, Status, check_pin_advised, resolve,
};

/// If a `Native`-origin layer declared a STRICTER (larger) bare window than the one that won, the
/// repo/global policy has weakened the project's stated intent. Returns the native window's days.
fn stricter_native_days(res: &Resolution) -> Option<f64> {
    let applied = res
        .trace
        .iter()
        .find(|s| s.field == "default" && s.applied)
        .and_then(|s| s.min_age_days)?;
    let native_max = res
        .trace
        .iter()
        .filter(|s| s.layer == Origin::Native && s.field == "default")
        .filter_map(|s| s.min_age_days)
        .fold(None, |acc: Option<f64>, d| {
            Some(acc.map_or(d, |a| a.max(d)))
        });
    match native_max {
        Some(n) if n > applied + 1e-9 => Some(n),
        _ => None,
    }
}

/// The result of `check`: the gate verdict, the findings, and the exit code that encodes it.
pub struct CheckOutcome {
    /// The scope of the evaluation (graph vs direct-only, environment vs all artifacts).
    pub meta: CheckMeta,
    /// Per-status counts across all evaluated pins.
    pub summary: CheckSummary,
    /// The findings: violations, acknowledged pins, unknown-age pins, and per-dependency errors.
    pub items: Vec<CheckItem>,
    /// Non-fatal diagnostics (stale lock under `--allow-stale-lock`, yanked pins, stricter-native).
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors that abort evaluation of that project.
    pub errors: Vec<Diagnostic>,
    /// The gate verdict as a process exit; see [`Exit`].
    pub exit: Exit,
}

/// The mutable state accumulated while gating a run: the per-status tallies, the findings, and the
/// non-fatal diagnostics. Finalized into a [`CheckOutcome`].
#[derive(Default)]
struct CheckAccum {
    checked: usize,
    skipped_stale_projects: usize,
    /// Whether a project already explained an empty selection, so the run-level note is redundant.
    empty_selection_noted: bool,
    direct: usize,
    exempt: usize,
    acknowledged: usize,
    allowed: usize,
    unknown_age: usize,
    violations: usize,
    /// How many evaluated pins are security-relevant (the locked version is an advisory's fix).
    security_relevant: usize,
    /// Set when a stricter-native override tripped under `strict-native`.
    stricter_native_tripped: bool,
    items: Vec<CheckItem>,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
}

/// The outcome of the fail-closed lock-currency probe: continue evaluating, or skip this project.
enum LockProbe {
    /// The lock is current; continue dependency evaluation.
    Continue,
    /// The stale lock was accepted as a warning; do not inspect a graph Cargo would have to update.
    StaleAllowed,
    /// The lock could not be soundly evaluated; this project is skipped.
    Skip,
}

struct CheckRunner<'a> {
    ws: &'a Workspace,
    opts: &'a RunOpts,
    scope: DepScope,
    acc: CheckAccum,
}

impl Workspace {
    /// Gate the resolved graph: exit non-zero if anything is younger than its cooldown (the CI
    /// gate).
    ///
    /// Fail-closed: a stale lock, a per-dependency evaluation error, or (under
    /// `--fail-on-unknown-age`) an unknown publish time forces a non-zero [`Exit`]. Evaluates the
    /// full graph by default, or direct deps under `--transitive hide`.
    pub async fn check(&self, opts: &RunOpts) -> CheckOutcome {
        CheckRunner::new(self, opts).run().await
    }
}

impl<'a> CheckRunner<'a> {
    fn new(ws: &'a Workspace, opts: &'a RunOpts) -> Self {
        // `check --transitive hide` skips evaluating transitive deps; every other mode (including
        // `allow`) gates the full resolved graph.
        let scope = if opts.transitive_mode == TransitiveGate::Hide {
            DepScope::Direct
        } else {
            DepScope::Graph
        };
        CheckRunner {
            ws,
            opts,
            scope,
            acc: CheckAccum::default(),
        }
    }

    async fn run(mut self) -> CheckOutcome {
        for pctx in self.ws.scoped_projects(self.opts) {
            let _progress = self
                .opts
                .progress
                .project(pctx.tool, pctx.rel_path.as_str());
            self.run_project(pctx).await;
        }

        // Locked-release metadata is fetched concurrently (`buffer_unordered`); sort for a stable
        // report and `--json`, status-first so gate violations lead.
        self.acc.items.sort_by(|a, b| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.status.sort_rank().cmp(&b.status.sort_rank()))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.current.cmp(&b.current))
        });
        let err_count = self
            .acc
            .items
            .iter()
            .filter(|item| item.error.is_some())
            .count()
            + self.acc.errors.len();
        let summary = CheckSummary {
            checked: self.acc.checked,
            skipped_stale_projects: self.acc.skipped_stale_projects,
            direct: self.acc.direct,
            exempt: self.acc.exempt,
            acknowledged: self.acc.acknowledged,
            allowed: self.acc.allowed,
            unknown_age: self.acc.unknown_age,
            errors: err_count,
            violations: self.acc.violations,
            security_relevant: self.acc.security_relevant,
        };
        let meta = CheckMeta {
            scope: if self.scope == DepScope::Graph {
                "lockfile-graph".into()
            } else {
                "direct-only".into()
            },
            artifact_scope: if self.opts.all_artifacts {
                "all".into()
            } else {
                "environment".into()
            },
        };
        let exit = check_exit(&self.acc, err_count, self.opts);
        // An error already explains an empty run; otherwise a selection that evaluated nothing
        // must say so, or the gate passes on zero dependencies without a word.
        if err_count == 0
            && !self.acc.empty_selection_noted
            && let Some(note) = super::empty_selection_diagnostic(
                self.opts,
                self.acc.checked,
                self.acc.skipped_stale_projects,
            )
        {
            self.acc.warnings.push(note);
        }

        CheckOutcome {
            meta,
            summary,
            items: self.acc.items,
            warnings: self.acc.warnings,
            errors: self.acc.errors,
            exit,
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

    async fn run_project(&mut self, pctx: &'a super::ProjectCtx) {
        let Some(read) = self.ws.read_project_ctx(pctx, self.opts) else {
            return;
        };

        let (refreshed, refresh_guard) = self.refresh_lock(pctx, &read.project_label).await;
        if matches!(&refreshed, Some(LockProbe::Skip)) {
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
        let lock_probe = match refreshed {
            Some(probe) => probe,
            None => {
                self.probe_lock(read.adapter, pctx, &read.project_label)
                    .await
            }
        };
        if matches!(lock_probe, LockProbe::StaleAllowed) {
            self.acc.skipped_stale_projects += 1;
            return;
        }
        if matches!(lock_probe, LockProbe::Skip) {
            return;
        }

        let mut deps = match self
            .ws
            .dependencies_in_scope(read.adapter, pctx, self.scope, self.opts)
            .await
        {
            Ok(deps) => deps,
            Err(error) => {
                self.acc.errors.push(diag_from_error(
                    &error,
                    pctx.tool,
                    &read.project_label,
                    None,
                ));
                return;
            }
        };
        drop(read_guard);
        drop(refresh_guard);
        self.note_empty_project(pctx, deps.len());

        // Identities must be adapter-confirmed before they are queried, matched, or counted
        // (see `ToolRead::confirm_advisory_identities`) — and only when the feed will run.
        if super::advisories::advisory_fetch_policy(pctx).is_some() {
            read.adapter
                .confirm_advisory_identities(&pctx.project, &mut deps)
                .await;
        }
        let advisory_fetch = self.ws.fetch_project_advisories(
            read.adapter,
            pctx,
            &read.project_label,
            super::advisories::AdvisoryPackages::from_deps(&deps),
            self.opts,
        );
        self.opts
            .progress
            .phase(format!("checking {} resolved dependencies", deps.len()));
        let release_fetch = self.ws.fetch_locked_releases(
            read.adapter,
            deps,
            &read.fetch,
            &self.opts.progress,
            self.opts.fanout(),
        );
        let (advisory_fetch, fetched) = tokio::join!(advisory_fetch, release_fetch);
        let advisories = advisory_fetch.record(&mut self.acc.warnings, &mut self.acc.errors);
        for FetchedRelease {
            dependency: dep,
            result,
        } in fetched
        {
            self.gate_pin(pctx, &read, advisories.as_ref(), &dep, result);
        }
    }

    /// Run the fail-closed lock-currency probe for one project, recording any diagnostic.
    ///
    /// `--allow-stale-lock` downgrades a genuine stale/absent-lock result to a warning; a
    /// tool/transient failure stays fail-closed.
    async fn probe_lock(
        &mut self,
        adapter: &dyn cooldown_core::ToolRead,
        pctx: &super::ProjectCtx,
        project_label: &str,
    ) -> LockProbe {
        match adapter.verify_lock_current(&pctx.project).await {
            Ok(report) => self.handle_lock_report(report, pctx, project_label),
            Err(err) => self.handle_lock_error(&err, pctx, project_label),
        }
    }

    async fn refresh_lock(
        &mut self,
        pctx: &super::ProjectCtx,
        project_label: &str,
    ) -> (Option<LockProbe>, Option<ProjectAccessWriteGuard>) {
        match self.ws.refresh_project_lock(pctx, self.opts).await {
            Ok(refresh) => {
                self.acc.warnings.extend(refresh.warnings);
                let probe = match refresh.report {
                    Ok(report) => {
                        report.map(|report| self.handle_lock_report(report, pctx, project_label))
                    }
                    Err(error) => Some(self.handle_lock_error(&error, pctx, project_label)),
                };
                (probe, refresh.guard)
            }
            Err(err) => (
                Some(self.handle_lock_error(&err, pctx, project_label)),
                None,
            ),
        }
    }

    fn handle_lock_report(
        &mut self,
        report: LockVerifyReport,
        pctx: &super::ProjectCtx,
        project_label: &str,
    ) -> LockProbe {
        let stale = report.status == cooldown_core::LockStatus::Stale;
        let outcome = lock_report_outcome(
            report,
            pctx.tool,
            project_label,
            &pctx.project.manifest,
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
                    LockProbe::StaleAllowed
                } else {
                    LockProbe::Continue
                }
            }
            LockReportAction::Skip => {
                if let Some(diagnostic) = outcome.diagnostic {
                    self.acc.errors.push(diagnostic);
                }
                LockProbe::Skip
            }
        }
    }

    fn handle_lock_error(
        &mut self,
        err: &cooldown_core::CoreError,
        pctx: &super::ProjectCtx,
        project_label: &str,
    ) -> LockProbe {
        let diag = diag_from_error(err, pctx.tool, project_label, None)
            .with_path(pctx.project.manifest.as_str());
        let downgradable = matches!(
            diag.kind,
            DiagnosticKind::StaleLock | DiagnosticKind::NotFound
        );
        if self.opts.allow_stale_lock && downgradable {
            self.acc.warnings.push(stale_evaluation_skipped(diag));
            LockProbe::StaleAllowed
        } else {
            self.acc.errors.push(diag);
            LockProbe::Skip
        }
    }

    /// Evaluate one fetched pin: tally it, emit any finding, and surface yanked/stricter-native
    /// warnings.
    fn gate_pin(
        &mut self,
        pctx: &super::ProjectCtx,
        read: &super::read::ReadProjectCtx<'_>,
        advisories: Option<&ProjectAdvisories>,
        dep: &Dependency,
        result: cooldown_core::Result<cooldown_core::Release>,
    ) {
        let project_label = read.project_label.as_str();
        self.acc.checked += 1;
        if dep.direct {
            self.acc.direct += 1;
        }
        let locked = match result {
            Ok(l) => l,
            Err(e) => {
                // A failure attributable to one dependency → an item with status:"error".
                let diag = diag_from_error(&e, pctx.tool, project_label, Some(&dep.package.name));
                self.acc
                    .items
                    .push(error_item(dep, project_label, pctx.tool.as_str(), diag));
                return;
            }
        };
        if locked.yanked {
            self.acc.warnings.push(
                Diagnostic::new(DiagnosticKind::Yanked, "locked version is yanked")
                    .with_tool(pctx.tool.as_str())
                    .with_project(project_label)
                    .with_package(&dep.package.name)
                    .with_version(dep.current.as_str()),
            );
        }

        // Classify against the locked release alone: the pin-side advisory test is the exact
        // "is this version a fix?" match, which needs no candidate ordering.
        let advised = advisories
            .and_then(|project| project.classify(read.adapter, dep, std::slice::from_ref(&locked)));
        let advisory_ctx = advised.as_ref().map(ClassifiedAdvisories::context);
        let pv = check_pin_advised(
            dep,
            &locked,
            advisory_ctx.as_ref(),
            &pctx.policy.layers,
            &read.resolve,
            self.ws.now(),
        );
        if pv.security.is_some() {
            self.acc.security_relevant += 1;
        }
        if let Some(diag) = self.stricter_native_warning(pctx, project_label, dep) {
            self.acc.warnings.push(diag);
            if pctx.policy.strict_native {
                self.acc.stricter_native_tripped = true;
            }
        }

        if pv.status == Status::Exempt {
            self.acc.exempt += 1;
            return;
        }

        let baseline_acked = pv.status == Status::CurrentInCooldown
            && self.ws.baseline.is_acknowledged(
                pctx.tool.as_str(),
                project_label,
                &dep.package.name,
                dep.current.as_str(),
                dep.package.registry.as_deref(),
                self.ws.now(),
            );
        // `check --transitive allow` permits a too-fresh *transitive* dep without failing the gate;
        // it is reported but non-fatal, with the distinct `Allowed` status so it stays auditable
        // apart from a baselined acknowledgment. A per-version baseline record wins over the blanket
        // policy.
        let allowed_transitive = self.opts.transitive_mode == TransitiveGate::Allow && !dep.direct;
        let status = match CheckStatus::from_pin_status(pv.status, baseline_acked) {
            Some(CheckStatus::Violation) if allowed_transitive => Some(CheckStatus::Allowed),
            other => other,
        };

        let Some(status) = status else {
            return; // mature pass → counted in `checked`, not a finding
        };

        match status {
            CheckStatus::Violation => self.acc.violations += 1,
            CheckStatus::Acknowledged => self.acc.acknowledged += 1,
            CheckStatus::Allowed => self.acc.allowed += 1,
            CheckStatus::UnknownAge => self.acc.unknown_age += 1,
            CheckStatus::Error => {}
        }

        self.acc.items.push(CheckItem {
            name: dep.package.name.clone(),
            tool: pctx.tool.as_str().to_string(),
            project: project_label.to_string(),
            members: dep.members.clone(),
            registry: dep.package.registry.clone(),
            direct: dep.direct,
            current: dep.current.to_string(),
            published_at: pv.published_at.map(|p| p.to_string()),
            age_days: pv.published_at.map(|p| age_days(p, self.ws.now())),
            window: render_window(&pv.window, self.ws.now()),
            status,
            graph_held: pv.graph_held,
            graph_floor: pv.graph_floor.map(|v| v.to_string()),
            security: pv
                .security
                .as_ref()
                .map(|security| super::security_info(security, &dep.current)),
            error: None,
        });
    }

    /// The stricter-native diagnostic for a pin, when repo/global policy overrides a stricter
    /// declared native window. Returns `None` when no native layer is stricter.
    fn stricter_native_warning(
        &self,
        pctx: &super::ProjectCtx,
        project_label: &str,
        dep: &Dependency,
    ) -> Option<Diagnostic> {
        let q = ResolveQuery {
            tool: pctx.tool,
            package: &dep.package.name,
            registry: dep.package.registry.as_deref(),
            project: &pctx.rel_path,
            kind: ResolveKind::CurrentPin,
        };
        let res = resolve(&pctx.policy.layers, &q, self.ws.now());
        let native_days = stricter_native_days(&res)?;
        Some(
            Diagnostic::new(
                DiagnosticKind::StricterNative,
                format!(
                    "repo/global policy ({:.0}d) overrides a stricter native min-age ({:.0}d)",
                    res.window.effective_min_age_days(self.ws.now()),
                    native_days
                ),
            )
            .with_tool(pctx.tool.as_str())
            .with_project(project_label)
            .with_package(&dep.package.name),
        )
    }
}

fn stale_evaluation_skipped(mut diagnostic: Diagnostic) -> Diagnostic {
    diagnostic
        .message
        .push_str("; dependency evaluation was skipped for this project");
    diagnostic
}

/// Map the tallies to the fail-closed exit code: errors/unknown-age first, then a tripped
/// stricter-native gate, then any violation.
fn check_exit(acc: &CheckAccum, err_count: usize, opts: &RunOpts) -> Exit {
    let unknown_fail = opts.fail_on_unknown_age && acc.unknown_age > 0;
    if err_count > 0 || unknown_fail {
        Exit::Environment
    } else if acc.stricter_native_tripped {
        Exit::Usage // --fail-on-stricter-native / strict-native tripped
    } else if acc.violations > 0 {
        Exit::Policy
    } else {
        Exit::Ok
    }
}

fn error_item(dep: &Dependency, project: &str, tool: &str, diag: Diagnostic) -> CheckItem {
    CheckItem {
        name: dep.package.name.clone(),
        tool: tool.to_string(),
        project: project.to_string(),
        members: dep.members.clone(),
        registry: dep.package.registry.clone(),
        direct: dep.direct,
        current: dep.current.to_string(),
        published_at: None,
        age_days: None,
        window: Window {
            min_age_days: 0.0,
            source: "n/a".into(),
            clamped_by: None,
            shortened_by: None,
        },
        status: CheckStatus::Error,
        graph_held: false,
        graph_floor: None,
        security: None,
        error: Some(diag),
    }
}
