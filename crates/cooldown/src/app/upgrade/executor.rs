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
    /// Whether the last committed batch introduced transitive cooldown violations to reconcile.
    reconcile_needed: bool,
}

/// A restore point captured before the optimistic `upgrade` lock batch, so a transitive the reconcile
/// pass cannot mature down rolls the accumulator and trial state back to exactly here — the lock files
/// themselves are restored from a separate mutation journal.
struct Checkpoint {
    /// `acc.items` length: the optimistically-recorded applied/collateral rows are truncated back to it.
    items_len: usize,
    /// `acc.warnings` length: reconcile's deferred warnings are truncated back to it.
    warnings_len: usize,
    /// `acc.errors` length: a project-level error recorded during the rolled-back lock/reconcile batch
    /// is truncated back to it, so a reverted batch never leaves a stale error (and a flipped exit).
    errors_len: usize,
    /// The graph violations present before the lock batch — the baseline a residual is measured against.
    baseline_violations: HashSet<(String, String)>,
    /// Whether an apply had already proven the lock current before the lock batch.
    lock_refreshed: bool,
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
            self.apply_batch(build_changes, state).await;
        }
        if lock_changes.is_empty() {
            return;
        }

        // Snapshot the lock *after* the independent build batch and *before* the optimistic lock
        // batch. The upgrade gate keeps the lock even when a forward move floats a too-fresh transitive
        // up, trusting the reconcile pass to mature it down; if a violation turns out irreducible, the
        // final gate below restores this snapshot — reverting the lock batch and its reconcile
        // downgrades while leaving the build-backend adoption intact. Capturing it must succeed: with
        // no safety net the optimistic commit could not be undone, so a capture failure skips the lock
        // batch as an error rather than risking an unverifiable graph.
        let lock_plan = Plan {
            changes: lock_changes.clone(),
            rewrite: self.ctx.opts.rewrite,
        };
        let snapshot = match self
            .ctx
            .writer
            .mutation_journal(&self.ctx.pctx.project, &lock_plan)
            .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.record_change_errors(&error, &lock_changes);
                return;
            }
        };
        let checkpoint = self.checkpoint(state);

        // Measure progress for the reconcile decision over the *lock* batch only — a build-only batch
        // floats no transitive up, so it must not trigger (or suppress) reconciliation.
        let applied_before = self.applied_count();
        let errored_before = self.errored_count();
        self.apply_batch(lock_changes.clone(), state).await;
        // Skip reconciliation when the lock upgrade made no clean forward progress: nothing floated up,
        // and a broken re-lock probe must not be re-hit.
        let upgraded_cleanly =
            self.applied_count() > applied_before && self.errored_count() == errored_before;
        if self.transitive_mode() == TransitiveGate::Enforce
            && upgraded_cleanly
            && state.reconcile_needed
        {
            self.reconcile_to_fixpoint(state).await;
        }

        // Final gate: a too-fresh transitive newly forced in that the reconcile pass could not mature
        // down — none matured below it, or a requirer pins it there — is a residual the optimistic
        // commit must not keep. A pre-existing dirty package may float to a different fresh version
        // without rollback, but an additional fresh version line for that package is still new.
        if self.transitive_mode() == TransitiveGate::Enforce {
            let residual = newly_introduced_violations(
                &checkpoint.baseline_violations,
                &state.baseline_violations,
            );
            if !residual.is_empty() {
                self.roll_back_unreconciled(
                    &snapshot,
                    &checkpoint,
                    &lock_changes,
                    &residual,
                    state,
                );
            }
        }

        self.collapse_collateral(&checkpoint.baseline_violations);
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

    /// Capture the accumulator and trial-state restore point before the optimistic lock batch.
    fn checkpoint(&self, state: &TrialState) -> Checkpoint {
        Checkpoint {
            items_len: self.acc.items.len(),
            warnings_len: self.acc.warnings.len(),
            errors_len: self.acc.errors.len(),
            baseline_violations: state.baseline_violations.clone(),
            lock_refreshed: self.lock_refreshed_by_apply,
        }
    }

    /// Undo an optimistic lock batch whose floated-up transitive the reconcile pass could not clear:
    /// restore the snapshotted lock, rewind the accumulator/trial state to `checkpoint`, and re-report
    /// each planned lock change as held by the still-too-fresh transitive it would force in.
    fn roll_back_unreconciled(
        &mut self,
        snapshot: &cooldown_core::ProjectMutationJournal,
        checkpoint: &Checkpoint,
        lock_changes: &[Change],
        residual: &[(String, String)],
        state: &mut TrialState,
    ) {
        // Rewind the accumulator to the checkpoint BEFORE restoring the lock: a restore failure records
        // its own project error, which must survive the truncation (the on-disk lock is then in an
        // unknown state the user has to see). acc.errors is rewound alongside items/warnings so a
        // project error from the now-reverted lock/reconcile batch does not flip the exit.
        self.acc.items.truncate(checkpoint.items_len);
        self.acc.warnings.truncate(checkpoint.warnings_len);
        self.acc.errors.truncate(checkpoint.errors_len);
        self.lock_refreshed_by_apply = checkpoint.lock_refreshed;
        state
            .baseline_violations
            .clone_from(&checkpoint.baseline_violations);
        state.reconcile_needed = false;
        self.restore_journal(snapshot);
        self.acc.strict_incomplete = true;
        // Name one stuck transitive as the offender (sorted for a stable report).
        let offender = residual.iter().map(|(name, _)| name.clone()).min();
        for change in lock_changes {
            self.record_change_skip(
                change,
                Some(SkippedInfo {
                    reason: SkipReason::TransitiveInCooldown,
                    message: SkipReason::TransitiveInCooldown.message().to_string(),
                    offending: offender.clone(),
                }),
            );
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
    /// transitives; `Allow` keeps the lock and reports them; `Enforce` reconciles forward `upgrade`
    /// batches optimistically, while backward `fix` batches still roll back immediately when a new
    /// violation has no lower graph floor to try. Returns `true` when the caller should restore the
    /// batch journal.
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
                // `upgrade` is optimistic: a forward move that floats a too-fresh transitive up is
                // never rolled back on a per-node prediction, because a cooled parent cannot require a
                // child newer than the cooldown window — an older version every requirer already
                // accepts exists by construction. Keep the lock and let the reconcile pass mature the
                // floated-up transitives down; `run_upgrade` makes a final gate check and rolls the
                // lock back only for a violation reconcile genuinely could not clear. `fix` stays
                // conservative: it moves *backward*, so a fresh transitive it cannot reduce here is a
                // real, unrecoverable conflict that must roll the batch back immediately.
                if matches!(self.mode, PlanMode::Upgrade) {
                    state.reconcile_needed = true;
                    return false;
                }
                let Some((forced_pkg, _)) = new_violations
                    .iter()
                    .find(|key| !after.get(**key).copied().unwrap_or(false))
                else {
                    // Every new violation is reconcilable; keep the lock and let the reconcile pass
                    // (after the fix loop) downgrade the floated-up transitives.
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
        // A scoped upgrade can float transitives that do not match `--package`; reconcile is the
        // safety pass over the post-apply graph, so it must see the raw graph like `graph_violations`.
        match self
            .ctx
            .reader
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await
        {
            Ok(mut deps) => {
                deps.sort_by(|a, b| {
                    a.package
                        .name
                        .cmp(&b.package.name)
                        .then_with(|| a.current.to_string().cmp(&b.current.to_string()))
                });
                Some(deps)
            }
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
        PlanMode, candidate_scope, collapse_applied_legs, combine_lock_status,
        conflict_skip_message, is_downgrade, newly_introduced_violations, planned_changes_landed,
        target_package, verify_applied_targets,
    };
    use crate::app::{TransitiveGate, UpgradeItem};
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
