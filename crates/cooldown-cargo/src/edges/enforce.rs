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
    EdgeBindingAction, EdgePolicy, EdgeRebind, PackageId, Project, Result, Version,
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

const MAX_SUBSET_PROBES: usize = 64;

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
    recover_pending(project)?;
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
pub(crate) fn recover_pending(project: &Project) -> Result<()> {
    recover_speculative_write(&project.root.join("Cargo.lock"))
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
        Some(requirements) if requirements.identifies(&change.dependent, &change.dependency) => None,
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
    let recovery_path = match begin_speculative_write(lock_path, resolver_text, &rewritten) {
        Ok(path) => path,
        Err(error) => {
            recover_speculative_write(lock_path)?;
            return Err(error);
        }
    };
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
            match isolate_rewrites(cargo, project, lock_path, resolver_text, guarded).await {
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
    resolver_text: &str,
    guarded: &mut GuardedRewrites,
) -> Result<CommittedRewrites> {
    let mut corrected = Vec::new();
    let mut current_text = resolver_text.to_string();
    let mut verified_graph = None;

    let mut components: VecDeque<_> =
        super::rewrite::rewrite_components(std::mem::take(&mut guarded.accepted)).into();
    while let Some(component) = components.pop_front() {
        match try_candidate(cargo, project, lock_path, &current_text, &component).await? {
            Ok((candidate_text, graph)) => {
                corrected.extend(component);
                current_text = candidate_text;
                verified_graph = Some(graph);
            }
            Err(failure) if component.len() > 1 => {
                if let Some(subset) =
                    verified_subset(cargo, project, lock_path, &current_text, &component).await?
                {
                    corrected.extend(subset.accepted);
                    current_text = subset.lock_text;
                    verified_graph = Some(subset.graph);
                    let mut remainder = super::rewrite::rewrite_components(subset.remainder);
                    remainder.reverse();
                    for component in remainder {
                        components.push_front(component);
                    }
                } else {
                    for rewrite in component {
                        reject(guarded, rewrite, failure_reason(failure));
                    }
                }
            }
            Err(failure) => {
                for rewrite in component {
                    reject(guarded, rewrite, failure_reason(failure));
                }
            }
        }
    }
    match verified_graph {
        Some(graph) => Ok(CommittedRewrites::Changed {
            corrected,
            lock_text: current_text,
            graph: Box::new(graph),
        }),
        None => Ok(CommittedRewrites::Unchanged),
    }
}

struct VerifiedSubset {
    accepted: Vec<EdgeRewrite>,
    remainder: Vec<EdgeRewrite>,
    lock_text: String,
    graph: ResolvedGraph,
}

async fn verified_subset(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    current_text: &str,
    component: &[EdgeRewrite],
) -> Result<Option<VerifiedSubset>> {
    for indices in isolation_subsets(component.len()) {
        let selected: BTreeSet<_> = indices.iter().copied().collect();
        let accepted: Vec<_> = indices
            .iter()
            .filter_map(|index| component.get(*index).cloned())
            .collect();
        if let Ok((lock_text, graph)) =
            try_candidate(cargo, project, lock_path, current_text, &accepted).await?
        {
            let remainder = component
                .iter()
                .enumerate()
                .filter(|(index, _)| !selected.contains(index))
                .map(|(_, rewrite)| rewrite.clone())
                .collect();
            return Ok(Some(VerifiedSubset {
                accepted,
                remainder,
                lock_text,
                graph,
            }));
        }
    }
    Ok(None)
}

fn isolation_subsets(size: usize) -> Vec<Vec<usize>> {
    let full: Vec<_> = (0..size).collect();
    let mut pending = VecDeque::from([full.clone()]);
    let mut seen = BTreeSet::from([full]);
    let mut subsets = Vec::new();
    while let Some(parent) = pending.pop_front() {
        if subsets.len() >= MAX_SUBSET_PROBES {
            break;
        }
        for dropped in 0..parent.len() {
            let mut child = parent.clone();
            child.remove(dropped);
            if child.is_empty() || !seen.insert(child.clone()) {
                continue;
            }
            subsets.push(child.clone());
            if subsets.len() >= MAX_SUBSET_PROBES {
                break;
            }
            if child.len() > 1 {
                pending.push_back(child);
            }
        }
    }
    subsets
}

async fn try_candidate(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    current_text: &str,
    rewrites: &[EdgeRewrite],
) -> Result<std::result::Result<(String, ResolvedGraph), CandidateFailure>> {
    let Some(candidate_text) = rewrite_lock_text(current_text, rewrites) else {
        return Ok(Err(CandidateFailure::TextMismatch));
    };
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

fn recover_speculative_write(lock_path: &Utf8Path) -> Result<()> {
    let recovery_path = recovery_path(lock_path);
    match std::fs::read(&recovery_path) {
        Ok(contents) => {
            cooldown_core::fs::atomic_write(lock_path.as_std_path(), &contents)?;
            remove_recovery_file(&recovery_path)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn begin_speculative_write(
    lock_path: &Utf8Path,
    current_text: &str,
    candidate_text: &str,
) -> Result<Utf8PathBuf> {
    let recovery_path = recovery_path(lock_path);
    cooldown_core::fs::atomic_write(recovery_path.as_std_path(), current_text.as_bytes())?;
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
                "packages": [{
                    "id": "app 0.1.0 (path+file:///app)",
                    "name": "app",
                    "version": "0.1.0",
                    "dependencies": [{"name": "dep", "req": ">=1, <3"}]
                }],
                "workspace_members": ["app 0.1.0 (path+file:///app)"],
                "workspace_root": "/app",
                "resolve": {"nodes": []}
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
    fn subset_isolation_tries_drop_one_complements_before_singletons() {
        let subsets = isolation_subsets(3);

        assert_eq!(&subsets[..3], &[vec![1, 2], vec![0, 2], vec![0, 1]]);
        assert!(
            subsets
                .iter()
                .position(|subset| subset == &[0, 1])
                .is_some()
        );
        let first_singleton = subsets
            .iter()
            .position(|subset| subset.len() == 1)
            .unwrap_or(usize::MAX);
        assert!(first_singleton >= 3);
    }

    #[test]
    fn pending_speculative_write_restores_the_saved_lock() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lock_path =
            Utf8PathBuf::from_path_buf(dir.path().join("Cargo.lock")).expect("UTF-8 temp path");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"verified").expect("write lock");
        let marker = recovery_path(&lock_path);
        cooldown_core::fs::atomic_write(marker.as_std_path(), b"resolver")
            .expect("write recovery marker");

        recover_speculative_write(&lock_path).expect("recover lock");

        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read lock"),
            "resolver"
        );
        assert!(!marker.exists(), "recovery marker is consumed");
    }

    #[test]
    fn recovery_marker_spans_candidate_retries_until_commit() {
        let dir = tempfile::tempdir().expect("temp dir");
        let lock_path =
            Utf8PathBuf::from_path_buf(dir.path().join("Cargo.lock")).expect("UTF-8 temp path");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"resolver").expect("write lock");

        let marker = begin_speculative_write(&lock_path, "resolver", "candidate-one")
            .expect("begin speculative write");
        restore_candidate(&lock_path, "resolver").expect("reject first candidate");
        cooldown_core::fs::atomic_write(lock_path.as_std_path(), b"candidate-two")
            .expect("write second candidate");

        assert_eq!(
            std::fs::read_to_string(&marker).expect("read recovery marker"),
            "resolver"
        );
        finish_speculative_write(&lock_path, &marker, "resolver").expect("commit second candidate");
        assert_eq!(
            std::fs::read_to_string(&lock_path).expect("read final lock"),
            "candidate-two"
        );
        assert!(!marker.exists(), "commit consumes the recovery marker");
    }
}
