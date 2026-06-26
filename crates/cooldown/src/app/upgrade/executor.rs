use super::{UpgradeAccum, UpgradeCtx};
use crate::app::lock::ProjectLock;
use crate::app::{SkippedInfo, TransitiveGate, UpgradeItem, Workspace, diag_from_error};
use cooldown_core::{
    ApplyReport, Change, DepScope, Dependency, Diagnostic, DiagnosticKind, LockStatus, MajorKey,
    PackageId, Plan, Release, ResolveContext, SkipReason, Skipped, Status, UpdateKind, Version,
    check_pin, evaluate, evaluate_fix,
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

/// The dependency scope the planner hands to the resolver as upgrade/downgrade *candidates*.
///
/// `upgrade` actively moves only DIRECT requires forward; the resolver promotes indirect deps as a
/// consequence of those bumps (surfaced as collateral applied rows), so handing indirect deps to the
/// resolver as candidates only produces attempt-and-reject noise for floors nothing direct raises.
/// `fix` instead walks the resolved graph to downgrade too-fresh transitives, so it stays
/// graph-scoped unless `--transitive hide` narrows it to direct. The window is still enforced on
/// indirect deps regardless: `graph_violations` and `reconcile_to_fixpoint` read the raw unscoped
/// graph and roll back any too-fresh transitive that floats up.
fn candidate_scope(mode: PlanMode) -> DepScope {
    match mode {
        PlanMode::Upgrade
        | PlanMode::Fix {
            transitive: TransitiveGate::Hide,
            ..
        } => DepScope::Direct,
        PlanMode::Fix { .. } => DepScope::Graph,
    }
}

/// Backstop on the `fix`/reconcile fixpoint loop: a downgrade can lower another dep's floor and
/// make it newly fixable (an umbrella module freeing its submodules), so planning re-runs after each
/// round until nothing new is planned. Real graphs converge in a few rounds; this only guards a
/// pathological cycle from looping forever.
const MAX_FIX_ROUNDS: usize = 12;

/// A `fix` downgrade that could not be planned, deferred so the caller emits it only once the
/// fixpoint settles — a dep held in an early round may become fixable in a later one, so its warning
/// would be stale if emitted eagerly.
struct FixWarning {
    package: String,
    message: String,
}

/// One round of `fix` planning: the downgrades to apply and the unfixable violations to report.
struct FixPlan {
    changes: Vec<Change>,
    warnings: Vec<FixWarning>,
}

type ChangeTargetKey = (String, Option<String>, String);

/// The evolving per-project state during one-change upgrade trials.
struct TrialState {
    /// In-cooldown, non-baselined pins present before the next trial.
    baseline_violations: HashSet<(String, String)>,
    /// Whether the last committed batch introduced reconcilable transitive cooldown violations.
    reconcile_needed: bool,
}

/// The cohesive per-project upgrade state machine: dependency discovery, planning, one-change
/// trials, rollback, and final verification.
pub(super) struct ProjectUpgradeExecutor<'a, 'b> {
    ws: &'a Workspace,
    ctx: UpgradeCtx<'b>,
    project_label: String,
    mode: PlanMode,
    acc: &'a mut UpgradeAccum,
    lock_refreshed_by_apply: bool,
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
            lock_refreshed_by_apply: false,
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

        let _guard = match ProjectLock::acquire(&self.ctx.pctx.project.root) {
            Ok(guard) => guard,
            Err(error) => {
                self.record_project_error(&error, None);
                return;
            }
        };

        self.ctx.opts.progress.say(&format!(
            "Checking current resolved graph cooldown in {} ({})…",
            self.project_label(),
            self.ctx.pctx.tool
        ));
        let baseline_violations = match self.graph_violations().await {
            Ok(violations) => violations.into_keys().collect(),
            Err(error) => {
                self.record_project_error(&error, None);
                return;
            }
        };

        let mut state = TrialState {
            baseline_violations,
            reconcile_needed: false,
        };
        match self.mode {
            PlanMode::Upgrade => self.run_upgrade(deps, &mut state).await,
            PlanMode::Fix {
                transitive,
                downgrade_pinned,
            } => {
                self.fix_to_fixpoint(deps, transitive, downgrade_pinned, &mut state)
                    .await;
            }
        }

        self.finalize().await;
    }

    /// Apply the forward moves, then (under the default transitive mode) reconcile the graph the
    /// re-lock produced: downgrade any too-fresh transitive a forward move floated up, so a single
    /// `upgrade` ends gate-clean — no separate `fix` needed.
    async fn run_upgrade(&mut self, deps: Vec<Dependency>, state: &mut TrialState) {
        let applied_before = self.applied_count();
        let errored_before = self.errored_count();
        let planned = self.plan_upgrade_changes(&deps).await;
        self.apply_batch(planned, state).await;
        // Skip reconciliation when the upgrade made no clean forward progress: nothing floated up,
        // and a broken re-lock probe must not be re-hit.
        let upgraded_cleanly =
            self.applied_count() > applied_before && self.errored_count() == errored_before;
        if self.transitive_mode() == TransitiveGate::Enforce
            && upgraded_cleanly
            && state.reconcile_needed
        {
            self.reconcile_to_fixpoint(state).await;
        }
    }

    async fn scoped_deps(&mut self) -> Option<Vec<Dependency>> {
        let scope = candidate_scope(self.mode);
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

    async fn plan_upgrade_changes(&mut self, deps: &[Dependency]) -> Vec<Change> {
        self.ctx.opts.progress.say(&format!(
            "Fetching release metadata for {} upgrade candidate(s) in {} ({})…",
            deps.len(),
            self.project_label(),
            self.ctx.pctx.tool
        ));
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
                &dep_resolve_ctx(&rctx, &dep),
                self.ws.now(),
            );
            // Surface an adoptable cross-major update the user could take with `--major` (it would
            // otherwise vanish from a default run even though `outdated` lists it).
            self.record_held_back_major(&dep, &releases, &rctx, verdict.adoptable_target.as_ref());
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
            let package = target_package_for(&releases, &dep, &target);
            // Whether this move is a rollback. The forward planner only adopts a strictly newer
            // matured version (`evaluate` filters to `order > current`), so this is currently always
            // false — a too-fresh pin is rolled back by the fix/reconcile pass instead, which flags it
            // directly. Computed rather than hardcoded so the label stays correct if that ever changes.
            let downgrade = is_downgrade(&releases, &dep.current, &target);
            planned.push(Change {
                package,
                from: dep.current.clone(),
                to: target,
                kind,
                downgrade,
                direct: dep.direct,
                members: dep.members.clone(),
            });
        }
        planned
    }

    /// On a default (major-off) run, record an adoptable cross-major update as a `needs --major`
    /// skip — only for a directly-declared, non-pinned dep where re-running with `--major` would
    /// actually adopt it. `scoped_target` is the major-off run's own adoptable target, so a
    /// coincident in-range adoptable is not re-flagged as a major.
    fn record_held_back_major(
        &mut self,
        dep: &Dependency,
        releases: &[Release],
        rctx: &ResolveContext,
        scoped_target: Option<&Version>,
    ) {
        if self.ctx.opts.allow_major || !dep.direct || dep.pinned {
            return;
        }
        let major_rctx = ResolveContext {
            allow_major: true,
            ..*rctx
        };
        let major = evaluate(
            dep,
            releases,
            &self.ctx.pctx.policy.layers,
            &major_rctx,
            self.ws.now(),
        );
        let Some(major_target) = major.adoptable_target else {
            return;
        };
        if Some(&major_target) == scoped_target {
            return;
        }
        let kind = major
            .candidates
            .iter()
            .find(|candidate| candidate.version == major_target)
            .map_or(UpdateKind::Major, |candidate| candidate.kind);
        let change = Change {
            package: dep.package.clone(),
            from: dep.current.clone(),
            to: major_target,
            kind,
            // A held-back cross-major is a forward move the user could take with `--major`.
            downgrade: false,
            direct: dep.direct,
            members: dep.members.clone(),
        };
        self.record_change_skip(
            &change,
            Some(SkippedInfo {
                reason: SkipReason::NeedsMajor,
                message: SkipReason::NeedsMajor.message().to_string(),
                offending: None,
            }),
        );
    }

    /// Plan downgrades for `fix`: every dependency whose locked version is too fresh moves to the
    /// newest matured version older than it. A pin is left in place with a warning unless
    /// `downgrade_pinned`; a violation with no matured older version is reported as a warning too.
    /// Warnings are returned (not emitted) so the fixpoint caller surfaces only the final round's —
    /// a dep held now may become fixable once an umbrella module ahead of it is downgraded.
    async fn plan_fix_changes(
        &mut self,
        deps: &[Dependency],
        transitive: TransitiveGate,
        downgrade_pinned: bool,
    ) -> FixPlan {
        self.ctx.opts.progress.say(&format!(
            "Fetching release metadata for {} cooldown fix candidate(s) in {} ({})…",
            deps.len(),
            self.project_label(),
            self.ctx.pctx.tool
        ));
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
        let mut warnings = Vec::new();
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
                &dep_resolve_ctx(&rctx, &dep),
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
                warnings.push(FixWarning {
                    package: dep.package.name.clone(),
                    message: format!(
                        "{}@{} is younger than its cooldown; left in place by --transitive allow",
                        dep.package.name, dep.current
                    ),
                });
                continue;
            }
            if fix.current.graph_held {
                warnings.push(FixWarning {
                    package: dep.package.name.clone(),
                    message: format!(
                        "{}@{} is younger than its cooldown, but the resolved graph requires that version; baseline it or relax the dependency forcing it",
                        dep.package.name, dep.current
                    ),
                });
                continue;
            }
            // An exact pin is a deliberate choice: warn and leave it unless `--downgrade-pinned`.
            if dep.pinned && !downgrade_pinned {
                warnings.push(FixWarning {
                    package: dep.package.name.clone(),
                    message: format!(
                        "{}@{} is pinned and younger than its cooldown; downgrade it manually or rerun with --downgrade-pinned",
                        dep.package.name, dep.current
                    ),
                });
                continue;
            }
            let Some(target) = fix.target else {
                warnings.push(FixWarning {
                    package: dep.package.name.clone(),
                    message: format!(
                        "{}@{} is younger than its cooldown and no older version has matured; `baseline` it or wait",
                        dep.package.name, dep.current
                    ),
                });
                continue;
            };
            let kind = releases
                .iter()
                .find(|release| release.version == target)
                .and_then(|release| release.kind_from_current)
                .unwrap_or(UpdateKind::Minor);
            // A cross-major downgrade (Go `/v3` → `/v2`, or `/v2` → the v1 base path) changes the
            // import path; `target_package_for` reconstructs it (a no-op for same-major moves and for
            // tools whose name is stable across majors).
            planned.push(Change {
                package: target_package_for(&releases, &dep, &target),
                from: dep.current.clone(),
                to: target,
                kind,
                // `fix` only ever rolls a too-fresh pin back.
                downgrade: true,
                direct: dep.direct,
                members: dep.members.clone(),
            });
        }
        FixPlan {
            changes: planned,
            warnings,
        }
    }

    /// Apply `fix` downgrades round by round until the graph stops changing: each round re-discovers
    /// the (re-locked) graph and re-plans, because a downgrade can free a dep that was graph-held a
    /// round earlier. The deferred unfixable warnings are surfaced once, from the settling round.
    async fn fix_to_fixpoint(
        &mut self,
        mut deps: Vec<Dependency>,
        transitive: TransitiveGate,
        downgrade_pinned: bool,
        state: &mut TrialState,
    ) {
        for _ in 0..MAX_FIX_ROUNDS {
            let plan = self
                .plan_fix_changes(&deps, transitive, downgrade_pinned)
                .await;
            if plan.changes.is_empty() {
                self.emit_fix_warnings(plan.warnings);
                return;
            }
            let applied_before = self.applied_count();
            if !self.apply_batch(plan.changes, state).await {
                self.emit_fix_warnings(plan.warnings);
                return;
            }
            // No forward progress despite a non-empty plan (e.g. every downgrade was held): stop
            // rather than spin, and surface what is left.
            if self.applied_count() == applied_before {
                self.emit_fix_warnings(plan.warnings);
                return;
            }
            let Some(next) = self.scoped_deps().await else {
                return;
            };
            deps = next;
        }
    }

    /// Downgrade any too-fresh transitive a forward `upgrade` move floated up, to a fixpoint — the
    /// `fix` half of a single-pass `upgrade`. Reuses the per-change trial so each downgrade is
    /// applied, re-locked, and verified like any other.
    async fn reconcile_to_fixpoint(&mut self, state: &mut TrialState) {
        for _ in 0..MAX_FIX_ROUNDS {
            if !state.reconcile_needed {
                return;
            }
            state.reconcile_needed = false;
            self.ctx.opts.progress.say(&format!(
                "Reconciling transitive cooldown violations in {} ({})…",
                self.project_label(),
                self.ctx.pctx.tool
            ));
            let Some(deps) = self.reconcile_deps().await else {
                return;
            };
            let plan = self
                .plan_fix_changes(&deps, TransitiveGate::Enforce, false)
                .await;
            if plan.changes.is_empty() {
                self.emit_fix_warnings(plan.warnings);
                return;
            }
            let applied_before = self.applied_count();
            if !self.apply_batch(plan.changes, state).await {
                self.emit_fix_warnings(plan.warnings);
                return;
            }
            if self.applied_count() == applied_before {
                self.emit_fix_warnings(plan.warnings);
                return;
            }
        }
    }

    fn emit_fix_warnings(&mut self, warnings: Vec<FixWarning>) {
        for warning in warnings {
            self.record_fix_warning(&warning.message, &warning.package);
        }
    }

    fn record_fix_warning(&mut self, message: &str, package: &str) {
        let diag = Diagnostic::new(DiagnosticKind::Held, message.to_string())
            .with_tool(self.ctx.tool_name())
            .with_project(self.project_label.clone())
            .with_package(package);
        self.acc.strict_incomplete = true;
        self.acc.warnings.push(diag);
    }

    /// Apply a round's planned changes as **one** atomic plan, so a tool that re-resolves jointly
    /// (uv's ceiling resolve) settles conflicts between candidates in a single pass and produces one
    /// consistent lock. The whole batch shares a single journal: the resulting lock is indivisible, so
    /// if it drags in an irreducible fresh transitive the entire batch is rolled back rather than a
    /// single change. Returns whether the round made forward progress worth continuing the fixpoint.
    async fn apply_batch(&mut self, changes: Vec<Change>, state: &mut TrialState) -> bool {
        if changes.is_empty() {
            return false;
        }
        let plan = Plan {
            changes: changes.clone(),
            rewrite: self.ctx.opts.rewrite,
        };
        let primary = changes
            .first()
            .map(|change| change.package.name.clone())
            .unwrap_or_default();
        self.ctx.opts.progress.say(&format!(
            "Applying {} planned change(s) in {} ({})…",
            changes.len(),
            self.project_label(),
            self.ctx.pctx.tool
        ));
        let journal = match self
            .ctx
            .writer
            .mutation_journal(&self.ctx.pctx.project, &plan)
            .await
        {
            Ok(journal) => journal,
            Err(error) => {
                self.record_project_error(&error, Some(&primary));
                return false;
            }
        };

        // Resilient apply: if the joint resolve is unsatisfiable as a whole because of one unfetchable
        // or conflicting candidate, isolate it and apply the rest rather than holding every candidate.
        let report = match super::super::resilient_apply::apply_resilient(
            self.ctx.writer,
            &self.ctx.pctx.project,
            &plan,
            &journal,
        )
        .await
        {
            Ok(report) => report,
            Err(error) => {
                let restored = self.restore_journal(&journal);
                self.record_change_errors(&error, &changes);
                return restored;
            }
        };
        if report.applied.is_empty() {
            self.record_batch_skips(report.skipped);
            return self.restore_journal(&journal);
        }
        let report = match self.verify_apply_report(report, &changes).await {
            Ok(report) => report,
            Err(error) => {
                let restored = self.restore_journal(&journal);
                self.record_change_errors(&error, &changes);
                return restored;
            }
        };

        // A held candidate (the resolve could not place it at its target without breaking the lock) is
        // reported as a skip naming the package that blocks it.
        let applied: HashSet<ChangeTargetKey> =
            report.applied.iter().map(change_target_key).collect();
        let planned_applied = planned_changes_landed(&changes, &applied);
        let planned: HashSet<PackageId> = changes
            .iter()
            .map(|change| change.package.clone())
            .collect();
        // Net version changes the resolve forced on packages the plan did not name (a transitive
        // pushed backward for consistency, or matured down by a downgrade). These are part of the
        // committed lock and must be surfaced, never silent — the whole point of the full-lock-diff
        // report. They are recorded as applied rows once the batch commits below.
        let collateral: Vec<Change> = report
            .applied
            .iter()
            .filter(|change| !planned.contains(&change.package))
            .cloned()
            .collect();
        self.record_batch_skips(report.skipped);

        if !planned_applied {
            // No requested target landed: roll any incidental resolver movement back to the captured
            // state instead of committing a collateral-only mutation.
            return self.restore_journal(&journal);
        }

        self.ctx.opts.progress.say(&format!(
            "Checking resolved graph cooldown after apply in {} ({})…",
            self.project_label(),
            self.ctx.pctx.tool
        ));
        let after = match self.graph_violations().await {
            Ok(after) => after,
            Err(error) => {
                // The post-apply gate probe failed: fail closed by rolling the batch back and
                // recording the failure against each applied change (never committing an unverified
                // lock).
                let restored = self.restore_journal(&journal);
                self.record_change_errors(
                    &error,
                    changes
                        .iter()
                        .filter(|change| applied.contains(&change_target_key(change))),
                );
                return restored;
            }
        };
        let after_keys: HashSet<(String, String)> = after.keys().cloned().collect();
        if self.gate_batch_transitives(&after, &after_keys, &changes, &applied, state) {
            return self.restore_journal(&journal);
        }

        self.commit_batch_report(&changes, &collateral, &applied, after_keys, state);
        true
    }

    fn commit_batch_report(
        &mut self,
        changes: &[Change],
        collateral: &[Change],
        applied: &HashSet<ChangeTargetKey>,
        after_keys: HashSet<(String, String)>,
        state: &mut TrialState,
    ) {
        self.lock_refreshed_by_apply |= self.ctx.writer.successful_apply_proves_lock_current();
        state.baseline_violations = after_keys;
        for change in changes {
            if applied.contains(&change_target_key(change)) {
                self.record_change_applied(change);
            }
        }
        for change in collateral {
            self.record_change_applied(change);
        }
    }

    async fn verify_apply_report(
        &self,
        report: ApplyReport,
        planned: &[Change],
    ) -> cooldown_core::Result<ApplyReport> {
        let deps = self
            .ctx
            .reader
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await?;
        Ok(verify_applied_targets(report, planned, &deps))
    }

    /// Record each held candidate (uv lowered it below its ceiling, or the resolve rejected it) as a
    /// skip, naming the package that blocks it via [`conflict_skip_message`].
    fn record_batch_skips(&mut self, skipped: Vec<cooldown_core::Skipped>) {
        for skipped in skipped {
            let offending = skipped.offending.map(|package| package.name);
            // A multi-version dependency held within its own line is conservative-correct, not a
            // failed upgrade — like `NeedsMajor` it must not fail a `--strict` run.
            if skipped.reason != SkipReason::MultiVersionHeld {
                self.acc.strict_incomplete = true;
            }
            let change = skipped.change;
            self.record_change_skip(
                &change,
                Some(SkippedInfo {
                    reason: skipped.reason,
                    message: conflict_skip_message(
                        skipped.reason,
                        offending.as_deref(),
                        &change.package.name,
                    ),
                    offending,
                }),
            );
        }
    }

    /// The transitive-cooldown gate over a committed batch. The joint resolve may drag a fresh
    /// transitive into the graph; how we react follows the transitive mode: `Hide` ignores
    /// transitives; `Allow` keeps the lock and reports them; `Enforce` (default) keeps it only when
    /// every new violation is *reconcilable* (the reconcile pass can downgrade it later). Returns
    /// `true` when `Enforce` found a fresh, irreducible dep the resolve forced in — the caller rolls
    /// the whole batch back, since committing a version that requires a too-fresh transitive defeats
    /// the cooldown. Records the applied changes as `TransitiveInCooldown` skips in that case.
    fn gate_batch_transitives(
        &mut self,
        after: &HashMap<(String, String), bool>,
        after_keys: &HashSet<(String, String)>,
        changes: &[Change],
        applied: &HashSet<ChangeTargetKey>,
        state: &mut TrialState,
    ) -> bool {
        let new_violations: Vec<&(String, String)> =
            after_keys.difference(&state.baseline_violations).collect();
        if new_violations.is_empty() {
            return false;
        }
        match self.transitive_mode() {
            TransitiveGate::Hide => false,
            TransitiveGate::Allow => {
                for (package, version) in &new_violations {
                    self.record_fix_warning(
                        &format!(
                            "{package}@{version} is younger than its cooldown; left in place by --transitive allow"
                        ),
                        package,
                    );
                }
                false
            }
            TransitiveGate::Enforce => {
                let Some((forced_pkg, _)) = new_violations
                    .iter()
                    .find(|key| !after.get(**key).copied().unwrap_or(false))
                else {
                    // Every new violation is reconcilable; keep the lock and let the reconcile pass
                    // (after the upgrade loop) downgrade the floated-up transitives.
                    state.reconcile_needed = true;
                    return false;
                };
                self.acc.strict_incomplete = true;
                for change in changes {
                    if applied.contains(&change_target_key(change)) {
                        self.record_change_skip(
                            change,
                            Some(SkippedInfo {
                                reason: SkipReason::TransitiveInCooldown,
                                message: SkipReason::TransitiveInCooldown.message().to_string(),
                                offending: Some(forced_pkg.clone()),
                            }),
                        );
                    }
                }
                true
            }
        }
    }

    fn transitive_mode(&self) -> TransitiveGate {
        self.ctx.opts.transitive_mode
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

    async fn finalize(&mut self) {
        self.ctx.opts.progress.say(&format!(
            "Verifying lock state in {} ({})…",
            self.project_label(),
            self.ctx.pctx.tool
        ));
        match self
            .ctx
            .reader
            .verify_lock_current(&self.ctx.pctx.project)
            .await
        {
            Ok(report) => match report.status {
                LockStatus::Current => {
                    self.record_lock_status(LockStatus::Current);
                }
                LockStatus::Stale => {
                    self.record_lock_status(LockStatus::Stale);
                    let diag = Diagnostic::new(DiagnosticKind::StaleLock, report.detail)
                        .with_tool(self.ctx.tool_name())
                        .with_project(self.project_label())
                        .with_path(self.ctx.pctx.project.manifest.as_str());
                    if self.ctx.opts.allow_stale_lock {
                        self.acc.warnings.push(diag);
                    } else {
                        self.acc.errors.push(diag);
                    }
                }
                LockStatus::Unknown => {
                    if self.lock_refreshed_by_apply {
                        self.record_lock_status(LockStatus::Current);
                    } else {
                        self.record_lock_status(LockStatus::Unknown);
                        self.acc.warnings.push(
                            Diagnostic::new(DiagnosticKind::LockUnknown, report.detail)
                                .with_tool(self.ctx.tool_name())
                                .with_project(self.project_label())
                                .with_path(self.ctx.pctx.project.manifest.as_str()),
                        );
                    }
                }
            },
            Err(error) => {
                self.record_lock_status(LockStatus::Stale);
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

    fn record_lock_status(&mut self, status: LockStatus) {
        self.acc.lock_status = Some(combine_lock_status(self.acc.lock_status, status));
        self.acc.lock_verified = self.acc.lock_status.and_then(LockStatus::verified);
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

    fn record_change_errors<'c>(
        &mut self,
        error: &cooldown_core::CoreError,
        changes: impl IntoIterator<Item = &'c Change>,
    ) {
        for change in changes {
            let diag = diag_from_error(
                error,
                self.ctx.pctx.tool,
                self.project_label(),
                Some(&change.package.name),
            );
            self.record_change_error(change, diag);
        }
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
        self.acc.errors.len()
            + self
                .acc
                .items
                .iter()
                .filter(|item| item.error.is_some())
                .count()
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

/// The resolve context for one dependency. A cross-major move needs an editable manifest constraint
/// to rewrite, which only a direct dependency has; an indirect dep can never take a cross-major bump
/// on its own (the resolver rejects it, producing noise), so it is always capped to its current major
/// even under `--major`. It still re-adopts a matured newer version *within* its major.
fn dep_resolve_ctx<'a>(rctx: &ResolveContext<'a>, dep: &Dependency) -> ResolveContext<'a> {
    ResolveContext {
        allow_major: dep.direct && rctx.allow_major,
        ..*rctx
    }
}

/// Whether `to` is an older release than `from` — i.e. the move is a cooldown rollback, not a forward
/// upgrade. Compares the releases' [`ReleaseOrder`] tokens (the canonical ordering the rest of the
/// module uses), so it is independent of slice order. Unknown versions compare as not-a-downgrade.
fn is_downgrade(releases: &[Release], from: &Version, to: &Version) -> bool {
    let order = |v: &Version| {
        releases
            .iter()
            .find(|release| &release.version == v)
            .map(|r| &r.order)
    };
    matches!((order(to), order(from)), (Some(t), Some(f)) if t < f)
}

/// The target [`PackageId`] for a move from `dep.current` to `target`, derived from the matching
/// releases' major keys — rewriting a Go `/vN` path-major and keeping the name stable otherwise.
/// Shared by the upgrade and fix planners.
fn target_package_for(releases: &[Release], dep: &Dependency, target: &Version) -> PackageId {
    let current_major = releases
        .iter()
        .find(|release| release.version == dep.current)
        .map_or(MajorKey(String::new()), |release| release.major.clone());
    let target_major = releases
        .iter()
        .find(|release| release.version == *target)
        .map(|release| release.major.clone())
        .unwrap_or(current_major.clone());
    target_package(&dep.package, &current_major, &target_major)
}

/// Reconstruct the target `PackageId`, handling Go-style `/vN` path-major changes (the `MajorKey`
/// is a path suffix). For tools where the package name is stable across majors, the name is kept.
fn target_package(
    package: &PackageId,
    current_major: &MajorKey,
    target_major: &MajorKey,
) -> PackageId {
    let suffix = &target_major.0;
    // A Go `MajorKey` is a path suffix (`/v2`, `.v2`); a registry tool's is version-derived (`1`).
    // Rewrite the path on any cross-major Go move — including a downgrade to the v1 base path, where
    // the *current* major is the suffix and the target is empty (so checking only `suffix` misses it).
    let is_path_major = |key: &str| key.starts_with('/') || key.starts_with('.');
    let name = if current_major.0 != target_major.0
        && (is_path_major(&current_major.0) || is_path_major(suffix))
    {
        let prefix = package
            .name
            .strip_suffix(&current_major.0)
            .unwrap_or(&package.name);
        format!("{prefix}{suffix}")
    } else {
        package.name.clone()
    };
    PackageId::new(package.tool, name, package.registry.clone())
}

fn verify_applied_targets(
    report: ApplyReport,
    planned: &[Change],
    deps: &[Dependency],
) -> ApplyReport {
    let planned: HashSet<ChangeTargetKey> = planned.iter().map(change_target_key).collect();
    let mut skipped_keys: HashSet<ChangeTargetKey> = report
        .skipped
        .iter()
        .map(|skip| change_target_key(&skip.change))
        .collect();
    let mut verified = ApplyReport {
        applied: Vec::new(),
        skipped: report.skipped,
    };

    for change in report.applied {
        let key = change_target_key(&change);
        if !planned.contains(&key) {
            verified.applied.push(change);
            continue;
        }

        if target_reached(deps, &change) {
            verified.applied.push(change);
            continue;
        }

        if skipped_keys.insert(key) {
            verified.skipped.push(resolver_conflict(&change));
        }
    }
    verified
}

fn target_reached(deps: &[Dependency], change: &Change) -> bool {
    deps.iter()
        .any(|dep| dep.package == change.package && dep.current == change.to)
}

fn change_target_key(change: &Change) -> ChangeTargetKey {
    (
        change.package.name.clone(),
        change.package.registry.clone(),
        change.to.as_str().to_string(),
    )
}

fn resolver_conflict(change: &Change) -> Skipped {
    Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    }
}

fn planned_changes_landed(changes: &[Change], applied: &HashSet<ChangeTargetKey>) -> bool {
    changes
        .iter()
        .any(|change| applied.contains(&change_target_key(change)))
}

fn combine_lock_status(current: Option<LockStatus>, next: LockStatus) -> LockStatus {
    match (current, next) {
        (Some(LockStatus::Stale), _) | (_, LockStatus::Stale) => LockStatus::Stale,
        (Some(LockStatus::Unknown), _) | (_, LockStatus::Unknown) => LockStatus::Unknown,
        _ => LockStatus::Current,
    }
}

/// The user-facing skip message. A resolver conflict caused by a *different* package — adopting the
/// candidate would have regressed it (a mutually-exclusive requirement) — names that package, so the
/// report says which dependency is holding the candidate back rather than the generic "the resolver
/// rejected this change". Any other skip keeps the reason's own message.
fn conflict_skip_message(reason: SkipReason, offending: Option<&str>, changed: &str) -> String {
    match (reason, offending) {
        // A conflict blamed on a *different* package: adopting the candidate would have regressed it.
        // A conflict whose offender is the candidate itself is just "the resolver rejected this pin",
        // so it keeps the generic message.
        (SkipReason::ResolverConflict, Some(offending)) if offending != changed => {
            format!("held: conflicts with {offending}")
        }
        _ => reason.message().to_string(),
    }
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
        direct: change.direct,
        downgrade: change.downgrade,
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

#[cfg(test)]
mod tests {
    use super::{
        PlanMode, candidate_scope, combine_lock_status, conflict_skip_message, is_downgrade,
        planned_changes_landed, target_package, verify_applied_targets,
    };
    use crate::app::TransitiveGate;
    use cooldown_core::{
        ApplyReport, Change, DepScope, Dependency, LockStatus, MajorKey, PackageId, Release,
        ReleaseOrder, ReleaseQuality, SkipReason, ToolId, UpdateKind, Version,
    };
    use std::collections::HashSet;

    #[test]
    fn upgrade_scopes_candidates_to_direct_requires() {
        // `upgrade` hands only direct requires to the resolver; MVS promotes indirect deps as a
        // consequence, so indirect deps are never attempt-and-rejected as candidates.
        assert_eq!(candidate_scope(PlanMode::Upgrade), DepScope::Direct);
    }

    #[test]
    fn fix_walks_the_graph_unless_transitive_hidden() {
        // `fix` must see the resolved graph to downgrade too-fresh transitives.
        assert_eq!(
            candidate_scope(PlanMode::Fix {
                transitive: TransitiveGate::Enforce,
                downgrade_pinned: false,
            }),
            DepScope::Graph
        );
        assert_eq!(
            candidate_scope(PlanMode::Fix {
                transitive: TransitiveGate::Allow,
                downgrade_pinned: false,
            }),
            DepScope::Graph
        );
        // `--transitive hide` narrows `fix` candidates to direct deps.
        assert_eq!(
            candidate_scope(PlanMode::Fix {
                transitive: TransitiveGate::Hide,
                downgrade_pinned: false,
            }),
            DepScope::Direct
        );
    }

    #[test]
    fn conflict_skip_message_names_a_different_offender() {
        // A resolver conflict blamed on another package — adopting it would have regressed that
        // package — names it, so the report explains which dependency holds the candidate back.
        assert_eq!(
            conflict_skip_message(
                SkipReason::ResolverConflict,
                Some("typer"),
                "huggingface-hub"
            ),
            "held: conflicts with typer"
        );
    }

    #[test]
    fn conflict_skip_message_keeps_generic_message_when_offender_is_self() {
        // The resolver rejected the pin itself (no other package to blame): keep the generic message.
        assert_eq!(
            conflict_skip_message(SkipReason::ResolverConflict, Some("foo"), "foo"),
            SkipReason::ResolverConflict.message()
        );
        assert_eq!(
            conflict_skip_message(SkipReason::ResolverConflict, None, "foo"),
            SkipReason::ResolverConflict.message()
        );
    }

    #[test]
    fn conflict_skip_message_passes_through_non_conflict_reasons() {
        assert_eq!(
            conflict_skip_message(SkipReason::NeedsMajor, Some("bar"), "foo"),
            SkipReason::NeedsMajor.message()
        );
    }

    fn rel(version: &str, order: u8) -> Release {
        Release {
            version: Version::new(version),
            order: ReleaseOrder(vec![order]),
            major: MajorKey(String::new()),
            kind_from_current: None,
            published_at: None,
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
    }

    fn dep(name: &str, version: &str) -> Dependency {
        Dependency {
            package: PackageId::new(ToolId("mock"), name, None),
            current: Version::new(version),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            members: Vec::new(),
            pinned: false,
        }
    }

    fn change(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(ToolId("mock"), name, None),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    #[test]
    fn verify_applied_targets_turns_unreached_planned_success_into_skip() {
        let planned = vec![change("serde", "1.0.0", "1.0.1")];
        let report = ApplyReport {
            applied: planned.clone(),
            skipped: Vec::new(),
        };

        let verified = verify_applied_targets(report, &planned, &[dep("serde", "1.0.0")]);

        assert!(verified.applied.is_empty());
        assert_eq!(verified.skipped.len(), 1);
        assert_eq!(verified.skipped[0].reason, SkipReason::ResolverConflict);
        assert_eq!(verified.skipped[0].change.package.name, "serde");
    }

    #[test]
    fn verify_applied_targets_keys_same_package_by_target_version() {
        let landed = change("serde", "1.0.0", "1.0.1");
        let missed = change("serde", "1.0.0", "1.0.2");
        let planned = vec![landed.clone(), missed.clone()];
        let report = ApplyReport {
            applied: planned.clone(),
            skipped: Vec::new(),
        };

        let verified = verify_applied_targets(report, &planned, &[dep("serde", "1.0.1")]);

        assert_eq!(verified.applied, vec![landed]);
        assert_eq!(verified.skipped.len(), 1);
        assert_eq!(verified.skipped[0].change, missed);
    }

    #[test]
    fn verify_applied_targets_keeps_lock_diff_collateral_outside_dependency_graph() {
        let planned_change = change("serde", "1.0.0", "1.0.1");
        let collateral = change("itoa", "1.0.0", "1.0.1");
        let report = ApplyReport {
            applied: vec![planned_change.clone(), collateral.clone()],
            skipped: Vec::new(),
        };

        let verified = verify_applied_targets(
            report,
            std::slice::from_ref(&planned_change),
            &[dep("serde", "1.0.1")],
        );

        assert_eq!(verified.applied, vec![planned_change, collateral]);
        assert!(verified.skipped.is_empty());
    }

    #[test]
    fn planned_changes_landed_rejects_collateral_only_results() {
        let planned = change("serde", "1.0.0", "1.0.1");
        let collateral = change("itoa", "1.0.0", "1.0.1");
        let applied = HashSet::from([super::change_target_key(&collateral)]);

        assert!(!planned_changes_landed(&[planned], &applied));
    }

    #[test]
    fn lock_status_aggregation_keeps_unknown_distinct_from_verified_current() {
        let current_then_unknown =
            combine_lock_status(Some(LockStatus::Current), LockStatus::Unknown);
        assert_eq!(current_then_unknown, LockStatus::Unknown);
        assert_eq!(current_then_unknown.verified(), None);

        let unknown_then_stale = combine_lock_status(Some(LockStatus::Unknown), LockStatus::Stale);
        assert_eq!(unknown_then_stale, LockStatus::Stale);
        assert_eq!(unknown_then_stale.verified(), Some(false));
    }

    #[test]
    fn is_downgrade_compares_release_order() {
        let releases = [rel("1.0.0", 0), rel("1.0.1", 1), rel("1.0.2", 2)];
        // Rolling a too-fresh pin back to an older release is a downgrade.
        assert!(is_downgrade(
            &releases,
            &Version::new("1.0.2"),
            &Version::new("1.0.1")
        ));
        // A forward move is not.
        assert!(!is_downgrade(
            &releases,
            &Version::new("1.0.0"),
            &Version::new("1.0.2")
        ));
        // A version not in the set is treated as not-a-downgrade.
        assert!(!is_downgrade(
            &releases,
            &Version::new("1.0.0"),
            &Version::new("9.9.9")
        ));
    }

    fn go(name: &str) -> PackageId {
        PackageId::new(ToolId("go"), name, None)
    }

    fn major(key: &str) -> MajorKey {
        MajorKey(key.to_string())
    }

    #[test]
    fn target_package_rewrites_a_go_path_major_in_both_directions() {
        // Upgrade base → /v2 and /v2 → /v3.
        assert_eq!(
            target_package(&go("example.com/foo"), &major(""), &major("/v2")).name,
            "example.com/foo/v2"
        );
        assert_eq!(
            target_package(&go("example.com/foo/v2"), &major("/v2"), &major("/v3")).name,
            "example.com/foo/v3"
        );
        // Downgrade /v3 → /v2 and /v2 → the v1 base path — the moves `fix --major` now makes.
        assert_eq!(
            target_package(&go("example.com/foo/v3"), &major("/v3"), &major("/v2")).name,
            "example.com/foo/v2"
        );
        assert_eq!(
            target_package(&go("example.com/foo/v2"), &major("/v2"), &major("")).name,
            "example.com/foo"
        );
    }

    #[test]
    fn target_package_keeps_the_name_when_the_major_is_version_derived() {
        // A registry tool's `MajorKey` is a bare version axis (`0.23` → `0.25`), not a path suffix,
        // so the package name is stable across majors.
        let cargo = PackageId::new(ToolId("cargo"), "toml_edit", Some("crates.io".to_string()));
        assert_eq!(
            target_package(&cargo, &major("0.23"), &major("0.25")).name,
            "toml_edit"
        );
        // A same-major move keeps the name too.
        assert_eq!(
            target_package(&go("example.com/foo/v2"), &major("/v2"), &major("/v2")).name,
            "example.com/foo/v2"
        );
    }
}
