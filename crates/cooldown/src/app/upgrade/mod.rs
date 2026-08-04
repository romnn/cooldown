//! Shared mutation flow for `upgrade` and `fix`: plan the version changes, apply them as one batch,
//! re-lock, verify the resolved graph against the cooldown gate, and reconcile or roll back.
//!
//! `upgrade` is optimistic about transitives: a forward move that floats a too-fresh transitive up is
//! kept (a cooled parent cannot require a child newer than the window, so an older satisfying version
//! exists by construction), and a reconcile pass matures the floated-up nodes back down to their
//! newest matured version. A trial whose violation cannot be cleared is restored and partitioned;
//! the safe subset is replayed and committed together while unsafe singletons report
//! `TransitiveInCooldown`. No committed lock can make a subsequent `check` reject. `fix` is the dual,
//! downgrading too-fresh pins.

mod executor;

pub(super) use self::executor::target_package_for;
use self::executor::{PlanMode, ProjectRunStatus, ProjectUpgradeExecutor};
use super::{
    BuildInfo, Exit, RunOpts, UpgradeItem, UpgradeMeta, UpgradeSummary, Workspace, diag_from_error,
};
use cooldown_core::{
    AcceptedPublication, Change, Diagnostic, DiagnosticKind, EdgeBindingAction, IsolatedMutation,
    IsolatedMutationStrategy, LockStatus, MutationExecution, PackageId, ToolRead, ToolWrite,
};
use std::collections::HashSet;

/// The result of `upgrade`: the plan that was applied (or, with `--dry-run`, the plan that would
/// be), plus the re-lock/build status and the exit it implies.
pub struct UpgradeOutcome {
    /// Whether anything was applied, the final lock-verification result, and the build outcome.
    pub meta: UpgradeMeta,
    /// Applied / skipped / error counts.
    pub summary: UpgradeSummary,
    /// One entry per planned change, marked applied, skipped (with reason), or errored.
    pub items: Vec<UpgradeItem>,
    /// Non-fatal diagnostics.
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors (a failed apply, a failed re-lock probe, etc.).
    pub errors: Vec<Diagnostic>,
    /// The process exit; non-zero on any error, or under `--strict` when the mutation could not
    /// complete cleanly.
    pub exit: Exit,
}

/// The mutable state accumulated across all projects in an upgrade run.
#[derive(Default)]
pub(super) struct UpgradeAccum {
    pub(super) items: Vec<UpgradeItem>,
    /// Edge relationships remain separate until every version-only reduction is complete.
    pub(super) edge_items: Vec<UpgradeItem>,
    pub(super) errors: Vec<Diagnostic>,
    /// Non-fatal advisories — `fix` records a too-fresh pin it left in place, or a violation with no
    /// matured older version to downgrade to.
    pub(super) warnings: Vec<Diagnostic>,
    pub(super) strict_incomplete: bool,
    /// `None` until a build is attempted; `Some(false)` once any project's build fails.
    pub(super) build_ok: Option<bool>,
    pub(super) build_requested: bool,
    /// `None` until lock currency is probed; tracks the strongest non-current outcome across
    /// projects.
    pub(super) lock_status: Option<LockStatus>,
}

/// The read/write adapter pair and shared per-project inputs the upgrade executor needs.
pub(super) struct UpgradeCtx<'a> {
    pub(super) reader: &'a dyn ToolRead,
    pub(super) writer: &'a dyn ToolWrite,
    pub(super) pctx: &'a super::ProjectCtx,
    pub(super) opts: &'a RunOpts,
    repo_root: &'a camino::Utf8Path,
    access: ProjectExecution,
    defer_build: bool,
}

enum ProjectExecution {
    Source,
    Copy,
    Isolated(cooldown_core::IsolatedMutationProject),
}

impl ProjectExecution {
    const fn is_isolated(&self) -> bool {
        !matches!(self, ProjectExecution::Source)
    }
}

impl<'a> UpgradeCtx<'a> {
    fn new(
        reader: &'a dyn ToolRead,
        writer: &'a dyn ToolWrite,
        pctx: &'a super::ProjectCtx,
        opts: &'a RunOpts,
        repo_root: &'a camino::Utf8Path,
        access: ProjectExecution,
        defer_build: bool,
    ) -> Self {
        UpgradeCtx {
            reader,
            writer,
            pctx,
            opts,
            repo_root,
            access,
            defer_build,
        }
    }

    pub(super) fn tool_name(&self) -> &'static str {
        self.pctx.tool.as_str()
    }

    fn write_guard(&self) -> cooldown_core::Result<Option<super::lock::ProjectAccessWriteGuard>> {
        match &self.access {
            ProjectExecution::Source => super::lock::ProjectAccessWriteGuard::acquire(
                self.repo_root,
                &self.pctx.project.root,
                self.pctx.tool,
                self.writer.sync_scope() == cooldown_core::SyncScope::Repo,
            )
            .map(Some),
            ProjectExecution::Copy | ProjectExecution::Isolated(_) => Ok(None),
        }
    }

    async fn prepare_mutation(
        &self,
        plan: &cooldown_core::Plan,
    ) -> cooldown_core::Result<cooldown_core::PreparedMutation> {
        match &self.access {
            ProjectExecution::Isolated(project) => {
                cooldown_core::PreparedMutation::prepare_isolated(self.writer, project, plan).await
            }
            ProjectExecution::Source | ProjectExecution::Copy => {
                cooldown_core::PreparedMutation::prepare(self.writer, &self.pctx.project, plan)
                    .await
            }
        }
    }
}

impl Workspace {
    /// Move every dependency to the newest version that has matured past the cooldown, applying
    /// changes one at a time and re-locking after each.
    ///
    /// By default this works the whole resolved graph (`opts.transitive_mode`): direct *and* indirect
    /// deps advance to their newest matured version, so an indirect dep a `fix` rolled back is
    /// re-adopted once its newer version clears the window. `Hide` narrows to direct deps; `Allow`
    /// leaves floated-up transitives in place. After the forward moves, the graph is reconciled —
    /// any too-fresh transitive a re-lock dragged in is rolled back — so a single `upgrade` ends
    /// gate-clean. If a forced fresh transitive can't be reconciled, that change is restored and
    /// reported as skipped, never committing a state a subsequent `check` would reject. With
    /// `--dry-run` the plan is reported without mutation.
    pub async fn upgrade(&self, opts: &RunOpts) -> UpgradeOutcome {
        self.run_plan(opts, PlanMode::Upgrade).await
    }

    /// Fix cooldown violations by downgrading every dependency whose locked version is younger than
    /// the cooldown to the newest version that has already matured past it — the dual of `upgrade`,
    /// which never moves a dependency forward.
    ///
    /// By default the whole resolved graph is fixed — too-fresh direct *and* transitive deps are
    /// downgraded to a matured version; `opts.transitive_mode` relaxes that (`Allow` leaves
    /// transitives in place, `Hide` is direct-only), and `opts.downgrade_pinned` rewrites pins down
    /// too. Exact pins are otherwise left in place with a warning. Each downgrade is applied one at a
    /// time with the same rollback/verify trial.
    pub async fn fix(&self, opts: &RunOpts) -> UpgradeOutcome {
        let mode = PlanMode::Fix {
            transitive: opts.transitive_mode,
            downgrade_pinned: opts.downgrade_pinned,
        };
        self.run_plan(opts, mode).await
    }

    async fn run_plan(&self, opts: &RunOpts, mode: PlanMode) -> UpgradeOutcome {
        let mut acc = UpgradeAccum {
            build_requested: opts.build,
            ..UpgradeAccum::default()
        };

        for pctx in self.scoped_projects(opts) {
            let _progress = opts.progress.project(pctx.tool, pctx.rel_path.as_str());
            let Some(reader) = self.adapter(pctx.tool) else {
                continue;
            };
            let Some(writer) = self.mutator(pctx.tool) else {
                acc.errors.push(read_only_mutator_diag(pctx));
                continue;
            };
            if let MutationExecution::Isolated(strategy) = writer.mutation_execution() {
                self.run_isolated_source_project(pctx, opts, mode, strategy, &mut acc)
                    .await;
                continue;
            }

            // Under `--dry-run`, run the same mutation and verification flow against a throwaway
            // copy assembled from the adapter's declared preview inputs.
            // The source lock and manifest are never written.
            // `dry_copy` keeps the temp tree alive while the executor borrows `dry_pctx`.
            let _dry_copy;
            let dry_pctx;
            let effective_pctx = if opts.dry_run {
                opts.progress.phase("preparing isolated dry-run project");
                let copy = match self.project_read_guard(pctx).await {
                    Ok(_guard) => super::project_copy::ProjectCopy::create(
                        &pctx.project,
                        &writer.resolve_inputs(),
                    ),
                    Err(error) => Err(error),
                };
                match copy {
                    Ok(copy) => {
                        dry_pctx = super::ProjectCtx {
                            tool: pctx.tool,
                            project: copy.project.clone(),
                            rel_path: pctx.rel_path.clone(),
                            policy: pctx.policy.clone(),
                            edge_policy: pctx.edge_policy,
                        };
                        _dry_copy = copy;
                        &dry_pctx
                    }
                    Err(error) => {
                        acc.errors.push(diag_from_error(
                            &error,
                            pctx.tool,
                            pctx.rel_path.as_str(),
                            None,
                        ));
                        continue;
                    }
                }
            } else {
                pctx
            };

            let access = if opts.dry_run {
                ProjectExecution::Copy
            } else {
                ProjectExecution::Source
            };
            ProjectUpgradeExecutor::new(
                self,
                UpgradeCtx::new(
                    reader,
                    writer,
                    effective_pctx,
                    opts,
                    self.repo_root(),
                    access,
                    false,
                ),
                mode,
                &mut acc,
            )
            .run()
            .await;
        }

        finalize_outcome(opts, acc)
    }

    /// Runs preselected upgrade targets through the complete policy trial in a project copy.
    pub(super) async fn preview_project_upgrade(
        &self,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
        changes: Vec<Change>,
        manifest_only: HashSet<PackageId>,
    ) -> UpgradeAccum {
        let mut acc = UpgradeAccum::default();
        let Some(reader) = self.adapter(pctx.tool) else {
            return acc;
        };
        let Some(writer) = self.mutator(pctx.tool) else {
            acc.errors.push(read_only_mutator_diag(pctx));
            return acc;
        };
        let guard = match self.project_read_guard(pctx).await {
            Ok(guard) => guard,
            Err(error) => {
                acc.errors.push(diag_from_error(
                    &error,
                    pctx.tool,
                    pctx.rel_path.as_str(),
                    None,
                ));
                return acc;
            }
        };
        let prepared = match writer.mutation_execution() {
            MutationExecution::InPlace => {
                super::project_copy::ProjectCopy::create(&pctx.project, &writer.resolve_inputs())
                    .map(PreparedPreview::Generic)
            }
            MutationExecution::Isolated(strategy) => strategy
                .prepare(&pctx.project, guard.coordination())
                .await
                .map(PreparedPreview::Isolated),
        };
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                acc.errors.push(diag_from_error(
                    &error,
                    pctx.tool,
                    pctx.rel_path.as_str(),
                    None,
                ));
                return acc;
            }
        };
        let copied_pctx = super::ProjectCtx {
            tool: pctx.tool,
            project: prepared.project().clone(),
            rel_path: pctx.rel_path.clone(),
            policy: pctx.policy.clone(),
            edge_policy: pctx.edge_policy,
        };
        let mut preview_opts = opts.clone();
        preview_opts.build = false;
        preview_opts.dry_run = false;
        preview_opts.lock = false;

        ProjectUpgradeExecutor::new(
            self,
            UpgradeCtx::new(
                reader,
                writer,
                &copied_pctx,
                &preview_opts,
                self.repo_root(),
                prepared.execution(),
                false,
            ),
            PlanMode::Upgrade,
            &mut acc,
        )
        .run_policy(changes, manifest_only)
        .await;
        acc
    }

    async fn run_isolated_source_project(
        &self,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
        mode: PlanMode,
        strategy: &dyn IsolatedMutationStrategy,
        acc: &mut UpgradeAccum,
    ) {
        let Some(reader) = self.adapter(pctx.tool) else {
            return;
        };
        let Some(writer) = self.mutator(pctx.tool) else {
            return;
        };
        let guard = match self
            .acquire_isolated_source_access(writer, pctx, opts)
            .await
        {
            Ok((guard, recovery_warnings)) => {
                acc.warnings.extend(recovery_warnings);
                guard
            }
            Err(error) => {
                acc.errors.push(diag_from_error(
                    &error,
                    pctx.tool,
                    pctx.rel_path.as_str(),
                    None,
                ));
                return;
            }
        };
        opts.progress.phase("preparing isolated mutation project");
        let trial = match strategy.prepare(&pctx.project, guard.coordination()).await {
            Ok(trial) => trial,
            Err(error) => {
                acc.errors.push(diag_from_error(
                    &error,
                    pctx.tool,
                    pctx.rel_path.as_str(),
                    None,
                ));
                return;
            }
        };
        let copied_pctx = super::ProjectCtx {
            tool: pctx.tool,
            project: trial.project().clone(),
            rel_path: pctx.rel_path.clone(),
            policy: pctx.policy.clone(),
            edge_policy: pctx.edge_policy,
        };
        let mut trial_opts = opts.clone();
        trial_opts.build = false;
        let execution = ProjectExecution::Isolated(trial.mutation_project());
        let mut project_acc = UpgradeAccum::default();
        let status = ProjectUpgradeExecutor::new(
            self,
            UpgradeCtx::new(
                reader,
                writer,
                &copied_pctx,
                &trial_opts,
                self.repo_root(),
                execution,
                true,
            ),
            mode,
            &mut project_acc,
        )
        .run()
        .await;
        if status == ProjectRunStatus::Terminated {
            merge_discarded_trial(acc, project_acc);
            return;
        }
        publish_isolated_trial(trial.as_ref(), writer, pctx, opts, project_acc, acc).await;
    }

    async fn acquire_isolated_source_access(
        &self,
        writer: &dyn ToolWrite,
        pctx: &super::ProjectCtx,
        opts: &RunOpts,
    ) -> cooldown_core::Result<(IsolatedAccessGuard, Vec<Diagnostic>)> {
        let (guard, recovery_warnings) = if opts.dry_run {
            (
                IsolatedAccessGuard::Read(self.project_read_guard(pctx).await?),
                Vec::new(),
            )
        } else {
            let guard = super::lock::ProjectAccessWriteGuard::acquire(
                self.repo_root(),
                &pctx.project.root,
                pctx.tool,
                writer.sync_scope() == cooldown_core::SyncScope::Repo,
            )?;
            let recovery = writer
                .recover_pending_mutation(&pctx.project, guard.coordination())
                .await?;
            let diagnostics =
                super::recovery_diagnostics(recovery, pctx.tool, pctx.rel_path.as_str());
            (IsolatedAccessGuard::Write(guard), diagnostics)
        };
        Ok((guard, recovery_warnings))
    }
}

enum IsolatedAccessGuard {
    Read(super::lock::ProjectAccessReadGuard),
    Write(super::lock::ProjectAccessWriteGuard),
}

impl IsolatedAccessGuard {
    fn coordination(&self) -> &cooldown_core::fs::ProjectCoordination {
        match self {
            IsolatedAccessGuard::Read(guard) => guard.coordination(),
            IsolatedAccessGuard::Write(guard) => guard.coordination(),
        }
    }
}

enum PreparedPreview {
    Generic(super::project_copy::ProjectCopy),
    Isolated(Box<dyn IsolatedMutation>),
}

impl PreparedPreview {
    fn project(&self) -> &cooldown_core::Project {
        match self {
            PreparedPreview::Generic(copy) => &copy.project,
            PreparedPreview::Isolated(copy) => copy.project(),
        }
    }

    fn execution(&self) -> ProjectExecution {
        match self {
            PreparedPreview::Generic(_) => ProjectExecution::Copy,
            PreparedPreview::Isolated(copy) => ProjectExecution::Isolated(copy.mutation_project()),
        }
    }
}

fn merge_upgrade_accum(target: &mut UpgradeAccum, mut source: UpgradeAccum) {
    target.items.append(&mut source.items);
    target.edge_items.append(&mut source.edge_items);
    target.errors.append(&mut source.errors);
    target.warnings.append(&mut source.warnings);
    target.strict_incomplete |= source.strict_incomplete;
    target.lock_status = combine_lock_status(target.lock_status, source.lock_status);
}

fn merge_discarded_trial(target: &mut UpgradeAccum, source: UpgradeAccum) {
    target.errors.extend(source.errors);
    target
        .errors
        .extend(source.items.into_iter().filter_map(|item| item.error));
}

async fn publish_isolated_trial(
    trial: &dyn IsolatedMutation,
    writer: &dyn ToolWrite,
    pctx: &super::ProjectCtx,
    opts: &RunOpts,
    mut project_acc: UpgradeAccum,
    acc: &mut UpgradeAccum,
) {
    let accepted = match trial.accepted_state() {
        Ok(accepted) => accepted,
        Err(error) => {
            merge_discarded_trial(acc, project_acc);
            acc.errors.push(diag_from_error(
                &error,
                pctx.tool,
                pctx.rel_path.as_str(),
                None,
            ));
            return;
        }
    };
    if opts.dry_run {
        merge_upgrade_accum(acc, project_acc);
        return;
    }
    opts.progress.phase("publishing accepted project state");
    let publication = match trial.publish(&accepted).await {
        Ok(publication) => publication,
        Err(error) => {
            merge_discarded_trial(acc, project_acc);
            acc.errors.push(diag_from_error(
                &error,
                pctx.tool,
                pctx.rel_path.as_str(),
                None,
            ));
            return;
        }
    };
    let (warnings, pending_recovery) = match publication {
        AcceptedPublication::Published { warnings } => (warnings, None),
        AcceptedPublication::PublishedPendingRecovery { warnings, error } => {
            (warnings, Some(error))
        }
    };
    project_acc
        .warnings
        .extend(warnings.into_iter().map(|warning| {
            warning
                .with_tool(pctx.tool.as_str())
                .with_project(pctx.rel_path.as_str())
        }));
    merge_upgrade_accum(acc, project_acc);
    if let Some(error) = pending_recovery {
        acc.errors.push(diag_from_error(
            &error,
            pctx.tool,
            pctx.rel_path.as_str(),
            None,
        ));
        return;
    }
    if opts.build {
        build_published_project(writer, pctx, opts, acc).await;
    }
}

const fn combine_lock_status(
    left: Option<LockStatus>,
    right: Option<LockStatus>,
) -> Option<LockStatus> {
    match (left, right) {
        (Some(LockStatus::Stale), _) | (_, Some(LockStatus::Stale)) => Some(LockStatus::Stale),
        (Some(LockStatus::Unknown), _) | (_, Some(LockStatus::Unknown)) => {
            Some(LockStatus::Unknown)
        }
        (Some(LockStatus::Current), _) | (_, Some(LockStatus::Current)) => {
            Some(LockStatus::Current)
        }
        (None, None) => None,
    }
}

async fn build_published_project(
    writer: &dyn ToolWrite,
    pctx: &super::ProjectCtx,
    opts: &RunOpts,
    acc: &mut UpgradeAccum,
) {
    acc.build_requested = true;
    opts.progress.phase("building updated project");
    match writer.build(&pctx.project).await {
        Ok(report) => {
            acc.build_ok = Some(acc.build_ok.unwrap_or(true) && report.ok);
            if !report.ok {
                acc.errors.push(
                    Diagnostic::new(DiagnosticKind::ToolFailed, report.detail)
                        .with_tool(pctx.tool.as_str())
                        .with_project(pctx.rel_path.as_str()),
                );
            }
        }
        Err(error) => {
            acc.build_ok = Some(false);
            acc.errors.push(diag_from_error(
                &error,
                pctx.tool,
                pctx.rel_path.as_str(),
                None,
            ));
        }
    }
}

fn read_only_mutator_diag(pctx: &super::ProjectCtx) -> Diagnostic {
    Diagnostic::new(
        DiagnosticKind::Config,
        format!(
            "{} adapter is read-only; upgrade/fix is not supported",
            pctx.tool
        ),
    )
    .with_tool(pctx.tool.as_str())
    .with_project(pctx.rel_path.as_str())
    .with_path(pctx.project.manifest.as_str())
}

/// Distills the accumulated per-project state into the final report: dedupes and sorts the rows,
/// derives the counts, folds edge outcomes into strict/meta, and decides the exit.
fn finalize_outcome(opts: &RunOpts, mut acc: UpgradeAccum) -> UpgradeOutcome {
    dedupe_edge_items(&mut acc.edge_items);
    acc.items.append(&mut acc.edge_items);
    // Changes are planned/applied in the (now-sorted) dependency order, but sort the report
    // items explicitly so the output is stable, status-first (errored/skipped lead, applied last).
    // A `--dry-run` runs the same whole-graph resolve against a throwaway copy, so its items carry
    // the real applied/skipped outcome and sort identically to the real run.
    acc.items.sort_by(|a, b| {
        a.project
            .cmp(&b.project)
            .then_with(|| a.sort_rank().cmp(&b.sort_rank()))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.from.cmp(&b.from))
    });
    let applied = acc
        .items
        .iter()
        .filter(|item| item.edge.is_none() && item.applied)
        .count();
    // Every non-applied, non-errored change is a skip — including the `needs --major` rows because a
    // held-back cross-major is a skip.
    // The renderer breaks out how many of them need `--major`.
    let skipped = acc
        .items
        .iter()
        .filter(|item| item.skipped.is_some())
        .count();
    let err_count = acc.items.iter().filter(|item| item.error.is_some()).count() + acc.errors.len();
    let edges_corrected = count_edge_actions(
        &acc.items,
        &[
            EdgeBindingAction::Restored,
            EdgeBindingAction::Canonicalized,
        ],
    );
    let edges_rebound = count_edge_actions(&acc.items, &[EdgeBindingAction::Rebound]);
    let edges_held = count_edge_actions(&acc.items, &[EdgeBindingAction::Held]);
    let edges_unaddressable = count_edge_actions(&acc.items, &[EdgeBindingAction::Unaddressable]);
    // Held and unaddressable edges make a corrective policy incomplete.
    // Plain `rebound` rows do not: under a corrective policy they are allowed planned follows, and
    // under policy `none` observation is the contract.
    acc.strict_incomplete |= edges_held > 0 || edges_unaddressable > 0;

    let exit = if err_count > 0 || acc.build_ok == Some(false) {
        Exit::Environment
    } else if opts.strict && acc.strict_incomplete {
        Exit::Policy
    } else {
        Exit::Ok
    };

    let committed = acc.items.iter().filter(|item| item.applied).count();
    let meta = upgrade_meta(opts, &acc, committed);
    let summary = UpgradeSummary {
        applied,
        skipped,
        errors: err_count,
        edges_corrected,
        edges_rebound,
        edges_held,
        edges_unaddressable,
    };
    UpgradeOutcome {
        meta,
        summary,
        items: acc.items,
        warnings: acc.warnings,
        errors: acc.errors,
        exit,
    }
}

/// The number of edge rows whose action is one of `actions`.
fn count_edge_actions(items: &[UpgradeItem], actions: &[EdgeBindingAction]) -> usize {
    items
        .iter()
        .filter(|item| {
            item.edge
                .as_ref()
                .is_some_and(|edge| actions.contains(&edge.action))
        })
        .count()
}

/// Drops edge rows that duplicate an earlier one exactly (same project, packages, versions, and
/// outcome).
/// Version rows are never touched.
fn dedupe_edge_items(items: &mut Vec<UpgradeItem>) {
    let mut seen = HashSet::new();
    items.retain(|item| {
        let Some(edge) = &item.edge else {
            return true;
        };
        seen.insert((
            item.project.clone(),
            item.name.clone(),
            item.registry.clone(),
            item.from.clone(),
            item.to.clone(),
            edge.dependent.clone(),
            edge.dependent_version.clone(),
            edge.dependent_source.clone(),
            edge.action.wire_value(),
            edge.detail.clone(),
        ))
    });
}

/// `mutations` counts every reported change present in the committed result.
fn upgrade_meta(opts: &RunOpts, acc: &UpgradeAccum, mutations: usize) -> UpgradeMeta {
    UpgradeMeta {
        applied: mutations > 0,
        lock_status: if opts.dry_run { None } else { acc.lock_status },
        build: BuildInfo {
            requested: acc.build_requested,
            ok: acc.build_ok,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{UpgradeAccum, UpgradeItem, dedupe_edge_items, finalize_outcome};
    use crate::app::RunOpts;
    use crate::app::UpgradeEdgeInfo;
    use cooldown_core::{EdgeBindingAction, UpdateKind};

    fn edge_item(registry: &str) -> UpgradeItem {
        UpgradeItem {
            name: "dep".to_string(),
            tool: "cargo".to_string(),
            project: ".".to_string(),
            direct: false,
            downgrade: false,
            members: Vec::new(),
            registry: Some(registry.to_string()),
            from: "1.0.0".to_string(),
            to: "2.0.0".to_string(),
            kind: UpdateKind::Major,
            applied: true,
            skipped: None,
            error: None,
            edge: Some(UpgradeEdgeInfo {
                dependent: "consumer".to_string(),
                dependent_version: "1.0.0".to_string(),
                dependent_source: None,
                action: EdgeBindingAction::Rebound,
                detail: None,
            }),
        }
    }

    #[test]
    fn edge_deduplication_preserves_distinct_target_sources() {
        let mut items = vec![
            edge_item("crates.io"),
            edge_item("git+https://example.com/dep#abcdef"),
        ];

        dedupe_edge_items(&mut items);

        assert_eq!(items.len(), 2);
    }

    #[test]
    fn committed_rebound_is_counted_and_marks_the_run_applied() {
        let mut acc = UpgradeAccum::default();
        acc.edge_items.push(edge_item("crates.io"));

        let outcome = finalize_outcome(&RunOpts::default(), acc);

        assert_eq!(outcome.summary.applied, 0);
        assert_eq!(outcome.summary.edges_rebound, 1);
        assert!(outcome.meta.applied);
    }
}
