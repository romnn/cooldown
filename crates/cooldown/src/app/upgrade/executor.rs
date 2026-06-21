use super::{UpgradeAccum, UpgradeCtx};
use crate::app::lock::ProjectLock;
use crate::app::{SkippedInfo, TransitiveGate, UpgradeItem, Workspace, diag_from_error};
use cooldown_core::{
    Change, DepScope, Dependency, Diagnostic, DiagnosticKind, MajorKey, PackageId, Plan,
    SkipReason, Status, UpdateKind, check_pin, evaluate, evaluate_fix,
};
use std::collections::{HashMap, HashSet};

/// Whether the executor moves dependencies *forward* (`upgrade`) or *backward* to a compliant
/// version (`fix`). The trial/rollback/verify machinery is shared; only planning differs.
#[derive(Clone, Copy)]
pub(super) enum PlanMode {
    /// `upgrade`: move direct deps to the newest matured version.
    Upgrade,
    /// `fix`: downgrade deps whose locked version is too fresh to the newest matured older version.
    Fix {
        /// How too-fresh transitive deps are handled (`--transitive <mode>`): `Enforce` downgrades
        /// them too, `Allow` reports but leaves them, `Hide` skips them entirely (direct-only).
        transitive: TransitiveGate,
        /// Downgrade and rewrite exact-pinned deps too (`--downgrade-pinned`); otherwise a pinned
        /// violation is left in place with a warning.
        downgrade_pinned: bool,
    },
}

/// The evolving per-project state during one-change upgrade trials.
struct TrialState {
    /// In-cooldown, non-baselined pins present before the next trial.
    baseline_violations: HashSet<(String, String)>,
}

/// The cohesive per-project upgrade state machine: dependency discovery, planning, one-change
/// trials, rollback, and final verification.
pub(super) struct ProjectUpgradeExecutor<'a, 'b> {
    ws: &'a Workspace,
    ctx: UpgradeCtx<'b>,
    project_label: String,
    mode: PlanMode,
    acc: &'a mut UpgradeAccum,
}

impl<'a, 'b> ProjectUpgradeExecutor<'a, 'b> {
    pub(super) fn new(
        ws: &'a Workspace,
        ctx: UpgradeCtx<'b>,
        mode: PlanMode,
        acc: &'a mut UpgradeAccum,
    ) -> Self {
        ProjectUpgradeExecutor {
            ws,
            project_label: ctx.pctx.rel_path.to_string(),
            mode,
            ctx,
            acc,
        }
    }

    pub(super) async fn run(&mut self) {
        let Some(deps) = self.scoped_deps().await else {
            return;
        };
        let verb = match self.mode {
            PlanMode::Upgrade => "upgrades",
            PlanMode::Fix { .. } => "downgrades",
        };
        self.ctx.opts.progress.say(&format!(
            "Planning {verb} for {} dependencies in {} ({})…",
            deps.len(),
            self.project_label(),
            self.ctx.pctx.tool
        ));
        let planned = self.plan_changes(&deps).await;

        if self.ctx.opts.dry_run {
            self.record_dry_run(&planned);
            return;
        }

        let _guard = match ProjectLock::acquire(&self.ctx.pctx.project.root) {
            Ok(guard) => guard,
            Err(error) => {
                self.record_project_error(&error, None);
                return;
            }
        };

        let baseline_violations = match self.graph_violations().await {
            Ok(violations) => violations.into_keys().collect(),
            Err(error) => {
                self.record_project_error(&error, None);
                return;
            }
        };

        let mut state = TrialState {
            baseline_violations,
        };
        let applied_before = self.applied_count();
        let errored_before = self.errored_count();
        for change in planned {
            if !self.apply_change(change, &mut state).await {
                break;
            }
        }

        // `upgrade` (default mode) reconciles the graph it just re-locked: downgrade any too-fresh
        // transitive a forward move floated up, so the new lock is gate-clean in one pass — no
        // separate `fix` needed. `--transitive allow`/`hide` opt out (the floated-up transitive was
        // already kept-and-reported / ignored in `apply_change`). Skip it when the upgrade made no
        // clean forward progress: nothing floated up, and a broken re-lock probe must not be re-hit.
        let upgraded_cleanly =
            self.applied_count() > applied_before && self.errored_count() == errored_before;
        if matches!(self.mode, PlanMode::Upgrade)
            && self.transitive_mode() == TransitiveGate::Enforce
            && upgraded_cleanly
        {
            self.reconcile_graph(&mut state).await;
        }

        self.finalize().await;
    }

    async fn scoped_deps(&mut self) -> Option<Vec<Dependency>> {
        // `fix` evaluates the whole resolved graph unless transitives are hidden; `upgrade` plans
        // forward moves on direct deps only (its graph reconciliation is a separate post-lock pass).
        let scope = match self.mode {
            PlanMode::Fix { transitive, .. } if transitive != TransitiveGate::Hide => {
                DepScope::Graph
            }
            _ => DepScope::Direct,
        };
        match self
            .ws
            .dependencies_in_scope(self.ctx.reader, self.ctx.pctx, scope, self.ctx.opts)
            .await
        {
            Ok(deps) => Some(deps),
            Err(error) => {
                self.record_project_error(&error, None);
                None
            }
        }
    }

    async fn plan_changes(&mut self, deps: &[Dependency]) -> Vec<Change> {
        match self.mode {
            PlanMode::Upgrade => self.plan_upgrade_changes(deps).await,
            PlanMode::Fix {
                transitive,
                downgrade_pinned,
            } => {
                self.plan_fix_changes(deps, transitive, downgrade_pinned)
                    .await
            }
        }
    }

    async fn plan_upgrade_changes(&mut self, deps: &[Dependency]) -> Vec<Change> {
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
        let fetched = self
            .ws
            .fetch_candidate_releases(
                self.ctx.reader,
                deps.to_vec(),
                &fctx,
                self.ctx.opts.candidate_scope(),
                self.ctx.opts.fanout(),
            )
            .await;
        let mut planned = Vec::new();
        for (dep, releases) in fetched {
            let releases = match releases {
                Ok(releases) => releases,
                Err(error) => {
                    self.record_project_error(&error, Some(&dep.package.name));
                    continue;
                }
            };
            let verdict = evaluate(
                &dep,
                &releases,
                &self.ctx.pctx.policy.layers,
                &rctx,
                self.ws.now(),
            );
            // A held dep (exact pin or commit pin) carries an `adoptable_target` for the report — the
            // version a human could manually pin to — but `upgrade` must never move it on its own.
            if verdict.status == cooldown_core::Status::Held {
                continue;
            }
            let Some(target) = verdict.adoptable_target else {
                continue;
            };
            if target == dep.current {
                continue;
            }
            let kind = verdict
                .candidates
                .iter()
                .find(|candidate| candidate.version == target)
                .map_or(cooldown_core::UpdateKind::Minor, |candidate| candidate.kind);
            let current_major = releases
                .iter()
                .find(|release| release.version == dep.current)
                .map_or(MajorKey(String::new()), |release| release.major.clone());
            let target_major = releases
                .iter()
                .find(|release| release.version == target)
                .map(|release| release.major.clone())
                .unwrap_or(current_major.clone());
            let package = target_package(&dep, &current_major, &target_major);
            planned.push(Change {
                package,
                from: dep.current.clone(),
                to: target,
                kind,
                members: dep.members.clone(),
            });
        }
        planned
    }

    /// Plan downgrades for `fix`: every dependency whose locked version is too fresh moves to the
    /// newest matured version older than it. A pin is left in place with a warning unless
    /// `downgrade_pinned`; a violation with no matured older version is reported as a warning too.
    async fn plan_fix_changes(
        &mut self,
        deps: &[Dependency],
        transitive: TransitiveGate,
        downgrade_pinned: bool,
    ) -> Vec<Change> {
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
        let fetched = self
            .ws
            .fetch_candidate_releases(
                self.ctx.reader,
                deps.to_vec(),
                &fctx,
                self.ctx.opts.candidate_scope(),
                self.ctx.opts.fanout(),
            )
            .await;
        let mut planned = Vec::new();
        for (dep, releases) in fetched {
            let releases = match releases {
                Ok(releases) => releases,
                Err(error) => {
                    self.record_project_error(&error, Some(&dep.package.name));
                    continue;
                }
            };
            let fix = evaluate_fix(
                &dep,
                &releases,
                &self.ctx.pctx.policy.layers,
                &rctx,
                self.ws.now(),
            );
            // Only a too-fresh pin needs fixing; a compliant (or exempt / unknown-age) dep is left
            // alone, so `fix` only ever touches what `check` would reject.
            if fix.current.status != Status::CurrentInCooldown {
                continue;
            }
            // `--transitive allow`: leave a too-fresh transitive in place (still reported), only
            // downgrade direct deps. `hide` never reaches here — transitives aren't in scope.
            if transitive == TransitiveGate::Allow && !dep.direct {
                self.record_fix_warning(&format!(
                    "{}@{} is younger than its cooldown; left in place by --transitive allow",
                    dep.package.name, dep.current
                ), &dep.package.name);
                continue;
            }
            if fix.current.graph_held {
                self.record_fix_warning(&format!(
                    "{}@{} is younger than its cooldown, but the resolved graph requires that version; baseline it or relax the dependency forcing it",
                    dep.package.name, dep.current
                ), &dep.package.name);
                continue;
            }
            // An exact pin is a deliberate choice: warn and leave it unless `--downgrade-pinned`.
            if dep.pinned && !downgrade_pinned {
                self.record_fix_warning(&format!(
                    "{}@{} is pinned and younger than its cooldown; downgrade it manually or rerun with --downgrade-pinned",
                    dep.package.name, dep.current
                ), &dep.package.name);
                continue;
            }
            let Some(target) = fix.target else {
                self.record_fix_warning(&format!(
                    "{}@{} is younger than its cooldown and no older version has matured; `baseline` it or wait",
                    dep.package.name, dep.current
                ), &dep.package.name);
                continue;
            };
            let kind = releases
                .iter()
                .find(|release| release.version == target)
                .and_then(|release| release.kind_from_current)
                .unwrap_or(UpdateKind::Minor);
            planned.push(Change {
                package: dep.package.clone(),
                from: dep.current.clone(),
                to: target,
                kind,
                members: dep.members.clone(),
            });
        }
        planned
    }

    fn record_fix_warning(&mut self, message: &str, package: &str) {
        let diag = Diagnostic::new(DiagnosticKind::Held, message.to_string())
            .with_tool(self.ctx.tool_name())
            .with_project(self.project_label.clone())
            .with_package(package);
        self.acc.strict_incomplete = true;
        self.acc.warnings.push(diag);
    }

    fn record_dry_run(&mut self, planned: &[Change]) {
        for change in planned {
            self.acc.items.push(plan_item(
                change,
                self.project_label(),
                self.ctx.tool_name(),
                false,
                None,
            ));
        }
    }

    async fn apply_change(&mut self, change: Change, state: &mut TrialState) -> bool {
        let plan = Plan {
            changes: vec![change.clone()],
            rewrite: self.ctx.opts.rewrite,
        };
        let journal = match self
            .ctx
            .writer
            .mutation_journal(&self.ctx.pctx.project, &plan)
            .await
        {
            Ok(journal) => journal,
            Err(error) => {
                self.record_project_error(&error, Some(&change.package.name));
                return false;
            }
        };

        let report = match self
            .ctx
            .writer
            .apply(&self.ctx.pctx.project, &plan, &journal)
            .await
        {
            Ok(report) => report,
            Err(error) => {
                return self.restore_with_change_error(&journal, &change, &error);
            }
        };

        if report.applied.is_empty() {
            let restored = self.restore_journal(&journal);
            self.acc.strict_incomplete = true;
            self.record_change_skip(
                &change,
                report
                    .skipped
                    .into_iter()
                    .next()
                    .map(|skipped| SkippedInfo {
                        reason: skipped.reason,
                        message: skipped.reason.message().to_string(),
                        offending: skipped.offending.map(|package| package.name),
                    }),
            );
            return restored;
        }

        let after = match self.graph_violations().await {
            Ok(after) => after,
            Err(error) => {
                return self.restore_with_change_error(&journal, &change, &error);
            }
        };
        let after_keys: HashSet<(String, String)> = after.keys().cloned().collect();
        let new_violations: Vec<&(String, String)> =
            after_keys.difference(&state.baseline_violations).collect();
        // A change that drags a fresh transitive into the graph. How we react follows the transitive
        // mode: `Hide` ignores transitives; `Allow` keeps the change and reports them; `Enforce`
        // (default) keeps it only when every new violation is *reconcilable* (the reconcile pass can
        // downgrade it later) and rolls back when the upgrade *forces* a fresh, irreducible dep —
        // adopting a version that requires a too-fresh transitive defeats the cooldown.
        if !new_violations.is_empty() {
            match self.transitive_mode() {
                TransitiveGate::Hide => {}
                TransitiveGate::Allow => {
                    for (package, version) in &new_violations {
                        self.record_fix_warning(
                            &format!(
                                "{package}@{version} is younger than its cooldown; left in place by --transitive allow"
                            ),
                            package,
                        );
                    }
                }
                TransitiveGate::Enforce => {
                    if let Some((forced_pkg, _)) = new_violations
                        .iter()
                        .find(|key| !after.get(**key).copied().unwrap_or(false))
                    {
                        let restored = self.restore_journal(&journal);
                        self.acc.strict_incomplete = true;
                        self.record_change_skip(
                            &change,
                            Some(SkippedInfo {
                                reason: SkipReason::TransitiveInCooldown,
                                message: SkipReason::TransitiveInCooldown.message().to_string(),
                                offending: Some(forced_pkg.clone()),
                            }),
                        );
                        return restored;
                    }
                    // Every new violation is reconcilable; keep the change and let `reconcile_graph`
                    // (after the upgrade loop) downgrade the floated-up transitives.
                }
            }
        }

        state.baseline_violations = after_keys;
        self.record_change_applied(&change);
        true
    }

    fn transitive_mode(&self) -> TransitiveGate {
        self.ctx.opts.transitive_mode
    }

    /// Downgrade any too-fresh, reconcilable transitive the forward moves floated up, so `upgrade`
    /// leaves the same gate-clean lock a follow-up `fix` would. Reuses the per-change trial
    /// (`apply_change`), so each downgrade is applied, re-locked, and verified like any other.
    async fn reconcile_graph(&mut self, state: &mut TrialState) {
        let Some(deps) = self.reconcile_deps().await else {
            return;
        };
        let downgrades = self
            .plan_fix_changes(&deps, TransitiveGate::Enforce, false)
            .await;
        for change in downgrades {
            if !self.apply_change(change, state).await {
                break;
            }
        }
    }

    async fn reconcile_deps(&mut self) -> Option<Vec<Dependency>> {
        match self
            .ws
            .dependencies_in_scope(
                self.ctx.reader,
                self.ctx.pctx,
                DepScope::Graph,
                self.ctx.opts,
            )
            .await
        {
            Ok(deps) => Some(deps),
            Err(error) => {
                self.record_project_error(&error, None);
                None
            }
        }
    }

    fn restore_journal(&mut self, journal: &cooldown_core::ProjectMutationJournal) -> bool {
        match journal.restore(&self.ctx.pctx.project.root) {
            Ok(()) => true,
            Err(error) => {
                self.record_project_error(&error, None);
                false
            }
        }
    }

    fn restore_with_change_error(
        &mut self,
        journal: &cooldown_core::ProjectMutationJournal,
        change: &Change,
        error: &cooldown_core::CoreError,
    ) -> bool {
        let restored = self.restore_journal(journal);
        let diag = diag_from_error(
            error,
            self.ctx.pctx.tool,
            self.project_label(),
            Some(&change.package.name),
        );
        self.record_change_error(change, diag);
        restored
    }

    async fn finalize(&mut self) {
        match self
            .ctx
            .reader
            .verify_lock_current(&self.ctx.pctx.project)
            .await
        {
            Ok(report) => {
                self.acc.lock_verified = Some(self.acc.lock_verified.unwrap_or(true) && report.ok);
                if !report.ok {
                    self.acc.errors.push(
                        Diagnostic::new(DiagnosticKind::StaleLock, report.detail)
                            .with_tool(self.ctx.tool_name())
                            .with_project(self.project_label())
                            .with_path(self.ctx.pctx.project.manifest.as_str()),
                    );
                }
            }
            Err(error) => {
                self.acc.lock_verified = Some(false);
                self.record_project_error(&error, None);
            }
        }

        if self.ctx.opts.build {
            self.acc.build_requested = true;
            match self.ctx.writer.build(&self.ctx.pctx.project).await {
                Ok(report) => {
                    self.acc.build_ok = Some(self.acc.build_ok.unwrap_or(true) && report.ok);
                    if !report.ok {
                        self.acc.errors.push(
                            Diagnostic::new(DiagnosticKind::ToolFailed, report.detail)
                                .with_tool(self.ctx.tool_name())
                                .with_project(self.project_label()),
                        );
                    }
                }
                Err(error) => {
                    self.acc.build_ok = Some(false);
                    self.record_project_error(&error, None);
                }
            }
        }
    }

    /// The graph's too-fresh, non-baselined violations, each mapped to whether it is *reconcilable* —
    /// the graph floor sits below the locked version, so a `fix` downgrade could roll it back. A
    /// violation the graph pins at the fresh version (floor equals current, or no floor is known) is
    /// not reconcilable: nothing lower satisfies its requirers.
    async fn graph_violations(&self) -> cooldown_core::Result<HashMap<(String, String), bool>> {
        // Intentionally the raw, unscoped graph (not `dependencies_in_scope`): a graph-level cooldown
        // violation counts no matter which member pulls the offending version, so `exclude`/`-p`
        // must not narrow it. Only pin ages are read here — never `members` — so nothing leaks.
        let deps = self
            .ctx
            .reader
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await?;
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
        let fetched = self
            .ws
            .fetch_locked_releases(self.ctx.reader, deps, &fctx, self.ctx.opts.fanout())
            .await;

        let mut violations = HashMap::new();
        for (dep, result) in fetched {
            let locked = result?;
            let pin = check_pin(
                &dep,
                &locked,
                &self.ctx.pctx.policy.layers,
                &rctx,
                self.ws.now(),
            );
            if pin.status == Status::CurrentInCooldown {
                let acknowledged = self.ws.baseline.is_acknowledged(
                    self.ctx.tool_name(),
                    self.project_label(),
                    &dep.package.name,
                    dep.current.as_str(),
                    dep.package.registry.as_deref(),
                    self.ws.now(),
                );
                if !acknowledged {
                    let reconcilable = dep
                        .graph_floor
                        .as_ref()
                        .is_some_and(|floor| *floor != dep.current);
                    violations.insert(
                        (dep.package.name.clone(), dep.current.to_string()),
                        reconcilable,
                    );
                }
            }
        }
        Ok(violations)
    }

    fn project_label(&self) -> &str {
        &self.project_label
    }

    fn record_project_error(&mut self, error: &cooldown_core::CoreError, package: Option<&str>) {
        self.acc.errors.push(diag_from_error(
            error,
            self.ctx.pctx.tool,
            self.project_label(),
            package,
        ));
    }

    fn record_change_applied(&mut self, change: &Change) {
        let project_label = self.project_label.clone();
        let tool = self.ctx.tool_name();
        self.acc
            .items
            .push(plan_item(change, &project_label, tool, true, None));
    }

    /// Changes applied so far this run (across projects) — used as a before/after delta to detect
    /// whether the current project's upgrade loop made forward progress worth reconciling.
    fn applied_count(&self) -> usize {
        self.acc.items.iter().filter(|item| item.applied).count()
    }

    /// Errors recorded so far this run (project-level plus per-change) — the before/after delta tells
    /// `reconcile_graph` whether the upgrade loop hit a failure it must not re-trigger.
    fn errored_count(&self) -> usize {
        self.acc.errors.len() + self.acc.items.iter().filter(|item| item.error.is_some()).count()
    }

    fn record_change_error(&mut self, change: &Change, diag: Diagnostic) {
        let project_label = self.project_label.clone();
        let tool = self.ctx.tool_name();
        record_error_item(self.acc, change, &project_label, tool, diag);
    }

    fn record_change_skip(&mut self, change: &Change, skipped: Option<SkippedInfo>) {
        let project_label = self.project_label.clone();
        let tool = self.ctx.tool_name();
        record_skip_item(self.acc, change, &project_label, tool, skipped);
    }
}

/// Reconstruct the target `PackageId`, handling Go-style `/vN` path-major changes (the `MajorKey`
/// is a path suffix). For tools where the package name is stable across majors, the name is kept.
fn target_package(
    dep: &Dependency,
    current_major: &MajorKey,
    target_major: &MajorKey,
) -> PackageId {
    let suffix = &target_major.0;
    let name = if current_major.0 != target_major.0
        && (suffix.starts_with('/') || suffix.starts_with('.'))
    {
        let prefix = dep
            .package
            .name
            .strip_suffix(&current_major.0)
            .unwrap_or(&dep.package.name);
        format!("{prefix}{suffix}")
    } else {
        dep.package.name.clone()
    };
    PackageId::new(dep.package.tool, name, dep.package.registry.clone())
}

fn plan_item(
    change: &Change,
    project: &str,
    tool: &str,
    applied: bool,
    skipped: Option<SkippedInfo>,
) -> UpgradeItem {
    UpgradeItem {
        name: change.package.name.clone(),
        tool: tool.to_string(),
        project: project.to_string(),
        members: change.members.clone(),
        registry: change.package.registry.clone(),
        from: change.from.to_string(),
        to: change.to.to_string(),
        kind: change.kind,
        applied,
        skipped,
        error: None,
    }
}

fn record_error_item(
    acc: &mut UpgradeAccum,
    change: &Change,
    project: &str,
    tool: &str,
    diag: Diagnostic,
) {
    let mut item = plan_item(change, project, tool, false, None);
    item.error = Some(diag);
    acc.items.push(item);
}

fn record_skip_item(
    acc: &mut UpgradeAccum,
    change: &Change,
    project: &str,
    tool: &str,
    skipped: Option<SkippedInfo>,
) {
    acc.items
        .push(plan_item(change, project, tool, false, skipped));
}
