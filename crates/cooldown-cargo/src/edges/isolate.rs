//! Verifies correction batches and isolates a deterministic safe subset within a fixed Cargo
//! budget.
//! Partition and singleton probes improve coverage without claiming optimality.

use super::recovery::{CommitOutcome, SpeculativeLockTransaction};
use super::{
    EdgeRewrite, GuardedRewrites, LockEdgeView, RejectedRewrite, guard_rewrites, rewrite_lock_text,
};
use crate::cargocmd::{Cargo, ResolvedGraph};
use crate::lockfile::CargoLock;
use camino::Utf8Path;
use cooldown_core::{Diagnostic, Project, Result};
use std::collections::{BTreeSet, VecDeque};

/// Bounds every Cargo validation spawned after the initial all-rewrites candidate fails.
const MAX_ISOLATION_PROBES: usize = 64;
const ISOLATION_BUDGET_REASON: &str =
    "the correction isolation budget was exhausted before cargo could validate this edge";

pub(super) enum CommittedRewrites {
    Unchanged,
    Changed {
        corrected: Vec<EdgeRewrite>,
        lock_text: String,
        graph: Box<ResolvedGraph>,
    },
}

pub(super) struct RewriteApplication {
    pub(super) committed: CommittedRewrites,
    pub(super) warnings: Vec<Diagnostic>,
}

#[derive(Clone, Copy)]
enum CandidateFailure {
    TextMismatch,
    Verification,
}

pub(super) async fn apply_rewrites(
    cargo: &Cargo,
    project: &Project,
    lock_path: &Utf8Path,
    resolver_text: &str,
    guarded: &mut GuardedRewrites,
) -> Result<RewriteApplication> {
    if guarded.accepted.is_empty() {
        return Ok(RewriteApplication {
            committed: CommittedRewrites::Unchanged,
            warnings: Vec::new(),
        });
    }
    let Some(rewritten) = rewrite_lock_text(resolver_text, &guarded.accepted) else {
        reject_all(
            guarded,
            "the lock text did not match the parsed entry; correction skipped",
        );
        return Ok(RewriteApplication {
            committed: CommittedRewrites::Unchanged,
            warnings: Vec::new(),
        });
    };
    let mut transaction =
        SpeculativeLockTransaction::begin(project, lock_path, resolver_text, &rewritten)?;
    match cargo.verify_locked(&project.root).await {
        Ok(Some(graph)) => {
            transaction.accept()?;
            let warnings = commit_warnings(transaction.commit()?);
            Ok(RewriteApplication {
                committed: CommittedRewrites::Changed {
                    corrected: std::mem::take(&mut guarded.accepted),
                    lock_text: rewritten,
                    graph: Box::new(graph),
                },
                warnings,
            })
        }
        Ok(None) => {
            transaction.reject()?;
            match isolate_rewrites(cargo, project, resolver_text, guarded, &mut transaction).await {
                Ok(result) => {
                    let warnings = commit_warnings(transaction.commit()?);
                    Ok(RewriteApplication {
                        committed: result,
                        warnings,
                    })
                }
                Err(error) => {
                    transaction.rollback()?;
                    Err(error)
                }
            }
        }
        Err(error) => {
            transaction.rollback()?;
            Err(error)
        }
    }
}

fn commit_warnings(outcome: CommitOutcome) -> Vec<Diagnostic> {
    match outcome {
        CommitOutcome::Committed => Vec::new(),
        CommitOutcome::DurabilityUncertain(error) => {
            vec![Diagnostic::new(error.diagnostic_kind(), error.to_string())]
        }
    }
}

async fn isolate_rewrites(
    cargo: &Cargo,
    project: &Project,
    resolver_text: &str,
    guarded: &mut GuardedRewrites,
    transaction: &mut SpeculativeLockTransaction,
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
            transaction,
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
                    transaction,
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
    transaction: &mut SpeculativeLockTransaction,
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
            transaction,
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
    transaction: &mut SpeculativeLockTransaction,
    current_text: &str,
    rewrites: &[EdgeRewrite],
    probes_remaining: &mut usize,
) -> Result<IsolationCandidate> {
    if *probes_remaining == 0 {
        return Ok(IsolationCandidate::Exhausted);
    }
    *probes_remaining -= 1;
    Ok(
        match try_candidate(cargo, project, transaction, current_text, rewrites).await? {
            Ok((lock_text, graph)) => IsolationCandidate::Verified(lock_text, Box::new(graph)),
            Err(failure) => IsolationCandidate::Rejected(failure),
        },
    )
}

async fn try_candidate(
    cargo: &Cargo,
    project: &Project,
    transaction: &mut SpeculativeLockTransaction,
    current_text: &str,
    rewrites: &[EdgeRewrite],
) -> Result<std::result::Result<(String, ResolvedGraph), CandidateFailure>> {
    let Some(candidate_text) = rewrite_lock_text(current_text, rewrites) else {
        return Ok(Err(CandidateFailure::TextMismatch));
    };
    transaction.stage(&candidate_text)?;
    match cargo.verify_locked(&project.root).await {
        Ok(Some(graph)) => {
            transaction.accept()?;
            Ok(Ok((candidate_text, graph)))
        }
        Ok(None) => {
            transaction.reject()?;
            Ok(Err(CandidateFailure::Verification))
        }
        Err(error) => {
            transaction.reject()?;
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
    use super::super::tests::{path_key, view};
    use super::*;
    use crate::lockfile::LockPackageId;
    use indoc::indoc;

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

    #[test]
    fn isolation_budget_reason_is_distinct_from_cargo_rejection() {
        let mut guarded = GuardedRewrites {
            accepted: Vec::new(),
            rejected: Vec::new(),
        };
        reject(
            &mut guarded,
            EdgeRewrite {
                dependent: LockPackageId::new("consumer", "1.0.0", None::<String>),
                dependency: "dep".to_string(),
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
            },
            ISOLATION_BUDGET_REASON,
        );

        assert_eq!(guarded.rejected[0].reason, ISOLATION_BUDGET_REASON);
    }
}
