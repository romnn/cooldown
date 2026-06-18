//! `upgrade` — move direct deps to the newest version older than the cooldown, then re-lock.
//!
//! Acting on transitive deps is a non-goal, so the app applies changes **one at a time**: snapshot
//! the lock, apply a single-change plan, and if the re-lock drags in a too-fresh (non-baselined)
//! transitive, **restore the snapshot** and skip that change as `TransitiveInCooldown` — never
//! committing a lock a subsequent `check` would reject.

use super::lock::ProjectLock;
use super::{Exit, RunOpts, Workspace, diag_from_error};
use cooldown_core::{
    Change, DepScope, Dependency, Diagnostic, DiagnosticKind, FetchContext, MajorKey, PackageId,
    Plan, Release, SkipReason, Status, check_pin, evaluate,
};
use cooldown_render as render;
use futures::stream::{self, StreamExt};
use std::collections::HashSet;

/// The result of `upgrade`: the plan that was applied (or, with `--dry-run`, the plan that would
/// be), plus the re-lock/build status and the exit it implies.
pub struct UpgradeOutcome {
    /// Whether anything was applied, the final lock-verification result, and the build outcome.
    pub meta: render::UpgradeMeta,
    /// Applied / skipped / error counts.
    pub summary: render::UpgradeSummary,
    /// One entry per planned change, marked applied, skipped (with reason), or errored.
    pub items: Vec<render::UpgradeItem>,
    /// Non-fatal diagnostics (currently unused; reserved for parity with other commands).
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors (a failed apply, a failed re-lock probe, etc.).
    pub errors: Vec<Diagnostic>,
    /// The process exit; non-zero on any error, or under `--strict` when a change was skipped.
    pub exit: Exit,
}

/// The evolving lock state during one project's one-change-at-a-time apply loop: the snapshot to
/// restore to, and the violation set as of that snapshot.
struct LockState {
    snapshot: cooldown_core::LockSnapshot,
    /// In-cooldown, non-baselined pins present at `snapshot` — the ones we are NOT introducing.
    baseline_violations: HashSet<(String, String)>,
}

/// The mutable state accumulated across all projects in an upgrade run.
#[derive(Default)]
struct UpgradeAccum {
    items: Vec<render::UpgradeItem>,
    errors: Vec<Diagnostic>,
    any_skipped: bool,
    /// `None` until a build is attempted; `Some(false)` once any project's build fails.
    build_ok: Option<bool>,
    build_requested: bool,
    /// `None` until the lock is verified; `Some(false)` once any project's lock is non-current.
    lock_verified: Option<bool>,
}

/// The read-only per-project context shared by the upgrade executor: the ecosystem adapter, the
/// scoped project, and the run options.
struct UpgradeCtx<'a> {
    adapter: &'a dyn cooldown_core::Ecosystem,
    pctx: &'a super::ProjectCtx,
    opts: &'a RunOpts,
}

impl<'a> UpgradeCtx<'a> {
    fn new(
        adapter: &'a dyn cooldown_core::Ecosystem,
        pctx: &'a super::ProjectCtx,
        opts: &'a RunOpts,
    ) -> Self {
        UpgradeCtx {
            adapter,
            pctx,
            opts,
        }
    }

    fn ecosystem_name(&self) -> &'static str {
        self.pctx.ecosystem.as_str()
    }

    fn fetch_context(&self) -> FetchContext<'_> {
        FetchContext {
            project: &self.pctx.project,
            environments: &[],
            artifacts: self.opts.artifact_scope(),
        }
    }
}

/// The cohesive per-project upgrade state machine: dependency discovery, planning, one-change
/// trials, rollback, and final verification.
struct ProjectUpgradeExecutor<'a, 'b> {
    ws: &'a Workspace,
    ctx: UpgradeCtx<'b>,
    project_label: String,
    acc: &'a mut UpgradeAccum,
}

impl Workspace {
    /// Move direct deps to the newest version older than the cooldown, applying changes one at a
    /// time and re-locking after each.
    ///
    /// If a re-lock drags in a too-fresh, non-baselined transitive, the lock snapshot is restored
    /// and that change is reported as skipped — never committing a lock a subsequent `check` would
    /// reject. With `--dry-run` the plan is reported without mutation.
    pub async fn upgrade(&self, opts: &RunOpts) -> UpgradeOutcome {
        let mut acc = UpgradeAccum {
            build_requested: opts.build,
            ..UpgradeAccum::default()
        };

        for pctx in self.scoped_projects(opts) {
            let Some(adapter) = self.adapter(pctx.ecosystem) else {
                continue;
            };
            ProjectUpgradeExecutor::new(self, adapter, pctx, opts, &mut acc)
                .run()
                .await;
        }

        let applied = acc.items.iter().filter(|i| i.applied).count();
        let skipped = acc.items.iter().filter(|i| i.skipped.is_some()).count();
        let err_count = acc.items.iter().filter(|i| i.error.is_some()).count() + acc.errors.len();

        // Fail-closed on a failed re-lock or build: a passing `upgrade` must leave a sound lock.
        let lock_or_build_failed = acc.lock_verified == Some(false) || acc.build_ok == Some(false);
        let exit = if err_count > 0 || lock_or_build_failed {
            Exit::Environment
        } else if opts.strict && acc.any_skipped {
            Exit::Policy
        } else {
            Exit::Ok
        };

        let meta = render::UpgradeMeta {
            applied: applied > 0,
            lock_verified: if opts.dry_run {
                None
            } else {
                acc.lock_verified
            },
            build: render::BuildInfo {
                requested: acc.build_requested,
                ok: acc.build_ok,
            },
        };
        let summary = render::UpgradeSummary {
            applied,
            skipped,
            errors: err_count,
        };
        UpgradeOutcome {
            meta,
            summary,
            items: acc.items,
            warnings: Vec::new(),
            errors: acc.errors,
            exit,
        }
    }
}

impl<'a, 'b> ProjectUpgradeExecutor<'a, 'b> {
    fn new(
        ws: &'a Workspace,
        adapter: &'b dyn cooldown_core::Ecosystem,
        pctx: &'b super::ProjectCtx,
        opts: &'b RunOpts,
        acc: &'a mut UpgradeAccum,
    ) -> Self {
        ProjectUpgradeExecutor {
            ws,
            ctx: UpgradeCtx::new(adapter, pctx, opts),
            project_label: pctx.rel_path.to_string(),
            acc,
        }
    }

    async fn run(&mut self) {
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
            Err(e) => {
                self.record_project_error(&e, None);
                return;
            }
        };

        let baseline_violations = match self.graph_violations().await {
            Ok(violations) => violations,
            Err(e) => {
                self.record_project_error(&e, None);
                return;
            }
        };
        let snapshot = match self.ctx.adapter.snapshot_lock(&self.ctx.pctx.project).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                self.record_project_error(&e, None);
                return;
            }
        };

        let mut lock = LockState {
            snapshot,
            baseline_violations,
        };
        for change in planned {
            if !self.apply_change(change, &mut lock).await {
                break;
            }
        }

        self.finalize().await;
    }

    async fn direct_deps(&mut self) -> Option<Vec<Dependency>> {
        match self
            .ctx
            .adapter
            .dependencies(&self.ctx.pctx.project, DepScope::Direct)
            .await
        {
            Ok(deps) => Some(
                deps.into_iter()
                    .filter(|d| Workspace::package_in_scope(self.ctx.opts, &d.package.name))
                    .collect(),
            ),
            Err(e) => {
                self.record_project_error(&e, None);
                None
            }
        }
    }

    /// Build the candidate change list from each dep's adoptable target, recording any
    /// release-fetch error.
    async fn plan_changes(&mut self, deps: &[Dependency]) -> Vec<Change> {
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let mut planned: Vec<Change> = Vec::new();
        for dep in deps {
            let releases = {
                let fctx = self.ctx.fetch_context();
                self.ctx
                    .adapter
                    .releases(dep, &fctx, self.ctx.opts.candidate_scope())
                    .await
            };
            let releases = match releases {
                Ok(r) => r,
                Err(e) => {
                    self.record_project_error(&e, Some(&dep.package.name));
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
                .find(|c| c.version == target)
                .map_or(cooldown_core::UpdateKind::Minor, |c| c.kind);
            let current_major = releases
                .iter()
                .find(|r| r.version == dep.current)
                .map_or(MajorKey(String::new()), |r| r.major.clone());
            let target_major = releases
                .iter()
                .find(|r| r.version == target)
                .map(|r| r.major.clone())
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

    /// Apply a single planned change with restore-on-regression, updating `lock` on acceptance.
    async fn apply_change(&mut self, change: Change, lock: &mut LockState) -> bool {
        let single = Plan {
            changes: vec![change.clone()],
        };
        let report = match self
            .ctx
            .adapter
            .apply(&self.ctx.pctx.project, &single)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return self
                    .restore_with_change_error(&lock.snapshot, &change, &e)
                    .await;
            }
        };

        if report.applied.is_empty() {
            // The adapter itself skipped (MVS/resolver conflict).
            self.acc.any_skipped = true;
            self.record_change_skip(
                &change,
                report
                    .skipped
                    .into_iter()
                    .next()
                    .map(|s| render::SkippedInfo {
                        reason: s.reason,
                        message: s.reason.message().to_string(),
                        offending: s.offending.map(|p| p.name),
                    }),
            );
            return true;
        }

        // Did the re-lock introduce a fresh, non-baselined transitive?
        let after = match self.graph_violations().await {
            Ok(after) => after,
            Err(e) => {
                return self
                    .restore_with_change_error(&lock.snapshot, &change, &e)
                    .await;
            }
        };
        if let Some((offending_pkg, _)) = after.difference(&lock.baseline_violations).next() {
            let restored = self.restore_snapshot(&lock.snapshot).await;
            self.acc.any_skipped = true;
            self.record_change_skip(
                &change,
                Some(render::SkippedInfo {
                    reason: SkipReason::TransitiveInCooldown,
                    message: SkipReason::TransitiveInCooldown.message().to_string(),
                    offending: Some(offending_pkg.clone()),
                }),
            );
            return restored;
        }

        self.accept_change(lock, change, after).await
    }

    async fn restore_snapshot(&mut self, snapshot: &cooldown_core::LockSnapshot) -> bool {
        match self
            .ctx
            .adapter
            .restore_lock(&self.ctx.pctx.project, snapshot)
            .await
        {
            Ok(()) => true,
            Err(e) => {
                self.record_project_error(&e, None);
                false
            }
        }
    }

    async fn restore_with_change_error(
        &mut self,
        snapshot: &cooldown_core::LockSnapshot,
        change: &Change,
        error: &cooldown_core::CoreError,
    ) -> bool {
        let restored = self.restore_snapshot(snapshot).await;
        let diag = diag_from_error(
            error,
            self.ctx.pctx.ecosystem,
            self.project_label(),
            Some(&change.package.name),
        );
        self.record_change_error(change, diag);
        restored
    }

    async fn accept_change(
        &mut self,
        lock: &mut LockState,
        change: Change,
        after: HashSet<(String, String)>,
    ) -> bool {
        let snapshot = match self.ctx.adapter.snapshot_lock(&self.ctx.pctx.project).await {
            Ok(snapshot) => snapshot,
            Err(e) => {
                self.record_project_error(&e, Some(&change.package.name));
                self.record_change_applied(&change);
                return false;
            }
        };
        lock.snapshot = snapshot;
        lock.baseline_violations = after;
        self.record_change_applied(&change);
        true
    }

    /// Re-verify the final lock and, when requested, build — folding both into `acc`.
    async fn finalize(&mut self) {
        // Re-verify the final lock is current. A failed probe is a non-`ok` lock, not silence.
        match self
            .ctx
            .adapter
            .verify_lock_current(&self.ctx.pctx.project)
            .await
        {
            Ok(v) => {
                self.acc.lock_verified = Some(self.acc.lock_verified.unwrap_or(true) && v.ok);
                if !v.ok {
                    self.acc.errors.push(
                        Diagnostic::new(DiagnosticKind::StaleLock, v.detail)
                            .with_ecosystem(self.ctx.ecosystem_name())
                            .with_project(self.project_label())
                            .with_path(self.ctx.pctx.project.manifest.as_str()),
                    );
                }
            }
            Err(e) => {
                self.acc.lock_verified = Some(false);
                self.record_project_error(&e, None);
            }
        }

        if self.ctx.opts.build {
            self.acc.build_requested = true;
            match self.ctx.adapter.build(&self.ctx.pctx.project).await {
                Ok(v) => {
                    self.acc.build_ok = Some(self.acc.build_ok.unwrap_or(true) && v.ok);
                    if !v.ok {
                        self.acc.errors.push(
                            Diagnostic::new(DiagnosticKind::ToolFailed, v.detail)
                                .with_ecosystem(self.ctx.ecosystem_name())
                                .with_project(self.project_label())
                                .with_tool(build_tool_name(self.ctx.pctx.ecosystem)),
                        );
                    }
                }
                Err(e) => {
                    self.acc.build_ok = Some(false);
                    self.record_project_error(&e, None);
                }
            }
        }
    }

    /// The set of `(package, version)` pins currently in cooldown (non-exempt, non-acknowledged).
    async fn graph_violations(&self) -> cooldown_core::Result<HashSet<(String, String)>> {
        let deps = self
            .ctx
            .adapter
            .dependencies(&self.ctx.pctx.project, DepScope::Graph)
            .await?;
        let rctx = Workspace::resolve_ctx(self.ctx.pctx, self.ctx.opts);
        let fctx = self.ctx.fetch_context();
        let adapter = self.ctx.adapter;
        let fctx_ref = &fctx;
        let fetched: Vec<(Dependency, cooldown_core::Result<Release>)> = stream::iter(deps)
            .map(|dep| async move {
                let r = adapter.locked_release(&dep, fctx_ref).await;
                (dep, r)
            })
            .buffer_unordered(self.ctx.opts.fanout())
            .collect()
            .await;

        let mut set = HashSet::new();
        for (dep, result) in fetched {
            let Ok(locked) = result else { continue };
            let pv = check_pin(
                &dep,
                &locked,
                &self.ctx.pctx.policy.layers,
                &rctx,
                self.ws.now,
            );
            if pv.status == Status::CurrentInCooldown {
                let acked = self.ws.baseline.is_acknowledged(
                    self.ctx.ecosystem_name(),
                    self.project_label(),
                    &dep.package.name,
                    dep.current.as_str(),
                    dep.package.registry.as_deref(),
                    self.ws.now,
                );
                if !acked {
                    set.insert((dep.package.name.clone(), dep.current.to_string()));
                }
            }
        }
        Ok(set)
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

    fn record_change_skip(&mut self, change: &Change, skipped: Option<render::SkippedInfo>) {
        let project_label = self.project_label.clone();
        let ecosystem = self.ctx.ecosystem_name();
        record_skip_item(self.acc, change, &project_label, ecosystem, skipped);
    }
}

fn build_tool_name(ecosystem: cooldown_core::EcosystemId) -> &'static str {
    match ecosystem.as_str() {
        "go" => "go",
        "rust" => "cargo",
        "python" => "uv",
        _ => "tool",
    }
}

/// Reconstruct the target `PackageId`, handling Go-style `/vN` path-major changes (the `MajorKey` is a
/// path suffix). For ecosystems where the package name is stable across majors, the name is kept.
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
    skipped: Option<render::SkippedInfo>,
) -> render::UpgradeItem {
    render::UpgradeItem {
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
    skipped: Option<render::SkippedInfo>,
) {
    acc.items
        .push(plan_item(change, project, ecosystem, false, skipped));
}
