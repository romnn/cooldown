//! Proposes, guards, verifies, commits, and reports edge-policy outcomes.

use super::{
    BindingChange, EdgeRewrite, GuardedRewrites, LockEdgeView, RejectedRewrite, RequirementIndex,
    binding_changes, guard_rewrites, rewrite_lock_text,
};
use crate::CARGO_ID;
use crate::cargocmd::{CRATES_IO_SOURCE, Cargo, ResolvedGraph};
use crate::index::CRATES_IO;
use crate::lockfile::CargoLock;
use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{
    CoreError, EdgeBindingAction, EdgePolicy, EdgeRebind, PackageId, Project, Result, Version,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::io::Write;

/// Bounds every Cargo validation spawned after the initial all-rewrites candidate fails.
const MAX_ISOLATION_PROBES: usize = 64;
const ISOLATION_BUDGET_REASON: &str =
    "the correction isolation budget was exhausted before cargo could validate this edge";

/// The reported binding outcomes and the graph that describes the committed lock.
pub(crate) struct EnforcementResult {
    pub(crate) rebinds: Vec<EdgeRebind>,
    pub(crate) graph: Option<ResolvedGraph>,
}

enum BindingOutcome {
    Corrected {
        rewrite: EdgeRewrite,
        action: EdgeBindingAction,
    },
    Withheld {
        rewrite: EdgeRewrite,
        reason: String,
    },
    ObservedAllowed(BindingChange),
    Unaddressable {
        change: BindingChange,
        reason: String,
    },
}

#[derive(Clone, Copy)]
enum CandidateFailure {
    TextMismatch,
    Verification,
}

enum CommittedRewrites {
    Unchanged,
    Changed {
        corrected: Vec<EdgeRewrite>,
        lock_text: String,
        graph: Box<ResolvedGraph>,
    },
}

/// Enforces `policy` over the resolver-produced lock and reports every committed or limited move.
pub(crate) async fn enforce(
    cargo: &Cargo,
    project: &Project,
    policy: EdgePolicy,
    before: Option<&CargoLock>,
    graph: Option<ResolvedGraph>,
) -> Result<EnforcementResult> {
    let lock_path = project.root.join("Cargo.lock");
    let resolver_text = std::fs::read_to_string(&lock_path)?;
    let resolver_lock = CargoLock::parse(&resolver_text)?;
    let resolver_view = LockEdgeView::from_lock(&resolver_lock);
    let before_view = before.map(LockEdgeView::from_lock);

    let proposed = match (policy, graph.as_ref()) {
        // Without metadata there are no declared requirements to validate a rewrite against.
        (EdgePolicy::None, _) | (_, None) => Vec::new(),
        (EdgePolicy::Preserve, Some(graph)) => before_view
            .as_ref()
            .map(|before_view| {
                super::preserve::restorations(
                    before_view,
                    &resolver_view,
                    &RequirementIndex::new(graph),
                )
            })
            .unwrap_or_default(),
        (EdgePolicy::Canonicalize, Some(graph)) => {
            super::canonicalize::rebindings(&resolver_view, &RequirementIndex::new(graph))
        }
    };
    let mut guarded = guard_rewrites(&resolver_view, proposed);
    let committed =
        apply_rewrites(cargo, project, &lock_path, &resolver_text, &mut guarded).await?;

    let (corrected, final_text, verified_graph) = match committed {
        CommittedRewrites::Unchanged => (Vec::new(), None, None),
        CommittedRewrites::Changed {
            corrected,
            lock_text,
            graph,
        } => (corrected, Some(lock_text), Some(*graph)),
    };
    let graph = verified_graph.or(graph);
    let corrected_action = match policy {
        EdgePolicy::Preserve => EdgeBindingAction::Restored,
        EdgePolicy::Canonicalize => EdgeBindingAction::Canonicalized,
        EdgePolicy::None => EdgeBindingAction::Rebound,
    };
    let mut outcomes: Vec<BindingOutcome> = corrected
        .iter()
        .cloned()
        .map(|rewrite| BindingOutcome::Corrected {
            rewrite,
            action: corrected_action,
        })
        .collect();
    outcomes.extend(
        guarded
            .rejected
            .iter()
            .map(|rejected| BindingOutcome::Withheld {
                rewrite: rejected.rewrite.clone(),
                reason: rejected.reason.clone(),
            }),
    );

    if let Some(before_view) = &before_view {
        let final_view = match final_text {
            Some(final_text) => LockEdgeView::from_lock(&CargoLock::parse(&final_text)?),
            None => resolver_view,
        };
        let covered: BTreeSet<_> = outcomes.iter().filter_map(outcome_edge_key).collect();
        let requirements = graph.as_ref().map(RequirementIndex::new);
        outcomes.extend(residual_outcomes(
            policy,
            before_view,
            &final_view,
            &covered,
            requirements.as_ref(),
        ));
    }

    Ok(EnforcementResult {
        rebinds: outcomes.into_iter().map(outcome_row).collect(),
        graph,
    })
}

/// Restores the pre-candidate lock when a prior enforcement terminated during verification.
///
/// The mutation lifecycle calls this only while holding the project's exclusive lock.
pub(crate) fn recover_pending(project: &Project) -> Result<()> {
    recover_speculative_write(project, &project.root.join("Cargo.lock"))
}

/// Refuses a read while an interrupted mutation still owns recovery state.
pub(crate) fn ensure_no_pending(project: &Project) -> Result<()> {
    let lock_path = project.root.join("Cargo.lock");
    let recovery_path = recovery_path(&lock_path);
    if recovery_path.exists() {
        return Err(cooldown_core::CoreError::StaleLock(format!(
            "pending Cargo.lock transaction at {recovery_path}; run `cooldown upgrade` or `cooldown fix` to recover it"
        )));
    }
    Ok(())
}

/// Reconciles committed batch corrections with the final saved binding state.
///
/// A later state-changing batch outcome supersedes an earlier correction even when the final
/// binding eventually returns to the same version. Final-pass corrections win over batch
/// provenance; otherwise a surviving batch correction replaces the generic run-level observation
/// for that edge while final held attempts remain visible beside it.
pub(crate) fn reconcile_committed_outcomes(
    final_view: &LockEdgeView,
    final_outcomes: &mut Vec<EdgeRebind>,
    committed: &[EdgeRebind],
) {
    let mut latest_state = BTreeMap::new();
    for outcome in committed {
        if outcome.action != EdgeBindingAction::Held {
            latest_state.insert(rebind_edge_key(outcome), outcome);
        }
    }

    let final_corrections: BTreeSet<_> = final_outcomes
        .iter()
        .filter(|outcome| is_correction(outcome.action))
        .map(rebind_edge_key)
        .collect();
    for (key, outcome) in latest_state {
        if !is_correction(outcome.action)
            || final_corrections.contains(&key)
            || !outcome_matches_final_binding(final_view, outcome)
        {
            continue;
        }
        final_outcomes.retain(|existing| {
            rebind_edge_key(existing) != key
                || !matches!(
                    existing.action,
                    EdgeBindingAction::Rebound | EdgeBindingAction::Unaddressable
                )
        });
        if !final_outcomes.contains(outcome) {
            final_outcomes.push(outcome.clone());
        }
    }
}

fn rebind_edge_key(rebind: &EdgeRebind) -> (super::LockPackageId, String) {
    (
        super::LockPackageId::new(
            &rebind.dependent,
            rebind.dependent_version.as_str(),
            rebind.dependent_source.as_deref(),
        ),
        rebind.dependency.name.clone(),
    )
}

fn is_correction(action: EdgeBindingAction) -> bool {
    matches!(
        action,
        EdgeBindingAction::Restored | EdgeBindingAction::Canonicalized
    )
}

fn outcome_matches_final_binding(final_view: &LockEdgeView, outcome: &EdgeRebind) -> bool {
    let (dependent, dependency) = rebind_edge_key(outcome);
    final_view.binding(&dependent, &dependency) == Some(outcome.to.as_str())
}

fn residual_outcomes(
    policy: EdgePolicy,
    before: &LockEdgeView,
    after: &LockEdgeView,
    covered: &BTreeSet<(super::LockPackageId, String)>,
    requirements: Option<&RequirementIndex<'_>>,
) -> Vec<BindingOutcome> {
    binding_changes(before, after)
        .into_iter()
        .filter_map(|change| {
            let key = (change.dependent.clone(), change.dependency.clone());
            if covered.contains(&key) {
                return None;
            }
            let limitation = corrective_limitation(policy, after, requirements, &change);
            Some(match limitation {
                Some(reason) => BindingOutcome::Unaddressable { change, reason },
                None => BindingOutcome::ObservedAllowed(change),
            })
        })
        .collect()
}

fn corrective_limitation(
    policy: EdgePolicy,
    after: &LockEdgeView,
    requirements: Option<&RequirementIndex<'_>>,
    change: &BindingChange,
) -> Option<String> {
    if !matches!(policy, EdgePolicy::Preserve | EdgePolicy::Canonicalize) {
        return None;
    }
    if let Some(reason) = after.unaddressable_reason(&change.dependent, &change.dependency) {
        return Some(reason.to_string());
    }
    match requirements {
        Some(requirements)
            if requirements.identifies(
                &change.dependent,
                &change.dependency,
                &change.after.version,
            ) =>
        {
            None
        }
        Some(_) => Some(
            "cargo metadata did not identify the dependent's requirement for this lock edge"
                .to_string(),
        ),
        None => Some(
            "cargo metadata was unavailable to identify the dependent's requirement for this lock edge"
                .to_string(),
        ),
    }
}

fn outcome_edge_key(outcome: &BindingOutcome) -> Option<(super::LockPackageId, String)> {
    match outcome {
        BindingOutcome::Corrected { rewrite, .. } => {
            Some((rewrite.dependent.clone(), rewrite.dependency.clone()))
        }
        BindingOutcome::Withheld { .. }
        | BindingOutcome::ObservedAllowed(_)
        | BindingOutcome::Unaddressable { .. } => None,
    }
}

fn outcome_row(outcome: BindingOutcome) -> EdgeRebind {
    match outcome {
        BindingOutcome::Corrected { rewrite, action } => corrective_row(&rewrite, action, None),
        BindingOutcome::Withheld { rewrite, reason } => {
            corrective_row(&rewrite, EdgeBindingAction::Held, Some(reason))
        }
        BindingOutcome::ObservedAllowed(change) => {
            observed_row(change, EdgeBindingAction::Rebound, None)
        }
        BindingOutcome::Unaddressable { change, reason } => {
            observed_row(change, EdgeBindingAction::Unaddressable, Some(reason))
        }
    }
}

fn corrective_row(
    rewrite: &EdgeRewrite,
    action: EdgeBindingAction,
    detail: Option<String>,
) -> EdgeRebind {
    EdgeRebind {
        dependent: rewrite.dependent.name.clone(),
        dependent_version: Version::new(rewrite.dependent.version.clone()),
        dependent_source: rewrite.dependent.source().map(str::to_string),
        dependency: PackageId::new(
            CARGO_ID,
            rewrite.dependency.clone(),
            Some(CRATES_IO.to_string()),
        ),
        from: Version::new(rewrite.from.clone()),
        to: Version::new(rewrite.to.clone()),
        action,
        detail,
    }
}

fn observed_row(
    change: BindingChange,
    action: EdgeBindingAction,
    limitation: Option<String>,
) -> EdgeRebind {
    let dependent_source = change.dependent.source().map(str::to_string);
    let after_source = change.after.source().map(str::to_string);
    let registry = if after_source.as_deref() == Some(CRATES_IO_SOURCE) {
        Some(CRATES_IO.to_string())
    } else {
        after_source
    };
    let detail = match (limitation, change.detail) {
        (Some(reason), Some(observation)) => Some(format!("{reason}; {observation}")),
        (Some(reason), None) => Some(reason),
        (None, observation) => observation,
    };
    EdgeRebind {
        dependent: change.dependent.name,
        dependent_version: Version::new(change.dependent.version),
        dependent_source,
        dependency: PackageId::new(CARGO_ID, change.dependency, registry),
        from: Version::new(change.before.version),
        to: Version::new(change.after.version),
        action,
        detail,
    }
}

async fn apply_rewrites(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    resolver_text: &str,
    guarded: &mut GuardedRewrites,
) -> Result<CommittedRewrites> {
    if guarded.accepted.is_empty() {
        return Ok(CommittedRewrites::Unchanged);
    }
    let Some(rewritten) = rewrite_lock_text(resolver_text, &guarded.accepted) else {
        reject_all(
            guarded,
            "the lock text did not match the parsed entry; correction skipped",
        );
        return Ok(CommittedRewrites::Unchanged);
    };
    let recovery_path = begin_speculative_write(project, lock_path, resolver_text, &rewritten)?;
    match cargo.verify_locked(&project.root).await {
        Ok(Some(graph)) => {
            finish_speculative_write(lock_path, &recovery_path, resolver_text)?;
            Ok(CommittedRewrites::Changed {
                corrected: std::mem::take(&mut guarded.accepted),
                lock_text: rewritten,
                graph: Box::new(graph),
            })
        }
        Ok(None) => {
            restore_candidate(lock_path, resolver_text)?;
            match isolate_rewrites(
                cargo,
                project,
                lock_path,
                &recovery_path,
                resolver_text,
                guarded,
            )
            .await
            {
                Ok(result) => {
                    finish_speculative_write(lock_path, &recovery_path, resolver_text)?;
                    Ok(result)
                }
                Err(error) => {
                    rollback_speculative_write(lock_path, &recovery_path, resolver_text)?;
                    Err(error)
                }
            }
        }
        Err(error) => {
            rollback_speculative_write(lock_path, &recovery_path, resolver_text)?;
            Err(error)
        }
    }
}

async fn isolate_rewrites(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    recovery_path: &Utf8Path,
    resolver_text: &str,
    guarded: &mut GuardedRewrites,
) -> Result<CommittedRewrites> {
    let mut corrected = Vec::new();
    let mut current_text = resolver_text.to_string();
    let mut verified_graph: Option<Box<ResolvedGraph>> = None;
    let mut isolation_probes = MAX_ISOLATION_PROBES;

    let mut components: VecDeque<_> =
        super::rewrite::rewrite_components(std::mem::take(&mut guarded.accepted)).into();
    while let Some(component) = components.pop_front() {
        match try_isolation_candidate(
            cargo,
            project,
            lock_path,
            recovery_path,
            &current_text,
            &component,
            &mut isolation_probes,
        )
        .await?
        {
            IsolationCandidate::Verified(candidate_text, graph) => {
                corrected.extend(component);
                current_text = candidate_text;
                verified_graph = Some(graph);
            }
            IsolationCandidate::Rejected(failure) if component.len() > 1 => {
                match verified_subset(
                    cargo,
                    project,
                    lock_path,
                    recovery_path,
                    &current_text,
                    &component,
                    &mut isolation_probes,
                )
                .await?
                {
                    SubsetSearch::Verified(subset) => {
                        let VerifiedSubset {
                            accepted,
                            remainder,
                            lock_text,
                            graph,
                        } = *subset;
                        corrected.extend(accepted);
                        current_text = lock_text;
                        verified_graph = Some(graph);
                        let mut remainder = super::rewrite::rewrite_components(remainder);
                        remainder.reverse();
                        for component in remainder {
                            components.push_front(component);
                        }
                    }
                    SubsetSearch::Exhausted => {
                        for rewrite in component {
                            reject(guarded, rewrite, ISOLATION_BUDGET_REASON);
                        }
                    }
                    SubsetSearch::Rejected => {
                        for rewrite in component {
                            reject(guarded, rewrite, failure_reason(failure));
                        }
                    }
                }
            }
            IsolationCandidate::Rejected(failure) => {
                for rewrite in component {
                    reject(guarded, rewrite, failure_reason(failure));
                }
            }
            IsolationCandidate::Exhausted => {
                for rewrite in component {
                    reject(guarded, rewrite, ISOLATION_BUDGET_REASON);
                }
            }
        }
    }
    match verified_graph {
        Some(graph) => Ok(CommittedRewrites::Changed {
            corrected,
            lock_text: current_text,
            graph,
        }),
        None => Ok(CommittedRewrites::Unchanged),
    }
}

struct VerifiedSubset {
    accepted: Vec<EdgeRewrite>,
    remainder: Vec<EdgeRewrite>,
    lock_text: String,
    graph: Box<ResolvedGraph>,
}

enum SubsetSearch {
    Verified(Box<VerifiedSubset>),
    Exhausted,
    Rejected,
}

enum IsolationCandidate {
    Verified(String, Box<ResolvedGraph>),
    Rejected(CandidateFailure),
    Exhausted,
}

async fn verified_subset(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    recovery_path: &Utf8Path,
    current_text: &str,
    component: &[EdgeRewrite],
    probes_remaining: &mut usize,
) -> Result<SubsetSearch> {
    let view = LockEdgeView::from_lock(&CargoLock::parse(current_text)?);
    let units = balanced_retry_units(&view, component);
    for unit_indices in partition_subsets(units.len()) {
        let accepted: Vec<_> = unit_indices
            .iter()
            .filter_map(|index| units.get(*index))
            .flatten()
            .cloned()
            .collect();
        if !guard_rewrites(&view, accepted.clone()).rejected.is_empty() {
            continue;
        }
        match try_isolation_candidate(
            cargo,
            project,
            lock_path,
            recovery_path,
            current_text,
            &accepted,
            probes_remaining,
        )
        .await?
        {
            IsolationCandidate::Verified(lock_text, graph) => {
                let selected: BTreeSet<_> = accepted
                    .iter()
                    .map(|rewrite| (rewrite.dependent.clone(), rewrite.dependency.clone()))
                    .collect();
                let remainder = component
                    .iter()
                    .filter(|rewrite| {
                        !selected.contains(&(rewrite.dependent.clone(), rewrite.dependency.clone()))
                    })
                    .cloned()
                    .collect();
                return Ok(SubsetSearch::Verified(Box::new(VerifiedSubset {
                    accepted,
                    remainder,
                    lock_text,
                    graph,
                })));
            }
            IsolationCandidate::Rejected(_) => {}
            IsolationCandidate::Exhausted => return Ok(SubsetSearch::Exhausted),
        }
    }
    Ok(SubsetSearch::Rejected)
}

fn balanced_retry_units(view: &LockEdgeView, component: &[EdgeRewrite]) -> Vec<Vec<EdgeRewrite>> {
    let mut remaining = component.to_vec();
    let mut units = Vec::new();
    while !remaining.is_empty() {
        let mut unit = vec![remaining.remove(0)];
        loop {
            let rejected = guard_rewrites(view, unit.clone()).rejected;
            if rejected.is_empty() {
                break;
            }
            let mut added = false;
            for rejected in rejected {
                if let Some(position) = remaining.iter().position(|candidate| {
                    candidate.dependency == rejected.rewrite.dependency
                        && candidate.to == rejected.rewrite.from
                }) {
                    unit.push(remaining.remove(position));
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        units.push(unit);
    }
    units
}

fn partition_subsets(size: usize) -> Vec<Vec<usize>> {
    let mut pending = VecDeque::from([(0..size).collect::<Vec<_>>()]);
    let mut subsets = Vec::new();
    while let Some(parent) = pending.pop_front() {
        if parent.len() <= 1 {
            continue;
        }
        let (left, right) = parent.split_at(parent.len() / 2);
        let left = left.to_vec();
        let right = right.to_vec();
        subsets.push(left.clone());
        subsets.push(right.clone());
        if left.len() > 1 {
            pending.push_back(left);
        }
        if right.len() > 1 {
            pending.push_back(right);
        }
    }
    subsets
}

async fn try_isolation_candidate(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    recovery_path: &Utf8Path,
    current_text: &str,
    rewrites: &[EdgeRewrite],
    probes_remaining: &mut usize,
) -> Result<IsolationCandidate> {
    if *probes_remaining == 0 {
        return Ok(IsolationCandidate::Exhausted);
    }
    *probes_remaining -= 1;
    Ok(
        match try_candidate(
            cargo,
            project,
            lock_path,
            recovery_path,
            current_text,
            rewrites,
        )
        .await?
        {
            Ok((lock_text, graph)) => IsolationCandidate::Verified(lock_text, Box::new(graph)),
            Err(failure) => IsolationCandidate::Rejected(failure),
        },
    )
}

async fn try_candidate(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    recovery_path: &Utf8Path,
    current_text: &str,
    rewrites: &[EdgeRewrite],
) -> Result<std::result::Result<(String, ResolvedGraph), CandidateFailure>> {
    let Some(candidate_text) = rewrite_lock_text(current_text, rewrites) else {
        return Ok(Err(CandidateFailure::TextMismatch));
    };
    update_recovery_candidate(
        project,
        lock_path,
        recovery_path,
        current_text,
        &candidate_text,
    )?;
    cooldown_core::fs::atomic_write(lock_path.as_std_path(), candidate_text.as_bytes())?;
    match cargo.verify_locked(&project.root).await {
        Ok(Some(graph)) => Ok(Ok((candidate_text, graph))),
        Ok(None) => {
            restore_candidate(lock_path, current_text)?;
            Ok(Err(CandidateFailure::Verification))
        }
        Err(error) => {
            restore_candidate(lock_path, current_text)?;
            Err(error)
        }
    }
}

fn recovery_path(lock_path: &Utf8Path) -> Utf8PathBuf {
    lock_path.with_extension("lock.cooldown-recovery")
}

const RECOVERY_FORMAT: &str = "cooldown-cargo-lock-recovery-v1";

#[derive(serde::Deserialize, serde::Serialize)]
struct RecoveryRecord {
    format: String,
    project_root: String,
    original_hash: String,
    previous_hash: String,
    candidate_hash: String,
    original_lock: String,
    previous_lock: String,
    candidate_lock: String,
}

impl RecoveryRecord {
    fn new(project: &Project, original_lock: &str, candidate_lock: &str) -> Result<Self> {
        Ok(RecoveryRecord {
            format: RECOVERY_FORMAT.to_string(),
            project_root: canonical_project_root(project)?,
            original_hash: lock_fingerprint(original_lock),
            previous_hash: lock_fingerprint(original_lock),
            candidate_hash: lock_fingerprint(candidate_lock),
            original_lock: original_lock.to_string(),
            previous_lock: original_lock.to_string(),
            candidate_lock: candidate_lock.to_string(),
        })
    }

    fn validate(&self, project: &Project, path: &Utf8Path) -> Result<()> {
        let valid = self.format == RECOVERY_FORMAT
            && self.project_root == canonical_project_root(project)?
            && self.original_hash == lock_fingerprint(&self.original_lock)
            && self.previous_hash == lock_fingerprint(&self.previous_lock)
            && self.candidate_hash == lock_fingerprint(&self.candidate_lock)
            && self.previous_lock != self.candidate_lock;
        if valid {
            Ok(())
        } else {
            Err(CoreError::LockUnreadable(format!(
                "untrusted Cargo.lock recovery record at {path}; left both files untouched"
            )))
        }
    }
}

fn canonical_project_root(project: &Project) -> Result<String> {
    let canonical = std::fs::canonicalize(&project.root)?;
    Utf8PathBuf::from_path_buf(canonical)
        .map(|path| path.to_string())
        .map_err(|path| CoreError::PathEncoding(format!("non-utf8 path: {}", path.display())))
}

fn lock_fingerprint(text: &str) -> String {
    format!("{:016x}:{}", cooldown_core::fs::fnv1a_64(text), text.len())
}

fn read_recovery_record(project: &Project, path: &Utf8Path) -> Result<RecoveryRecord> {
    let contents = std::fs::read(path)?;
    let record: RecoveryRecord = serde_json::from_slice(&contents).map_err(|error| {
        CoreError::LockUnreadable(format!(
            "invalid Cargo.lock recovery record at {path}: {error}; left both files untouched"
        ))
    })?;
    record.validate(project, path)?;
    Ok(record)
}

fn recover_speculative_write(project: &Project, lock_path: &Utf8Path) -> Result<()> {
    let recovery_path = recovery_path(lock_path);
    if let Err(error) = std::fs::metadata(&recovery_path) {
        return if error.kind() == std::io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(error.into())
        };
    }
    let record = read_recovery_record(project, &recovery_path)?;
    let current = std::fs::read_to_string(lock_path)?;
    if current == record.original_lock {
        return remove_recovery_file(&recovery_path);
    }
    if current != record.previous_lock && current != record.candidate_lock {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock no longer matches the interrupted transaction recorded at {recovery_path}; left both files untouched"
        )));
    }
    cooldown_core::fs::atomic_write(lock_path.as_std_path(), record.original_lock.as_bytes())?;
    remove_recovery_file(&recovery_path)
}

fn update_recovery_candidate(
    project: &Project,
    lock_path: &Utf8Path,
    recovery_path: &Utf8Path,
    current_text: &str,
    candidate_text: &str,
) -> Result<()> {
    let actual = std::fs::read_to_string(lock_path)?;
    if actual != current_text {
        return Err(CoreError::LockUnreadable(format!(
            "Cargo.lock changed while preparing a speculative correction; left the recovery record at {recovery_path} untouched"
        )));
    }
    let mut record = read_recovery_record(project, recovery_path)?;
    record.previous_hash = lock_fingerprint(current_text);
    record.candidate_hash = lock_fingerprint(candidate_text);
    record.previous_lock = current_text.to_string();
    record.candidate_lock = candidate_text.to_string();
    let contents =
        serde_json::to_vec(&record).map_err(|error| CoreError::Serialization(error.to_string()))?;
    cooldown_core::fs::atomic_write(recovery_path.as_std_path(), &contents)
}

fn begin_speculative_write(
    project: &Project,
    lock_path: &Utf8Path,
    current_text: &str,
    candidate_text: &str,
) -> Result<Utf8PathBuf> {
    let recovery_path = recovery_path(lock_path);
    let record = RecoveryRecord::new(project, current_text, candidate_text)?;
    let contents =
        serde_json::to_vec(&record).map_err(|error| CoreError::Serialization(error.to_string()))?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&recovery_path)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                CoreError::LockConflict(format!(
                    "pending Cargo.lock transaction already exists at {recovery_path}"
                ))
            } else {
                error.into()
            }
        })?;
    if let Err(error) = file.write_all(&contents).and_then(|()| file.sync_all()) {
        let _ = std::fs::remove_file(&recovery_path);
        return Err(error.into());
    }
    cooldown_core::fs::atomic_write(lock_path.as_std_path(), candidate_text.as_bytes())?;
    Ok(recovery_path)
}

fn finish_speculative_write(
    lock_path: &Utf8Path,
    recovery_path: &Utf8Path,
    current_text: &str,
) -> Result<()> {
    if let Err(error) = remove_recovery_file(recovery_path) {
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), current_text.as_bytes())?;
        return Err(error);
    }
    Ok(())
}

fn rollback_speculative_write(
    lock_path: &Utf8Path,
    recovery_path: &Utf8Path,
    current_text: &str,
) -> Result<()> {
    cooldown_core::fs::atomic_write(lock_path.as_std_path(), current_text.as_bytes())?;
    remove_recovery_file(recovery_path)
}

fn restore_candidate(lock_path: &Utf8Path, current_text: &str) -> Result<()> {
    cooldown_core::fs::atomic_write(lock_path.as_std_path(), current_text.as_bytes())
}

fn remove_recovery_file(path: &Utf8Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn failure_reason(failure: CandidateFailure) -> &'static str {
    match failure {
        CandidateFailure::TextMismatch => {
            "the lock text did not match the parsed entry; correction skipped"
        }
        CandidateFailure::Verification => {
            "the corrected lock failed cargo's --locked verification; kept the resolver's binding"
        }
    }
}

fn reject(guarded: &mut GuardedRewrites, rewrite: EdgeRewrite, reason: &str) {
    guarded.rejected.push(RejectedRewrite {
        rewrite,
        reason: reason.to_string(),
    });
}

fn reject_all(guarded: &mut GuardedRewrites, reason: &str) {
    for rewrite in guarded.accepted.drain(..) {
        guarded.rejected.push(RejectedRewrite {
            rewrite,
            reason: reason.to_string(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{path_key, view};
    use super::*;
    use indoc::indoc;

    const AMBIGUOUS_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [
         "dep 1.0.0",
         "dep 2.0.0",
        ]

        [[package]]
        name = "dep"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "dep"
        version = "2.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
    "#};

    const SIMPLE_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [
         "dep 1.0.0",
        ]

        [[package]]
        name = "dep"
        version = "1.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"

        [[package]]
        name = "dep"
        version = "2.0.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
    "#};

    fn graph_with_app_requirement() -> ResolvedGraph {
        Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {
                        "id": "app 0.1.0 (path+file:///app)",
                        "name": "app",
                        "version": "0.1.0",
                        "dependencies": [{"name": "dep", "req": ">=1, <3"}]
                    },
                    {
                        "id": "dep 2.0.0 (registry+https://github.com/rust-lang/crates.io-index)",
                        "name": "dep",
                        "version": "2.0.0",
                        "source": "registry+https://github.com/rust-lang/crates.io-index",
                        "dependencies": []
                    }
                ],
                "workspace_members": ["app 0.1.0 (path+file:///app)"],
                "workspace_root": "/app",
                "resolve": {"nodes": [{
                    "id": "app 0.1.0 (path+file:///app)",
                    "deps": [{
                        "name": "dep",
                        "pkg": "dep 2.0.0 (registry+https://github.com/rust-lang/crates.io-index)"
                    }]
                }]}
            }
        "#})
    }

    fn rebind(action: EdgeBindingAction, from: &str, to: &str) -> EdgeRebind {
        corrective_row(
            &EdgeRewrite {
                dependent: path_key("app", "0.1.0"),
                dependency: "dep".to_string(),
                from: from.to_string(),
                to: to.to_string(),
            },
            action,
            None,
        )
    }

    #[test]
    fn corrective_policy_reports_ambiguous_churn_as_unaddressable() {
        let after_text = AMBIGUOUS_LOCK.replace(
            " \"dep 1.0.0\",\n \"dep 2.0.0\",",
            " \"dep 1.0.0\",\n \"dep 1.0.0\",",
        );
        let outcomes = residual_outcomes(
            EdgePolicy::Canonicalize,
            &view(AMBIGUOUS_LOCK),
            &view(&after_text),
            &BTreeSet::new(),
            None,
        );

        assert!(matches!(
            outcomes.as_slice(),
            [BindingOutcome::Unaddressable { change, reason }]
                if change.before.version == "2.0.0"
                    && change.after.version == "1.0.0"
                    && reason.contains("several version-qualified entries")
        ));
    }

    #[test]
    fn observation_policy_allows_the_same_ambiguous_churn() {
        let after_text = AMBIGUOUS_LOCK.replace(
            " \"dep 1.0.0\",\n \"dep 2.0.0\",",
            " \"dep 1.0.0\",\n \"dep 1.0.0\",",
        );
        let outcomes = residual_outcomes(
            EdgePolicy::None,
            &view(AMBIGUOUS_LOCK),
            &view(&after_text),
            &BTreeSet::new(),
            None,
        );

        assert!(matches!(
            outcomes.as_slice(),
            [BindingOutcome::ObservedAllowed(change)]
                if change.before.version == "2.0.0" && change.after.version == "1.0.0"
        ));
    }

    #[test]
    fn unchanged_ambiguous_lock_does_not_invent_a_held_target() {
        let lock = view(AMBIGUOUS_LOCK);
        assert!(
            residual_outcomes(
                EdgePolicy::Canonicalize,
                &lock,
                &lock,
                &BTreeSet::new(),
                None,
            )
            .is_empty()
        );
    }

    #[test]
    fn missing_requirement_join_is_unaddressable_under_a_corrective_policy() {
        let after_text = SIMPLE_LOCK.replace("\"dep 1.0.0\",", "\"dep 2.0.0\",");
        let graph = Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [],
                "workspace_members": [],
                "workspace_root": "/app",
                "resolve": {"nodes": []}
            }
        "#});
        let requirements = RequirementIndex::new(&graph);

        let outcomes = residual_outcomes(
            EdgePolicy::Preserve,
            &view(SIMPLE_LOCK),
            &view(&after_text),
            &BTreeSet::new(),
            Some(&requirements),
        );

        assert!(matches!(
            outcomes.as_slice(),
            [BindingOutcome::Unaddressable { reason, .. }]
                if reason.contains("did not identify")
        ));
    }

    #[test]
    fn withheld_target_does_not_cover_the_committed_rebind() {
        let rewrite = EdgeRewrite {
            dependent: path_key("app", "0.1.0"),
            dependency: "dep".to_string(),
            from: "2.0.0".to_string(),
            to: "1.0.0".to_string(),
        };
        let withheld = BindingOutcome::Withheld {
            rewrite,
            reason: "verification failed".to_string(),
        };
        let graph = graph_with_app_requirement();
        let requirements = RequirementIndex::new(&graph);
        let after_text = SIMPLE_LOCK.replace("\"dep 1.0.0\",", "\"dep 2.0.0\",");
        let covered = outcome_edge_key(&withheld).into_iter().collect();

        let observed = residual_outcomes(
            EdgePolicy::Canonicalize,
            &view(SIMPLE_LOCK),
            &view(&after_text),
            &covered,
            Some(&requirements),
        );

        assert!(outcome_edge_key(&withheld).is_none());
        assert!(matches!(
            observed.as_slice(),
            [BindingOutcome::ObservedAllowed(change)]
                if change.before.version == "1.0.0" && change.after.version == "2.0.0"
        ));
    }

    #[test]
    fn surviving_batch_correction_replaces_the_generic_final_observation() {
        let final_text = SIMPLE_LOCK.replace("\"dep 1.0.0\",", "\"dep 2.0.0\",");
        let correction = rebind(EdgeBindingAction::Canonicalized, "1.0.0", "2.0.0");
        let held = rebind(EdgeBindingAction::Held, "2.0.0", "1.0.0");
        let mut final_outcomes = vec![
            rebind(EdgeBindingAction::Rebound, "1.0.0", "2.0.0"),
            held.clone(),
        ];

        reconcile_committed_outcomes(
            &view(&final_text),
            &mut final_outcomes,
            std::slice::from_ref(&correction),
        );

        assert_eq!(final_outcomes, vec![held, correction]);
    }

    #[test]
    fn a_later_committed_rebound_invalidates_an_earlier_correction() {
        let final_text = SIMPLE_LOCK.replace("\"dep 1.0.0\",", "\"dep 2.0.0\",");
        let correction = rebind(EdgeBindingAction::Canonicalized, "1.0.0", "2.0.0");
        let rebound = rebind(EdgeBindingAction::Rebound, "1.0.0", "2.0.0");
        let mut final_outcomes = vec![rebound.clone()];

        reconcile_committed_outcomes(
            &view(&final_text),
            &mut final_outcomes,
            &[correction, rebound.clone()],
        );

        assert_eq!(final_outcomes, vec![rebound]);
    }

    #[test]
    fn partition_isolation_tries_large_groups_before_singletons() {
        let subsets = partition_subsets(5);

        assert_eq!(&subsets[..2], &[vec![0, 1], vec![2, 3, 4]]);
        let first_singleton = subsets
            .iter()
            .position(|subset| subset.len() == 1)
            .unwrap_or(usize::MAX);
        assert!(first_singleton >= 2);
    }

    #[test]
    fn balanced_retry_units_keep_reciprocal_swaps_atomic() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "consumer-a"
            version = "1.0.0"
            dependencies = ["dep 1.0.0"]

            [[package]]
            name = "consumer-b"
            version = "1.0.0"
            dependencies = ["dep 2.0.0"]

            [[package]]
            name = "dep"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "dep"
            version = "2.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#};
        let rewrites = vec![
            EdgeRewrite {
                dependent: path_key("consumer-a", "1.0.0"),
                dependency: "dep".to_string(),
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
            },
            EdgeRewrite {
                dependent: path_key("consumer-b", "1.0.0"),
                dependency: "dep".to_string(),
                from: "2.0.0".to_string(),
                to: "1.0.0".to_string(),
            },
        ];

        let units = balanced_retry_units(&view(lock), &rewrites);

        assert_eq!(units, vec![rewrites]);
    }

    fn recovery_project(root: &Utf8Path) -> Project {
        Project {
            root: root.to_owned(),
            kind: CARGO_ID,
            manifest: root.join("Cargo.toml"),
            exclude_newer: None,
        }
    }

    #[test]
    fn pending_speculative_write_restores_only_its_recorded_candidate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 temp path");
        let project = recovery_project(&root);
        let lock_path = root.join("Cargo.lock");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"resolver").expect("write lock");
        let marker = begin_speculative_write(&project, &lock_path, "resolver", "candidate")
            .expect("begin speculative write");

        recover_speculative_write(&project, &lock_path).expect("recover lock");

        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "resolver"
        );
        assert!(!marker.exists(), "recovery marker is consumed");
    }

    #[test]
    fn recovery_marker_spans_candidate_retries_until_commit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 temp path");
        let project = recovery_project(&root);
        let lock_path = root.join("Cargo.lock");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"resolver").expect("write lock");

        let marker = begin_speculative_write(&project, &lock_path, "resolver", "candidate-one")
            .expect("begin speculative write");
        restore_candidate(&lock_path, "resolver").expect("reject first candidate");
        update_recovery_candidate(&project, &lock_path, &marker, "resolver", "candidate-two")
            .expect("record second candidate");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"candidate-two")
            .expect("write second candidate");

        let record = read_recovery_record(&project, &marker).expect("read recovery marker");
        assert_eq!(record.original_lock, "resolver");
        finish_speculative_write(&lock_path, &marker, "resolver").expect("commit second candidate");
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read final lock"),
            "candidate-two"
        );
        assert!(!marker.exists(), "commit consumes the recovery marker");
    }

    #[test]
    fn untrusted_recovery_record_leaves_both_files_untouched() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 temp path");
        let project = recovery_project(&root);
        let lock_path = root.join("Cargo.lock");
        let marker = recovery_path(&lock_path);
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"current").expect("write lock");
        cooldown_core::fs::atomic_write(marker.as_std_path(), b"user data").expect("write marker");

        let error = recover_speculative_write(&project, &lock_path).expect_err("reject marker");

        assert!(matches!(error, CoreError::LockUnreadable(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "current"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "user data"
        );
    }

    #[test]
    fn recovery_refuses_a_lock_changed_after_the_recorded_candidate() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 temp path");
        let project = recovery_project(&root);
        let lock_path = root.join("Cargo.lock");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"original").expect("write lock");
        let marker = begin_speculative_write(&project, &lock_path, "original", "candidate")
            .expect("begin speculative write");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"user edit")
            .expect("replace lock");
        let marker_before = std::fs::read(&marker).expect("read marker");

        let error = recover_speculative_write(&project, &lock_path).expect_err("reject drift");

        assert!(matches!(error, CoreError::LockUnreadable(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "user edit"
        );
        assert_eq!(std::fs::read(&marker).expect("read marker"), marker_before);
    }

    #[test]
    fn read_side_pending_check_never_recovers_the_lock() {
        let dir = tempfile::tempdir().expect("temp dir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_owned()).expect("UTF-8 temp path");
        let project = recovery_project(&root);
        let lock_path = root.join("Cargo.lock");
        let marker = recovery_path(&lock_path);
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"candidate").expect("write lock");
        cooldown_core::fs::atomic_write(marker.as_std_path(), b"pending").expect("write marker");

        let error = ensure_no_pending(&project).expect_err("pending transaction");

        assert!(matches!(error, CoreError::StaleLock(_)));
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "candidate"
        );
        assert_eq!(
            std::fs::read_to_string(&marker).expect("read marker"),
            "pending"
        );
    }
}
