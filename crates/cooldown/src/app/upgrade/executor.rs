use super::{UpgradeAccum, UpgradeCtx};
use crate::app::change_key::{ChangeTargetKey, change_target_key};
use crate::app::lock::ProjectLock;
use crate::app::{SkippedInfo, TransitiveGate, UpgradeItem, Workspace, diag_from_error};
use cooldown_core::{
    ApplyReport, Change, DepScope, Dependency, Diagnostic, DiagnosticKind, LockStatus, MajorKey,
    PackageId, Plan, ProjectMutationJournal, Release, ResolveContext, SkipReason, Skipped, Status,
    UpdateKind, Version, check_pin, evaluate, evaluate_fix,
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

/// One round of `fix` planning: downgrades, unfixable violations, and metadata failures.
struct FixPlan {
    changes: Vec<Change>,
    warnings: Vec<FixWarning>,
    errors: Vec<Diagnostic>,
}

/// The report and state delta produced by one native resolver batch.
#[derive(Default)]
struct BatchOutcome {
    items: Vec<UpgradeItem>,
    warnings: Vec<Diagnostic>,
    errors: Vec<Diagnostic>,
    strict_incomplete: bool,
    lock_refreshed: bool,
    /// Present iff the batch's mutation was kept: the lock landed and the post-apply graph
    /// verification ran. `None` means the journal was restored (or nothing was attempted).
    committed: Option<CommittedBatch>,
}

/// What a kept batch changes about the trial state.
struct CommittedBatch {
    /// The graph's non-baselined violations after this batch (the next trial baseline).
    violations_after: HashSet<(String, String)>,
    /// Whether the batch floated up violations the reconcile pass should mature down.
    reconcile_needed: bool,
}

/// The transitive-cooldown gate's decision over a freshly-applied batch.
enum TransitiveGateVerdict {
    /// Restore the journal; the batch must not be kept.
    RollBack,
    /// Keep the batch; `reconcile_needed` says whether a reconcile pass should follow.
    Keep {
        /// Whether a reconcile pass should follow to mature floated-up violations down.
        reconcile_needed: bool,
    },
}

/// One policy trial's verdict over a candidate group: committed outcomes to keep, the residual
/// cooldown violations that reject the group, or an error that aborts recovery entirely.
enum UpgradeTrialResult {
    Settled(Vec<BatchOutcome>),
    PolicyBlocked(Vec<(String, String)>),
    Aborted(BatchOutcome),
}

/// A candidate isolation rejected, with the residual violations its trial forced into the graph.
type RejectedUpgrade = (Change, Vec<(String, String)>);

/// The outcome of isolating a policy-blocked batch into safe and unsafe candidates.
enum UpgradeSelectionResult {
    Selected {
        accepted: Vec<Change>,
        rejected: Vec<RejectedUpgrade>,
    },
    Aborted(BatchOutcome),
}

struct VerifiedBatchReport {
    applied: HashSet<ChangeTargetKey>,
    collateral: Vec<Change>,
    planned_applied: bool,
}

impl BatchOutcome {
    fn applied_count(&self) -> usize {
        self.items.iter().filter(|item| item.applied).count()
    }

    fn errored_count(&self) -> usize {
        self.errors.len()
            + self
                .items
                .iter()
                .filter(|item| item.error.is_some())
                .count()
    }

    fn merge_into(self, acc: &mut UpgradeAccum) {
        acc.items.extend(self.items);
        acc.warnings.extend(self.warnings);
        acc.errors.extend(self.errors);
        acc.strict_incomplete |= self.strict_incomplete;
    }
}

/// The evolving per-project state during upgrade trials.
#[derive(Clone)]
struct TrialState {
    /// In-cooldown, non-baselined pins present before the next trial.
    baseline_violations: HashSet<(String, String)>,
    /// Whether the last committed batch introduced transitive cooldown violations to reconcile.
    reconcile_needed: bool,
}

/// The cohesive per-project upgrade state machine: dependency discovery, planning, group trials,
/// rollback, and final verification.
pub(super) struct ProjectUpgradeExecutor<'a, 'b> {
    ws: &'a Workspace,
    ctx: UpgradeCtx<'b>,
    project_label: String,
    mode: PlanMode,
    acc: &'a mut UpgradeAccum,
    lock_refreshed_by_apply: bool,
    /// Packages whose only requirement is a manifest constraint with no lock entry (a build backend).
    /// Their floor raise has no lock interaction, so they are applied in their own batch — a lock
    /// conflict elsewhere in the same run must not roll back (and mislabel) an independent adoption.
    manifest_only: HashSet<PackageId>,
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
            manifest_only: HashSet::new(),
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
        let planned = self.plan_upgrade_changes(&deps).await;
        // Build-backend floor raises ([build-system].requires) have no lock interaction, so apply them
        // in their own batch: a transitive-cooldown rollback of the lock batch must not revert (or
        // mislabel as a conflict) an independent, valid build-backend adoption.
        let (build_changes, lock_changes): (Vec<Change>, Vec<Change>) = planned
            .into_iter()
            .partition(|change| self.manifest_only.contains(&change.package));
        if !build_changes.is_empty() {
            let outcome = self.apply_batch(build_changes, state).await;
            Self::advance_trial_state(&outcome, state);
            self.merge_batch_outcome(outcome);
        }
        if lock_changes.is_empty() {
            return;
        }

        self.run_lock_upgrades(lock_changes, state).await;
    }

    /// Applies the lock batch, isolating candidates when the joint result violates cooldown policy.
    ///
    /// The fast path is one trial of the complete batch: settled outcomes commit as-is. A policy
    /// residual restores the fixed pre-lock baseline and — for more than one candidate —
    /// partitions the batch to find the maximal safe subset, which is then replayed jointly from
    /// that same baseline; only the replay commits. Errors abort recovery and restore the
    /// baseline: an infrastructure failure must surface as an error, never as a cooldown skip.
    async fn run_lock_upgrades(&mut self, lock_changes: Vec<Change>, state: &mut TrialState) {
        let baseline_before_lock = state.clone();
        let mut rollback = ProjectMutationJournal::default();
        let initial = self
            .try_upgrade_group(
                lock_changes.clone(),
                &baseline_before_lock.baseline_violations,
                state,
                &mut rollback,
            )
            .await;
        match initial {
            UpgradeTrialResult::Settled(outcomes) => {
                self.merge_batch_outcomes(outcomes);
                self.collapse_collateral(&baseline_before_lock.baseline_violations);
                return;
            }
            UpgradeTrialResult::Aborted(mut outcome) => {
                self.restore_upgrade_trial(&rollback, &baseline_before_lock, state, &mut outcome);
                self.merge_batch_outcome(outcome);
                self.collapse_collateral(&baseline_before_lock.baseline_violations);
                return;
            }
            UpgradeTrialResult::PolicyBlocked(violations) => {
                let mut outcome = BatchOutcome::default();
                if !self.restore_upgrade_trial(
                    &rollback,
                    &baseline_before_lock,
                    state,
                    &mut outcome,
                ) {
                    self.merge_batch_outcome(outcome);
                    return;
                }
                // A singleton batch has nothing to isolate: the lone candidate is the culprit.
                if lock_changes.len() == 1 {
                    self.record_unreconciled_skips(&lock_changes, &violations);
                    self.collapse_collateral(&baseline_before_lock.baseline_violations);
                    return;
                }
            }
        }

        self.recover_policy_blocked_upgrade(
            lock_changes,
            &baseline_before_lock,
            state,
            &mut rollback,
        )
        .await;
        self.collapse_collateral(&baseline_before_lock.baseline_violations);
    }

    /// Isolates a policy-blocked multi-candidate batch, then commits its safe subset via one joint
    /// replay. With no safe candidate every rejection is reported held; a selection abort merges
    /// only the failing trial's errors.
    async fn recover_policy_blocked_upgrade(
        &mut self,
        lock_changes: Vec<Change>,
        baseline: &TrialState,
        state: &mut TrialState,
        rollback: &mut ProjectMutationJournal,
    ) {
        let selection = self
            .select_safe_upgrade_changes(lock_changes, baseline, state, rollback)
            .await;
        match selection {
            UpgradeSelectionResult::Selected { accepted, rejected } if accepted.is_empty() => {
                self.record_rejected_upgrade_changes(rejected);
            }
            UpgradeSelectionResult::Selected { accepted, rejected } => {
                self.replay_selected_upgrade_changes(accepted, rejected, baseline, state, rollback)
                    .await;
            }
            UpgradeSelectionResult::Aborted(outcome) => self.merge_batch_outcome(outcome),
        }
    }

    /// Partitions a policy-blocked batch into the maximal safe subset and rejected singletons —
    /// delta-debugging partitioning over `accepted + group` trials, like `apply_resilient`, with
    /// the tool's whole pipeline (apply, reconcile, residual gate) as the oracle.
    async fn select_safe_upgrade_changes(
        &mut self,
        lock_changes: Vec<Change>,
        baseline: &TrialState,
        state: &mut TrialState,
        rollback: &mut ProjectMutationJournal,
    ) -> UpgradeSelectionResult {
        // Selection trials always start from the same pre-lock graph and include every previously
        // accepted candidate. A later whole-graph resolve therefore cannot silently displace an
        // earlier target; only the final joint replay contributes rows to the report.
        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        let mut work = Vec::new();
        push_upgrade_halves(&mut work, lock_changes);
        while let Some(group) = work.pop() {
            let mut trial_changes = accepted.clone();
            trial_changes.extend(group.iter().cloned());
            let result = self
                .try_upgrade_group(
                    trial_changes,
                    &baseline.baseline_violations,
                    state,
                    rollback,
                )
                .await;
            match result {
                UpgradeTrialResult::Settled(_) => {
                    let mut outcome = BatchOutcome::default();
                    if !self.restore_upgrade_trial(rollback, baseline, state, &mut outcome) {
                        return UpgradeSelectionResult::Aborted(outcome);
                    }
                    accepted.extend(group);
                }
                UpgradeTrialResult::PolicyBlocked(violations) => {
                    let mut outcome = BatchOutcome::default();
                    if !self.restore_upgrade_trial(rollback, baseline, state, &mut outcome) {
                        return UpgradeSelectionResult::Aborted(outcome);
                    }
                    if group.len() > 1 {
                        push_upgrade_halves(&mut work, group);
                    } else {
                        rejected
                            .extend(group.into_iter().map(|change| (change, violations.clone())));
                    }
                }
                UpgradeTrialResult::Aborted(mut outcome) => {
                    self.restore_upgrade_trial(rollback, baseline, state, &mut outcome);
                    return UpgradeSelectionResult::Aborted(outcome);
                }
            }
        }

        UpgradeSelectionResult::Selected { accepted, rejected }
    }

    /// Replays the accepted candidates jointly from the restored baseline — the only trial whose
    /// outcomes reach the report and the committed lock.
    ///
    /// The accepted set's final composition always equals the last settled selection trial, so the
    /// replay normally settles too. A replay that still blocks (the registry moved between trials)
    /// fails closed: the baseline is restored and the accepted candidates report as held rather
    /// than committing a lock no trial verified.
    async fn replay_selected_upgrade_changes(
        &mut self,
        accepted: Vec<Change>,
        rejected: Vec<RejectedUpgrade>,
        baseline: &TrialState,
        state: &mut TrialState,
        rollback: &mut ProjectMutationJournal,
    ) {
        match self
            .try_upgrade_group(
                accepted.clone(),
                &baseline.baseline_violations,
                state,
                rollback,
            )
            .await
        {
            UpgradeTrialResult::Settled(outcomes) => {
                self.merge_batch_outcomes(outcomes);
                self.record_rejected_upgrade_changes(rejected);
            }
            UpgradeTrialResult::PolicyBlocked(violations) => {
                let mut outcome = BatchOutcome::default();
                if !self.restore_upgrade_trial(rollback, baseline, state, &mut outcome) {
                    self.merge_batch_outcome(outcome);
                    return;
                }
                self.record_unreconciled_skips(&accepted, &violations);
                self.record_rejected_upgrade_changes(rejected);
            }
            UpgradeTrialResult::Aborted(mut outcome) => {
                self.restore_upgrade_trial(rollback, baseline, state, &mut outcome);
                self.merge_batch_outcome(outcome);
                self.record_rejected_upgrade_changes(rejected);
            }
        }
    }

    /// Runs one policy trial: applies `changes` as one resolver batch, reconciles the floated
    /// transitives, then judges the residual violations against the fixed `policy_baseline`.
    ///
    /// Every mutation is captured into `rollback` (first snapshot per path), so the caller can
    /// restore the pre-trial worktree no matter how far the trial got. A settled trial's outcomes
    /// stay unmerged — the caller decides whether this trial is the one that commits — and an
    /// aborted trial surfaces only its errors, because its other rows describe a lock the restore
    /// discards.
    async fn try_upgrade_group(
        &mut self,
        changes: Vec<Change>,
        policy_baseline: &HashSet<(String, String)>,
        state: &mut TrialState,
        rollback: &mut ProjectMutationJournal,
    ) -> UpgradeTrialResult {
        let mut pending = Vec::new();
        let lock_outcome = self
            .apply_batch_with_rollback(changes, state, Some(rollback))
            .await;
        // Skip reconciliation when the lock upgrade made no clean forward progress: nothing floated
        // up, and a broken re-lock probe must not be re-hit.
        let upgraded_cleanly =
            lock_outcome.applied_count() > 0 && lock_outcome.errored_count() == 0;
        Self::advance_trial_state(&lock_outcome, state);
        let lock_committed = lock_outcome.committed.is_some();
        pending.push(lock_outcome);
        if pending.iter().any(|outcome| outcome.errored_count() > 0) {
            return UpgradeTrialResult::Aborted(trial_errors(pending));
        }
        if !lock_committed {
            return UpgradeTrialResult::Settled(pending);
        }
        if self.transitive_mode() == TransitiveGate::Enforce
            && upgraded_cleanly
            && state.reconcile_needed
        {
            pending.extend(self.reconcile_to_fixpoint(state, rollback).await);
        }
        if pending.iter().any(|outcome| outcome.errored_count() > 0) {
            return UpgradeTrialResult::Aborted(trial_errors(pending));
        }

        if self.transitive_mode() != TransitiveGate::Enforce {
            return UpgradeTrialResult::Settled(pending);
        }
        // A pre-existing dirty package may move between fresh versions, but an additional fresh
        // version line for that package is still a new residual.
        let residual = newly_introduced_violations(policy_baseline, &state.baseline_violations);
        if residual.is_empty() {
            return UpgradeTrialResult::Settled(pending);
        }
        UpgradeTrialResult::PolicyBlocked(residual)
    }

    /// Collapse this project's multi-leg applied rows into net rows (see [`collapse_applied_legs`]).
    fn collapse_collateral(&mut self, prior_violations: &HashSet<(String, String)>) {
        let project = self.project_label.clone();
        let tool = self.ctx.tool_name();
        let classifier = self.ctx.reader;
        collapse_applied_legs(
            &mut self.acc.items,
            &project,
            tool,
            prior_violations,
            |from, to| classifier.classify_update_kind(from, to),
        );
    }

    /// Fold a batch's report into the run accumulator. Report-only: trial state advances exactly
    /// once, via [`advance_trial_state`](Self::advance_trial_state) right after the batch runs, so
    /// merging (which may happen later, after buffering) can never re-apply or clobber it.
    fn merge_batch_outcome(&mut self, outcome: BatchOutcome) {
        self.lock_refreshed_by_apply |= outcome.lock_refreshed;
        outcome.merge_into(self.acc);
    }

    fn merge_batch_outcomes(&mut self, outcomes: impl IntoIterator<Item = BatchOutcome>) {
        for outcome in outcomes {
            self.merge_batch_outcome(outcome);
        }
    }

    /// Advance the trial state with a committed batch's after-graph. Called exactly once per
    /// outcome, immediately after [`apply_batch`](Self::apply_batch) returns; a rolled-back run
    /// resets the state explicitly instead of un-applying outcomes.
    fn advance_trial_state(outcome: &BatchOutcome, state: &mut TrialState) {
        if let Some(committed) = &outcome.committed {
            state
                .baseline_violations
                .clone_from(&committed.violations_after);
            state.reconcile_needed = committed.reconcile_needed;
        }
    }

    /// Reports each change of a policy-blocked trial as held by the transitive it would force
    /// into the graph.
    fn record_unreconciled_skips(&mut self, changes: &[Change], residual: &[(String, String)]) {
        self.acc.strict_incomplete = true;
        // Name one stuck transitive as the offender (sorted for a stable report).
        let offender = residual.iter().map(|(name, _)| name.clone()).min();
        for change in changes {
            self.record_change_skip(
                change,
                Some(SkippedInfo {
                    reason: SkipReason::TransitiveInCooldown,
                    message: conflict_skip_message(
                        SkipReason::TransitiveInCooldown,
                        offender.as_deref(),
                        &change.package.name,
                    ),
                    offending: offender.clone(),
                }),
            );
        }
    }

    fn record_rejected_upgrade_changes(&mut self, rejected: Vec<(Change, Vec<(String, String)>)>) {
        for (change, violations) in rejected {
            self.record_unreconciled_skips(std::slice::from_ref(&change), &violations);
        }
    }

    /// Restores the fixed pre-lock worktree snapshot and executor baseline after a trial.
    ///
    /// A restore failure leaves the worktree in no known state, so it is pushed as an error and
    /// the caller must stop recovering instead of running further trials.
    fn restore_upgrade_trial(
        &self,
        snapshot: &ProjectMutationJournal,
        baseline: &TrialState,
        state: &mut TrialState,
        outcome: &mut BatchOutcome,
    ) -> bool {
        match snapshot.restore(&self.ctx.pctx.project.root) {
            Ok(()) => {
                state.clone_from(baseline);
                true
            }
            Err(error) => {
                outcome.errors.push(self.project_diag(&error, None));
                false
            }
        }
    }

    async fn scoped_deps(&mut self) -> Option<Vec<Dependency>> {
        let scope = candidate_scope(self.mode);
        let mut deps = match self
            .ws
            .dependencies_in_scope(self.ctx.reader, self.ctx.pctx, scope, self.ctx.opts)
            .await
        {
            Ok(deps) => deps,
            Err(error) => {
                self.record_project_error(&error, None);
                return None;
            }
        };
        // Build-backend requirements (`[build-system].requires`) have no lock entry; `upgrade` adopts
        // them by raising the requirement floor like Dependabot. `fix` leaves them alone — it
        // remediates the resolved lock graph, which never contains the build backend, so there is
        // nothing to downgrade.
        if matches!(self.mode, PlanMode::Upgrade) {
            match self
                .ws
                .manifest_constraints_in_scope(self.ctx.reader, self.ctx.pctx, self.ctx.opts)
                .await
            {
                Ok(constraints) => {
                    // Remember which packages are manifest-only so `run_upgrade` applies them in their
                    // own batch: their floor raise has no lock interaction and must not be rolled back
                    // by an unrelated lock-resolve conflict in the same run.
                    self.manifest_only
                        .extend(constraints.iter().map(|dep| dep.package.clone()));
                    deps.extend(constraints);
                }
                // A build-system read failure is non-fatal: the build backend is an optional additive
                // surface, so warn and continue with the resolved deps rather than failing the project
                // — matching `outdated`, which records the identical failure as a warning.
                Err(error) => tracing::warn!(
                    project = %self.project_label,
                    error = %error,
                    "could not read build-system requirements; skipping build-backend candidates"
                ),
            }
        }
        Some(deps)
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
        sort_planned_changes(&mut planned);
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
        let mut errors = Vec::new();
        for (dep, releases) in fetched {
            let releases = match releases {
                Ok(releases) => releases,
                Err(error) => {
                    errors.push(self.project_diag(&error, Some(&dep.package.name)));
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
            planned.push(fix_change(&releases, &dep, target, kind));
        }
        sort_planned_changes(&mut planned);
        FixPlan {
            changes: planned,
            warnings,
            errors,
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
            let FixPlan {
                changes,
                warnings,
                errors,
            } = self
                .plan_fix_changes(&deps, transitive, downgrade_pinned)
                .await;
            self.acc.errors.extend(errors);
            if changes.is_empty() {
                self.emit_fix_warnings(warnings);
                return;
            }
            let outcome = self.apply_batch(changes, state).await;
            let applied = outcome.applied_count();
            Self::advance_trial_state(&outcome, state);
            self.merge_batch_outcome(outcome);
            if applied == 0 {
                self.emit_fix_warnings(warnings);
                return;
            }
            let Some(next) = self.scoped_deps().await else {
                return;
            };
            deps = next;
        }
    }

    /// Downgrade any too-fresh transitive a forward `upgrade` move floated up, to a fixpoint — the
    /// `fix` half of a single-pass `upgrade`. Each downgrade batch is applied, re-locked, and
    /// verified like the forward batch that made it necessary.
    ///
    /// `reconcile_needed` gates only **entry**; the rounds then run to a fixpoint on progress, like
    /// [`fix_to_fixpoint`](Self::fix_to_fixpoint). A round's downgrades can make a violation that
    /// was graph-held plannable (maturing `zbus_macros` down lowers the floor its `^` requirement
    /// put under `zbus_names`), and that unblocking raises no *new* violation — so re-arming on new
    /// violations alone would stop after one round and leave the now-plannable violation fresh, for
    /// the final residual gate to then roll the whole batch back.
    async fn reconcile_to_fixpoint(
        &mut self,
        state: &mut TrialState,
        rollback: &mut ProjectMutationJournal,
    ) -> Vec<BatchOutcome> {
        let mut outcomes = Vec::new();
        if !state.reconcile_needed {
            return outcomes;
        }
        state.reconcile_needed = false;
        for _ in 0..MAX_FIX_ROUNDS {
            self.ctx.opts.progress.say(&format!(
                "Reconciling transitive cooldown violations in {} ({})…",
                self.project_label(),
                self.ctx.pctx.tool
            ));
            let deps = match self.read_reconcile_deps().await {
                Ok(deps) => deps,
                Err(error) => {
                    let mut outcome = BatchOutcome::default();
                    outcome.errors.push(self.project_diag(&error, None));
                    outcomes.push(outcome);
                    return outcomes;
                }
            };
            let FixPlan {
                changes,
                warnings,
                errors,
            } = self
                .plan_fix_changes(&deps, TransitiveGate::Enforce, false)
                .await;
            if !errors.is_empty() {
                outcomes.push(BatchOutcome {
                    errors,
                    strict_incomplete: true,
                    ..BatchOutcome::default()
                });
                return outcomes;
            }
            if changes.is_empty() {
                outcomes.push(self.fix_warnings_outcome(warnings));
                return outcomes;
            }
            let outcome = self
                .apply_batch_with_rollback(changes, state, Some(rollback))
                .await;
            let applied = outcome.applied_count();
            Self::advance_trial_state(&outcome, state);
            outcomes.push(outcome);
            if applied == 0 {
                outcomes.push(self.fix_warnings_outcome(warnings));
                return outcomes;
            }
        }
        outcomes
    }

    fn emit_fix_warnings(&mut self, warnings: Vec<FixWarning>) {
        self.fix_warnings_outcome(warnings).merge_into(self.acc);
    }

    fn fix_warnings_outcome(&self, warnings: Vec<FixWarning>) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();
        for warning in warnings {
            self.add_fix_warning_to_outcome(&mut outcome, &warning.message, &warning.package);
        }
        outcome
    }

    fn add_fix_warning_to_outcome(&self, outcome: &mut BatchOutcome, message: &str, package: &str) {
        outcome.strict_incomplete = true;
        outcome
            .warnings
            .push(self.fix_warning_diag(message, package));
    }

    fn fix_warning_diag(&self, message: &str, package: &str) -> Diagnostic {
        Diagnostic::new(DiagnosticKind::Held, message.to_string())
            .with_tool(self.ctx.tool_name())
            .with_project(self.project_label.clone())
            .with_package(package)
    }

    /// Applies one trial's planned changes as one resolver batch under one rollback journal.
    ///
    /// A whole-graph resolver settles candidate interactions in one consistent lock. The caller may
    /// restore and partition a policy-rejected multi-change trial, but this method's outcome remains
    /// invisible to the report until the caller keeps and merges it.
    async fn apply_batch(&mut self, changes: Vec<Change>, state: &TrialState) -> BatchOutcome {
        self.apply_batch_with_rollback(changes, state, None).await
    }

    async fn apply_batch_with_rollback(
        &mut self,
        changes: Vec<Change>,
        state: &TrialState,
        rollback: Option<&mut ProjectMutationJournal>,
    ) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();
        if changes.is_empty() {
            return outcome;
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
                outcome
                    .errors
                    .push(self.project_diag(&error, Some(&primary)));
                return outcome;
            }
        };
        if let Some(rollback) = rollback {
            preserve_rollback_entries(rollback, &journal);
        }

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
                self.restore_journal_into_outcome(&journal, &mut outcome);
                self.add_change_errors(&mut outcome, &error, &changes);
                return outcome;
            }
        };
        if report.applied.is_empty() {
            self.add_batch_skips(&mut outcome, report.skipped);
            self.restore_journal_into_outcome(&journal, &mut outcome);
            return outcome;
        }
        let report = match self.verify_apply_report(report, &changes).await {
            Ok(report) => report,
            Err(error) => {
                self.restore_journal_into_outcome(&journal, &mut outcome);
                self.add_change_errors(&mut outcome, &error, &changes);
                return outcome;
            }
        };

        let report = self.classify_batch_report(report, &changes, &mut outcome);
        if !report.planned_applied {
            // No requested target landed: roll any incidental resolver movement back to the captured
            // state instead of committing a collateral-only mutation.
            self.restore_journal_into_outcome(&journal, &mut outcome);
            return outcome;
        }

        let Some(committed) = self
            .verify_batch_graph(
                &mut outcome,
                &changes,
                &report.applied,
                &state.baseline_violations,
            )
            .await
        else {
            self.restore_journal_into_outcome(&journal, &mut outcome);
            return outcome;
        };

        self.commit_batch_report(
            &mut outcome,
            &changes,
            &report.collateral,
            &report.applied,
            committed,
        );
        outcome
    }

    fn classify_batch_report(
        &self,
        report: ApplyReport,
        changes: &[Change],
        outcome: &mut BatchOutcome,
    ) -> VerifiedBatchReport {
        let applied: HashSet<ChangeTargetKey> =
            report.applied.iter().map(change_target_key).collect();
        let planned_applied = planned_changes_landed(changes, &applied);
        // Net version changes the resolve forced beyond the plan's own claimed rows (a transitive
        // pushed backward for consistency, matured down by a downgrade, or a held candidate's real
        // float off its baseline). These are part of the committed lock and must be surfaced, never
        // silent — the whole point of the full-lock-diff report. They are recorded as applied rows
        // once the batch commits below.
        let collateral = collateral_rows(&report.applied, changes);
        self.add_batch_skips(outcome, report.skipped);
        VerifiedBatchReport {
            applied,
            collateral,
            planned_applied,
        }
    }

    async fn verify_batch_graph(
        &self,
        outcome: &mut BatchOutcome,
        changes: &[Change],
        applied: &HashSet<ChangeTargetKey>,
        baseline_violations: &HashSet<(String, String)>,
    ) -> Option<CommittedBatch> {
        self.ctx.opts.progress.say(&format!(
            "Checking resolved graph cooldown after apply in {} ({})…",
            self.project_label(),
            self.ctx.pctx.tool
        ));
        let after = match self.graph_violations().await {
            Ok(after) => after,
            Err(error) => {
                self.add_change_errors(
                    outcome,
                    &error,
                    changes
                        .iter()
                        .filter(|change| applied.contains(&change_target_key(change))),
                );
                return None;
            }
        };
        let after_keys: HashSet<(String, String)> = after.keys().cloned().collect();
        match self.gate_batch_transitives(
            outcome,
            &after,
            &after_keys,
            changes,
            applied,
            baseline_violations,
        ) {
            TransitiveGateVerdict::RollBack => None,
            TransitiveGateVerdict::Keep { reconcile_needed } => Some(CommittedBatch {
                violations_after: after_keys,
                reconcile_needed,
            }),
        }
    }

    fn commit_batch_report(
        &self,
        outcome: &mut BatchOutcome,
        changes: &[Change],
        collateral: &[Change],
        applied: &HashSet<ChangeTargetKey>,
        committed: CommittedBatch,
    ) {
        outcome.lock_refreshed = self.ctx.writer.successful_apply_proves_lock_current();
        outcome.committed = Some(committed);
        for change in changes {
            if applied.contains(&change_target_key(change)) {
                outcome.items.push(self.change_applied_item(change));
            }
        }
        for change in collateral {
            outcome.items.push(self.change_applied_item(change));
        }
    }

    async fn verify_apply_report(
        &self,
        report: ApplyReport,
        planned: &[Change],
    ) -> cooldown_core::Result<ApplyReport> {
        let mut deps = self
            .ctx
            .reader
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await?;
        // A build-backend requirement ([build-system].requires) is adopted by raising its floor in the
        // manifest, never by a lock move, so the lock-driven graph never shows the new version. Re-read
        // it from the now-rewritten manifest — its `current` is the raised floor — so a build-backend
        // bump verifies as reached instead of being mistaken for a resolver conflict. Only `upgrade`
        // plans build changes, and the read is best-effort: an unreadable build-system table must not
        // roll back an otherwise-valid batch (`dependencies` tolerates the same parse failure), so the
        // call is gated to upgrade mode and its error swallowed.
        if matches!(self.mode, PlanMode::Upgrade)
            && let Ok(constraints) = self
                .ctx
                .reader
                .manifest_constraints(&self.ctx.pctx.project)
                .await
        {
            deps.extend(constraints);
        }
        Ok(verify_applied_targets(report, planned, &deps))
    }

    /// Record each held candidate (uv lowered it below its ceiling, or the resolve rejected it) as a
    /// skip, naming the package that blocks it via [`conflict_skip_message`].
    fn add_batch_skips(&self, outcome: &mut BatchOutcome, skipped: Vec<cooldown_core::Skipped>) {
        for skipped in skipped {
            let offending = skipped.offending.map(|package| package.name);
            // A multi-version dependency held within its own line is conservative-correct, not a
            // failed upgrade — like `NeedsMajor` it must not fail a `--strict` run.
            if skipped.reason != SkipReason::MultiVersionHeld {
                outcome.strict_incomplete = true;
            }
            let change = skipped.change;
            outcome.items.push(self.change_skip_item(
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
            ));
        }
    }

    /// The transitive-cooldown gate over a committed batch. The joint resolve may drag a fresh
    /// transitive into the graph; how we react follows the transitive mode: `Hide` ignores
    /// transitives; `Allow` keeps the lock and reports them; `Enforce` reconciles forward `upgrade`
    /// batches optimistically, while backward `fix` batches still roll back immediately when a new
    /// violation has no lower graph floor to try. `outcome` receives only report rows (warnings and
    /// skip items); the state consequences travel in the returned [`TransitiveGateVerdict`].
    fn gate_batch_transitives(
        &self,
        outcome: &mut BatchOutcome,
        after: &HashMap<(String, String), bool>,
        after_keys: &HashSet<(String, String)>,
        changes: &[Change],
        applied: &HashSet<ChangeTargetKey>,
        baseline_violations: &HashSet<(String, String)>,
    ) -> TransitiveGateVerdict {
        let keep = |reconcile_needed| TransitiveGateVerdict::Keep { reconcile_needed };
        let new_violations: Vec<&(String, String)> =
            after_keys.difference(baseline_violations).collect();
        if new_violations.is_empty() {
            return keep(false);
        }
        match self.transitive_mode() {
            TransitiveGate::Hide => keep(false),
            TransitiveGate::Allow => {
                for (package, version) in &new_violations {
                    self.add_fix_warning_to_outcome(
                        outcome,
                        &format!(
                            "{package}@{version} is younger than its cooldown; left in place by --transitive allow"
                        ),
                        package,
                    );
                }
                keep(false)
            }
            TransitiveGate::Enforce => {
                // `upgrade` is optimistic: a forward move that floats a too-fresh transitive up is
                // never rolled back on a per-node prediction, because a cooled parent cannot require a
                // child newer than the cooldown window — an older version every requirer already
                // accepts exists by construction. Keep the lock and let the reconcile pass mature the
                // floated-up transitives down; `run_upgrade` makes a final gate check and rolls the
                // lock back only for a violation reconcile genuinely could not clear. `fix` stays
                // conservative: it moves *backward*, so a fresh transitive it cannot reduce here is a
                // real, unrecoverable conflict that must roll the batch back immediately.
                if matches!(self.mode, PlanMode::Upgrade) {
                    return keep(true);
                }
                let Some((forced_pkg, _)) = new_violations
                    .iter()
                    .find(|key| !after.get(**key).copied().unwrap_or(false))
                else {
                    // Every new violation is reconcilable; keep the lock and let the reconcile pass
                    // (after the fix loop) downgrade the floated-up transitives.
                    return keep(true);
                };
                outcome.strict_incomplete = true;
                for change in changes {
                    if applied.contains(&change_target_key(change)) {
                        outcome.items.push(self.change_skip_item(
                            change,
                            Some(SkippedInfo {
                                reason: SkipReason::TransitiveInCooldown,
                                message: conflict_skip_message(
                                    SkipReason::TransitiveInCooldown,
                                    Some(forced_pkg),
                                    &change.package.name,
                                ),
                                offending: Some(forced_pkg.clone()),
                            }),
                        ));
                    }
                }
                TransitiveGateVerdict::RollBack
            }
        }
    }

    fn transitive_mode(&self) -> TransitiveGate {
        self.ctx.opts.transitive_mode
    }

    async fn read_reconcile_deps(&self) -> cooldown_core::Result<Vec<Dependency>> {
        // A scoped upgrade can float transitives that do not match `--package`; reconcile is the
        // safety pass over the post-apply graph, so it must see the raw graph like `graph_violations`.
        let mut deps = self
            .ctx
            .reader
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await?;
        deps.sort_by(|a, b| {
            a.package
                .name
                .cmp(&b.package.name)
                .then_with(|| a.current.to_string().cmp(&b.current.to_string()))
        });
        Ok(deps)
    }

    fn restore_journal_into_outcome(
        &self,
        journal: &cooldown_core::ProjectMutationJournal,
        outcome: &mut BatchOutcome,
    ) {
        if let Err(error) = journal.restore(&self.ctx.pctx.project.root) {
            outcome.errors.push(self.project_diag(&error, None));
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
    }

    /// The graph's too-fresh, non-baselined violations, each mapped to whether a conservative `fix`
    /// gate can prove it is reducible: the graph floor sits below the locked version, so a downgrade
    /// can try to roll it back without violating known lower bounds. `upgrade` uses this same set for
    /// the final residual check, but it no longer relies on the boolean prediction before attempting
    /// reconciliation.
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

    fn project_diag(&self, error: &cooldown_core::CoreError, package: Option<&str>) -> Diagnostic {
        diag_from_error(error, self.ctx.pctx.tool, self.project_label(), package)
    }

    fn record_project_error(&mut self, error: &cooldown_core::CoreError, package: Option<&str>) {
        self.acc.errors.push(self.project_diag(error, package));
    }

    fn add_change_errors<'c>(
        &self,
        outcome: &mut BatchOutcome,
        error: &cooldown_core::CoreError,
        changes: impl IntoIterator<Item = &'c Change>,
    ) {
        for change in changes {
            let diag = self.project_diag(error, Some(&change.package.name));
            outcome.items.push(self.change_error_item(change, diag));
        }
    }

    fn change_applied_item(&self, change: &Change) -> UpgradeItem {
        plan_item(
            change,
            &self.project_label,
            self.ctx.tool_name(),
            true,
            None,
        )
    }

    fn change_error_item(&self, change: &Change, diag: Diagnostic) -> UpgradeItem {
        let mut item = plan_item(
            change,
            &self.project_label,
            self.ctx.tool_name(),
            false,
            None,
        );
        item.error = Some(diag);
        item
    }

    fn change_skip_item(&self, change: &Change, skipped: Option<SkippedInfo>) -> UpgradeItem {
        plan_item(
            change,
            &self.project_label,
            self.ctx.tool_name(),
            false,
            skipped,
        )
    }

    fn record_change_skip(&mut self, change: &Change, skipped: Option<SkippedInfo>) {
        let item = self.change_skip_item(change, skipped);
        self.acc.items.push(item);
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

/// Splits `changes` in two and pushes the halves so the left half is processed first (LIFO).
fn push_upgrade_halves(work: &mut Vec<Vec<Change>>, mut changes: Vec<Change>) {
    let right = changes.split_off(changes.len() / 2);
    if !right.is_empty() {
        work.push(right);
    }
    if !changes.is_empty() {
        work.push(changes);
    }
}

/// Distills an aborted trial's outcomes down to their errors — the only rows that may outlive
/// the rollback, since applied or skipped rows would describe a lock the restore discards.
fn trial_errors(outcomes: Vec<BatchOutcome>) -> BatchOutcome {
    let mut errors = BatchOutcome::default();
    for outcome in outcomes {
        errors.errors.extend(outcome.errors);
        errors.items.extend(
            outcome
                .items
                .into_iter()
                .filter(|item| item.error.is_some()),
        );
    }
    errors.strict_incomplete = errors.errored_count() > 0;
    errors
}

/// Folds a batch journal into the trial-wide rollback journal, keeping the first snapshot per
/// path.
fn preserve_rollback_entries(
    rollback: &mut ProjectMutationJournal,
    journal: &ProjectMutationJournal,
) {
    for file in &journal.files {
        if rollback
            .files
            .iter()
            .all(|captured| captured.path != file.path)
        {
            // By the mutation-journal contract, an earlier plan could not have changed a path it did
            // not capture. Its first appearance therefore still contains the pre-trial bytes even
            // when a reconciliation plan expands the write set.
            rollback.files.push(file.clone());
        }
    }
}

/// Restores deterministic mutation order after concurrent registry fetches return in completion
/// order. Adapters must tolerate any order, but a stable plan keeps conflict winners reproducible.
fn sort_planned_changes(changes: &mut [Change]) {
    changes.sort_by(|a, b| {
        a.package
            .name
            .cmp(&b.package.name)
            .then_with(|| a.package.registry.cmp(&b.package.registry))
            .then_with(|| a.from.as_str().cmp(b.from.as_str()))
            .then_with(|| a.to.as_str().cmp(b.to.as_str()))
            .then_with(|| {
                a.members
                    .iter()
                    .map(|member| (&member.name, &member.path))
                    .cmp(b.members.iter().map(|member| (&member.name, &member.path)))
            })
    });
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

/// The `fix` downgrade maturing `dep` back to `target`.
fn fix_change(releases: &[Release], dep: &Dependency, target: Version, kind: UpdateKind) -> Change {
    // A cross-major downgrade (Go `/v3` → `/v2`, or `/v2` → the v1 base path) changes the import
    // path; `target_package_for` reconstructs it (a no-op for same-major moves and for tools whose
    // name is stable across majors).
    Change {
        package: target_package_for(releases, dep, &target),
        from: dep.current.clone(),
        to: target,
        kind,
        // `fix` only ever rolls a too-fresh pin back.
        downgrade: true,
        direct: dep.direct,
        members: dep.members.clone(),
    }
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
    if change.direct && !change.members.is_empty() {
        return change.members.iter().all(|member| {
            let reached = deps.iter().any(|dep| {
                dep.package == change.package
                    && dep.current == change.to
                    && dep.members.iter().any(|dep_member| dep_member == member)
            });
            // The planned line must actually have moved. A member that declares the crate twice
            // (`[dependencies] toml = "1"` beside `[build-dependencies] toml = "0.5"`) reaches the
            // target through the sibling entry while the planned old-major line sits untouched;
            // counting that as applied reports an upgrade the lock never took, forever. A direct
            // node still at the from version attributed to this member is that untouched line (a
            // from node the member only *reaches* transitively is fine — its own edges moved).
            let from_line_remains = deps.iter().any(|dep| {
                dep.direct
                    && dep.package == change.package
                    && dep.current == change.from
                    && dep.members.iter().any(|dep_member| dep_member == member)
            });
            reached && !from_line_remains
        });
    }

    deps.iter()
        .any(|dep| dep.package == change.package && dep.current == change.to)
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

/// The applied rows the plan did not itself claim — the collateral movement recorded as its own
/// applied rows when the batch commits.
///
/// Filtered by exact change identity ([`change_target_key`]), not by package: a held candidate's
/// package is planned, but the row reporting its real off-target float is not, and a package-level
/// filter would silently drop that movement behind the held skip.
fn collateral_rows(applied: &[Change], planned: &[Change]) -> Vec<Change> {
    let planned: HashSet<ChangeTargetKey> = planned.iter().map(change_target_key).collect();
    applied
        .iter()
        .filter(|change| !planned.contains(&change_target_key(change)))
        .cloned()
        .collect()
}

fn newly_introduced_violations(
    before: &HashSet<(String, String)>,
    after: &HashSet<(String, String)>,
) -> Vec<(String, String)> {
    let before_counts = violation_counts_by_name(before);
    let after_counts = violation_counts_by_name(after);
    let mut residual: Vec<(String, String)> = after
        .difference(before)
        .filter(|(name, _)| {
            after_counts.get(name.as_str()).copied().unwrap_or(0)
                > before_counts.get(name.as_str()).copied().unwrap_or(0)
        })
        .cloned()
        .collect();
    residual.sort();
    residual
}

fn violation_counts_by_name(violations: &HashSet<(String, String)>) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for (name, _) in violations {
        *counts.entry(name.as_str()).or_default() += 1;
    }
    counts
}

/// Collapse the several applied rows one package accrues across the optimistic forward batch and the
/// reconcile pass of an `upgrade` into a single net row, in place. A transitive the upgrade floats up
/// and then matures back down records a leg for each (`quote 1.0.44→1.0.46`, then `1.0.46→1.0.45`);
/// the report must show only the net change against the committed lock (`quote 1.0.44→1.0.45`), never
/// the phantom intermediate.
///
/// Legs are linked into **contiguous chains** — a leg extends a chain whose tail `to` equals this
/// leg's `from` — so only the legs of one moving node fold together. Two coexisting version lines of a
/// crate (cargo keeps e.g. `serde 0.9` and `serde 1.0`) share a `(name, registry)` key but their
/// versions do not chain, so each stays its own row. A chain that lands exactly back where it started
/// is dropped (no net move). The net row's direction is recomputed: the move is a downgrade if its
/// first leg already moved backward (a forced collateral downgrade) or if its start version was an
/// unacknowledged too-fresh violation before this batch (`prior_violations`) — in which case the
/// reconcile, which only matures *down*, settled the line below the start. Otherwise the start was
/// matured and the reconcile lands at or above it, so the net is a forward move. The net row's kind is
/// reclassified through the adapter when it can classify the collapsed `from -> to` pair; otherwise
/// the first leg's kind is kept. Scoped to one `(project, tool)`; only applied (non-skipped,
/// non-errored) rows merge. Runs before the report is sorted, so the rows are still in chronological
/// leg order.
fn collapse_applied_legs(
    items: &mut Vec<UpgradeItem>,
    project: &str,
    tool: &str,
    prior_violations: &HashSet<(String, String)>,
    classify_update_kind: impl Fn(&str, &str) -> Option<UpdateKind>,
) {
    let mut groups: HashMap<(String, Option<String>), Vec<usize>> = HashMap::new();
    for (idx, item) in items.iter().enumerate() {
        let applied = item.applied && item.skipped.is_none() && item.error.is_none();
        if applied && item.project == project && item.tool == tool {
            groups
                .entry((item.name.clone(), item.registry.clone()))
                .or_default()
                .push(idx);
        }
    }
    let mut remove: HashSet<usize> = HashSet::new();
    // (first_leg_idx, net_to, net_is_downgrade, net_kind) for each chain that folds into a net row.
    let mut retarget: Vec<(usize, String, bool, UpdateKind)> = Vec::new();
    for indices in groups.values() {
        // Link legs into contiguous chains: a leg joins the chain whose tail `to` equals its `from`.
        let mut chains: Vec<Vec<usize>> = Vec::new();
        for &idx in indices {
            let Some(from) = items.get(idx).map(|item| item.from.clone()) else {
                continue;
            };
            let slot = chains.iter_mut().find(|chain| {
                chain
                    .last()
                    .and_then(|&tail| items.get(tail))
                    .is_some_and(|tail| tail.to == from)
            });
            match slot {
                Some(chain) => chain.push(idx),
                None => chains.push(vec![idx]),
            }
        }
        for chain in &chains {
            let (Some(&first), Some(&last)) = (chain.first(), chain.last()) else {
                continue;
            };
            if first == last {
                continue; // a single-leg chain keeps its row untouched
            }
            let (Some(head), Some(net_to)) = (
                items.get(first),
                items.get(last).map(|item| item.to.clone()),
            ) else {
                continue;
            };
            if head.from == net_to {
                // Floated out and back: no net move, drop the whole chain.
                remove.extend(chain.iter().copied());
                continue;
            }
            let downgrade = head.downgrade
                || prior_violations.contains(&(head.name.clone(), head.from.clone()));
            let kind = classify_update_kind(&head.from, &net_to).unwrap_or(head.kind);
            retarget.push((first, net_to, downgrade, kind));
            remove.extend(chain.iter().skip(1).copied());
        }
    }
    for (first, net_to, downgrade, kind) in retarget {
        if let Some(head) = items.get_mut(first) {
            head.to = net_to;
            head.downgrade = downgrade;
            head.kind = kind;
        }
    }
    if remove.is_empty() {
        return;
    }
    *items = std::mem::take(items)
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !remove.contains(idx))
        .map(|(_, item)| item)
        .collect();
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
        // Name the stuck transitive: without it the report says only that *some* dependency is too
        // fresh, leaving no lead on what to baseline or wait out.
        (SkipReason::TransitiveInCooldown, Some(offending)) => {
            format!("{} ({offending})", reason.message())
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

#[cfg(test)]
mod tests {
    use super::{
        PlanMode, candidate_scope, collapse_applied_legs, collateral_rows, combine_lock_status,
        conflict_skip_message, is_downgrade, newly_introduced_violations, planned_changes_landed,
        preserve_rollback_entries, sort_planned_changes, target_package, verify_applied_targets,
    };
    use crate::app::{TransitiveGate, UpgradeItem};
    use cooldown_core::{
        ApplyReport, Change, DepScope, Dependency, LockStatus, MajorKey, MemberRef, PackageId,
        ProjectMutationFile, ProjectMutationJournal, Release, ReleaseOrder, ReleaseQuality,
        SkipReason, ToolId, UpdateKind, Version,
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
    fn rollback_journal_keeps_the_first_snapshot_for_each_path() {
        let mut rollback = ProjectMutationJournal {
            files: vec![ProjectMutationFile {
                path: "package-lock.json".into(),
                contents: Some(b"baseline lock".to_vec()),
            }],
        };
        let later = ProjectMutationJournal {
            files: vec![
                ProjectMutationFile {
                    path: "package-lock.json".into(),
                    contents: Some(b"trial lock".to_vec()),
                },
                ProjectMutationFile {
                    path: "package.json".into(),
                    contents: Some(b"baseline manifest".to_vec()),
                },
            ],
        };

        preserve_rollback_entries(&mut rollback, &later);

        assert_eq!(rollback.files.len(), 2);
        assert_eq!(
            rollback.files[0].contents.as_deref(),
            Some(b"baseline lock".as_slice())
        );
        assert_eq!(rollback.files[1].path, "package.json");
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

    fn member(name: &str, path: &str) -> MemberRef {
        MemberRef {
            name: name.to_string(),
            path: path.to_string(),
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
    fn planned_changes_restore_stable_order_after_concurrent_fetches() {
        let mut changes = vec![
            change("referencing", "0.46.5", "0.46.6"),
            change("jsonschema", "0.46.5", "0.46.6"),
        ];

        sort_planned_changes(&mut changes);

        assert_eq!(
            changes
                .iter()
                .map(|change| change.package.name.as_str())
                .collect::<Vec<_>>(),
            vec!["jsonschema", "referencing"]
        );
    }

    #[test]
    fn collateral_rows_keep_a_held_candidates_real_movement() {
        let planned = vec![change("referencing", "0.46.5", "0.46.6")];
        // The adapter reported the held candidate's real float (an off-target row for a planned
        // package) and an unplanned transitive move; the plan's own claimed row is the only one
        // excluded from collateral.
        let applied = vec![
            change("referencing", "0.46.5", "0.46.6"),
            change("referencing", "0.46.5", "0.46.10"),
            change("quote", "1.0.44", "1.0.45"),
        ];

        let collateral = collateral_rows(&applied, &planned);

        assert_eq!(
            collateral
                .iter()
                .map(|change| (change.package.name.as_str(), change.to.as_str()))
                .collect::<Vec<_>>(),
            vec![("referencing", "0.46.10"), ("quote", "1.0.45")],
            "a package-level filter would drop the held candidate's movement row"
        );
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
    fn verify_applied_targets_requires_the_planned_direct_member_to_land() {
        let mut planned_change = change("nix", "0.28.0", "0.31.3");
        planned_change.members = vec![member("micromux-mcp", "crates/micromux-mcp")];
        let planned = vec![planned_change.clone()];
        let report = ApplyReport {
            applied: planned.clone(),
            skipped: Vec::new(),
        };
        let mut old_dep = dep("nix", "0.28.0");
        old_dep.members = vec![member("micromux-mcp", "crates/micromux-mcp")];
        let mut other_member_dep = dep("nix", "0.31.3");
        other_member_dep.members = vec![member("micromux", "crates/micromux")];

        let verified = verify_applied_targets(report, &planned, &[old_dep, other_member_dep]);

        assert!(verified.applied.is_empty());
        assert_eq!(verified.skipped.len(), 1);
        assert_eq!(verified.skipped[0].change, planned_change);
    }

    #[test]
    fn verify_applied_targets_rejects_a_target_reached_through_a_sibling_entry() {
        // A member that declares the crate twice ([dependencies] toml = "1" beside
        // [build-dependencies] toml = "0.5") resolves the target version before the planned 0.5.x
        // move is even attempted. As long as the member's old direct line survives, the change has
        // not landed — the sibling edge must not verify it as applied (it would report an upgrade
        // the lock never took, on every run, forever).
        let mut planned_change = change("toml", "0.5.11", "1.1.2");
        planned_change.members = vec![member("rawloader", "rawloader")];
        let planned = vec![planned_change.clone()];
        let report = ApplyReport {
            applied: planned.clone(),
            skipped: Vec::new(),
        };
        let mut old_line = dep("toml", "0.5.11");
        old_line.members = vec![member("rawloader", "rawloader")];
        let mut new_line = dep("toml", "1.1.2");
        new_line.members = vec![member("rawloader", "rawloader")];

        let verified =
            verify_applied_targets(report, &planned, &[old_line.clone(), new_line.clone()]);

        assert!(verified.applied.is_empty());
        assert_eq!(verified.skipped.len(), 1);
        assert_eq!(verified.skipped[0].change, planned_change);

        // Once the old line is gone — or survives only transitively (another crate's requirement,
        // not the member's own entry) — the member's move has landed.
        let report = ApplyReport {
            applied: planned.clone(),
            skipped: Vec::new(),
        };
        old_line.direct = false;
        let verified = verify_applied_targets(report, &planned, &[old_line, new_line]);
        assert_eq!(verified.applied.len(), 1);
        assert!(verified.skipped.is_empty());
    }

    #[test]
    fn verify_applied_targets_keeps_each_held_member_when_targets_collide() {
        // Two members bump the same crate to the same target from different current versions, so the
        // changes share (name, registry, to) and differ only by member. Both are held; neither may
        // be dropped by a member-blind key collapsing them into a single skip.
        let mut held_a = change("nix", "0.28.0", "0.31.3");
        held_a.members = vec![member("app-a", "crates/app-a")];
        let mut held_b = change("nix", "0.30.0", "0.31.3");
        held_b.members = vec![member("app-b", "crates/app-b")];
        let planned = vec![held_a.clone(), held_b.clone()];
        let report = ApplyReport {
            applied: planned.clone(),
            skipped: Vec::new(),
        };
        let mut a_dep = dep("nix", "0.28.0");
        a_dep.members = vec![member("app-a", "crates/app-a")];
        let mut b_dep = dep("nix", "0.30.0");
        b_dep.members = vec![member("app-b", "crates/app-b")];

        let verified = verify_applied_targets(report, &planned, &[a_dep, b_dep]);

        assert!(verified.applied.is_empty());
        assert_eq!(verified.skipped.len(), 2);
        let held: Vec<&Change> = verified.skipped.iter().map(|skip| &skip.change).collect();
        assert!(held.contains(&&held_a));
        assert!(held.contains(&&held_b));
    }

    #[test]
    fn verify_applied_targets_does_not_split_transitive_attribution_by_member() {
        let mut held_a = change("shared", "1.0.0", "1.2.0");
        held_a.direct = false;
        held_a.members = vec![member("app-a", "crates/app-a")];
        let mut held_b = change("shared", "1.1.0", "1.2.0");
        held_b.direct = false;
        held_b.members = vec![member("app-b", "crates/app-b")];
        let planned = vec![held_a.clone(), held_b];
        let report = ApplyReport {
            applied: planned.clone(),
            skipped: Vec::new(),
        };

        let verified = verify_applied_targets(report, &planned, &[dep("shared", "1.0.0")]);

        assert!(verified.applied.is_empty());
        assert_eq!(verified.skipped.len(), 1);
        assert_eq!(verified.skipped[0].change, held_a);
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

    fn applied_item(name: &str, from: &str, to: &str, downgrade: bool) -> UpgradeItem {
        UpgradeItem {
            name: name.to_string(),
            tool: "cargo".to_string(),
            project: ".".to_string(),
            direct: false,
            downgrade,
            members: Vec::new(),
            registry: Some("crates.io".to_string()),
            from: from.to_string(),
            to: to.to_string(),
            kind: UpdateKind::Minor,
            applied: true,
            skipped: None,
            error: None,
        }
    }

    fn no_prior() -> HashSet<(String, String)> {
        HashSet::new()
    }

    fn violations(items: &[(&str, &str)]) -> HashSet<(String, String)> {
        items
            .iter()
            .map(|(name, version)| ((*name).to_string(), (*version).to_string()))
            .collect()
    }

    fn no_kind(_: &str, _: &str) -> Option<UpdateKind> {
        None
    }

    #[test]
    fn residual_gate_allows_a_pre_existing_violation_to_float_versions() {
        let before = violations(&[("t", "0.5.0")]);
        let after = violations(&[("t", "0.6.0")]);

        assert!(
            newly_introduced_violations(&before, &after).is_empty(),
            "one dirty version line stayed one dirty version line"
        );
    }

    #[test]
    fn residual_gate_flags_an_added_version_line_for_a_dirty_package() {
        let before = violations(&[("t", "0.5.0")]);
        let after = violations(&[("t", "0.5.0"), ("t", "1.0.0")]);

        assert_eq!(
            newly_introduced_violations(&before, &after),
            vec![("t".to_string(), "1.0.0".to_string())]
        );
    }

    #[test]
    fn residual_gate_flags_a_new_dirty_package() {
        let before = violations(&[("t", "0.5.0")]);
        let after = violations(&[("other", "2.0.0"), ("t", "0.5.0")]);

        assert_eq!(
            newly_introduced_violations(&before, &after),
            vec![("other".to_string(), "2.0.0".to_string())]
        );
    }

    #[test]
    fn collapse_merges_float_then_reconcile_into_a_net_forward_row() {
        // The forward batch floats `quote` up (collateral); the reconcile pass matures it back down.
        let mut items = vec![
            applied_item("quote", "1.0.44", "1.0.46", false),
            applied_item("quote", "1.0.46", "1.0.45", true),
        ];
        collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
        assert_eq!(items.len(), 1, "the two legs collapse to one net row");
        assert_eq!(items[0].from, "1.0.44");
        assert_eq!(items[0].to, "1.0.45");
        assert!(
            !items[0].downgrade,
            "the net move (1.0.44 → 1.0.45) is forward"
        );
    }

    #[test]
    fn collapse_reclassifies_kind_against_the_net_target_when_available() {
        // The first leg is a minor float-up, but the committed net row is only a patch move. The
        // report kind should describe the collapsed row, not the phantom intermediate.
        let mut items = vec![
            applied_item("quote", "1.0.0", "1.1.0", false),
            applied_item("quote", "1.1.0", "1.0.1", true),
        ];
        collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), |from, to| {
            if from == "1.0.0" && to == "1.0.1" {
                Some(UpdateKind::Patch)
            } else {
                None
            }
        });
        assert_eq!(items.len(), 1);
        assert_eq!(
            (items[0].from.as_str(), items[0].to.as_str()),
            ("1.0.0", "1.0.1")
        );
        assert_eq!(items[0].kind, UpdateKind::Patch);
        assert!(!items[0].downgrade);
    }

    #[test]
    fn collapse_drops_a_package_that_floats_up_then_fully_back() {
        let mut items = vec![
            applied_item("quote", "1.0.44", "1.0.46", false),
            applied_item("quote", "1.0.46", "1.0.44", true),
        ];
        collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
        assert!(
            items.is_empty(),
            "no net move: the package is dropped from the report"
        );
    }

    #[test]
    fn collapse_keeps_single_leg_rows_including_a_genuine_downgrade() {
        // A direct forward bump and a pre-existing too-fresh transitive matured down in one leg each
        // stay as they are — only multi-leg float-then-reconcile chains merge.
        let mut a = applied_item("a", "1.0.0", "1.1.0", false);
        a.direct = true;
        let mut items = vec![a, applied_item("referencing", "0.46.6", "0.46.5", true)];
        collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
        assert_eq!(items.len(), 2);
        let refr = items
            .iter()
            .find(|item| item.name == "referencing")
            .expect("referencing row");
        assert_eq!((refr.from.as_str(), refr.to.as_str()), ("0.46.6", "0.46.5"));
        assert!(refr.downgrade, "a single-leg downgrade is preserved");
    }

    #[test]
    fn collapse_does_not_merge_across_projects() {
        let mut first = applied_item("quote", "1.0.44", "1.0.46", false);
        first.project = "a".to_string();
        let mut second = applied_item("quote", "1.0.46", "1.0.45", true);
        second.project = "b".to_string();
        let mut items = vec![first, second];
        collapse_applied_legs(&mut items, "a", "cargo", &no_prior(), no_kind);
        // Only project `a` is in scope, and it has a single leg there, so nothing merges.
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn collapse_marks_a_net_downgrade_when_the_start_was_a_prior_violation() {
        // A pre-existing too-fresh `quote 1.0.5` floats up to 1.0.7, then the reconcile pass matures
        // its line down past the start to 1.0.4 — a genuine net downgrade, not a forward move.
        let mut items = vec![
            applied_item("quote", "1.0.5", "1.0.7", false),
            applied_item("quote", "1.0.7", "1.0.4", true),
        ];
        let prior: HashSet<(String, String)> =
            HashSet::from([("quote".to_string(), "1.0.5".to_string())]);
        collapse_applied_legs(&mut items, ".", "cargo", &prior, no_kind);
        assert_eq!(items.len(), 1);
        assert_eq!(
            (items[0].from.as_str(), items[0].to.as_str()),
            ("1.0.5", "1.0.4")
        );
        assert!(
            items[0].downgrade,
            "a net move below a too-fresh start is a downgrade, not an upgrade"
        );
    }

    #[test]
    fn collapse_does_not_merge_two_coexisting_majors_of_one_crate() {
        // cargo keeps two majors of `serde` in the lock; both bump in one run. They share the
        // (name, registry) key but their versions do not chain, so neither row is merged or dropped.
        let mut items = vec![
            applied_item("serde", "0.9.1", "0.9.3", false),
            applied_item("serde", "1.0.0", "1.0.5", false),
        ];
        collapse_applied_legs(&mut items, ".", "cargo", &no_prior(), no_kind);
        assert_eq!(items.len(), 2, "independent version lines stay distinct");
        let lines: HashSet<(String, String)> = items
            .iter()
            .map(|item| (item.from.clone(), item.to.clone()))
            .collect();
        assert!(lines.contains(&("0.9.1".to_string(), "0.9.3".to_string())));
        assert!(lines.contains(&("1.0.0".to_string(), "1.0.5".to_string())));
    }

    #[test]
    fn lock_status_aggregation_keeps_unknown_distinct_from_verified_current() {
        let current_then_unknown =
            combine_lock_status(Some(LockStatus::Current), LockStatus::Unknown);
        assert_eq!(current_then_unknown, LockStatus::Unknown);

        let unknown_then_stale = combine_lock_status(Some(LockStatus::Unknown), LockStatus::Stale);
        assert_eq!(unknown_then_stale, LockStatus::Stale);
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
