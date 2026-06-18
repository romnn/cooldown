//! `upgrade` — move direct deps to the newest version older than the cooldown, then re-lock.
//!
//! Acting on transitive deps is a non-goal, so the app applies changes **one at a time**: capture
//! a narrow mutation journal for the pending change, apply the single-change plan, and if the
//! re-lock drags in a too-fresh (non-baselined) transitive, restore that journal and skip the
//! change as `TransitiveInCooldown` — never committing a lock a subsequent `check` would reject.

mod executor;

use self::executor::ProjectUpgradeExecutor;
use super::{BuildInfo, Exit, RunOpts, UpgradeItem, UpgradeMeta, UpgradeSummary, Workspace};
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
    /// Non-fatal diagnostics (currently unused; reserved for parity with other commands).
    pub warnings: Vec<Diagnostic>,
    /// Project-level errors (a failed apply, a failed re-lock probe, etc.).
    pub errors: Vec<Diagnostic>,
    /// The process exit; non-zero on any error, or under `--strict` when a change was skipped.
    pub exit: Exit,
}

/// The mutable state accumulated across all projects in an upgrade run.
#[derive(Default)]
pub(super) struct UpgradeAccum {
    pub(super) items: Vec<UpgradeItem>,
    pub(super) errors: Vec<Diagnostic>,
    pub(super) any_skipped: bool,
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
    /// Move direct deps to the newest version older than the cooldown, applying changes one at a
    /// time and re-locking after each.
    ///
    /// If a re-lock drags in a too-fresh, non-baselined transitive, only the files that change may
    /// have touched are restored and that change is reported as skipped — never committing a state
    /// a subsequent `check` would reject. With `--dry-run` the plan is reported without mutation.
    pub async fn upgrade(&self, opts: &RunOpts) -> UpgradeOutcome {
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
            ProjectUpgradeExecutor::new(
                self,
                UpgradeCtx::new(reader, writer, pctx, opts),
                &mut acc,
            )
            .run()
            .await;
        }

        // Changes are planned/applied in the (now-sorted) dependency order, but sort the report
        // items explicitly so the output is stable regardless of how they were accumulated.
        acc.items.sort_by(|a, b| {
            a.project
                .cmp(&b.project)
                .then_with(|| a.name.cmp(&b.name))
                .then_with(|| a.from.cmp(&b.from))
        });
        let applied = acc.items.iter().filter(|item| item.applied).count();
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
        } else if opts.strict && acc.any_skipped {
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
            warnings: Vec::new(),
            errors: acc.errors,
            exit,
        }
    }
}
