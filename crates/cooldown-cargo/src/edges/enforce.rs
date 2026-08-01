//! Proposes, guards, verifies, commits, and reports edge-policy outcomes.

use super::{
    BindingChange, EdgeRewrite, GuardedRewrites, LockEdgeView, RejectedRewrite, RequirementIndex,
    binding_changes, guard_rewrites, rewrite_lock_text,
};
use crate::CARGO_ID;
use crate::cargocmd::{CRATES_IO_SOURCE, Cargo, ResolvedGraph};
use crate::index::CRATES_IO;
use crate::lockfile::CargoLock;
use camino::Utf8Path;
use cooldown_core::{
    EdgeBindingAction, EdgePolicy, EdgeRebind, PackageId, Project, Result, Version,
};
use std::collections::BTreeSet;

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

struct CommittedRewrites {
    corrected: Vec<EdgeRewrite>,
    lock_text: String,
    verified_graph: Option<ResolvedGraph>,
}

impl CommittedRewrites {
    fn unchanged(resolver_text: &str) -> Self {
        CommittedRewrites {
            corrected: Vec::new(),
            lock_text: resolver_text.to_string(),
            verified_graph: None,
        }
    }
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

    let corrected_action = match policy {
        EdgePolicy::Preserve => EdgeBindingAction::Restored,
        EdgePolicy::Canonicalize => EdgeBindingAction::Canonicalized,
        EdgePolicy::None => EdgeBindingAction::Rebound,
    };
    let mut outcomes: Vec<BindingOutcome> = committed
        .corrected
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
        let final_view = if committed.corrected.is_empty() {
            resolver_view
        } else {
            LockEdgeView::from_lock(&CargoLock::parse(&committed.lock_text)?)
        };
        let covered: BTreeSet<_> = outcomes.iter().filter_map(outcome_edge_key).collect();
        outcomes.extend(residual_outcomes(
            policy,
            before_view,
            &final_view,
            &covered,
        ));
    }

    let graph = committed.verified_graph.or(graph);
    Ok(EnforcementResult {
        rebinds: outcomes.into_iter().map(outcome_row).collect(),
        graph,
    })
}

fn residual_outcomes(
    policy: EdgePolicy,
    before: &LockEdgeView,
    after: &LockEdgeView,
    covered: &BTreeSet<(super::LockPackageId, String)>,
) -> Vec<BindingOutcome> {
    binding_changes(before, after)
        .into_iter()
        .filter_map(|change| {
            let key = (change.dependent.clone(), change.dependency.clone());
            if covered.contains(&key) {
                return None;
            }
            let limitation = matches!(policy, EdgePolicy::Preserve | EdgePolicy::Canonicalize)
                .then(|| {
                    after
                        .unaddressable_reason(&change.dependent, &change.dependency)
                        .map(str::to_string)
                })
                .flatten();
            Some(match limitation {
                Some(reason) => BindingOutcome::Unaddressable { change, reason },
                None => BindingOutcome::ObservedAllowed(change),
            })
        })
        .collect()
}

fn outcome_edge_key(outcome: &BindingOutcome) -> Option<(super::LockPackageId, String)> {
    match outcome {
        BindingOutcome::Corrected { rewrite, .. } | BindingOutcome::Withheld { rewrite, .. } => {
            Some((rewrite.dependent.clone(), rewrite.dependency.clone()))
        }
        BindingOutcome::ObservedAllowed(_) | BindingOutcome::Unaddressable { .. } => None,
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
        dependent_source: rewrite.dependent.source.clone(),
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
    let crates_io_endpoints = change.before_source.as_deref() == Some(CRATES_IO_SOURCE)
        && change.after_source.as_deref() == Some(CRATES_IO_SOURCE);
    let detail = match (limitation, change.detail) {
        (Some(reason), Some(observation)) => Some(format!("{reason}; {observation}")),
        (Some(reason), None) => Some(reason),
        (None, observation) => observation,
    };
    EdgeRebind {
        dependent: change.dependent.name,
        dependent_version: Version::new(change.dependent.version),
        dependent_source: change.dependent.source,
        dependency: PackageId::new(
            CARGO_ID,
            change.dependency,
            crates_io_endpoints.then(|| CRATES_IO.to_string()),
        ),
        from: Version::new(change.before),
        to: Version::new(change.after),
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
        return Ok(CommittedRewrites::unchanged(resolver_text));
    }
    let Some(rewritten) = rewrite_lock_text(resolver_text, &guarded.accepted) else {
        reject_all(
            guarded,
            "the lock text did not match the parsed entry; correction skipped",
        );
        return Ok(CommittedRewrites::unchanged(resolver_text));
    };
    cooldown_core::fs::atomic_write(lock_path.as_std_path(), rewritten.as_bytes())?;
    match cargo.verify_locked(&project.root).await {
        Ok(Some(verified_graph)) => Ok(CommittedRewrites {
            corrected: std::mem::take(&mut guarded.accepted),
            lock_text: rewritten,
            verified_graph: Some(verified_graph),
        }),
        Ok(None) => {
            cooldown_core::fs::atomic_write(lock_path.as_std_path(), resolver_text.as_bytes())?;
            isolate_rewrites(cargo, project, lock_path, resolver_text, guarded).await
        }
        Err(error) => {
            cooldown_core::fs::atomic_write(lock_path.as_std_path(), resolver_text.as_bytes())?;
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

    for component in super::rewrite::rewrite_components(std::mem::take(&mut guarded.accepted)) {
        match try_candidate(cargo, project, lock_path, &current_text, &component).await? {
            Ok((candidate_text, graph)) => {
                corrected.extend(component);
                current_text = candidate_text;
                verified_graph = Some(graph);
            }
            Err(_) if component.len() > 1 => {
                for rewrite in component {
                    match try_candidate(
                        cargo,
                        project,
                        lock_path,
                        &current_text,
                        std::slice::from_ref(&rewrite),
                    )
                    .await?
                    {
                        Ok((candidate_text, graph)) => {
                            corrected.push(rewrite);
                            current_text = candidate_text;
                            verified_graph = Some(graph);
                        }
                        Err(failure) => reject(guarded, rewrite, failure_reason(failure)),
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
    Ok(CommittedRewrites {
        corrected,
        lock_text: current_text,
        verified_graph,
    })
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
            cooldown_core::fs::atomic_write(lock_path.as_std_path(), current_text.as_bytes())?;
            Ok(Err(CandidateFailure::Verification))
        }
        Err(error) => {
            cooldown_core::fs::atomic_write(lock_path.as_std_path(), current_text.as_bytes())?;
            Err(error)
        }
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
    use super::super::tests::view;
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
        );

        assert!(matches!(
            outcomes.as_slice(),
            [BindingOutcome::Unaddressable { change, reason }]
                if change.before == "2.0.0"
                    && change.after == "1.0.0"
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
        );

        assert!(matches!(
            outcomes.as_slice(),
            [BindingOutcome::ObservedAllowed(change)]
                if change.before == "2.0.0" && change.after == "1.0.0"
        ));
    }

    #[test]
    fn unchanged_ambiguous_lock_does_not_invent_a_held_target() {
        let lock = view(AMBIGUOUS_LOCK);
        assert!(
            residual_outcomes(EdgePolicy::Canonicalize, &lock, &lock, &BTreeSet::new()).is_empty()
        );
    }
}
