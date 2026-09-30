//! Locks a mutating run stales itself.
//!
//! Projects of one tool can resolve against each other's manifests: a standalone cargo project
//! (a nested workspace, a cargo-fuzz crate) that path-depends on a member of the root workspace
//! reads the root's `[workspace.dependencies]`.
//! When one project's pass rewrites such a manifest, the other project's lock goes stale, and
//! evaluating it would fail on a condition the run caused.
//!
//! So a source-mutating run records which locks were current before its first project ran.
//! A recorded lock found stale at its project's turn was staled by the run: it is refreshed, and
//! the project is gated with a `fix` pass before the requested one, because the upgrade executor
//! accepts every violation it finds at its start as baseline and would otherwise wave through
//! whatever too-fresh release the refresh resolved.
//! A recorded lock a later project stales after its own turn is caught by a re-probe once the
//! lanes finish, and its project runs again the same way.
//! A lock that was already stale when the run began keeps the ordinary stale-lock handling.

use super::UpgradeAccum;
use super::executor::PlanMode;
use crate::app::lanes::LaneAccess;
use crate::app::{
    ProjectCtx, ProjectProgress, RunOpts, Workspace, diag_from_error, stale_evaluation_skipped,
};
use camino::Utf8PathBuf;
use cooldown_core::{Diagnostic, DiagnosticKind, LockStatus, LockVerifyReport, ToolId, ToolWrite};
use std::collections::{HashMap, HashSet};

/// A project's identity across the run: one tool never detects two projects at one root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProjectKey {
    tool: ToolId,
    root: Utf8PathBuf,
}

impl ProjectKey {
    fn of(pctx: &ProjectCtx) -> Self {
        ProjectKey {
            tool: pctx.tool,
            root: pctx.project.root.clone(),
        }
    }
}

/// The projects whose lock was current when the run began and whose staleness the run therefore
/// owns.
#[derive(Debug, Default)]
pub(super) struct CurrentAtStart {
    projects: HashSet<ProjectKey>,
}

impl CurrentAtStart {
    pub(super) fn contains(&self, pctx: &ProjectCtx) -> bool {
        self.projects.contains(&ProjectKey::of(pctx))
    }

    pub(super) fn len(&self) -> usize {
        self.projects.len()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.projects.is_empty()
    }

    /// Stops tracking a project whose pass failed: the run has already reported it, and running
    /// it again would only repeat the failure.
    pub(super) fn settle(&mut self, pctx: &ProjectCtx, acc: &UpgradeAccum) {
        if !acc.errors.is_empty() {
            self.projects.remove(&ProjectKey::of(pctx));
        }
    }
}

/// What a tracked project's turn found its lock in.
pub(super) enum TurnLock {
    /// The lock is as the run found it (or its state could not be probed); the project runs as
    /// usual.
    Unchanged,
    /// The run staled the lock and it was refreshed; the project needs the gate pass.
    Refreshed,
    /// The refresh failed, and the failure is recorded; the project is not evaluated.
    Failed,
}

/// A project's accumulator, labeled with the project, as a lane hands it back.
pub(super) struct ProjectPass<'a> {
    pub(super) pctx: &'a ProjectCtx,
    pub(super) acc: UpgradeAccum,
}

/// A tracked project the re-probe found stale, with the probe's detail.
struct StaledProject<'a> {
    pctx: &'a ProjectCtx,
    detail: String,
}

/// The tools with at least two projects among `tools`: a lone project cannot be staled by
/// another one.
fn tools_with_several_projects(tools: impl Iterator<Item = ToolId>) -> HashSet<ToolId> {
    let mut counts: HashMap<ToolId, usize> = HashMap::new();
    for tool in tools {
        *counts.entry(tool).or_default() += 1;
    }
    counts
        .into_iter()
        .filter(|&(_, count)| count > 1)
        .map(|(tool, _)| tool)
        .collect()
}

/// The pass that gates what a refresh resolved before the requested `mode` runs, or `None` when
/// `mode` already is a `fix`, which gates the whole graph itself.
///
/// It keeps the run's transitive mode and leaves exact pins alone, since the refresh never
/// rewrites a pin and `--downgrade-pinned` is a `fix` flag.
pub(super) fn refresh_gate(mode: PlanMode) -> Option<PlanMode> {
    match mode {
        PlanMode::Fix { .. } => None,
        PlanMode::Upgrade { transitive } => Some(PlanMode::Fix {
            transitive,
            downgrade_pinned: false,
        }),
    }
}

impl Workspace {
    /// The in-scope projects whose lock is current before the run mutates anything.
    ///
    /// Only a source mutation can stale a lock, and only a project that shares its tool with
    /// another in-scope project and whose adapter can refresh a lock is probed.
    /// A probe that fails leaves the project untracked, so it keeps the ordinary handling.
    pub(super) async fn locks_current_at_start(&self, opts: &RunOpts) -> CurrentAtStart {
        if !opts.mutates_source() {
            return CurrentAtStart::default();
        }
        let shared = tools_with_several_projects(self.scoped_projects(opts).map(|pctx| pctx.tool));
        let eligible = |pctx: &ProjectCtx| {
            shared.contains(&pctx.tool)
                && self
                    .mutator(pctx.tool)
                    .is_some_and(ToolWrite::supports_lock_refresh)
        };
        if !self.scoped_projects(opts).any(eligible) {
            return CurrentAtStart::default();
        }
        opts.progress.phase("checking which locks are current");
        let eligible = &eligible;
        let current = self
            .lanes_over(
                self.scoped_projects(opts).filter(|pctx| eligible(pctx)),
                opts,
                LaneAccess::Shared,
            )
            .run(|pctx| async move {
                self.probe_lock(pctx)
                    .await
                    .is_some_and(|report| report.status == LockStatus::Current)
                    .then(|| ProjectKey::of(pctx))
            })
            .await;
        CurrentAtStart {
            projects: current.into_iter().flatten().collect(),
        }
    }

    /// Probes whether `pctx`'s lock is current under a shared lease; `None` when the probe
    /// cannot run.
    async fn probe_lock(&self, pctx: &ProjectCtx) -> Option<LockVerifyReport> {
        let reader = self.adapter(pctx.tool)?;
        let _guard = self
            .project_read_guard(pctx)
            .await
            .inspect_err(
                |err| tracing::debug!(%err, project = %pctx.rel_path, "lock probe skipped"),
            )
            .ok()?;
        reader
            .verify_lock_current(&pctx.project)
            .await
            .inspect_err(|err| tracing::debug!(%err, project = %pctx.rel_path, "lock probe failed"))
            .ok()
    }

    /// Refreshes a tracked project's lock if another project in the run staled it, recording
    /// the refresh as a warning, or its failure, into `acc`.
    pub(super) async fn refresh_run_staled_lock(
        &self,
        pctx: &ProjectCtx,
        opts: &RunOpts,
        writer: &dyn ToolWrite,
        progress: &ProjectProgress,
        acc: &mut UpgradeAccum,
    ) -> TurnLock {
        progress.phase("checking lock state");
        let Some(probe) = self.probe_lock(pctx).await else {
            return TurnLock::Unchanged;
        };
        if probe.status != LockStatus::Stale {
            return TurnLock::Unchanged;
        }
        let project_diag = |diagnostic: Diagnostic| {
            diagnostic
                .with_tool(pctx.tool.as_str())
                .with_project(pctx.rel_path.as_str())
                .with_path(pctx.project.manifest.as_str())
        };
        let refresh = match self.refresh_lock_under_lease(pctx, writer, progress).await {
            Ok(refresh) => refresh,
            Err(err) => {
                acc.errors.push(
                    diag_from_error(&err, pctx.tool, pctx.rel_path.as_str(), None)
                        .with_path(pctx.project.manifest.as_str()),
                );
                return TurnLock::Failed;
            }
        };
        // The pass that follows takes the lease again, and a same-process conflict on it fails
        // at once, so the refresh gives it up first.
        drop(refresh.guard);
        acc.warnings.extend(refresh.warnings);
        match refresh.report {
            Ok(Some(report)) if report.status == LockStatus::Current => {
                acc.warnings.push(project_diag(Diagnostic::new(
                    DiagnosticKind::StaleLock,
                    format!(
                        "the lock in {} went stale because another project in this run changed a manifest it resolves against; it was refreshed ({}) and the result gated before the project was evaluated",
                        pctx.project.root,
                        cooldown_core::redact::url_secrets(&report.detail)
                    ),
                )));
                TurnLock::Refreshed
            }
            Ok(Some(report)) => {
                let diagnostic = project_diag(Diagnostic::new(
                    DiagnosticKind::StaleLock,
                    cooldown_core::redact::url_secrets(&report.detail),
                ));
                if opts.allow_stale_lock {
                    acc.warnings.push(stale_evaluation_skipped(diagnostic));
                } else {
                    acc.errors.push(diagnostic);
                }
                TurnLock::Failed
            }
            Ok(None) => TurnLock::Unchanged,
            Err(err) => {
                acc.errors.push(
                    diag_from_error(&err, pctx.tool, pctx.rel_path.as_str(), None)
                        .with_path(pctx.project.manifest.as_str()),
                );
                TurnLock::Failed
            }
        }
    }

    /// Runs again every tracked project a later project staled after its own turn, until none
    /// is stale, folding each pass into `acc` after the passes already there.
    ///
    /// A re-run can stale another tracked project in turn (two standalone projects that
    /// path-depend on each other's workspaces), so the rounds repeat.
    /// Each round re-runs only the projects the previous round's passes staled, so unless the
    /// staling forms a cycle, a chain of rounds visits each tracked project at most once and as
    /// many rounds as there are tracked projects settle it.
    /// A lock still stale after them is caught in a cycle and is reported rather than chased
    /// forever.
    pub(super) async fn rerun_staled_projects(
        &self,
        opts: &RunOpts,
        mode: PlanMode,
        mut tracked: CurrentAtStart,
        acc: &mut UpgradeAccum,
    ) {
        let rounds = tracked.len();
        for round in 0..=rounds {
            let staled = self.staled_tracked_projects(opts, &tracked).await;
            if staled.is_empty() {
                return;
            }
            if round == rounds {
                for StaledProject { pctx, detail } in staled {
                    let diagnostic = Diagnostic::new(
                        DiagnosticKind::StaleLock,
                        format!(
                            "{}: the lock was still stale after {rounds} rounds of re-runs, because projects in this run kept changing manifests it resolves against",
                            cooldown_core::redact::url_secrets(&detail)
                        ),
                    )
                    .with_tool(pctx.tool.as_str())
                    .with_project(pctx.rel_path.as_str())
                    .with_path(pctx.project.manifest.as_str());
                    if opts.allow_stale_lock {
                        acc.warnings.push(diagnostic);
                    } else {
                        acc.errors.push(diagnostic);
                    }
                }
                return;
            }
            let tracked_ref = &tracked;
            let passes = self
                .lanes_over(
                    staled.iter().map(|project| project.pctx),
                    opts,
                    LaneAccess::for_mutation(opts),
                )
                .run(|pctx| async move {
                    ProjectPass {
                        pctx,
                        acc: self.run_plan_project(pctx, opts, mode, tracked_ref).await,
                    }
                })
                .await;
            for ProjectPass { pctx, acc: pass } in passes {
                tracked.settle(pctx, &pass);
                super::merge_upgrade_accum(acc, pass);
            }
        }
    }

    /// The tracked projects whose lock is stale now, in scoped order.
    async fn staled_tracked_projects<'a>(
        &'a self,
        opts: &'a RunOpts,
        tracked: &CurrentAtStart,
    ) -> Vec<StaledProject<'a>> {
        if tracked.is_empty() {
            return Vec::new();
        }
        let probed = self
            .lanes_over(
                self.scoped_projects(opts)
                    .filter(|pctx| tracked.contains(pctx)),
                opts,
                LaneAccess::Shared,
            )
            .run(|pctx| async move {
                self.probe_lock(pctx)
                    .await
                    .filter(|report| report.status == LockStatus::Stale)
                    .map(|report| StaledProject {
                        pctx,
                        detail: report.detail,
                    })
            })
            .await;
        probed.into_iter().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::{CurrentAtStart, PlanMode, ProjectKey, refresh_gate, tools_with_several_projects};
    use crate::app::TransitiveGate;
    use crate::app::upgrade::UpgradeAccum;
    use crate::app::workspace::tests::project_ctx;
    use camino::Utf8PathBuf;
    use cooldown_core::{Diagnostic, DiagnosticKind, ToolId};
    use std::collections::HashSet;

    const CARGO: ToolId = ToolId("cargo");
    const GO: ToolId = ToolId("go");
    const UV: ToolId = ToolId("uv");

    /// Only a tool with a second project can have a lock staled by another project of the run.
    #[test]
    fn only_tools_with_several_projects_are_tracked() {
        let shared = tools_with_several_projects([CARGO, GO, CARGO, UV, CARGO].into_iter());

        assert_eq!(shared, HashSet::from([CARGO]));
        assert!(tools_with_several_projects(std::iter::empty()).is_empty());
    }

    /// An upgrade is preceded by a `fix` pass under the same transitive mode that leaves pins
    /// alone; a `fix` needs no extra pass, since it gates the whole graph itself.
    #[test]
    fn a_refresh_is_gated_by_a_fix_pass_unless_the_run_is_a_fix() {
        for transitive in [
            TransitiveGate::Enforce,
            TransitiveGate::Allow,
            TransitiveGate::Hide,
        ] {
            let gate = refresh_gate(PlanMode::Upgrade { transitive });
            assert!(
                matches!(
                    gate,
                    Some(PlanMode::Fix {
                        transitive: gated,
                        downgrade_pinned: false,
                    }) if gated == transitive
                ),
                "an upgrade under {transitive:?} is gated by a fix under the same mode"
            );
        }
        assert!(
            refresh_gate(PlanMode::Fix {
                transitive: TransitiveGate::Enforce,
                downgrade_pinned: true,
            })
            .is_none()
        );
    }

    /// A project whose pass failed is no longer re-run; one that passed stays tracked.
    #[test]
    fn a_failed_pass_stops_tracking_its_project() {
        let project = project_ctx(CARGO, "/repo/fuzz");
        let mut tracked = CurrentAtStart {
            projects: HashSet::from([ProjectKey::of(&project)]),
        };

        tracked.settle(&project, &UpgradeAccum::default());
        assert!(tracked.contains(&project));

        let failed = UpgradeAccum {
            errors: vec![Diagnostic::new(DiagnosticKind::StaleLock, "stale")],
            ..UpgradeAccum::default()
        };
        tracked.settle(&project, &failed);
        assert!(!tracked.contains(&project));
        assert_eq!(tracked.len(), 0);
    }

    /// Projects are told apart by tool and root.
    #[test]
    fn tracking_keys_on_tool_and_root() {
        let cargo = project_ctx(CARGO, "/repo");
        let tracked = CurrentAtStart {
            projects: HashSet::from([ProjectKey {
                tool: CARGO,
                root: Utf8PathBuf::from("/repo"),
            }]),
        };

        assert!(tracked.contains(&cargo));
        assert!(!tracked.contains(&project_ctx(GO, "/repo")));
        assert!(!tracked.contains(&project_ctx(CARGO, "/repo/fuzz")));
    }
}
