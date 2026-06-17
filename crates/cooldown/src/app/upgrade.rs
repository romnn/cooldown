//! `upgrade` — move direct deps to the newest version older than the cooldown, then re-lock.
//!
//! Acting on transitive deps is a non-goal, so the app applies changes **one at a time**: snapshot
//! the lock, apply a single-change plan, and if the re-lock drags in a too-fresh (non-baselined)
//! transitive, **restore the snapshot** and skip that change as `TransitiveInCooldown` — never
//! committing a lock a subsequent `check` would reject.

use super::lock::ProjectLock;
use super::{diag_from_error, Exit, RunOpts, Workspace};
use cooldown_core::{
    check_pin, evaluate, ArtifactScope, Change, DepScope, Dependency, Diagnostic, MajorKey,
    PackageId, Plan, Release, SkipReason, Status, TargetContext,
};
use cooldown_render as render;
use futures::stream::{self, StreamExt};
use std::collections::HashSet;

pub struct UpgradeOutcome {
    pub meta: render::UpgradeMeta,
    pub summary: render::UpgradeSummary,
    pub items: Vec<render::UpgradeItem>,
    pub warnings: Vec<Diagnostic>,
    pub errors: Vec<Diagnostic>,
    pub exit: Exit,
}

impl Workspace {
    pub async fn upgrade(&self, opts: &RunOpts) -> UpgradeOutcome {
        let mut items: Vec<render::UpgradeItem> = Vec::new();
        let mut errors: Vec<Diagnostic> = Vec::new();
        let warnings: Vec<Diagnostic> = Vec::new();
        let mut any_skipped = false;
        let mut build_ok: Option<bool> = None;
        let mut build_requested = opts.build;
        let mut lock_verified: Option<bool> = None;

        for pctx in self.scoped_projects(opts) {
            let Some(adapter) = self.adapter(pctx.ecosystem) else {
                continue;
            };
            let project_label = pctx.rel_path.to_string();

            // upgrade only changes DIRECT deps.
            let deps = match adapter.dependencies(&pctx.project, DepScope::Direct).await {
                Ok(d) => d,
                Err(e) => {
                    errors.push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                    continue;
                }
            };
            let deps: Vec<Dependency> = deps
                .into_iter()
                .filter(|d| Self::package_in_scope(opts, &d.package.name))
                .collect();

            // Build the candidate change list from each dep's adoptable target.
            let mut planned: Vec<Change> = Vec::new();
            let rctx = self.resolve_ctx(pctx, opts);
            let tctx = TargetContext {
                project: &pctx.project,
                environments: &[],
                artifacts: if opts.all_artifacts {
                    ArtifactScope::All
                } else {
                    ArtifactScope::Environment
                },
            };
            for dep in &deps {
                let releases = match adapter.releases(dep, &tctx).await {
                    Ok(r) => r,
                    Err(e) => {
                        errors.push(diag_from_error(
                            &e,
                            pctx.ecosystem,
                            &project_label,
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
                    .map(|c| c.kind)
                    .unwrap_or(cooldown_core::UpdateKind::Minor);
                let current_major = releases
                    .iter()
                    .find(|r| r.version == dep.current)
                    .map(|r| r.major.clone())
                    .unwrap_or(MajorKey(String::new()));
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

            if opts.dry_run {
                for c in planned {
                    items.push(plan_item(
                        &c,
                        &project_label,
                        pctx.ecosystem.as_str(),
                        false,
                        None,
                    ));
                }
                continue;
            }

            // Acquire the advisory lock for the mutating run.
            let _guard = match ProjectLock::acquire(&pctx.project.root) {
                Ok(g) => g,
                Err(e) => {
                    errors.push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                    continue;
                }
            };

            // The pre-existing violation set: in-cooldown pins we are NOT introducing.
            let mut baseline_violations =
                match self.graph_violations(adapter, pctx, opts, &tctx).await {
                    Ok(v) => v,
                    Err(e) => {
                        errors.push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                        continue;
                    }
                };

            let mut snapshot = match adapter.snapshot_lock(&pctx.project).await {
                Ok(s) => s,
                Err(e) => {
                    errors.push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                    continue;
                }
            };

            for change in planned {
                let single = Plan {
                    changes: vec![change.clone()],
                };
                let report = match adapter.apply(&pctx.project, &single).await {
                    Ok(r) => r,
                    Err(e) => {
                        // A hard apply error → restore and record an item error.
                        let _ = adapter.restore_lock(&pctx.project, &snapshot).await;
                        let diag = diag_from_error(
                            &e,
                            pctx.ecosystem,
                            &project_label,
                            Some(&change.package.name),
                        );
                        let mut it = plan_item(
                            &change,
                            &project_label,
                            pctx.ecosystem.as_str(),
                            false,
                            None,
                        );
                        it.error = Some(diag);
                        items.push(it);
                        continue;
                    }
                };

                if !report.applied.is_empty() {
                    // Did the re-lock introduce a fresh, non-baselined transitive?
                    let after = self
                        .graph_violations(adapter, pctx, opts, &tctx)
                        .await
                        .unwrap_or_default();
                    let introduced: Vec<(String, String)> =
                        after.difference(&baseline_violations).cloned().collect();
                    if let Some((offending_pkg, _)) = introduced.first() {
                        let _ = adapter.restore_lock(&pctx.project, &snapshot).await;
                        any_skipped = true;
                        items.push(plan_item(
                            &change,
                            &project_label,
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
                        snapshot = adapter
                            .snapshot_lock(&pctx.project)
                            .await
                            .unwrap_or(snapshot);
                        baseline_violations = after;
                        items.push(plan_item(
                            &change,
                            &project_label,
                            pctx.ecosystem.as_str(),
                            true,
                            None,
                        ));
                    }
                } else {
                    // The adapter itself skipped (MVS/resolver conflict).
                    any_skipped = true;
                    let sk = report.skipped.into_iter().next();
                    let info = sk.map(|s| render::SkippedInfo {
                        reason: s.reason,
                        message: s.reason.message().to_string(),
                        offending: s.offending.map(|p| p.name),
                    });
                    items.push(plan_item(
                        &change,
                        &project_label,
                        pctx.ecosystem.as_str(),
                        false,
                        info,
                    ));
                }
            }

            // Re-verify the final lock is current. A failed probe is a non-`ok` lock, not silence.
            match adapter.verify_lock_current(&pctx.project).await {
                Ok(v) => lock_verified = Some(lock_verified.unwrap_or(true) && v.ok),
                Err(e) => {
                    lock_verified = Some(false);
                    errors.push(diag_from_error(&e, pctx.ecosystem, &project_label, None));
                }
            }

            if opts.build {
                build_requested = true;
                match adapter.build(&pctx.project).await {
                    Ok(v) => build_ok = Some(build_ok.unwrap_or(true) && v.ok),
                    Err(_) => build_ok = Some(false),
                }
            }
        }

        let applied = items.iter().filter(|i| i.applied).count();
        let skipped = items.iter().filter(|i| i.skipped.is_some()).count();
        let err_count = items.iter().filter(|i| i.error.is_some()).count() + errors.len();
        let any_applied = applied > 0;

        // Fail-closed on a failed re-lock or build: a passing `upgrade` must leave a sound lock.
        let lock_or_build_failed = lock_verified == Some(false) || build_ok == Some(false);
        let exit = if err_count > 0 || lock_or_build_failed {
            Exit::Environment
        } else if opts.strict && any_skipped {
            Exit::Policy
        } else {
            Exit::Ok
        };

        let meta = render::UpgradeMeta {
            applied: any_applied,
            lock_verified: if opts.dry_run { None } else { lock_verified },
            build: render::BuildInfo {
                requested: build_requested,
                ok: build_ok,
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
            items,
            warnings,
            errors,
            exit,
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
        let rctx = self.resolve_ctx(pctx, opts);
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

/// Reconstruct the target `PackageId`, handling Go-style `/vN` path-major changes (the MajorKey is a
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
