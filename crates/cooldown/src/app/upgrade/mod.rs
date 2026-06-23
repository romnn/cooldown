//! Shared mutation flow for `upgrade` and `fix`: plan one version change, apply it, re-lock, verify,
//! and roll back if the resulting graph would fail the cooldown gate.
//!
//! The app applies changes **one at a time**: capture a narrow mutation journal for the pending
//! change, apply the single-change plan, and if the re-lock leaves a new too-fresh
//! (non-baselined) dependency in the graph, restore that journal and skip the change as
//! `TransitiveInCooldown` — never committing a lock a subsequent `check` would reject.

mod executor;

use self::executor::{PlanMode, ProjectUpgradeExecutor};
use super::{
    BuildInfo, Exit, RunOpts, UpgradeItem, UpgradeMeta, UpgradeSummary, Workspace, diag_from_error,
};
use cooldown_core::{Diagnostic, ToolRead, ToolWrite};

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
    pub(super) errors: Vec<Diagnostic>,
    /// Non-fatal advisories — `fix` records a too-fresh pin it left in place, or a violation with no
    /// matured older version to downgrade to.
    pub(super) warnings: Vec<Diagnostic>,
    pub(super) strict_incomplete: bool,
    /// `None` until a build is attempted; `Some(false)` once any project's build fails.
    pub(super) build_ok: Option<bool>,
    pub(super) build_requested: bool,
    /// `None` until the lock is verified; `Some(false)` once any project's lock is non-current.
    pub(super) lock_verified: Option<bool>,
}

/// The read/write adapter pair and shared per-project inputs the upgrade executor needs.
pub(super) struct UpgradeCtx<'a> {
    pub(super) reader: &'a dyn ToolRead,
    pub(super) writer: &'a dyn ToolWrite,
    pub(super) pctx: &'a super::ProjectCtx,
    pub(super) opts: &'a RunOpts,
}

impl<'a> UpgradeCtx<'a> {
    fn new(
        reader: &'a dyn ToolRead,
        writer: &'a dyn ToolWrite,
        pctx: &'a super::ProjectCtx,
        opts: &'a RunOpts,
    ) -> Self {
        UpgradeCtx {
            reader,
            writer,
            pctx,
            opts,
        }
    }

    pub(super) fn tool_name(&self) -> &'static str {
        self.pctx.tool.as_str()
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
            let Some(reader) = self.adapter(pctx.tool) else {
                continue;
            };
            let Some(writer) = self.mutator(pctx.tool) else {
                continue;
            };

            // Under `--dry-run`, preview the TRUE outcome of the real run: run the identical
            // whole-graph mutation flow against a throwaway recursive copy of the project, then discard
            // the copy. Because the copy drives the same `apply`/re-lock/reconcile path, the reported
            // lock diff (held candidates shown skipped with their blocker, landed candidates shown
            // applied) equals what the real run produces — the real `uv.lock`/`pyproject.toml` are
            // never written. `dry_copy` owns the temp tree (removed when it drops at the end of the
            // iteration); `dry_pctx` owns the copied context the executor borrows.
            let _dry_copy;
            let dry_pctx;
            let effective_pctx = if opts.dry_run {
                match super::project_copy::ProjectCopy::create(
                    &pctx.project,
                    &writer.resolve_inputs(),
                ) {
                    Ok(copy) => {
                        dry_pctx = super::ProjectCtx {
                            tool: pctx.tool,
                            project: copy.project.clone(),
                            rel_path: pctx.rel_path.clone(),
                            policy: pctx.policy.clone(),
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

            ProjectUpgradeExecutor::new(
                self,
                UpgradeCtx::new(reader, writer, effective_pctx, opts),
                mode,
                &mut acc,
            )
            .run()
            .await;
        }

        // Changes are planned/applied in the (now-sorted) dependency order, but sort the report
        // items explicitly so the output is stable, status-first (errored/skipped lead, applied
        // last). A `--dry-run` runs the same whole-graph resolve against a throwaway copy, so its
        // items carry the real applied/skipped outcome and sort identically to the real run.
        acc.items.sort_by(|a, b| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.sort_rank().cmp(&b.sort_rank()))
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.from.cmp(&b.from))
        });
        let applied = acc.items.iter().filter(|item| item.applied).count();
        // Every non-applied, non-errored change is a skip — including the `needs --major` rows (a
        // held-back cross-major *is* a skip). The renderer breaks out how many of them need `--major`.
        let skipped = acc
            .items
            .iter()
            .filter(|item| item.skipped.is_some())
            .count();
        let err_count =
            acc.items.iter().filter(|item| item.error.is_some()).count() + acc.errors.len();

        let lock_or_build_failed = acc.lock_verified == Some(false) || acc.build_ok == Some(false);
        let exit = if err_count > 0 || lock_or_build_failed {
            Exit::Environment
        } else if opts.strict && acc.strict_incomplete {
            Exit::Policy
        } else {
            Exit::Ok
        };

        let meta = UpgradeMeta {
            applied: applied > 0,
            lock_verified: if opts.dry_run {
                None
            } else {
                acc.lock_verified
            },
            build: BuildInfo {
                requested: acc.build_requested,
                ok: acc.build_ok,
            },
        };
        let summary = UpgradeSummary {
            applied,
            skipped,
            errors: err_count,
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
}
