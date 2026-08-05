//! Proposes, guards, verifies, commits, and reports edge-policy outcomes.

use super::isolate::{CommittedRewrites, apply_rewrites};
use super::{
    BindingChange, EdgeRewrite, LockEdgeView, RequirementIndex, binding_changes, guard_rewrites,
};
use crate::CARGO_ID;
use crate::cargocmd::{CRATES_IO_SOURCE, Cargo, ResolvedGraph};
use crate::index::CRATES_IO;
use crate::lockfile::CargoLock;
use cooldown_core::{
    EdgeBindingAction, EdgePolicy, EdgeRebind, PackageId, Project, Result, Version,
};
use std::collections::{BTreeMap, BTreeSet};

/// The reported binding outcomes and the graph that describes the committed lock.
pub(crate) struct EnforcementResult {
    pub(crate) rebinds: Vec<EdgeRebind>,
    pub(crate) graph: Option<ResolvedGraph>,
}

#[derive(Debug)]
enum BindingOutcome {
    Corrected {
        rewrite: EdgeRewrite,
        action: EdgeBindingAction,
    },
    Withheld {
        rewrite: EdgeRewrite,
        reason: String,
    },
    /// A withheld correction whose edge binding demonstrably moved baseline → final: one row
    /// carrying the real bindings, with the failed attempt in the reason. Splitting it into a
    /// `Withheld` proposal row plus an observation row reported the same decision twice, with the
    /// proposal's To naming a version the final lock does not contain.
    HeldObserved {
        change: BindingChange,
        reason: String,
    },
    ObservedAllowed(BindingChange),
    Unaddressable {
        change: BindingChange,
        reason: String,
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
    let application =
        apply_rewrites(cargo, project, &lock_path, &resolver_text, &mut guarded).await?;
    let committed = application.committed;

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
    let mut withheld_by_key: BTreeMap<(super::LockPackageId, String), (EdgeRewrite, String)> =
        guarded
            .rejected
            .iter()
            .map(|rejected| {
                (
                    (
                        rejected.rewrite.dependent.clone(),
                        rejected.rewrite.dependency.clone(),
                    ),
                    (rejected.rewrite.clone(), rejected.reason.clone()),
                )
            })
            .collect();

    if let Some(before_view) = &before_view {
        let final_view = match final_text {
            Some(final_text) => LockEdgeView::from_lock(&CargoLock::parse(&final_text)?),
            None => resolver_view,
        };
        let covered: BTreeSet<_> = outcomes.iter().filter_map(outcome_edge_key).collect();
        let requirements = graph.as_ref().map(RequirementIndex::new);
        let residuals = residual_outcomes(
            policy,
            before_view,
            &final_view,
            &covered,
            requirements.as_ref(),
        );
        merge_residuals_with_withheld(&mut outcomes, residuals, &mut withheld_by_key);
    }
    // Withheld corrections whose binding shows no residual change (or with no baseline to compare
    // against) keep the proposal presentation: attempted target in To, applied=false.
    outcomes.extend(
        withheld_by_key
            .into_values()
            .map(|(rewrite, reason)| BindingOutcome::Withheld { rewrite, reason }),
    );

    Ok(EnforcementResult {
        rebinds: outcomes.into_iter().map(outcome_row).collect(),
        graph,
    })
}

/// Folds each residual binding change whose edge also carries a withheld correction into a single
/// held row over the real baseline → final bindings: the binding moved and the policy's
/// counter-move failed — one decision, one row. Residuals without a withheld sibling pass through;
/// consumed withheld entries are removed from `withheld_by_key`.
fn merge_residuals_with_withheld(
    outcomes: &mut Vec<BindingOutcome>,
    residuals: Vec<BindingOutcome>,
    withheld_by_key: &mut BTreeMap<(super::LockPackageId, String), (EdgeRewrite, String)>,
) {
    for outcome in residuals {
        let change = match &outcome {
            BindingOutcome::ObservedAllowed(change)
            | BindingOutcome::Unaddressable { change, .. } => Some(change),
            _ => None,
        };
        let held = change.and_then(|change| {
            withheld_by_key.remove(&(change.dependent.clone(), change.dependency.clone()))
        });
        match (held, outcome) {
            (
                Some((_, reason)),
                BindingOutcome::ObservedAllowed(change)
                | BindingOutcome::Unaddressable { change, .. },
            ) => outcomes.push(BindingOutcome::HeldObserved { change, reason }),
            (_, outcome) => outcomes.push(outcome),
        }
    }
}

/// Restores the pre-candidate lock when a prior enforcement terminated during verification.
///
/// The mutation lifecycle calls this only while holding the project's exclusive lock.
pub(crate) fn recover_pending(
    project: &Project,
    authority: &cooldown_core::fs::RecoveryAuthority,
) -> Result<cooldown_core::MutationRecovery> {
    crate::publication::recover_pending(project, authority)
}

/// Refuses a read while an interrupted mutation still owns recovery state.
pub(crate) fn ensure_no_pending(project: &Project) -> Result<()> {
    crate::publication::ensure_no_pending(project)
}

/// Reconciles committed batch corrections with the final saved binding state.
///
/// A later state-changing batch outcome supersedes an earlier correction even when the final
/// binding eventually returns to the same version.
/// Final-pass corrections win over batch provenance; otherwise a surviving batch correction
/// replaces the generic run-level observation for that edge while final held attempts remain
/// visible beside it.
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
        | BindingOutcome::HeldObserved { .. }
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
        BindingOutcome::HeldObserved { change, reason } => {
            observed_row(change, EdgeBindingAction::Held, Some(reason))
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

        std::assert_matches!(outcomes.as_slice(),
            [BindingOutcome::Unaddressable { change, reason }]
                if change.before.version == "2.0.0"
                    && change.after.version == "1.0.0"
            && reason.contains("several version-qualified entries"));
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

        std::assert_matches!(outcomes.as_slice(),
            [BindingOutcome::ObservedAllowed(change)]
        if change.before.version == "2.0.0" && change.after.version == "1.0.0");
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

        std::assert_matches!(outcomes.as_slice(),
            [BindingOutcome::Unaddressable { reason, .. }]
        if reason.contains("did not identify"));
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
        std::assert_matches!(observed.as_slice(),
            [BindingOutcome::ObservedAllowed(change)]
        if change.before.version == "1.0.0" && change.after.version == "2.0.0");
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
}
