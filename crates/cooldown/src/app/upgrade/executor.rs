use super::{UpgradeAccum, UpgradeCtx};
mod batch;
mod planning;
mod report;
mod transitive_gate;

use self::batch::{
    BatchOutcome, CommittedBatch, VerifiedBatchReport, indeterminate_trial, trial_errors,
};
pub(crate) use self::planning::target_package_for;
use self::planning::{
    EffectiveHold, candidate_scope, dep_resolve_ctx, effective_hold, fix_change, is_downgrade,
    plan_baseline_violations, sort_planned_changes,
};
use self::report::{collapse_applied_legs, combine_lock_status, conflict_skip_message, plan_item};
use self::transitive_gate::{
    TransitiveGateVerdict, insert_graph_violation, newly_introduced_violations, package_label,
    violation_identity,
};
use crate::app::advisories::{ClassifiedAdvisories, ProjectAdvisories};
use crate::app::change_key::{ChangeTargetKey, change_target_key};
use crate::app::{
    FetchedRelease, SkippedInfo, TransitiveGate, UpgradeItem, Workspace, diag_from_error,
    recovery_diagnostics,
};
use cooldown_core::{
    ApplyReport, BaselineViolation, CeilingReason, Change, DepScope, Dependency, Diagnostic,
    DiagnosticKind, FixVerdict, LockStatus, PackageId, Plan, ProjectMutationJournal,
    ProjectMutationState, Release, ResolveContext, RewriteMode, SkipReason, Skipped, Status,
    UpdateKind, Version, check_pin_advised, evaluate_advised, evaluate_ceiling_hold_advised,
    evaluate_fix_advised,
};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

/// Whether the executor moves dependencies *forward* (`upgrade`) or *backward* to a compliant
/// version (`fix`). The trial/rollback/verify machinery is shared; only planning differs.
#[derive(Clone, Copy)]
pub(super) enum PlanMode {
    /// `upgrade`: move deps to the newest matured version — direct deps across majors when allowed,
    /// and matured in-range transitives the resolver would otherwise leave behind (a requirement
    /// only forces the minimum it needs, so a matured transitive can sit unadopted forever).
    Upgrade {
        /// How transitive deps are handled (`--transitive <mode>`): `Enforce` and `Allow` advance
        /// them within their major line, `Hide` narrows planning to direct deps.
        transitive: TransitiveGate,
    },
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

impl PlanMode {
    /// The `--transitive` gate this run plans and gates under — the single source both the
    /// candidate scope and the post-apply residual gate read, so a contradictory pairing is not
    /// constructible past the mode itself.
    const fn transitive_mode(self) -> TransitiveGate {
        match self {
            PlanMode::Upgrade { transitive } | PlanMode::Fix { transitive, .. } => transitive,
        }
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
    /// Warnings about planned changes, keyed by the change each one describes.
    ///
    /// Handed to the batch apply, which attaches each note to the outcome only once its
    /// change's landing is verified (the committed-batch seam) — published at plan time, a
    /// note would survive a rejected apply or a restored batch and claim a rollback that
    /// never happened.
    /// They never mark the run incomplete: the change each one annotates did apply.
    notes: Vec<(ChangeTargetKey, Diagnostic)>,
    errors: Vec<Diagnostic>,
}

/// The warning for a rollback that re-enters an advisory the current pin escapes — a security
/// bump `fix` is about to undo because the bump is younger than even the security window.
///
/// A warning, not a hold: `fix` exists to make `check` pass and the feed is explicitly not a
/// vulnerability gate, so the verdict is unchanged and the user decides (`baseline` the pin to
/// keep it, or accept the rollback).
/// Silence is the one thing that would be wrong.
///
/// The message matches the run's mood: a dry run *would* roll the pin back, a real run *is*
/// rolling it back — and even then the caller holds the note until the rollback verifiably
/// lands (see [`FixPlan`]'s `notes`).
fn reintroduces_advisory_warning(
    dep: &Dependency,
    releases: &[Release],
    target: &Version,
    advisories: Option<&cooldown_core::AdvisoryContext<'_>>,
    dry_run: bool,
) -> Option<Diagnostic> {
    let advisories = advisories?;
    let release_of = |version: &Version| releases.iter().find(|rel| rel.version == *version);
    let (current, target) = (release_of(&dep.current)?, release_of(target)?);
    let reintroduced =
        cooldown_core::advisories_reintroduced_by(advisories.advisories, current, target);
    let ids: Vec<&str> = reintroduced
        .iter()
        .map(|advisory| advisory.id.as_str())
        .collect();
    if ids.is_empty() {
        return None;
    }
    let rollback = if dry_run {
        format!("would roll it back to {}, re-entering", target.version)
    } else {
        format!("rolling it back to {} re-enters", target.version)
    };
    Some(
        Diagnostic::new(
            DiagnosticKind::AdvisoryRollback,
            format!(
                "{}@{} is younger than its cooldown; {rollback} {} — `baseline` the current pin instead to keep the fix",
                dep.package.name,
                dep.current,
                ids.join(", ")
            ),
        )
        .with_package(&dep.package.name),
    )
}

/// Why a too-fresh pin is left in place rather than downgraded, as the deferred warning `fix`
/// surfaces once the fixpoint settles — or `None` when the downgrade may proceed.
fn fix_hold_warning(
    dep: &Dependency,
    fix: &FixVerdict,
    effective: &EffectiveHold,
    transitive: TransitiveGate,
    downgrade_pinned: bool,
) -> Option<FixWarning> {
    // `--transitive allow`: leave a too-fresh transitive in place (still reported), only
    // downgrade direct deps.
    // `hide` never reaches here — transitives aren't in scope.
    let message = if transitive == TransitiveGate::Allow && !dep.direct {
        format!(
            "{}@{} is younger than its cooldown; left in place by --transitive allow",
            dep.package.name, dep.current
        )
    } else if fix.current.graph_held {
        match &effective.compliant_holder {
            // Attribution names the genuine obstacle: a *compliant* requirer whose requirement
            // demands the too-fresh version.
            Some(holder) => format!(
                "{}@{} is younger than its cooldown, but {} {} requires `{}`; baseline it or relax that dependency",
                dep.package.name,
                dep.current,
                holder.requirer,
                holder.requirer_version,
                holder.requirement
            ),
            None => format!(
                "{}@{} is younger than its cooldown, but the resolved graph requires that version; baseline it or relax the dependency forcing it",
                dep.package.name, dep.current
            ),
        }
    } else if dep.pinned && !downgrade_pinned {
        // An exact pin is a deliberate choice: warn and leave it unless `--downgrade-pinned`.
        format!(
            "{}@{} is pinned and younger than its cooldown; downgrade it manually or rerun with --downgrade-pinned",
            dep.package.name, dep.current
        )
    } else {
        return None;
    };
    Some(FixWarning {
        package: dep.package.name.clone(),
        message,
    })
}

struct MutationTerminated;

type MutationFlow = ControlFlow<MutationTerminated>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProjectRunStatus {
    Complete,
    Terminated,
}

/// One policy trial's verdict over a candidate group: committed outcomes to keep, the residual
/// cooldown violations that reject the group, or an error that aborts recovery entirely.
enum UpgradeTrialResult {
    Settled(Vec<BatchOutcome>),
    PolicyBlocked(Vec<BaselineViolation>),
    Aborted(Vec<BatchOutcome>),
}

/// A candidate isolation rejected, with the residual violations its trial forced into the graph.
struct RejectedUpgrade {
    change: Change,
    residual: Vec<BaselineViolation>,
}

/// The outcome of isolating a policy-blocked batch into safe and unsafe candidates.
enum UpgradeSelectionResult {
    Selected {
        accepted: Vec<Change>,
        rejected: Vec<RejectedUpgrade>,
    },
    Aborted {
        outcome: BatchOutcome,
        rejected: Vec<RejectedUpgrade>,
    },
}

/// The evolving per-project state during upgrade trials.
#[derive(Clone)]
struct TrialState {
    /// In-cooldown, non-baselined pins present before the next trial.
    baseline_violations: HashSet<BaselineViolation>,
    /// Whether the last committed batch introduced transitive cooldown violations to reconcile.
    reconcile_needed: bool,
}

/// The pre-trial journal paired with the exact cooldown-produced state it may replace.
#[derive(Default)]
enum TrialRollback {
    #[default]
    Empty,
    Captured {
        journal: ProjectMutationJournal,
        expected: ProjectMutationState,
    },
}

impl TrialRollback {
    fn preserve(&mut self, journal: &ProjectMutationJournal) -> cooldown_core::Result<()> {
        let state = journal.capture_state()?;
        match self {
            TrialRollback::Captured {
                journal: rollback,
                expected,
            } => {
                preserve_rollback_entries(rollback, journal)?;
                expected.extend_missing(state)?;
            }
            TrialRollback::Empty => {
                *self = TrialRollback::Captured {
                    journal: journal.clone(),
                    expected: state,
                };
            }
        }
        Ok(())
    }

    fn accept(&mut self, state: ProjectMutationState) -> cooldown_core::Result<()> {
        let TrialRollback::Captured { expected, .. } = self else {
            return Err(cooldown_core::CoreError::System(
                "cannot accept a trial before preserving its rollback state".to_string(),
            ));
        };
        expected.replace(state)
    }

    fn restore(&mut self) -> cooldown_core::Result<()> {
        match self {
            TrialRollback::Empty => Ok(()),
            TrialRollback::Captured { journal, expected } => {
                let restored = journal.state_for(journal)?;
                journal.restore_if_unchanged(expected)?;
                *expected = restored;
                Ok(())
            }
        }
    }
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
    /// Whether a committed apply already supplied edge rows for an adapter that cannot provide a
    /// run-level edge snapshot.
    lock_edges_enforced: bool,
    /// Adapter-owned edge state captured before this project's first mutation.
    initial_edge_snapshot: Option<Vec<u8>>,
    /// Correction provenance from committed batches, pending validation against the final lock.
    committed_edge_rebinds: Vec<cooldown_core::EdgeRebind>,
    /// Packages whose only requirement is a manifest constraint with no lock entry (a build backend).
    /// Their floor raise has no lock interaction, so they are applied in their own batch — a lock
    /// conflict elsewhere in the same run must not roll back (and mislabel) an independent adoption.
    manifest_only: HashSet<PackageId>,
    /// `acc.items` length at each batch merge — the batch boundaries the leg collapse needs:
    /// chronological legs of one copy can only span batches (one report emits one row per copy),
    /// so two same-batch rows sharing a name are coexisting version lines that must not chain.
    batch_item_offsets: Vec<usize>,
    /// The security provenance of planned targets, keyed like the landed/held bookkeeping.
    ///
    /// The report's rows are built from [`Change`]s, which carry no policy provenance, so the
    /// advisory evidence that made a target adoptable is parked here rather than pushed into the
    /// apply instruction.
    security_by_target: HashMap<ChangeTargetKey, crate::app::SecurityInfo>,
    /// Advisory-package identities a top-up tried and could not cover.
    ///
    /// A feed that just failed (or served data too stale to shorten) will fail the same way next
    /// round, so retrying would re-query it and repeat its warning once per round.
    top_up_declined: HashSet<String>,
    /// The project's fetched advisories, set once before planning.
    ///
    /// Advising the planner AND the graph gate from the same data is load-bearing under the
    /// shorten mode: a freshly-adopted security fix is itself a young pin, so an unadvised
    /// residual gate would immediately roll back the very change the advised planner just made.
    /// `Arc` so the per-round borrow of the planning loops is a refcount bump, not a deep copy
    /// of the advisory map.
    advisories: Option<std::sync::Arc<ProjectAdvisories>>,
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
            lock_edges_enforced: false,
            initial_edge_snapshot: None,
            committed_edge_rebinds: Vec::new(),
            manifest_only: HashSet::new(),
            batch_item_offsets: Vec::new(),
            security_by_target: HashMap::new(),
            top_up_declined: HashSet::new(),
            advisories: None,
        }
    }

    pub(super) async fn run(&mut self) -> ProjectRunStatus {
        let guard = match self.ctx.write_guard() {
            Ok(guard) => guard,
            Err(error) => {
                self.record_project_error(&error, None);
                return ProjectRunStatus::Terminated;
            }
        };
        let recovery_result = match guard.as_ref() {
            Some(guard) => {
                self.ctx
                    .writer
                    .recover_pending_mutation(&self.ctx.pctx.project, guard.coordination())
                    .await
            }
            None => Ok(cooldown_core::MutationRecovery::settled(
                cooldown_core::RecoveryDisposition::Unchanged,
            )),
        };
        let recovery = match recovery_result {
            Ok(recovery) => recovery,
            Err(error) => {
                self.record_project_error(&error, None);
                return ProjectRunStatus::Terminated;
            }
        };
        self.acc.warnings.extend(recovery_diagnostics(
            recovery,
            self.ctx.pctx.tool,
            self.ctx.pctx.rel_path.as_str(),
        ));
        self.ctx.opts.progress.phase("resolving dependency graph");
        let Some(deps) = self.scoped_deps().await else {
            return ProjectRunStatus::Terminated;
        };
        self.fetch_advisories(deps.iter().map(|dep| dep.package.name.clone()).collect())
            .await;
        let verb = match self.mode {
            PlanMode::Upgrade { .. } => "upgrades",
            PlanMode::Fix { .. } => "downgrades",
        };
        self.ctx
            .opts
            .progress
            .phase(format!("planning {verb} for {} dependencies", deps.len()));

        self.initial_edge_snapshot = match self
            .ctx
            .writer
            .lock_edge_snapshot(&self.ctx.pctx.project)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.record_project_error(&error, None);
                return ProjectRunStatus::Terminated;
            }
        };
        let Some(mut state) = self.initial_trial_state().await else {
            return ProjectRunStatus::Terminated;
        };
        let mutation = match self.mode {
            PlanMode::Upgrade { .. } => self.run_upgrade(deps, &mut state).await,
            PlanMode::Fix {
                transitive,
                downgrade_pinned,
            } => {
                self.fix_to_fixpoint(deps, transitive, downgrade_pinned, &mut state)
                    .await
            }
        };

        if mutation.is_break() {
            return ProjectRunStatus::Terminated;
        }

        // The final pass reconciles run-level observation with committed correction evidence, so
        // temporary batch limitations and overwritten corrections cannot survive in the report.
        // Adapters without snapshots retain their existing per-batch reporting fallback.
        if (self.initial_edge_snapshot.is_some() || !self.lock_edges_enforced)
            && self.normalize_edges().await.is_break()
        {
            return ProjectRunStatus::Terminated;
        }

        if self.finalize().await.is_break() {
            ProjectRunStatus::Terminated
        } else {
            ProjectRunStatus::Complete
        }
    }

    /// Final edge-binding enforcement and run-level audit for this project.
    async fn normalize_edges(&mut self) -> MutationFlow {
        let plan = Plan {
            edge_policy: self.ctx.pctx.edge_policy,
            ..Plan::default()
        };
        let mutation = match self.ctx.prepare_mutation(&plan).await {
            Ok(mutation) => mutation,
            Err(error) => {
                self.record_project_error(&error, None);
                return ControlFlow::Break(MutationTerminated);
            }
        };
        match self
            .ctx
            .writer
            .normalize_lock_edges(
                &mutation,
                self.ctx.pctx.edge_policy,
                self.initial_edge_snapshot.as_deref(),
                &self.committed_edge_rebinds,
            )
            .await
        {
            Ok(report) => {
                for warning in report.warnings {
                    self.acc.warnings.push(
                        warning
                            .with_tool(self.ctx.tool_name())
                            .with_project(self.project_label.clone()),
                    );
                }
                for rebind in &report.rebinds {
                    if let Err(error) = rebind.validate() {
                        self.record_project_error(&error, Some(&rebind.dependency.name));
                        continue;
                    }
                    let item = self.edge_rebind_item(rebind);
                    self.acc.edge_items.push(item);
                }
                ControlFlow::Continue(())
            }
            Err(error) => {
                self.record_project_error(&error, None);
                ControlFlow::Break(MutationTerminated)
            }
        }
    }

    /// Runs the upgrade policy state machine for targets selected by another read path.
    ///
    /// This deliberately stops before final lock/build reporting: candidate eligibility is decided
    /// by the apply, graph gate, reconciliation, and residual-isolation phases. The caller owns a
    /// throwaway project copy, so every settled mutation is discarded with that copy.
    pub(super) async fn run_policy(
        &mut self,
        mut changes: Vec<Change>,
        manifest_only: HashSet<PackageId>,
        advisories: Option<std::sync::Arc<ProjectAdvisories>>,
    ) {
        let guard = match self.ctx.write_guard() {
            Ok(guard) => guard,
            Err(error) => {
                self.record_project_error(&error, None);
                return;
            }
        };
        let recovery_result = match guard.as_ref() {
            Some(guard) => {
                self.ctx
                    .writer
                    .recover_pending_mutation(&self.ctx.pctx.project, guard.coordination())
                    .await
            }
            None => Ok(cooldown_core::MutationRecovery::settled(
                cooldown_core::RecoveryDisposition::Unchanged,
            )),
        };
        let recovery = match recovery_result {
            Ok(recovery) => recovery,
            Err(error) => {
                self.record_project_error(&error, None);
                return;
            }
        };
        self.acc.warnings.extend(recovery_diagnostics(
            recovery,
            self.ctx.pctx.tool,
            self.ctx.pctx.rel_path.as_str(),
        ));
        self.manifest_only = manifest_only;
        sort_planned_changes(&mut changes);
        // The caller's snapshot when it has one: the trial must judge from the same feed data
        // that selected these targets, or it could hold a candidate on a newer snapshot's
        // evidence.
        match advisories {
            Some(shared) => self.advisories = Some(shared),
            None => {
                self.fetch_advisories(
                    changes
                        .iter()
                        .map(|change| change.package.name.clone())
                        .collect(),
                )
                .await;
            }
        }
        let Some(mut state) = self.initial_trial_state().await else {
            return;
        };
        let _ = self.apply_upgrade_changes(changes, &mut state).await;
    }

    /// Fetches the project's advisories once, before planning, so the planner and every
    /// subsequent graph gate judge from the same feed data.
    ///
    /// The fetch covers `packages` (the planning scope) joined with the raw, unscoped graph's
    /// package names, because the residual gate judges the whole graph (see
    /// [`graph_violations`](Self::graph_violations)) and a pin outside the fetch set would be
    /// gated unadvised — rolling back the very security fix the advised planner adopted.
    /// A package a later re-lock introduces is picked up by
    /// [`top_up_advisories`](Self::top_up_advisories), which extends this snapshot without
    /// disturbing it.
    async fn fetch_advisories(&mut self, mut packages: Vec<String>) {
        let policy = cooldown_core::resolve_advisory_policy(&self.ctx.pctx.policy.layers);
        if !policy.enabled || policy.source == cooldown_core::AdvisorySourceKind::None {
            // `fetch_project_advisories` would be inert; skip the graph read below too.
            self.advisories = None;
            return;
        }
        // Without a wired source there is nothing to query, but the fetch still runs: it reports
        // that no feed implements the selected `source` rather than certifying silently.
        if self.ws.advisory_source.is_some()
            && let Ok(graph) = self
                .ctx
                .reader
                .dependencies(&self.ctx.pctx.project, DepScope::Graph)
                .await
        {
            packages.extend(graph.into_iter().map(|dep| dep.package.name));
        }
        let fetch = self
            .ws
            .fetch_project_advisories(
                self.ctx.reader,
                self.ctx.pctx,
                &self.project_label,
                &packages,
                self.ctx.opts,
            )
            .await;
        self.acc.warnings.extend(fetch.warnings);
        self.acc.errors.extend(fetch.errors);
        self.advisories = fetch.advisories.map(std::sync::Arc::new);
    }

    /// Extends the advisory snapshot to cover package identities it never queried — the ones a
    /// re-lock introduced after the initial fetch.
    ///
    /// Without this a freshly resolved pin is judged *unadvised*: the residual gate reads its
    /// young age as an ordinary cooldown violation, and reconciliation then rolls it back to the
    /// newest matured release — which, when the new pin is itself the security fix, is the
    /// vulnerable version it replaced.
    ///
    /// Already-queried identities are left exactly as they were, including the ones that came
    /// back with no advisories.
    /// Only *new* keys are added, so the planner's evidence for everything it has already
    /// decided cannot shift underneath a later round — the hazard a wholesale re-fetch would
    /// introduce.
    async fn top_up_advisories(&mut self, deps: &[Dependency]) {
        let Some(existing) = self.advisories.clone() else {
            return;
        };
        let names: Vec<String> = deps.iter().map(|dep| dep.package.name.clone()).collect();
        let unqueried: Vec<String> = existing
            .unqueried(self.ctx.reader, &names)
            .into_iter()
            .filter(|package| !self.top_up_declined.contains(package))
            .collect();
        if unqueried.is_empty() {
            return;
        }
        let topped_up = self
            .ws
            .top_up_project_advisories(
                self.ctx.reader,
                self.ctx.pctx,
                &self.project_label,
                &existing,
                &unqueried,
                self.ctx.opts,
            )
            .await;
        self.acc.warnings.extend(topped_up.warnings);
        self.acc.errors.extend(topped_up.errors);
        match topped_up.advisories {
            Some(advisories) => self.advisories = Some(std::sync::Arc::new(advisories)),
            // Asking again every round would re-query a feed that just failed and repeat its
            // warning per round; one report of it is the honest amount.
            None => self.top_up_declined.extend(unqueried),
        }
    }

    async fn initial_trial_state(&mut self) -> Option<TrialState> {
        self.ctx
            .opts
            .progress
            .phase("checking current resolved graph cooldown");
        let baseline_violations = match self.graph_violations().await {
            Ok(violations) => violations.into_keys().collect(),
            Err(error) => {
                self.record_project_error(&error, None);
                return None;
            }
        };

        Some(TrialState {
            baseline_violations,
            reconcile_needed: false,
        })
    }

    /// Apply the forward moves, then (under the default transitive mode) reconcile the graph the
    /// re-lock produced: downgrade any too-fresh transitive a forward move floated up, so a single
    /// `upgrade` ends gate-clean — no separate `fix` needed.
    async fn run_upgrade(&mut self, deps: Vec<Dependency>, state: &mut TrialState) -> MutationFlow {
        let planned = self.plan_upgrade_changes(&deps).await;
        self.apply_upgrade_changes(planned, state).await
    }

    /// Applies planned changes: manifest-only build-backend floors in their own batch, then the
    /// lock batch through the policy trials. Shared by the real `upgrade` and the policy preview.
    async fn apply_upgrade_changes(
        &mut self,
        planned: Vec<Change>,
        state: &mut TrialState,
    ) -> MutationFlow {
        // Build-backend floor raises ([build-system].requires) have no lock interaction, so apply them
        // in their own batch: a transitive-cooldown rollback of the lock batch must not revert (or
        // mislabel as a conflict) an independent, valid build-backend adoption.
        let (build_changes, lock_changes): (Vec<Change>, Vec<Change>) = planned
            .into_iter()
            .partition(|change| self.manifest_only.contains(&change.package));
        if !build_changes.is_empty() {
            self.ctx
                .opts
                .progress
                .candidates(&build_changes, "checking build backend updates");
            let decided = build_changes.clone();
            let outcome = self.apply_batch(build_changes, state).await;
            self.ctx.opts.progress.candidates_decided(&decided);
            Self::advance_trial_state(&outcome, state);
            if let ControlFlow::Break(conflict) = self.merge_batch_outcome(outcome) {
                return ControlFlow::Break(conflict);
            }
        }
        if lock_changes.is_empty() {
            return ControlFlow::Continue(());
        }

        self.run_lock_upgrades(lock_changes, state).await
    }

    /// Applies the lock batch, isolating candidates when the joint result violates cooldown policy.
    ///
    /// The fast path is one trial of the complete batch: settled outcomes commit as-is. A policy
    /// residual restores the fixed pre-lock baseline and — for more than one candidate —
    /// partitions the batch to find a deterministic verified subset, which is then replayed jointly
    /// from that same baseline; only the replay commits.
    /// Errors abort recovery and restore the baseline: an infrastructure failure must surface as an
    /// error, never as a cooldown skip.
    async fn run_lock_upgrades(
        &mut self,
        lock_changes: Vec<Change>,
        state: &mut TrialState,
    ) -> MutationFlow {
        let baseline_before_lock = state.clone();
        self.ctx
            .opts
            .progress
            .candidates(&lock_changes, "checking upgrade policy");
        let mut rollback = TrialRollback::default();
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
                self.ctx.opts.progress.candidates_decided(&lock_changes);
                let flow = self.merge_batch_outcomes(outcomes);
                self.collapse_collateral(&baseline_before_lock.baseline_violations);
                return flow;
            }
            UpgradeTrialResult::Aborted(outcomes) => {
                let outcome = self.settle_aborted_trial(
                    &mut rollback,
                    &baseline_before_lock,
                    state,
                    outcomes,
                );
                let flow = self.merge_batch_outcome(outcome);
                self.collapse_collateral(&baseline_before_lock.baseline_violations);
                return flow;
            }
            UpgradeTrialResult::PolicyBlocked(violations) => {
                let mut outcome = BatchOutcome::default();
                if !self.restore_upgrade_trial(
                    &mut rollback,
                    &baseline_before_lock,
                    state,
                    &mut outcome,
                ) {
                    return self.merge_batch_outcome(outcome);
                }
                // A singleton batch has nothing to isolate: the lone candidate is the culprit.
                if lock_changes.len() == 1 {
                    self.ctx.opts.progress.candidates_decided(&lock_changes);
                    self.record_unreconciled_skips(&lock_changes, &violations);
                    self.collapse_collateral(&baseline_before_lock.baseline_violations);
                    return ControlFlow::Continue(());
                }
            }
        }

        let flow = self
            .recover_policy_blocked_upgrade(
                lock_changes,
                &baseline_before_lock,
                state,
                &mut rollback,
            )
            .await;
        self.collapse_collateral(&baseline_before_lock.baseline_violations);
        flow
    }

    /// Isolates a policy-blocked multi-candidate batch, then commits its safe subset via one joint
    /// replay. With no safe candidate every rejection is reported held; a selection abort retains
    /// earlier settled rejections beside the failing trial's errors unless rollback leaves the
    /// project state indeterminate.
    async fn recover_policy_blocked_upgrade(
        &mut self,
        lock_changes: Vec<Change>,
        baseline: &TrialState,
        state: &mut TrialState,
        rollback: &mut TrialRollback,
    ) -> MutationFlow {
        let selection = self
            .select_safe_upgrade_changes(lock_changes, baseline, state, rollback)
            .await;
        match selection {
            UpgradeSelectionResult::Selected { accepted, rejected } if accepted.is_empty() => {
                self.record_rejected_upgrade_changes(rejected);
                ControlFlow::Continue(())
            }
            UpgradeSelectionResult::Selected { accepted, rejected } => {
                self.replay_selected_upgrade_changes(accepted, rejected, baseline, state, rollback)
                    .await
            }
            UpgradeSelectionResult::Aborted { outcome, rejected } => {
                if !outcome.has_restore_conflict() {
                    self.record_rejected_upgrade_changes(rejected);
                }
                self.merge_batch_outcome(outcome)
            }
        }
    }

    /// Partitions a policy-blocked batch into a deterministic verified subset and rejected
    /// singletons — delta-debugging partitioning over `accepted + group` trials, like
    /// `apply_resilient`, with the tool's whole pipeline (apply, reconcile, residual gate) as the
    /// oracle.
    async fn select_safe_upgrade_changes(
        &mut self,
        lock_changes: Vec<Change>,
        baseline: &TrialState,
        state: &mut TrialState,
        rollback: &mut TrialRollback,
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
                        return UpgradeSelectionResult::Aborted { outcome, rejected };
                    }
                    accepted.extend(group);
                }
                UpgradeTrialResult::PolicyBlocked(violations) => {
                    let mut outcome = BatchOutcome::default();
                    if !self.restore_upgrade_trial(rollback, baseline, state, &mut outcome) {
                        return UpgradeSelectionResult::Aborted { outcome, rejected };
                    }
                    if group.len() > 1 {
                        push_upgrade_halves(&mut work, group);
                    } else {
                        self.ctx.opts.progress.candidates_decided(&group);
                        rejected.extend(group.into_iter().map(|change| RejectedUpgrade {
                            change,
                            residual: violations.clone(),
                        }));
                    }
                }
                UpgradeTrialResult::Aborted(outcomes) => {
                    let outcome = self.settle_aborted_trial(rollback, baseline, state, outcomes);
                    return UpgradeSelectionResult::Aborted { outcome, rejected };
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
        rollback: &mut TrialRollback,
    ) -> MutationFlow {
        self.ctx
            .opts
            .progress
            .phase("replaying verified candidates from baseline");
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
                self.ctx.opts.progress.candidates_decided(&accepted);
                if let ControlFlow::Break(conflict) = self.merge_batch_outcomes(outcomes) {
                    return ControlFlow::Break(conflict);
                }
                self.record_rejected_upgrade_changes(rejected);
                ControlFlow::Continue(())
            }
            UpgradeTrialResult::PolicyBlocked(violations) => {
                let mut outcome = BatchOutcome::default();
                if !self.restore_upgrade_trial(rollback, baseline, state, &mut outcome) {
                    return self.merge_batch_outcome(outcome);
                }
                self.ctx.opts.progress.candidates_decided(&accepted);
                self.record_unreconciled_skips(&accepted, &violations);
                self.record_rejected_upgrade_changes(rejected);
                ControlFlow::Continue(())
            }
            UpgradeTrialResult::Aborted(outcomes) => {
                let outcome = self.settle_aborted_trial(rollback, baseline, state, outcomes);
                let flow = self.merge_batch_outcome(outcome);
                if flow.is_break() {
                    return flow;
                }
                self.record_rejected_upgrade_changes(rejected);
                ControlFlow::Continue(())
            }
        }
    }

    /// Runs one policy trial: applies `changes` as one resolver batch, reconciles the floated
    /// transitives, then judges the residual violations against the fixed `policy_baseline`.
    ///
    /// Every mutation is captured into `rollback` (first snapshot per path), so the caller can
    /// restore the pre-trial worktree no matter how far the trial got. A settled trial's outcomes
    /// stay unmerged — the caller decides whether this trial is the one that commits — and an
    /// aborted trial retains all outcomes until the caller knows whether restoring the outer
    /// baseline succeeded.
    async fn try_upgrade_group(
        &mut self,
        changes: Vec<Change>,
        policy_baseline: &HashSet<BaselineViolation>,
        state: &mut TrialState,
        rollback: &mut TrialRollback,
    ) -> UpgradeTrialResult {
        let mut pending = Vec::new();
        let lock_outcome = self
            .apply_batch_with_rollback(changes, state, Some(rollback), HashMap::new())
            .await;
        // Skip reconciliation when the lock upgrade made no clean forward progress: nothing floated
        // up, and a broken re-lock probe must not be re-hit.
        let upgraded_cleanly =
            lock_outcome.applied_count() > 0 && lock_outcome.errored_count() == 0;
        Self::advance_trial_state(&lock_outcome, state);
        let lock_committed = lock_outcome.is_committed();
        pending.push(lock_outcome);
        if pending.iter().any(|outcome| outcome.errored_count() > 0) {
            return UpgradeTrialResult::Aborted(pending);
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
            return UpgradeTrialResult::Aborted(pending);
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
    fn collapse_collateral(&mut self, prior_violations: &HashSet<BaselineViolation>) {
        let project = self.project_label.clone();
        let tool = self.ctx.tool_name();
        let classifier = self.ctx.reader;
        let offsets = self.batch_item_offsets.clone();
        collapse_applied_legs(
            &mut self.acc.items,
            &project,
            tool,
            prior_violations,
            move |idx| offsets.partition_point(|&start| start <= idx),
            |from, to| classifier.classify_update_kind(from, to),
        );
    }

    /// Fold a batch's report into the run accumulator. Report-only: trial state advances exactly
    /// once, via [`advance_trial_state`](Self::advance_trial_state) right after the batch runs, so
    /// merging (which may happen later, after buffering) can never re-apply or clobber it.
    fn merge_batch_outcome(&mut self, outcome: BatchOutcome) -> MutationFlow {
        let restore_conflict = outcome.has_restore_conflict();
        self.lock_refreshed_by_apply |= outcome.lock_refreshed;
        // Only kept batches reach the merge as committed, so this records exactly "the current lock
        // was produced by a committed apply" — which enforced the edge policy on its way out.
        self.lock_edges_enforced |= outcome.is_committed();
        self.committed_edge_rebinds
            .extend(outcome.edge_rebinds.iter().cloned());
        self.batch_item_offsets.push(self.acc.items.len());
        outcome.merge_into(self.acc);
        if restore_conflict {
            ControlFlow::Break(MutationTerminated)
        } else {
            ControlFlow::Continue(())
        }
    }

    fn merge_batch_outcomes(
        &mut self,
        outcomes: impl IntoIterator<Item = BatchOutcome>,
    ) -> MutationFlow {
        for outcome in outcomes {
            if let ControlFlow::Break(conflict) = self.merge_batch_outcome(outcome) {
                return ControlFlow::Break(conflict);
            }
        }
        ControlFlow::Continue(())
    }

    /// Advance the trial state with a committed batch's after-graph. Called exactly once per
    /// outcome, immediately after [`apply_batch`](Self::apply_batch) returns; a rolled-back run
    /// resets the state explicitly instead of un-applying outcomes.
    fn advance_trial_state(outcome: &BatchOutcome, state: &mut TrialState) {
        if let Some(committed) = outcome.committed_state() {
            state
                .baseline_violations
                .clone_from(&committed.violations_after);
            state.reconcile_needed = committed.reconcile_needed;
        }
    }

    /// Reports each change of a policy-blocked trial as held by the transitive it would force
    /// into the graph.
    fn record_unreconciled_skips(&mut self, changes: &[Change], residual: &[BaselineViolation]) {
        self.acc.strict_incomplete = true;
        // Name one stuck transitive as the offender (sorted for a stable report).
        let offender = residual
            .iter()
            .map(|violation| package_label(&violation.package))
            .min();
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

    fn record_rejected_upgrade_changes(&mut self, rejected: Vec<RejectedUpgrade>) {
        for rejected_upgrade in rejected {
            self.record_unreconciled_skips(
                std::slice::from_ref(&rejected_upgrade.change),
                &rejected_upgrade.residual,
            );
        }
    }

    /// Restores the fixed pre-lock worktree snapshot and executor baseline after a trial.
    ///
    /// A restore failure leaves the worktree in no known state, so it is pushed as an error and
    /// the caller must stop recovering instead of running further trials.
    fn restore_upgrade_trial(
        &self,
        snapshot: &mut TrialRollback,
        baseline: &TrialState,
        state: &mut TrialState,
        outcome: &mut BatchOutcome,
    ) -> bool {
        match snapshot.restore() {
            Ok(()) => {
                state.clone_from(baseline);
                true
            }
            Err(error) => {
                outcome.errors.push(self.restore_conflict_diag(&error));
                outcome.mark_restore_conflict();
                false
            }
        }
    }

    /// Resolves an aborted trial without claiming which mutations survived a restore conflict.
    fn settle_aborted_trial(
        &self,
        snapshot: &mut TrialRollback,
        baseline: &TrialState,
        state: &mut TrialState,
        outcomes: Vec<BatchOutcome>,
    ) -> BatchOutcome {
        match snapshot.restore() {
            Ok(()) => {
                state.clone_from(baseline);
                trial_errors(outcomes)
            }
            Err(error) => {
                let mut outcome = indeterminate_trial(outcomes);
                outcome.errors.push(self.restore_conflict_diag(&error));
                outcome
            }
        }
    }

    async fn scoped_deps(&mut self) -> Option<Vec<Dependency>> {
        let mut scope = candidate_scope(self.mode);
        // Graph-wide *upgrade* planning is additionally gated on the engine: advancing a
        // transitive needs an apply mechanism that can pin a package no manifest declares. An
        // engine without one (npm/yarn/bun per-package landing) would only report every such
        // candidate not-eligible, so it keeps direct-only upgrade planning. `fix` keeps the graph
        // scope regardless — a too-fresh transitive must at least be reported.
        if matches!(self.mode, PlanMode::Upgrade { .. })
            && !self.ctx.writer.supports_transitive_advance()
        {
            scope = DepScope::Direct;
        }
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
        if matches!(self.mode, PlanMode::Upgrade { .. }) {
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
        self.ctx.opts.progress.phase(format!(
            "fetching metadata for {} upgrade candidates",
            deps.len()
        ));
        let rctx = ResolveContext {
            honor_declared_bounds: self.ctx.opts.rewrite == RewriteMode::Auto,
            ..Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts)
        };
        let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
        let fetched = self
            .ws
            .fetch_candidate_releases(
                self.ctx.reader,
                deps.to_vec(),
                &fctx,
                self.ctx.opts.candidate_scope(),
                &self.ctx.opts.progress,
                self.ctx.opts.fanout(),
            )
            .await;
        let advisories = self.advisories.clone();
        let mut planned = Vec::new();
        for FetchedRelease {
            dependency: dep,
            result,
        } in fetched
        {
            let releases = match result {
                Ok(releases) => releases,
                Err(error) => {
                    self.record_project_error(&error, Some(&dep.package.name));
                    continue;
                }
            };
            let advised = advisories
                .as_ref()
                .and_then(|project| project.classify(self.ctx.reader, &dep, &releases));
            let advisory_ctx = advised.as_ref().map(ClassifiedAdvisories::context);
            let verdict = evaluate_advised(
                &dep,
                &releases,
                advisory_ctx.as_ref(),
                &self.ctx.pctx.policy.layers,
                &dep_resolve_ctx(&rctx, &dep),
                self.ws.now(),
            );
            self.record_held_back_ceiling(&dep, &releases, advisory_ctx.as_ref(), &rctx);
            // Surface an adoptable cross-major update the user could take with `--major` (it would
            // otherwise vanish from a default run even though `outdated` lists it).
            self.record_held_back_major(
                &dep,
                &releases,
                advisory_ctx.as_ref(),
                &rctx,
                verdict.adoptable_target.as_ref(),
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
            let chosen = verdict
                .candidates
                .iter()
                .find(|candidate| candidate.version == target);
            let kind = chosen.map_or(cooldown_core::UpdateKind::Minor, |candidate| candidate.kind);
            let security = chosen
                .and_then(|candidate| candidate.security.as_ref())
                .map(|relevance| crate::app::workspace::security_info(relevance, &target));
            let package = target_package_for(&releases, &dep, &target);
            // Whether this move is a rollback. The forward planner only adopts a strictly newer
            // matured version (`evaluate` filters to `order > current`), so this is currently always
            // false — a too-fresh pin is rolled back by the fix/reconcile pass instead, which flags it
            // directly. Computed rather than hardcoded so the label stays correct if that ever changes.
            let downgrade = is_downgrade(&releases, &dep.current, &target);
            let change = Change {
                package,
                from: dep.current.clone(),
                to: target,
                kind,
                downgrade,
                direct: dep.direct,
                members: dep.members.clone(),
            };
            // The advisory evidence lives on the verdict, but the report is built from the
            // `Change` — an apply instruction that deliberately carries no policy provenance —
            // so keep it beside the plan, keyed by the same identity the applied/held
            // bookkeeping uses.
            if let Some(security) = security {
                self.security_by_target
                    .insert(change_target_key(&change), security);
            }
            planned.push(change);
        }
        sort_planned_changes(&mut planned);
        planned
    }

    /// Records the matured target hidden by a package-owned ceiling: a declared manifest upper
    /// bound, a configured `max-major`, or the registry's `latest` dist-tag.
    ///
    /// A default major-off run probes with majors enabled so it reports the ceiling that would
    /// still block `--major`, rather than giving the false advice to re-run with `--major`.
    fn record_held_back_ceiling(
        &mut self,
        dep: &Dependency,
        releases: &[Release],
        advisory: Option<&cooldown_core::AdvisoryContext<'_>>,
        rctx: &ResolveContext<'_>,
    ) {
        let probe_ctx = ResolveContext {
            allow_major: dep.direct,
            ..dep_resolve_ctx(rctx, dep)
        };
        // Advised like the planner: a security fix matured only under the shortened window must
        // still surface as a ceiling hold while the ceiling hides it.
        let Some(hold) = evaluate_ceiling_hold_advised(
            dep,
            releases,
            advisory,
            &self.ctx.pctx.policy.layers,
            &probe_ctx,
            self.ws.now(),
        ) else {
            return;
        };
        let reason = match hold.reason {
            CeilingReason::DeclaredBound => SkipReason::DeclaredBoundHeld,
            CeilingReason::MaxMajor => SkipReason::MaxMajorHeld,
            CeilingReason::DistTag => SkipReason::DistTagHeld,
        };
        let change = Change {
            package: target_package_for(releases, dep, &hold.target),
            from: dep.current.clone(),
            to: hold.target,
            kind: hold.update_kind,
            downgrade: false,
            direct: dep.direct,
            members: dep.members.clone(),
        };
        self.record_change_skip(
            &change,
            Some(SkippedInfo {
                reason,
                message: reason.message().to_string(),
                offending: None,
            }),
        );
    }

    /// On a default (major-off) run, record an adoptable cross-major update as a `needs --major`
    /// skip — only for a directly-declared, non-pinned dep where re-running with `--major` would
    /// actually adopt it. `scoped_target` is the major-off run's own adoptable target, so a
    /// coincident in-range adoptable is not re-flagged as a major.
    fn record_held_back_major(
        &mut self,
        dep: &Dependency,
        releases: &[Release],
        advisory: Option<&cooldown_core::AdvisoryContext<'_>>,
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
        let major = evaluate_advised(
            dep,
            releases,
            advisory,
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

    /// Pass one of [`plan_fix_changes`](Self::plan_fix_changes): fetch releases, evaluate every
    /// candidate against the *current* resolution, and collect the round's violation set. The set
    /// keys the circular-hold discount in pass two: a graph hold contributed by a requirer that
    /// is itself in this set is conditioned on the resolution being fixed, not a real obstacle.
    ///
    /// Only *movable* violations enter the set: a too-fresh transitive `--transitive allow`
    /// leaves in place, or a pin left alone without `--downgrade-pinned`, holds its dependents
    /// exactly like a compliant requirer — discounting its edges would plan changes whose only
    /// landing path is the atomic reconcile moving the very node the flag promised not to touch.
    /// A violation with no matured target stays in the set: whether it can move is for the
    /// resolver to decide (the `icu_locale_fallback` shape — a fresh-era-only crate that simply
    /// drops out once its family moves — depends on discounting it).
    async fn evaluate_fix_candidates<'p>(
        &mut self,
        deps: &[Dependency],
        transitive: TransitiveGate,
        downgrade_pinned: bool,
        rctx: &ResolveContext<'_>,
        advisories: Option<&'p ProjectAdvisories>,
        errors: &mut Vec<Diagnostic>,
    ) -> (
        Vec<(
            Dependency,
            Vec<Release>,
            FixVerdict,
            Option<ClassifiedAdvisories<'p>>,
        )>,
        HashSet<(String, String)>,
    ) {
        let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
        let fetched = self
            .ws
            .fetch_candidate_releases(
                self.ctx.reader,
                deps.to_vec(),
                &fctx,
                self.ctx.opts.candidate_scope(),
                &self.ctx.opts.progress,
                self.ctx.opts.fanout(),
            )
            .await;
        let mut evaluated = Vec::new();
        let mut violations: HashSet<(String, String)> = HashSet::new();
        for FetchedRelease {
            dependency: dep,
            result,
        } in fetched
        {
            let releases = match result {
                Ok(releases) => releases,
                Err(error) => {
                    errors.push(self.project_diag(&error, Some(&dep.package.name)));
                    continue;
                }
            };
            let advised =
                advisories.and_then(|project| project.classify(self.ctx.reader, &dep, &releases));
            let advisory_ctx = advised.as_ref().map(ClassifiedAdvisories::context);
            let fix = evaluate_fix_advised(
                &dep,
                &releases,
                advisory_ctx.as_ref(),
                &self.ctx.pctx.policy.layers,
                &dep_resolve_ctx(rctx, &dep),
                self.ws.now(),
            );
            // The two immovable shapes, spelled positively for the lint: an allow-gated
            // transitive, and a pin without --downgrade-pinned.
            let movable = (dep.direct || transitive != TransitiveGate::Allow)
                && (downgrade_pinned || !dep.pinned);
            if fix.current.status == Status::CurrentInCooldown && movable {
                violations.insert((dep.package.name.clone(), dep.current.as_str().to_string()));
            }
            evaluated.push((dep, releases, fix, advised));
        }
        (evaluated, violations)
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
        self.ctx.opts.progress.phase(format!(
            "fetching metadata for {} cooldown fix candidates",
            deps.len()
        ));
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let advisories = self.advisories.clone();
        let mut planned = Vec::new();
        let mut warnings = Vec::new();
        let mut notes = Vec::new();
        let mut errors = Vec::new();
        let (evaluated, violations) = self
            .evaluate_fix_candidates(
                deps,
                transitive,
                downgrade_pinned,
                &rctx,
                advisories.as_deref(),
                &mut errors,
            )
            .await;
        for (dep, releases, fix, advised) in evaluated {
            // Only a too-fresh pin needs fixing; a compliant (or exempt / unknown-age) dep is left
            // alone, so `fix` only ever touches what `check` would reject.
            if fix.current.status != Status::CurrentInCooldown {
                continue;
            }
            let advisory_ctx = advised.as_ref().map(ClassifiedAdvisories::context);
            // Pass two: re-judge the violation with circular holds discounted, so a family of
            // too-fresh versions holding each other in place is planned as a whole instead of
            // being reported graph-held member by member. The advisory context rides along, so a
            // discounted re-judge keeps any advisory-shortened window the first pass applied.
            let effective = effective_hold(&dep, &releases, &violations);
            let fix = if effective.discounted {
                evaluate_fix_advised(
                    &effective.dep,
                    &releases,
                    advisory_ctx.as_ref(),
                    &self.ctx.pctx.policy.layers,
                    &dep_resolve_ctx(&rctx, &dep),
                    self.ws.now(),
                )
            } else {
                fix
            };
            if let Some(warning) =
                fix_hold_warning(&dep, &fix, &effective, transitive, downgrade_pinned)
            {
                warnings.push(warning);
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
            let change = fix_change(&releases, &dep, target, kind);
            if let Some(note) = reintroduces_advisory_warning(
                &dep,
                &releases,
                &change.to,
                advisory_ctx.as_ref(),
                self.ctx.opts.dry_run,
            ) {
                notes.push((
                    change_target_key(&change),
                    note.with_tool(self.ctx.tool_name())
                        .with_project(self.project_label.clone()),
                ));
            }
            planned.push(change);
        }
        sort_planned_changes(&mut planned);
        FixPlan {
            changes: planned,
            warnings,
            notes,
            errors,
        }
    }

    /// Apply `fix` downgrades round by round until the graph stops changing: each round re-discovers
    /// the (re-locked) graph and re-plans, because a downgrade can free a dep that was graph-held a
    /// round earlier. The deferred unfixable warnings are surfaced once, from the settling round.
    async fn fix_to_fixpoint(
        &mut self,
        deps: Vec<Dependency>,
        transitive: TransitiveGate,
        downgrade_pinned: bool,
        state: &mut TrialState,
    ) -> MutationFlow {
        // Collapse against the violations present before any round, mirroring `upgrade`'s use of
        // its pre-lock baseline on every exit path: a package moved by two rounds (a collateral
        // float, then the downgrade the re-plan schedules) must report one net row, and a chain
        // with no net movement must report none — the contract of `collapse_applied_legs`.
        let prior_violations = state.baseline_violations.clone();
        let flow = self
            .run_fix_rounds(deps, transitive, downgrade_pinned, state)
            .await;
        self.collapse_collateral(&prior_violations);
        flow
    }

    /// The rounds of [`fix_to_fixpoint`](Self::fix_to_fixpoint), split out so every exit —
    /// settled, terminated, or the round backstop — flows through exactly one report collapse.
    async fn run_fix_rounds(
        &mut self,
        mut deps: Vec<Dependency>,
        transitive: TransitiveGate,
        downgrade_pinned: bool,
        state: &mut TrialState,
    ) -> MutationFlow {
        let budget = self.ctx.opts.fix_round_budget.unwrap_or(MAX_FIX_ROUNDS);
        for _ in 0..budget {
            let FixPlan {
                changes,
                warnings,
                notes,
                errors,
            } = self
                .plan_fix_changes(&deps, transitive, downgrade_pinned)
                .await;
            self.acc.errors.extend(errors);
            if changes.is_empty() {
                self.emit_fix_warnings(warnings);
                return ControlFlow::Continue(());
            }
            self.ctx
                .opts
                .progress
                .candidates(&changes, "checking cooldown fixes");
            let decided = changes.clone();
            let outcome = self
                .apply_batch_with_rollback(changes, state, None, notes.into_iter().collect())
                .await;
            self.ctx.opts.progress.candidates_decided(&decided);
            let applied = outcome.applied_count();
            Self::advance_trial_state(&outcome, state);
            if let ControlFlow::Break(conflict) = self.merge_batch_outcome(outcome) {
                return ControlFlow::Break(conflict);
            }
            if applied == 0 {
                self.emit_fix_warnings(warnings);
                return ControlFlow::Continue(());
            }
            let Some(next) = self.scoped_deps().await else {
                return ControlFlow::Continue(());
            };
            deps = next;
        }
        // The round budget ran out with the final round still applying changes, so the re-plan
        // that normally surfaces residual violations never ran — and a round's apply can itself
        // float fresh collateral (the atomic reconcile resolves new era-mates against the live
        // registry). Without this pass those residuals would exit silently as a clean success
        // while the very next `check` fails. One plan-only pass turns them into warnings (each
        // marking the run incomplete); its still-plannable changes are deliberately not applied —
        // the budget is spent.
        let FixPlan {
            changes,
            mut warnings,
            // This pass deliberately applies nothing, and a note may only publish once its
            // change's landing is verified — see `FixPlan::notes`.
            notes: _,
            errors,
        } = self
            .plan_fix_changes(&deps, transitive, downgrade_pinned)
            .await;
        self.acc.errors.extend(errors);
        for change in &changes {
            warnings.push(FixWarning {
                package: change.package.name.clone(),
                message: format!(
                    "{}@{} is still younger than its cooldown after {budget} fix rounds; rerun fix",
                    change.package.name, change.from
                ),
            });
        }
        self.emit_fix_warnings(warnings);
        ControlFlow::Continue(())
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
        rollback: &mut TrialRollback,
    ) -> Vec<BatchOutcome> {
        let mut outcomes = Vec::new();
        if !state.reconcile_needed {
            return outcomes;
        }
        state.reconcile_needed = false;
        let budget = self.ctx.opts.fix_round_budget.unwrap_or(MAX_FIX_ROUNDS);
        for _ in 0..budget {
            self.ctx
                .opts
                .progress
                .phase("reconciling transitive cooldown violations");
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
                notes,
                errors,
            } = self
                .plan_fix_changes(&deps, TransitiveGate::Enforce, false)
                .await;
            if !errors.is_empty() {
                let mut outcome = BatchOutcome::default();
                outcome.errors = errors;
                outcome.strict_incomplete = true;
                outcomes.push(outcome);
                return outcomes;
            }
            if changes.is_empty() {
                outcomes.push(self.fix_warnings_outcome(warnings));
                return outcomes;
            }
            let outcome = self
                .apply_batch_with_rollback(
                    changes,
                    state,
                    Some(rollback),
                    notes.into_iter().collect(),
                )
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
        self.apply_batch_with_rollback(changes, state, None, HashMap::new())
            .await
    }

    /// `notes` carries the advisory-rollback warnings parked for this batch's changes, keyed by
    /// the change each one describes; a note whose change never verifiably lands is dropped
    /// with the batch, so a rejected apply or a restored batch cannot publish a claim about a
    /// mutation that never happened.
    async fn apply_batch_with_rollback(
        &mut self,
        changes: Vec<Change>,
        state: &TrialState,
        mut rollback: Option<&mut TrialRollback>,
        notes: HashMap<ChangeTargetKey, Diagnostic>,
    ) -> BatchOutcome {
        let mut outcome = BatchOutcome::default();
        if changes.is_empty() {
            return outcome;
        }
        let plan = self.batch_plan(&changes, state);
        let primary = changes
            .first()
            .map(|change| change.package.name.clone())
            .unwrap_or_default();
        self.ctx
            .opts
            .progress
            .phase(format!("applying {} planned changes", changes.len()));
        self.ctx.opts.progress.policy_pass(&changes);
        let prepared = match self
            .prepare_batch_journal(&plan, rollback.as_deref_mut())
            .await
        {
            Ok(prepared) => prepared,
            Err(error) => {
                outcome
                    .errors
                    .push(self.project_diag(&error, Some(&primary)));
                return outcome;
            }
        };

        // Resilient apply: if the joint resolve is unsatisfiable as a whole because of one unfetchable
        // or conflicting candidate, isolate it and apply the rest rather than holding every candidate.
        let mutation = match super::super::resilient_apply::apply_resilient_with_observer(
            self.ctx.writer,
            &prepared,
            &self.ctx.opts.progress,
        )
        .await
        {
            Ok(mutation) => mutation,
            Err(super::super::resilient_apply::ApplyFailure::Failed(error)) => {
                self.add_change_errors(&mut outcome, &error, &changes);
                return outcome;
            }
            Err(super::super::resilient_apply::ApplyFailure::RestoreConflict(error)) => {
                outcome.mark_restore_conflict();
                self.add_change_errors(&mut outcome, &error, &changes);
                return outcome;
            }
        };
        let journal = prepared.journal();
        let expected = mutation.expected;
        let report = mutation.report;
        // Candidates recovery dropped for a non-resolver failure (verification, stale lock — e.g.
        // a peer contract the resolve broke without a culpable candidate) are per-candidate
        // errors: rendering them as skips would hide a real defect behind "the resolver rejected
        // this change" and a `0 errors` summary.
        for rejection in mutation.rejected {
            let diag = self.project_diag(&rejection.error, Some(&rejection.change.package.name));
            outcome
                .items
                .push(self.change_error_item(&rejection.change, diag));
        }
        if let Err(error) = validate_edge_rebinds(&report.edge_rebinds) {
            self.restore_journal_into_outcome(journal, &expected, &mut outcome);
            self.add_change_errors(&mut outcome, &error, &changes);
            return outcome;
        }
        if report.applied.is_empty() {
            self.add_batch_skips(&mut outcome, report.skipped);
            self.restore_journal_into_outcome(journal, &expected, &mut outcome);
            return outcome;
        }
        let report = match self.verify_apply_report(report, &changes).await {
            Ok(report) => report,
            Err(error) => {
                self.restore_journal_into_outcome(journal, &expected, &mut outcome);
                self.add_change_errors(&mut outcome, &error, &changes);
                return outcome;
            }
        };

        let report = self.classify_batch_report(report, &changes, &mut outcome);
        if !report.planned_applied {
            // No requested target landed: roll any incidental resolver movement back to the captured
            // state instead of committing a collateral-only mutation.
            self.restore_journal_into_outcome(journal, &expected, &mut outcome);
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
            self.restore_journal_into_outcome(journal, &expected, &mut outcome);
            return outcome;
        };

        self.commit_batch_report(&mut outcome, &changes, report, committed, notes);
        if let Some(rollback) = rollback
            && let Err(error) = rollback.accept(expected)
        {
            outcome.errors.push(self.project_diag(&error, None));
            outcome.mark_restore_conflict();
        }
        outcome
    }

    async fn prepare_batch_journal(
        &self,
        plan: &Plan,
        rollback: Option<&mut TrialRollback>,
    ) -> cooldown_core::Result<cooldown_core::PreparedMutation> {
        let prepared = self.ctx.prepare_mutation(plan).await?;
        if let Some(rollback) = rollback {
            rollback.preserve(prepared.journal())?;
        }
        Ok(prepared)
    }

    fn batch_plan(&self, changes: &[Change], state: &TrialState) -> Plan {
        Plan {
            changes: changes.to_vec(),
            rewrite: self.ctx.opts.rewrite,
            edge_policy: self.ctx.pctx.edge_policy,
            baseline_violations: plan_baseline_violations(&state.baseline_violations),
        }
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
            edge_rebinds: report.edge_rebinds,
            warnings: report.warnings,
            planned_applied,
        }
    }

    async fn verify_batch_graph(
        &mut self,
        outcome: &mut BatchOutcome,
        changes: &[Change],
        applied: &HashSet<ChangeTargetKey>,
        baseline_violations: &HashSet<BaselineViolation>,
    ) -> Option<CommittedBatch> {
        self.ctx
            .opts
            .progress
            .phase("checking resolved graph cooldown after apply");
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
        let after_keys: HashSet<BaselineViolation> = after.keys().cloned().collect();
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
        report: VerifiedBatchReport,
        committed: CommittedBatch,
        mut notes: HashMap<ChangeTargetKey, Diagnostic>,
    ) {
        let VerifiedBatchReport {
            applied,
            collateral,
            edge_rebinds,
            warnings,
            planned_applied: _,
        } = report;
        outcome.lock_refreshed = self.ctx.writer.successful_apply_proves_lock_current();
        outcome.mark_committed(committed);
        outcome.warnings.extend(warnings.into_iter().map(|warning| {
            warning
                .with_tool(self.ctx.tool_name())
                .with_project(self.project_label.clone())
        }));
        for change in changes {
            let key = change_target_key(change);
            if applied.contains(&key) {
                // This is the first point that knows the change verifiably landed, so it is
                // where a parked advisory-rollback note may finally surface.
                if let Some(note) = notes.remove(&key) {
                    outcome.warnings.push(note);
                }
                outcome.items.push(self.change_applied_item(change));
            }
        }
        for change in &collateral {
            tracing::debug!(
                package = %change.package.name,
                from = %change.from,
                to = %change.to,
                "collateral applied row committed"
            );
            outcome.items.push(self.change_applied_item(change));
        }
        if self.initial_edge_snapshot.is_none() {
            for rebind in &edge_rebinds {
                outcome.edge_items.push(self.edge_rebind_item(rebind));
            }
        } else {
            outcome.edge_rebinds.extend(edge_rebinds);
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
        if matches!(self.mode, PlanMode::Upgrade { .. })
            && let Ok(constraints) = self
                .ctx
                .reader
                .manifest_constraints(&self.ctx.pctx.project)
                .await
        {
            deps.extend(constraints);
        }
        let mut report = verify_applied_targets(report, planned, &deps);
        // Adapter collateral rows (a transitive the joint resolve floated) carry no member
        // attribution — the adapter diffs lockfiles, not the graph. The post-apply graph read
        // above knows which members reach every landed slot, so fill the gap here; without it a
        // transitive applied row renders an empty "Used by" while its reconcile siblings say
        // "via …".
        let members_by_slot: HashMap<(&str, &str), &Vec<cooldown_core::MemberRef>> = deps
            .iter()
            .filter(|dep| !dep.members.is_empty())
            .map(|dep| {
                (
                    (dep.package.name.as_str(), dep.current.as_str()),
                    &dep.members,
                )
            })
            .collect();
        for change in &mut report.applied {
            if change.members.is_empty()
                && !change.direct
                && let Some(members) =
                    members_by_slot.get(&(change.package.name.as_str(), change.to.as_str()))
            {
                change.members = (*members).clone();
            }
        }
        Ok(report)
    }

    /// Record each held candidate (uv lowered it below its ceiling, or the resolve rejected it) as a
    /// skip, naming the package that blocks it via [`conflict_skip_message`].
    fn add_batch_skips(&self, outcome: &mut BatchOutcome, skipped: Vec<cooldown_core::Skipped>) {
        for skipped in skipped {
            // Self-blame (the adapters' generic "resolver rejected" form) must be dropped on the
            // package identity, before labeling: a registry suffix on the label ("… from
            // proxy.golang.org") would defeat [`conflict_skip_message`]'s name-based self check and
            // render "held: conflicts with <itself>".
            let offending = skipped
                .offending
                .filter(|package| *package != skipped.change.package)
                .map(|package| package_label(&package));
            // Deliberate policy holds are conservative-correct, not failed upgrades.
            let conservative_hold = matches!(
                skipped.reason,
                SkipReason::NeedsMajor
                    | SkipReason::DeclaredBoundHeld
                    | SkipReason::MaxMajorHeld
                    | SkipReason::DistTagHeld
                    | SkipReason::MultiVersionHeld
            )
            // `NotEligible` says cooldown has no editable requirement to act on (a transitive-only
            // or path/git dep, an unscopable override, a pnpm catalog-managed pin). Under forward
            // `upgrade` the dep merely stays on its already-matured older version — conservative-
            // correct, and permanently so: failing `--strict` would make e.g. a catalog-using repo
            // unable to ever pass, with no cooldown-side remedy. Under `fix` the same hold leaves a
            // live cooldown violation in the graph, so it still fails strict, consistent with fix's
            // other unfixable-hold paths (graph-held, no matured older release).
            || (skipped.reason == SkipReason::NotEligible
                && matches!(self.mode, PlanMode::Upgrade { .. }));
            if !conservative_hold {
                outcome.strict_incomplete = true;
            }
            let change = skipped.change;
            // An adapter-supplied detail (e.g. the dependent's verbatim peer range) beats the
            // generic per-reason message — it carries facts only the adapter knows.
            let message = skipped.detail.unwrap_or_else(|| {
                conflict_skip_message(skipped.reason, offending.as_deref(), &change.package.name)
            });
            outcome.items.push(self.change_skip_item(
                &change,
                Some(SkippedInfo {
                    reason: skipped.reason,
                    message,
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
        after: &HashMap<BaselineViolation, bool>,
        after_keys: &HashSet<BaselineViolation>,
        changes: &[Change],
        applied: &HashSet<ChangeTargetKey>,
        baseline_violations: &HashSet<BaselineViolation>,
    ) -> TransitiveGateVerdict {
        let keep = |reconcile_needed| TransitiveGateVerdict::Keep { reconcile_needed };
        let new_violations: Vec<&BaselineViolation> =
            after_keys.difference(baseline_violations).collect();
        if new_violations.is_empty() {
            return keep(false);
        }
        match self.transitive_mode() {
            TransitiveGate::Hide => keep(false),
            TransitiveGate::Allow => {
                for violation in &new_violations {
                    let package = package_label(&violation.package);
                    let identity = violation_identity(violation);
                    self.add_fix_warning_to_outcome(
                        outcome,
                        &format!(
                            "{identity} is younger than its cooldown; left in place by --transitive allow"
                        ),
                        &package,
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
                if matches!(self.mode, PlanMode::Upgrade { .. }) {
                    return keep(true);
                }
                let Some(forced) = new_violations
                    .iter()
                    .find(|key| !after.get(**key).copied().unwrap_or(false))
                else {
                    // Every new violation is reconcilable; keep the lock and let the reconcile pass
                    // (after the fix loop) downgrade the floated-up transitives.
                    return keep(true);
                };
                let forced = package_label(&forced.package);
                outcome.strict_incomplete = true;
                for change in changes {
                    if applied.contains(&change_target_key(change)) {
                        outcome.items.push(self.change_skip_item(
                            change,
                            Some(SkippedInfo {
                                reason: SkipReason::TransitiveInCooldown,
                                message: conflict_skip_message(
                                    SkipReason::TransitiveInCooldown,
                                    Some(&forced),
                                    &change.package.name,
                                ),
                                offending: Some(forced.clone()),
                            }),
                        ));
                    }
                }
                TransitiveGateVerdict::RollBack
            }
        }
    }

    fn transitive_mode(&self) -> TransitiveGate {
        self.mode.transitive_mode()
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
        expected: &cooldown_core::ProjectMutationState,
        outcome: &mut BatchOutcome,
    ) {
        match journal.restore_if_unchanged(expected) {
            Ok(()) => outcome.mark_restored(),
            Err(error) => {
                outcome.errors.push(self.project_diag(&error, None));
                outcome.mark_restore_conflict();
            }
        }
    }

    async fn finalize(&mut self) -> MutationFlow {
        let mut terminal = false;
        self.ctx.opts.progress.phase("verifying final lock state");
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
                    // Lock-verify and build details below are tool output (stderr can quote
                    // credentialed registry URLs), so each is redacted before it becomes a
                    // diagnostic, matching `diag_from_error`.
                    let diag = Diagnostic::new(
                        DiagnosticKind::StaleLock,
                        cooldown_core::redact::url_secrets(&report.detail),
                    )
                    .with_tool(self.ctx.tool_name())
                    .with_project(self.project_label())
                    .with_path(self.ctx.pctx.project.manifest.as_str());
                    if self.ctx.opts.allow_stale_lock {
                        self.acc.warnings.push(diag);
                    } else {
                        self.acc.errors.push(diag);
                        terminal = true;
                    }
                }
                LockStatus::Unknown => {
                    if self.lock_refreshed_by_apply {
                        self.record_lock_status(LockStatus::Current);
                    } else {
                        self.record_lock_status(LockStatus::Unknown);
                        self.acc.warnings.push(
                            Diagnostic::new(
                                DiagnosticKind::LockUnknown,
                                cooldown_core::redact::url_secrets(&report.detail),
                            )
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
                terminal = true;
            }
        }

        if terminal {
            return ControlFlow::Break(MutationTerminated);
        }

        if self.ctx.opts.build && !self.ctx.defer_build {
            self.acc.build_requested = true;
            self.ctx.opts.progress.phase("building updated project");
            match self.ctx.writer.build(&self.ctx.pctx.project).await {
                Ok(report) => {
                    self.acc.build_ok = Some(self.acc.build_ok.unwrap_or(true) && report.ok);
                    if !report.ok {
                        self.acc.errors.push(
                            Diagnostic::new(
                                DiagnosticKind::ToolFailed,
                                cooldown_core::redact::url_secrets(&report.detail),
                            )
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
        ControlFlow::Continue(())
    }

    fn record_lock_status(&mut self, status: LockStatus) {
        self.acc.lock_status = Some(combine_lock_status(self.acc.lock_status, status));
    }

    /// The graph's too-fresh, non-baselined violations, each mapped to whether a conservative `fix`
    /// gate can prove it is reducible: the graph floor sits below the locked version, so a downgrade
    /// can try to roll it back without violating known lower bounds. `upgrade` uses this same set for
    /// the final residual check, but it no longer relies on the boolean prediction before attempting
    /// reconciliation.
    async fn graph_violations(
        &mut self,
    ) -> cooldown_core::Result<HashMap<BaselineViolation, bool>> {
        // Intentionally the raw, unscoped graph (not `dependencies_in_scope`): a graph-level cooldown
        // violation counts no matter which member pulls the offending version, so `exclude`/`-p`
        // must not narrow it. Only pin ages are read here — never `members` — so nothing leaks.
        let deps = self
            .ctx
            .reader
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await?;
        // The gate is the first pass to see a re-locked graph, and the reconcile planner that
        // follows reads the same snapshot, so covering the new pins here covers both.
        self.top_up_advisories(&deps).await;
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
        let fetched = self
            .ws
            .fetch_locked_releases(
                self.ctx.reader,
                deps,
                &fctx,
                &self.ctx.opts.progress,
                self.ctx.opts.fanout(),
            )
            .await;

        let mut violations = HashMap::new();
        for FetchedRelease {
            dependency: dep,
            result,
        } in fetched
        {
            let locked = result?;
            // Advised with the same feed data the planner used: under the shorten mode a
            // freshly adopted security fix is itself a young pin, and an unadvised gate here
            // would count it as a new violation and roll back the very change the planner just
            // made.
            let advised = self.advisories.as_ref().and_then(|project| {
                project.classify(self.ctx.reader, &dep, std::slice::from_ref(&locked))
            });
            let advisory_ctx = advised.as_ref().map(ClassifiedAdvisories::context);
            let pin = check_pin_advised(
                &dep,
                &locked,
                advisory_ctx.as_ref(),
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
                    insert_graph_violation(&mut violations, &dep);
                }
            }
        }
        Ok(violations)
    }

    fn project_label(&self) -> &str {
        &self.project_label
    }

    fn project_diag(&self, error: &cooldown_core::CoreError, package: Option<&str>) -> Diagnostic {
        if self.ctx.access.is_isolated()
            && let cooldown_core::CoreError::PendingRecovery(detail) = error
        {
            let reason = detail
                .split_once("; recovery evidence at ")
                .map_or(detail.as_str(), |(reason, _)| reason);
            // The reason can carry adapter/tool text; redact like `diag_from_error`, which the
            // fallthrough below already applies to every other error.
            let reason = cooldown_core::redact::url_secrets(reason);
            let mut diagnostic = Diagnostic::new(
                DiagnosticKind::Filesystem,
                format!(
                    "isolated resolver trial was discarded without changing the source project: {reason}"
                ),
            )
            .with_tool(self.ctx.pctx.tool.as_str())
            .with_project(self.project_label());
            if let Some(package) = package {
                diagnostic = diagnostic.with_package(package);
            }
            return diagnostic;
        }
        diag_from_error(error, self.ctx.pctx.tool, self.project_label(), package)
    }

    fn restore_conflict_diag(&self, error: &cooldown_core::CoreError) -> Diagnostic {
        let mut diagnostic = self.project_diag(error, None);
        diagnostic.message = format!(
            "rollback could not restore a known project state; the final mutation state is indeterminate: {}",
            diagnostic.message
        );
        diagnostic
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

    /// One report row for `change`, carrying the security provenance the planner recorded for
    /// that exact target.
    fn change_item(
        &self,
        change: &Change,
        applied: bool,
        skipped: Option<SkippedInfo>,
    ) -> UpgradeItem {
        let mut item = plan_item(
            change,
            &self.project_label,
            self.ctx.tool_name(),
            applied,
            skipped,
        );
        item.security = self
            .security_by_target
            .get(&change_target_key(change))
            .cloned();
        item
    }

    fn change_applied_item(&self, change: &Change) -> UpgradeItem {
        self.change_item(change, true, None)
    }

    fn change_error_item(&self, change: &Change, diag: Diagnostic) -> UpgradeItem {
        let mut item = self.change_item(change, false, None);
        item.error = Some(diag);
        item
    }

    fn change_skip_item(&self, change: &Change, skipped: Option<SkippedInfo>) -> UpgradeItem {
        self.change_item(change, false, skipped)
    }

    /// The report row for one lock-edge rebind: the package column names the dependency whose
    /// binding moved, From/To carry the binding versions, and the `edge` block names the dependent
    /// and the policy outcome.
    /// Version summary counts exclude edge rows explicitly.
    fn edge_rebind_item(&self, rebind: &cooldown_core::EdgeRebind) -> UpgradeItem {
        UpgradeItem {
            name: rebind.dependency.name.clone(),
            tool: self.ctx.tool_name().to_string(),
            project: self.project_label.clone(),
            direct: false,
            downgrade: false,
            members: Vec::new(),
            registry: rebind
                .dependency
                .registry
                .as_deref()
                .map(cooldown_core::redact::url_secrets),
            from: rebind.from.to_string(),
            to: rebind.to.to_string(),
            // Informational: the kind of the binding jump, when classifiable.
            kind: self
                .ctx
                .reader
                .classify_update_kind(rebind.from.as_str(), rebind.to.as_str())
                .unwrap_or(UpdateKind::Minor),
            applied: rebind.action.is_applied(),
            skipped: None,
            error: None,
            edge: Some(crate::app::UpgradeEdgeInfo {
                dependent: rebind.dependent.clone(),
                dependent_version: rebind.dependent_version.to_string(),
                dependent_source: rebind
                    .dependent_source
                    .as_deref()
                    .map(cooldown_core::redact::url_secrets),
                action: rebind.action,
                detail: rebind
                    .detail
                    .as_deref()
                    .map(cooldown_core::redact::url_secrets),
            }),
            security: None,
        }
    }

    fn record_change_skip(&mut self, change: &Change, skipped: Option<SkippedInfo>) {
        let item = self.change_skip_item(change, skipped);
        self.acc.items.push(item);
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

fn validate_edge_rebinds(rebinds: &[cooldown_core::EdgeRebind]) -> cooldown_core::Result<()> {
    for rebind in rebinds {
        rebind.validate()?;
    }
    Ok(())
}

/// Folds a batch journal into the trial-wide rollback journal, keeping the first snapshot per
/// path.
fn preserve_rollback_entries(
    rollback: &mut ProjectMutationJournal,
    journal: &ProjectMutationJournal,
) -> cooldown_core::Result<()> {
    // By the mutation-journal contract, an earlier plan could not have changed a path it did not
    // capture.
    // Its first appearance therefore still contains the pre-trial bytes even when a reconciliation
    // plan expands the write set.
    rollback.extend_missing(journal)
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
        edge_rebinds: report.edge_rebinds,
        warnings: report.warnings,
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
        detail: None,
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

#[cfg(test)]
mod tests;
