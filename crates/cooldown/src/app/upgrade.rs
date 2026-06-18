//! `upgrade` — move direct deps to the newest version older than the cooldown, then re-lock.
//!
//! Acting on transitive deps is a non-goal, so the app applies changes **one at a time**: snapshot
//! the lock, apply a single-change plan, and if the re-lock drags in a too-fresh (non-baselined)
//! transitive, **restore the snapshot** and skip that change as `TransitiveInCooldown` — never
//! committing a lock a subsequent `check` would reject.

use super::lock::ProjectLock;
use super::{Exit, RunOpts, Workspace, diag_from_error};
use cooldown_core::{
    ArtifactScope, Change, DepScope, Dependency, Diagnostic, MajorKey, PackageId, Plan, Release,
    SkipReason, Status, TargetContext, check_pin, evaluate,
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

/// The read-only per-project context shared by the upgrade helpers: the ecosystem adapter, the
/// scoped project, the run options, and the artifact-target context.
struct UpgradeCtx<'a> {
    adapter: &'a dyn cooldown_core::Ecosystem,
    pctx: &'a super::ProjectCtx,
    opts: &'a RunOpts,
    tctx: &'a TargetContext<'a>,
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
            self.upgrade_project(adapter, pctx, opts, &mut acc).await;
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

    /// Plan and apply the upgrade for one project, recording into `acc`.
    async fn upgrade_project(
        &self,
        adapter: &dyn cooldown_core::Ecosystem,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
        acc: &mut UpgradeAccum,
    ) {
        let project_label = pctx.rel_path.to_string();
        let tctx = TargetContext {
            project: &pctx.project,
            environments: &[],
            artifacts: if opts.all_artifacts {
                ArtifactScope::All
            } else {
                ArtifactScope::Environment
            },
        };
        let ctx = UpgradeCtx {
            adapter,
            pctx,
            opts,
            tctx: &tctx,
        };

        // upgrade only changes DIRECT deps.
        let deps = match adapter.dependencies(&pctx.project, DepScope::Direct).await {
            Ok(d) => d
                .into_iter()
                .filter(|d| Self::package_in_scope(opts, &d.package.name))
                .collect::<Vec<Dependency>>(),
            Err(e) => {
                acc.errors
                    .push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                return;
            }
        };

        let planned = self.plan_changes(&ctx, &deps, &project_label, acc).await;

        if opts.dry_run {
            for c in planned {
                acc.items.push(plan_item(
                    &c,
                    &project_label,
                    pctx.ecosystem.as_str(),
                    false,
                    None,
                ));
            }
            return;
        }

        // Acquire the advisory lock for the mutating run.
        let _guard = match ProjectLock::acquire(&pctx.project.root) {
            Ok(g) => g,
            Err(e) => {
                acc.errors
                    .push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                return;
            }
        };

        // The pre-existing violation set: in-cooldown pins we are NOT introducing.
        let baseline_violations = match self.graph_violations(adapter, pctx, opts, &tctx).await {
            Ok(v) => v,
            Err(e) => {
                acc.errors
                    .push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                return;
            }
        };

        let snapshot = match adapter.snapshot_lock(&pctx.project).await {
            Ok(s) => s,
            Err(e) => {
                acc.errors
                    .push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                return;
            }
        };

        let mut lock = LockState {
            snapshot,
            baseline_violations,
        };
        for change in planned {
            self.apply_change(&ctx, change, &mut lock, acc).await;
        }

        self.finalize_project(adapter, pctx, opts, &project_label, acc)
            .await;
    }

    /// Build the candidate change list from each dep's adoptable target, recording any
    /// release-fetch error.
    async fn plan_changes(
        &self,
        ctx: &UpgradeCtx<'_>,
        deps: &[Dependency],
        project_label: &str,
        acc: &mut UpgradeAccum,
    ) -> Vec<Change> {
        let UpgradeCtx {
            adapter,
            pctx,
            opts,
            tctx,
        } = *ctx;
        let rctx = Self::resolve_ctx(pctx, opts);
        let mut planned: Vec<Change> = Vec::new();
        for dep in deps {
            let releases = match adapter.releases(dep, tctx).await {
                Ok(r) => r,
                Err(e) => {
                    acc.errors.push(diag_from_error(
                        &e,
                        pctx.ecosystem,
                        project_label,
                        Some(&dep.package.name),
                    ));
                    continue;
                }
            };
            let verdict = evaluate(dep, &releases, &pctx.policy.layers, &rctx, self.now);
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

    /// Apply a single planned change with restore-on-regression, updating `lock` on acceptance.
    async fn apply_change(
        &self,
        ctx: &UpgradeCtx<'_>,
        change: Change,
        lock: &mut LockState,
        acc: &mut UpgradeAccum,
    ) {
        let UpgradeCtx {
            adapter,
            pctx,
            opts,
            tctx,
        } = *ctx;
        let project_label = pctx.rel_path.to_string();
        let project_label = project_label.as_str();
        let single = Plan {
            changes: vec![change.clone()],
        };
        let report = match adapter.apply(&pctx.project, &single).await {
            Ok(r) => r,
            Err(e) => {
                // A hard apply error → restore and record an item error.
                let _ = adapter.restore_lock(&pctx.project, &lock.snapshot).await;
                let diag = diag_from_error(
                    &e,
                    pctx.ecosystem,
                    project_label,
                    Some(&change.package.name),
                );
                let mut it =
                    plan_item(&change, project_label, pctx.ecosystem.as_str(), false, None);
                it.error = Some(diag);
                acc.items.push(it);
                return;
            }
        };

        if report.applied.is_empty() {
            // The adapter itself skipped (MVS/resolver conflict).
            acc.any_skipped = true;
            let info = report
                .skipped
                .into_iter()
                .next()
                .map(|s| render::SkippedInfo {
                    reason: s.reason,
                    message: s.reason.message().to_string(),
                    offending: s.offending.map(|p| p.name),
                });
            acc.items.push(plan_item(
                &change,
                project_label,
                pctx.ecosystem.as_str(),
                false,
                info,
            ));
            return;
        }

        // Did the re-lock introduce a fresh, non-baselined transitive?
        let after = self
            .graph_violations(adapter, pctx, opts, tctx)
            .await
            .unwrap_or_default();
        if let Some((offending_pkg, _)) = after.difference(&lock.baseline_violations).next() {
            let _ = adapter.restore_lock(&pctx.project, &lock.snapshot).await;
            acc.any_skipped = true;
            acc.items.push(plan_item(
                &change,
                project_label,
                pctx.ecosystem.as_str(),
                false,
                Some(render::SkippedInfo {
                    reason: SkipReason::TransitiveInCooldown,
                    message: SkipReason::TransitiveInCooldown.message().to_string(),
                    offending: Some(offending_pkg.clone()),
                }),
            ));
        } else {
            // Accept: refresh the snapshot/baseline for subsequent changes.
            if let Ok(s) = adapter.snapshot_lock(&pctx.project).await {
                lock.snapshot = s;
            }
            lock.baseline_violations = after;
            acc.items.push(plan_item(
                &change,
                project_label,
                pctx.ecosystem.as_str(),
                true,
                None,
            ));
        }
    }

    /// Re-verify the final lock and, when requested, build — folding both into `acc`.
    async fn finalize_project(
        &self,
        adapter: &dyn cooldown_core::Ecosystem,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
        project_label: &str,
        acc: &mut UpgradeAccum,
    ) {
        // Re-verify the final lock is current. A failed probe is a non-`ok` lock, not silence.
        match adapter.verify_lock_current(&pctx.project).await {
            Ok(v) => acc.lock_verified = Some(acc.lock_verified.unwrap_or(true) && v.ok),
            Err(e) => {
                acc.lock_verified = Some(false);
                acc.errors
                    .push(diag_from_error(&e, pctx.ecosystem, project_label, None));
            }
        }

        if opts.build {
            acc.build_requested = true;
            match adapter.build(&pctx.project).await {
                Ok(v) => acc.build_ok = Some(acc.build_ok.unwrap_or(true) && v.ok),
                Err(_) => acc.build_ok = Some(false),
            }
        }
    }

    /// The set of `(package, version)` pins currently in cooldown (non-exempt, non-acknowledged).
    async fn graph_violations(
        &self,
        adapter: &dyn cooldown_core::Ecosystem,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
        tctx: &TargetContext<'_>,
    ) -> cooldown_core::Result<HashSet<(String, String)>> {
        let deps = adapter.dependencies(&pctx.project, DepScope::Graph).await?;
        let rctx = Self::resolve_ctx(pctx, opts);
        let fetched: Vec<(Dependency, cooldown_core::Result<Release>)> = stream::iter(deps)
            .map(|dep| async move {
                let r = adapter.locked_release(&dep, tctx).await;
                (dep, r)
            })
            .buffer_unordered(opts.fanout())
            .collect()
            .await;

        let mut set = HashSet::new();
        for (dep, result) in fetched {
            let Ok(locked) = result else { continue };
            let pv = check_pin(&dep, &locked, &pctx.policy.layers, &rctx, self.now);
            if pv.status == Status::CurrentInCooldown {
                let project_label = pctx.rel_path.to_string();
                let acked = self.baseline.is_acknowledged(
                    pctx.ecosystem.as_str(),
                    &project_label,
                    &dep.package.name,
                    dep.current.as_str(),
                    dep.package.registry.as_deref(),
                    self.now,
                );
                if !acked {
                    set.insert((dep.package.name.clone(), dep.current.to_string()));
                }
            }
        }
        Ok(set)
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
