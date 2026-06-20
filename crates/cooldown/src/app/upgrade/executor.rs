use super::{UpgradeAccum, UpgradeCtx};
use crate::app::lock::ProjectLock;
use crate::app::{SkippedInfo, UpgradeItem, Workspace, diag_from_error};
use cooldown_core::{
    Change, DepScope, Dependency, Diagnostic, DiagnosticKind, MajorKey, PackageId, Plan,
    SkipReason, Status, UpdateKind, check_pin, evaluate, evaluate_fix,
};
use std::collections::HashSet;

/// Whether the executor moves dependencies *forward* (`upgrade`) or *backward* to a compliant
/// version (`fix`). The trial/rollback/verify machinery is shared; only planning differs.
#[derive(Clone, Copy)]
pub(super) enum PlanMode {
    /// `upgrade`: move direct deps to the newest matured version.
    Upgrade,
    /// `fix`: downgrade deps whose locked version is too fresh to the newest matured older version.
    Fix {
        /// Also act on too-fresh transitive deps (`--transitive`), not just direct ones.
        transitive: bool,
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
            Ok(violations) => violations,
            Err(error) => {
                self.record_project_error(&error, None);
                return;
            }
        };

        let mut state = TrialState {
            baseline_violations,
        };
        for change in planned {
            if !self.apply_change(change, &mut state).await {
                break;
            }
        }

        self.finalize().await;
    }

    async fn scoped_deps(&mut self) -> Option<Vec<Dependency>> {
        // `fix --transitive` acts on the whole resolved graph; everything else is direct-only.
        let scope = match self.mode {
            PlanMode::Fix { transitive: true, .. } => DepScope::Graph,
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
                downgrade_pinned, ..
            } => self.plan_fix_changes(deps, downgrade_pinned).await,
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
            if fix.current_status != Status::CurrentInCooldown {
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
            self.acc.any_skipped = true;
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
        if let Some((offending_pkg, _)) = after.difference(&state.baseline_violations).next() {
            let restored = self.restore_journal(&journal);
            self.acc.any_skipped = true;
            self.record_change_skip(
                &change,
                Some(SkippedInfo {
                    reason: SkipReason::TransitiveInCooldown,
                    message: SkipReason::TransitiveInCooldown.message().to_string(),
                    offending: Some(offending_pkg.clone()),
                }),
            );
            return restored;
        }

        state.baseline_violations = after;
        self.record_change_applied(&change);
        true
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

    async fn graph_violations(&self) -> cooldown_core::Result<HashSet<(String, String)>> {
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

        let mut violations = HashSet::new();
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
                    violations.insert((dep.package.name.clone(), dep.current.to_string()));
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
