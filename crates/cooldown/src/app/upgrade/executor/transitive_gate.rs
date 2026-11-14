//! Defines graph policy-violation identities and residual comparisons.

use crate::app::change_key::{ChangeProvenanceKey, change_provenance_key};
use cooldown_core::{BaselineViolation, Change, Dependency, PackageId};
use std::collections::{HashMap, HashSet};

/// The transitive-cooldown gate's decision over a freshly applied batch.
pub(super) enum TransitiveGateVerdict {
    /// Restore the journal; the batch must not be kept.
    RollBack,
    /// Keep the batch and optionally reconcile floated-up violations.
    Keep { reconcile_needed: bool },
}

pub(super) fn newly_introduced_violations(
    before: &HashSet<BaselineViolation>,
    after: &HashSet<BaselineViolation>,
) -> Vec<BaselineViolation> {
    let mut residual: Vec<BaselineViolation> = after.difference(before).cloned().collect();
    sort_violations(&mut residual);
    residual
}

/// The locked versions outside a configured `max-major` that nothing authorized, sorted.
///
/// `crossings` holds every locked version outside its package's ceiling, and `allowed` the
/// authorized ones (see [`admitted_crossings`]).
pub(in crate::app::upgrade) fn unauthorized_crossings(
    crossings: &HashSet<BaselineViolation>,
    allowed: &HashSet<BaselineViolation>,
) -> Vec<BaselineViolation> {
    let mut residual: Vec<BaselineViolation> = crossings.difference(allowed).cloned().collect();
    sort_violations(&mut residual);
    residual
}

/// The targets of `planned` changes that landed from a version `allowed` already authorizes.
///
/// The run's starting graph authorizes every version it holds outside a ceiling, so a pin that
/// was already outside before the run does not fail it.
/// A planned move of such a pin is authorized too, because the planner deactivates the ceiling
/// for a pin already outside it.
/// Nothing else is: a version the resolver lands on its own, or a move planned from a version
/// only a trial put outside the ceiling, would carry a copy across it.
/// Without the graph's edges nothing proves which copy a new version continues, so a version
/// that rises beside an authorized one is not authorized by it.
///
/// A planned change counts as landed only when `applied` holds it with its source version: a
/// collateral move of another copy onto the same target proves nothing about the planned one.
pub(super) fn admitted_crossings(
    planned: &[Change],
    applied: &HashSet<ChangeProvenanceKey>,
    allowed: &HashSet<BaselineViolation>,
) -> HashSet<BaselineViolation> {
    planned
        .iter()
        .filter(|change| applied.contains(&change_provenance_key(change)))
        .filter(|change| {
            allowed.contains(&BaselineViolation {
                package: change.package.clone(),
                version: change.from.clone(),
            })
        })
        .map(|change| BaselineViolation {
            package: change.package.clone(),
            version: change.to.clone(),
        })
        .collect()
}

fn sort_violations(violations: &mut [BaselineViolation]) {
    violations.sort_by(|left, right| {
        left.package
            .tool
            .as_str()
            .cmp(right.package.tool.as_str())
            .then_with(|| left.package.name.cmp(&right.package.name))
            .then_with(|| left.package.registry.cmp(&right.package.registry))
            .then_with(|| left.version.as_str().cmp(right.version.as_str()))
    });
}

pub(super) fn insert_graph_violation(
    violations: &mut HashMap<BaselineViolation, bool>,
    dep: &Dependency,
) {
    let reconcilable = dep
        .graph_floor
        .as_ref()
        .is_some_and(|floor| *floor != dep.current);
    violations.insert(
        BaselineViolation {
            package: dep.package.clone(),
            version: dep.current.clone(),
        },
        reconcilable,
    );
}

pub(super) fn package_label(package: &PackageId) -> String {
    package
        .registry
        .as_deref()
        .filter(|source| !is_default_registry(package, source))
        .map_or_else(
            || package.name.clone(),
            |source| {
                format!(
                    "{} from {}",
                    package.name,
                    cooldown_core::redact::source_label(source)
                )
            },
        )
}

fn is_default_registry(package: &PackageId, source: &str) -> bool {
    matches!(
        (package.tool.as_str(), source),
        ("cargo", "crates.io")
            | ("npm" | "pnpm" | "yarn" | "bun", "npm")
            | ("go", "proxy.golang.org")
    )
}

pub(in crate::app::upgrade) fn violation_identity(violation: &BaselineViolation) -> String {
    violation.package.registry.as_deref().map_or_else(
        || format!("{}@{}", violation.package.name, violation.version),
        |source| {
            format!(
                "{}@{} from {}",
                violation.package.name,
                violation.version,
                cooldown_core::redact::source_label(source)
            )
        },
    )
}
