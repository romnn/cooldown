//! Defines native resolver batch outcomes and aborted-trial reduction.

use super::super::UpgradeAccum;
use crate::app::UpgradeItem;
use crate::app::change_key::ChangeTargetKey;
use cooldown_core::{BaselineViolation, Diagnostic, EdgeRebind};
use std::collections::HashSet;

/// The report produced by one native resolver batch.
#[derive(Default)]
pub(super) struct BatchReport {
    pub(super) items: Vec<UpgradeItem>,
    pub(super) edge_items: Vec<UpgradeItem>,
    /// Per-batch edge evidence retained only until the final adapter audit.
    pub(super) edge_rebinds: Vec<EdgeRebind>,
    pub(super) warnings: Vec<Diagnostic>,
    pub(super) errors: Vec<Diagnostic>,
    pub(super) strict_incomplete: bool,
    pub(super) lock_refreshed: bool,
}

/// The physical project state left by one resolver batch.
pub(super) enum BatchOutcome {
    /// No mutation was attempted or retained.
    Unchanged(BatchReport),
    /// The journal was restored to its pre-batch state.
    Restored(BatchReport),
    /// The post-apply graph was verified and retained.
    Committed {
        report: BatchReport,
        state: CommittedBatch,
    },
    /// Independent drift prevented restoration, so no later mutation may run.
    RestoreConflict(BatchReport),
}

/// What a kept batch changes about the trial state.
pub(super) struct CommittedBatch {
    /// The graph's non-baselined violations after this batch (the next trial baseline).
    pub(super) violations_after: HashSet<BaselineViolation>,
    /// Whether the batch floated up violations the reconcile pass should mature down.
    pub(super) reconcile_needed: bool,
}

impl Default for BatchOutcome {
    fn default() -> Self {
        BatchOutcome::Unchanged(BatchReport::default())
    }
}

impl BatchOutcome {
    pub(super) fn applied_count(&self) -> usize {
        self.items.iter().filter(|item| item.applied).count()
    }

    pub(super) fn errored_count(&self) -> usize {
        self.errors.len()
            + self
                .items
                .iter()
                .filter(|item| item.error.is_some())
                .count()
    }

    pub(super) fn has_restore_conflict(&self) -> bool {
        matches!(self, BatchOutcome::RestoreConflict(_))
    }

    pub(super) fn is_committed(&self) -> bool {
        matches!(self, BatchOutcome::Committed { .. })
    }

    pub(super) fn committed_state(&self) -> Option<&CommittedBatch> {
        match self {
            BatchOutcome::Committed { state, .. } => Some(state),
            BatchOutcome::Unchanged(_)
            | BatchOutcome::Restored(_)
            | BatchOutcome::RestoreConflict(_) => None,
        }
    }

    pub(super) fn mark_restored(&mut self) {
        let report = self.take_report();
        *self = BatchOutcome::Restored(report);
    }

    pub(super) fn mark_committed(&mut self, state: CommittedBatch) {
        let report = self.take_report();
        *self = BatchOutcome::Committed { report, state };
    }

    pub(super) fn mark_restore_conflict(&mut self) {
        let report = self.take_report();
        *self = BatchOutcome::RestoreConflict(report);
    }

    fn take_report(&mut self) -> BatchReport {
        match std::mem::take(self) {
            BatchOutcome::Unchanged(report)
            | BatchOutcome::Restored(report)
            | BatchOutcome::RestoreConflict(report)
            | BatchOutcome::Committed { report, .. } => report,
        }
    }

    pub(super) fn merge_into(self, acc: &mut UpgradeAccum) {
        let report = self.into_report();
        acc.items.extend(report.items);
        acc.edge_items.extend(report.edge_items);
        acc.warnings.extend(report.warnings);
        acc.errors.extend(report.errors);
        acc.strict_incomplete |= report.strict_incomplete;
    }

    fn into_report(self) -> BatchReport {
        match self {
            BatchOutcome::Unchanged(report)
            | BatchOutcome::Restored(report)
            | BatchOutcome::RestoreConflict(report)
            | BatchOutcome::Committed { report, .. } => report,
        }
    }
}

impl std::ops::Deref for BatchOutcome {
    type Target = BatchReport;

    fn deref(&self) -> &Self::Target {
        match self {
            BatchOutcome::Unchanged(report)
            | BatchOutcome::Restored(report)
            | BatchOutcome::RestoreConflict(report)
            | BatchOutcome::Committed { report, .. } => report,
        }
    }
}

impl std::ops::DerefMut for BatchOutcome {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            BatchOutcome::Unchanged(report)
            | BatchOutcome::Restored(report)
            | BatchOutcome::RestoreConflict(report)
            | BatchOutcome::Committed { report, .. } => report,
        }
    }
}

/// Distills an aborted trial's outcomes down to errors after the outer rollback succeeds.
pub(super) fn trial_errors(outcomes: Vec<BatchOutcome>) -> BatchOutcome {
    let mut errors = BatchOutcome::default();
    for outcome in outcomes {
        let restore_conflict = outcome.has_restore_conflict();
        let report = outcome.into_report();
        if restore_conflict {
            errors.mark_restore_conflict();
        }
        errors.errors.extend(report.errors);
        errors
            .items
            .extend(report.items.into_iter().filter(|item| item.error.is_some()));
    }
    errors.strict_incomplete = errors.errored_count() > 0;
    errors
}

/// Retains verified committed rows when an outer restore conflict leaves them in the project.
pub(super) fn retained_trial_outcomes(outcomes: Vec<BatchOutcome>) -> BatchOutcome {
    let mut retained = BatchOutcome::default();
    for outcome in outcomes {
        match outcome {
            BatchOutcome::Committed { report, .. } => merge_batch_report(&mut retained, report),
            BatchOutcome::Unchanged(report)
            | BatchOutcome::Restored(report)
            | BatchOutcome::RestoreConflict(report) => merge_batch_errors(&mut retained, report),
        }
    }
    retained.strict_incomplete |= retained.errored_count() > 0;
    retained.mark_restore_conflict();
    retained
}

fn merge_batch_report(outcome: &mut BatchOutcome, report: BatchReport) {
    outcome.items.extend(report.items);
    outcome.edge_items.extend(report.edge_items);
    outcome.edge_rebinds.extend(report.edge_rebinds);
    outcome.warnings.extend(report.warnings);
    outcome.errors.extend(report.errors);
    outcome.strict_incomplete |= report.strict_incomplete;
    outcome.lock_refreshed |= report.lock_refreshed;
}

fn merge_batch_errors(outcome: &mut BatchOutcome, report: BatchReport) {
    outcome.errors.extend(report.errors);
    outcome
        .items
        .extend(report.items.into_iter().filter(|item| item.error.is_some()));
}

/// The adapter evidence retained after graph verification accepts one batch.
pub(super) struct VerifiedBatchReport {
    pub(super) applied: HashSet<ChangeTargetKey>,
    pub(super) collateral: Vec<cooldown_core::Change>,
    /// Lock edges the adapter's edge policy corrected or observed rebound.
    pub(super) edge_rebinds: Vec<EdgeRebind>,
    /// Adapter warnings whose provenance is valid only if this batch commits.
    pub(super) warnings: Vec<Diagnostic>,
    pub(super) planned_applied: bool,
}
