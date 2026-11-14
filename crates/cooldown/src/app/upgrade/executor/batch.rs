//! Defines native resolver batch outcomes and aborted-trial reduction.

use super::super::UpgradeAccum;
use crate::app::UpgradeItem;
use crate::app::change_key::{ChangeProvenanceKey, ChangeTargetKey, change_provenance_key};
use cooldown_core::{ApplyReport, BaselineViolation, Change, Diagnostic, EdgeRebind};
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
    /// What the writer's apply attested about its skipped changes, when an apply ran.
    ///
    /// Boxed because most outcomes travel without one through enums sized by their largest
    /// variant.
    pub(super) attestation: Option<Box<HoldAttestation>>,
}

/// The skipped changes a writer attested it would skip again beside exactly the planned changes
/// its apply applied (see [`ApplyReport::independent_holds`]).
///
/// Changes are keyed with their source version, because the attestation is about one exact
/// change: another change to the same target from another version may be held for another
/// reason.
pub(super) struct HoldAttestation {
    /// The planned changes the apply reported applied, before any verification of the graph.
    pub(super) applied: HashSet<ChangeProvenanceKey>,
    pub(super) holds: HashSet<ChangeProvenanceKey>,
}

impl HoldAttestation {
    pub(super) fn of(report: &ApplyReport, planned: &[Change]) -> Self {
        let planned: HashSet<ChangeProvenanceKey> =
            planned.iter().map(change_provenance_key).collect();
        HoldAttestation {
            applied: report
                .applied
                .iter()
                .map(change_provenance_key)
                .filter(|key| planned.contains(key))
                .collect(),
            holds: report
                .independent_holds
                .iter()
                .map(change_provenance_key)
                .collect(),
        }
    }
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
        /// Boxed because its graph-sized sets would otherwise size every outcome.
        state: Box<CommittedBatch>,
    },
    /// Independent drift prevented restoration, so no later mutation may run.
    RestoreConflict(BatchReport),
}

/// What a kept batch changes about the trial state.
pub(super) struct CommittedBatch {
    /// The graph's non-baselined violations after this batch (the next trial baseline).
    pub(super) violations_after: HashSet<BaselineViolation>,
    /// The graph's versions above their configured `max-major` after this batch.
    pub(super) crossings_after: HashSet<BaselineViolation>,
    /// The versions outside a configured `max-major` this batch's planned changes authorize.
    pub(super) admitted_crossings: HashSet<BaselineViolation>,
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
            BatchOutcome::Committed { state, .. } => Some(state.as_ref()),
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
        *self = BatchOutcome::Committed {
            report,
            state: Box::new(state),
        };
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

/// Retains only error provenance when rollback leaves the final project state indeterminate.
pub(super) fn indeterminate_trial(outcomes: Vec<BatchOutcome>) -> BatchOutcome {
    let mut conflict = trial_errors(outcomes);
    conflict.mark_restore_conflict();
    conflict
}

/// The adapter evidence retained after graph verification accepts one batch.
pub(super) struct VerifiedBatchReport {
    pub(super) applied: HashSet<ChangeTargetKey>,
    /// The applied changes keyed with their source version, which tells a planned move that
    /// landed apart from a collateral move onto the same target.
    pub(super) applied_provenance: HashSet<ChangeProvenanceKey>,
    pub(super) collateral: Vec<cooldown_core::Change>,
    /// Lock edges the adapter's edge policy corrected or observed rebound.
    pub(super) edge_rebinds: Vec<EdgeRebind>,
    /// Adapter warnings whose provenance is valid only if this batch commits.
    pub(super) warnings: Vec<Diagnostic>,
    /// Generated manifests the adapter rewrote to follow this batch's moves
    /// ([`ApplyReport::followed_manifests`](cooldown_core::ApplyReport::followed_manifests)),
    /// each owed a regeneration note once the batch commits.
    pub(super) followed_manifests: Vec<camino::Utf8PathBuf>,
    pub(super) planned_applied: bool,
}
