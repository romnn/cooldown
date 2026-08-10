//! Defines transitive policy-violation identities and residual comparisons.

use cooldown_core::{BaselineViolation, Dependency, PackageId};
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
    let before_counts = violation_counts_by_package(before);
    let after_counts = violation_counts_by_package(after);
    let mut residual: Vec<BaselineViolation> = after
        .difference(before)
        .filter(|violation| {
            let package = (
                violation.package.name.as_str(),
                violation.package.registry.as_deref(),
            );
            after_counts.get(&package).copied().unwrap_or(0)
                > before_counts.get(&package).copied().unwrap_or(0)
        })
        .cloned()
        .collect();
    residual.sort_by(|left, right| {
        left.package
            .tool
            .as_str()
            .cmp(right.package.tool.as_str())
            .then_with(|| left.package.name.cmp(&right.package.name))
            .then_with(|| left.package.registry.cmp(&right.package.registry))
            .then_with(|| left.version.as_str().cmp(right.version.as_str()))
    });
    residual
}

fn violation_counts_by_package(
    violations: &HashSet<BaselineViolation>,
) -> HashMap<(&str, Option<&str>), usize> {
    let mut counts = HashMap::new();
    for violation in violations {
        *counts
            .entry((
                violation.package.name.as_str(),
                violation.package.registry.as_deref(),
            ))
            .or_default() += 1;
    }
    counts
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

pub(super) fn violation_identity(violation: &BaselineViolation) -> String {
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
