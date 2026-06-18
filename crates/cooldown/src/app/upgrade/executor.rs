use super::{UpgradeAccum, UpgradeCtx, build_tool_name};
use crate::app::lock::ProjectLock;
use crate::app::{SkippedInfo, UpgradeItem, Workspace, diag_from_error};
use cooldown_core::{
    Change, DepScope, Dependency, Diagnostic, DiagnosticKind, MajorKey, PackageId, Plan, Release,
    SkipReason, Status, check_pin, evaluate,
};
use futures::stream::{self, StreamExt};
use std::collections::HashSet;

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
    acc: &'a mut UpgradeAccum,
}

impl<'a, 'b> ProjectUpgradeExecutor<'a, 'b> {
    pub(super) fn new(ws: &'a Workspace, ctx: UpgradeCtx<'b>, acc: &'a mut UpgradeAccum) -> Self {
        ProjectUpgradeExecutor {
            ws,
            project_label: ctx.pctx.rel_path.to_string(),
            ctx,
            acc,
        }
    }

    pub(super) async fn run(&mut self) {
        let Some(deps) = self.direct_deps().await else {
            return;
        };
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

    async fn direct_deps(&mut self) -> Option<Vec<Dependency>> {
        match self
            .ws
            .dependencies_in_scope(
                self.ctx.reader,
                self.ctx.pctx,
                DepScope::Direct,
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

    async fn plan_changes(&mut self, deps: &[Dependency]) -> Vec<Change> {
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let mut planned = Vec::new();
        for dep in deps {
            let releases = {
                let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
                self.ctx
                    .reader
                    .releases(dep, &fctx, self.ctx.opts.candidate_scope())
                    .await
            };
            let releases = match releases {
                Ok(releases) => releases,
                Err(error) => {
                    self.record_project_error(&error, Some(&dep.package.name));
                    continue;
                }
            };
            let verdict = evaluate(
                dep,
                &releases,
                &self.ctx.pctx.policy.layers,
                &rctx,
                self.ws.now,
            );
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
            let package = target_package(dep, &current_major, &target_major);
            planned.push(Change {
                package,
                from: dep.current.clone(),
                to: target,
                kind,
            });
        }
        planned
    }

    fn record_dry_run(&mut self, planned: &[Change]) {
        for change in planned {
            self.acc.items.push(plan_item(
                change,
                self.project_label(),
                self.ctx.ecosystem_name(),
                false,
                None,
            ));
        }
    }

    async fn apply_change(&mut self, change: Change, state: &mut TrialState) -> bool {
        let plan = Plan {
            changes: vec![change.clone()],
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
            self.ctx.pctx.ecosystem,
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
                            .with_ecosystem(self.ctx.ecosystem_name())
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
                                .with_ecosystem(self.ctx.ecosystem_name())
                                .with_project(self.project_label())
                                .with_tool(build_tool_name(self.ctx.pctx.ecosystem)),
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
        let deps = self
            .ctx
            .reader
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await?;
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let fctx = Workspace::fetch_context(self.ctx.pctx, self.ctx.opts);
        let reader = self.ctx.reader;
        let fctx_ref = &fctx;
        let fetched: Vec<(Dependency, cooldown_core::Result<Release>)> = stream::iter(deps)
            .map(|dep| async move {
                let result = reader.locked_release(&dep, fctx_ref).await;
                (dep, result)
            })
            .buffer_unordered(self.ctx.opts.fanout())
            .collect()
            .await;

        let mut violations = HashSet::new();
        for (dep, result) in fetched {
            let locked = result?;
            let pin = check_pin(
                &dep,
                &locked,
                &self.ctx.pctx.policy.layers,
                &rctx,
                self.ws.now,
            );
            if pin.status == Status::CurrentInCooldown {
                let acknowledged = self.ws.baseline.is_acknowledged(
                    self.ctx.ecosystem_name(),
                    self.project_label(),
                    &dep.package.name,
                    dep.current.as_str(),
                    dep.package.registry.as_deref(),
                    self.ws.now,
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
            self.ctx.pctx.ecosystem,
            self.project_label(),
            package,
        ));
    }

    fn record_change_applied(&mut self, change: &Change) {
        let project_label = self.project_label.clone();
        let ecosystem = self.ctx.ecosystem_name();
        self.acc
            .items
            .push(plan_item(change, &project_label, ecosystem, true, None));
    }

    fn record_change_error(&mut self, change: &Change, diag: Diagnostic) {
        let project_label = self.project_label.clone();
        let ecosystem = self.ctx.ecosystem_name();
        record_error_item(self.acc, change, &project_label, ecosystem, diag);
    }

    fn record_change_skip(&mut self, change: &Change, skipped: Option<SkippedInfo>) {
        let project_label = self.project_label.clone();
        let ecosystem = self.ctx.ecosystem_name();
        record_skip_item(self.acc, change, &project_label, ecosystem, skipped);
    }
}

/// Reconstruct the target `PackageId`, handling Go-style `/vN` path-major changes (the `MajorKey`
/// is a path suffix). For ecosystems where the package name is stable across majors, the name is kept.
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
    PackageId::new(dep.package.ecosystem, name, dep.package.registry.clone())
}

fn plan_item(
    change: &Change,
    project: &str,
    ecosystem: &str,
    applied: bool,
    skipped: Option<SkippedInfo>,
) -> UpgradeItem {
    UpgradeItem {
        name: change.package.name.clone(),
        ecosystem: ecosystem.to_string(),
        project: project.to_string(),
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
    ecosystem: &str,
    diag: Diagnostic,
) {
    let mut item = plan_item(change, project, ecosystem, false, None);
    item.error = Some(diag);
    acc.items.push(item);
}

fn record_skip_item(
    acc: &mut UpgradeAccum,
    change: &Change,
    project: &str,
    ecosystem: &str,
    skipped: Option<SkippedInfo>,
) {
    acc.items
        .push(plan_item(change, project, ecosystem, false, skipped));
}
