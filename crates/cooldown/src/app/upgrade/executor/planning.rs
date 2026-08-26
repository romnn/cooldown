use super::PlanMode;
use crate::app::TransitiveGate;
use cooldown_core::{
    BaselineViolation, Change, DepScope, Dependency, GraphHoldEdge, GraphHoldKind, MajorKey,
    PackageId, Release, ResolveContext, UpdateKind, Version,
};
use std::collections::HashSet;

/// Selects the dependency scope that supplies upgrade or downgrade candidates.
///
/// Both directions walk the resolved graph by default. Resolvers only promote the minimum a
/// requirement forces, so a matured in-range release of a *transitive* can sit unadopted forever
/// (a security patch no direct pin drags); `upgrade` therefore plans transitives too, advancing
/// each within its major line, and `fix` downgrades too-fresh ones. `--transitive hide` narrows
/// either direction to direct dependencies.
pub(super) const fn candidate_scope(mode: PlanMode) -> DepScope {
    match mode.transitive_mode() {
        TransitiveGate::Hide => DepScope::Direct,
        TransitiveGate::Enforce | TransitiveGate::Allow => DepScope::Graph,
    }
}

/// Narrows cross-major resolution to dependencies with an editable direct requirement.
///
/// An indirect dependency cannot move across a major boundary on its own, but it can still adopt a
/// newer matured version within its current major.
pub(super) fn dep_resolve_ctx<'a>(
    rctx: &ResolveContext<'a>,
    dep: &Dependency,
) -> ResolveContext<'a> {
    ResolveContext {
        allow_major: dep.direct && rctx.allow_major,
        ..*rctx
    }
}

/// Restores deterministic mutation order after concurrent registry fetches complete.
///
/// Adapters must tolerate any order, but a stable plan keeps conflict winners reproducible.
pub(super) fn sort_planned_changes(changes: &mut [Change]) {
    changes.sort_by(|a, b| {
        a.package
            .name
            .cmp(&b.package.name)
            .then_with(|| a.package.registry.cmp(&b.package.registry))
            .then_with(|| a.from.as_str().cmp(b.from.as_str()))
            .then_with(|| a.to.as_str().cmp(b.to.as_str()))
            .then_with(|| {
                a.members
                    .iter()
                    .map(|member| (&member.name, &member.path))
                    .cmp(b.members.iter().map(|member| (&member.name, &member.path)))
            })
    });
}

pub(super) fn plan_baseline_violations(
    violations: &HashSet<BaselineViolation>,
) -> Vec<BaselineViolation> {
    let mut baseline = violations.iter().cloned().collect::<Vec<_>>();
    baseline.sort_by(|a, b| {
        a.package
            .tool
            .as_str()
            .cmp(b.package.tool.as_str())
            .then_with(|| a.package.name.cmp(&b.package.name))
            .then_with(|| a.package.registry.cmp(&b.package.registry))
            .then_with(|| a.version.as_str().cmp(b.version.as_str()))
    });
    baseline
}

/// Reports whether `to` precedes `from` in the adapter's canonical release order.
///
/// Unknown endpoints fail closed as not-a-downgrade because their direction cannot be proven.
pub(super) fn is_downgrade(releases: &[Release], from: &Version, to: &Version) -> bool {
    let order = |version: &Version| {
        releases
            .iter()
            .find(|release| &release.version == version)
            .map(|release| &release.order)
    };
    matches!((order(to), order(from)), (Some(target), Some(current)) if target < current)
}

/// Derives the target package identity for a version movement.
///
/// Go path majors are rewritten while ecosystems with stable package names retain their identity.
pub(crate) fn target_package_for(
    releases: &[Release],
    dep: &Dependency,
    target: &Version,
) -> PackageId {
    let current_major = releases
        .iter()
        .find(|release| release.version == dep.current)
        .map_or(MajorKey(String::new()), |release| release.major.clone());
    let target_major = releases
        .iter()
        .find(|release| release.version == *target)
        .map(|release| release.major.clone())
        .unwrap_or(current_major.clone());
    target_package(&dep.package, &current_major, &target_major)
}

/// A violating dependency's graph constraints with circular contributions discounted: the
/// constraints that remain once every hold edge whose requirer is *itself* a too-fresh violation
/// in the same planning round is set aside.
///
/// The raw [`Dependency::graph_floor`]/[`Dependency::graph_ceiling`] describe the current
/// resolution, so a family of violations holds itself in place: each member's floor comes from
/// siblings the fix wants to move — a hold conditioned on the very resolution being questioned.
/// Discounting those edges lets the planner schedule the whole family; the resolver remains the
/// constraint oracle (an infeasible optimistic pin is rejected non-fatally and the next fix round
/// re-plans against the re-locked graph, whose floors are then no longer circular).
pub(super) struct EffectiveHold {
    /// The dependency with its effective floor/ceiling substituted; identical to the input when
    /// nothing was discounted or the adapter provided no attribution.
    pub(super) dep: Dependency,
    /// Whether any hold edge was discounted (so the effective verdict must be re-evaluated).
    pub(super) discounted: bool,
    /// A non-discounted edge that still holds the dependency at its current version, if any — the
    /// requirer a genuine-hold warning names.
    pub(super) compliant_holder: Option<GraphHoldEdge>,
}

/// Computes the [`EffectiveHold`] of `dep` against this round's violation set (package name and
/// current version of every too-fresh dependency in scope).
///
/// Without attribution ([`Dependency::hold_edges`] empty) the collapsed constraints stand
/// unchanged. With attribution, the effective floor is derived from the kept (non-violating)
/// floor edges alone: a kept edge flooring the node at its own current version holds it exactly;
/// otherwise the highest kept bound present in `releases` becomes the effective floor (a bound
/// absent from the fetched releases imposes no clamp, matching how `evaluate_fix` treats an
/// unknown collapsed floor). The effective ceiling survives only while a kept exact-pin edge caps
/// the node.
pub(super) fn effective_hold(
    dep: &Dependency,
    releases: &[Release],
    violations: &HashSet<(String, String)>,
) -> EffectiveHold {
    if dep.hold_edges.is_empty() {
        return EffectiveHold {
            dep: dep.clone(),
            discounted: false,
            compliant_holder: None,
        };
    }
    let kept: Vec<&GraphHoldEdge> = dep
        .hold_edges
        .iter()
        .filter(|edge| {
            !violations.contains(&(
                edge.requirer.clone(),
                edge.requirer_version.as_str().to_string(),
            ))
        })
        .collect();
    let discounted = kept.len() != dep.hold_edges.len();
    let holding = |edge: &GraphHoldEdge| match edge.kind {
        GraphHoldKind::Floor => edge.bound == dep.current,
        // The adapter invariant guarantees an active exact-pin ceiling equals the resolved
        // version, so any kept ceiling edge caps the node at `current`.
        GraphHoldKind::Ceiling => true,
    };
    let compliant_holder = kept
        .iter()
        .find(|edge| holding(edge))
        .map(|edge| (*edge).clone());
    if !discounted {
        return EffectiveHold {
            dep: dep.clone(),
            discounted,
            compliant_holder,
        };
    }
    let held_at_current = kept
        .iter()
        .any(|edge| matches!(edge.kind, GraphHoldKind::Floor) && edge.bound == dep.current);
    let effective_floor = if held_at_current {
        Some(dep.current.clone())
    } else {
        kept.iter()
            .filter(|edge| matches!(edge.kind, GraphHoldKind::Floor))
            .filter_map(|edge| {
                releases
                    .iter()
                    .find(|release| release.version == edge.bound)
                    .map(|release| (release.order.clone(), edge.bound.clone()))
            })
            .max_by(|(a, _), (b, _)| a.cmp(b))
            .map(|(_, bound)| bound)
    };
    let effective_ceiling = kept
        .iter()
        .any(|edge| matches!(edge.kind, GraphHoldKind::Ceiling))
        .then(|| dep.current.clone());
    let mut effective = dep.clone();
    effective.graph_floor = effective_floor;
    effective.graph_ceiling = effective_ceiling;
    EffectiveHold {
        dep: effective,
        discounted,
        compliant_holder,
    }
}

/// Builds a `fix` change that matures one dependency back to its selected target.
pub(super) fn fix_change(
    releases: &[Release],
    dep: &Dependency,
    target: Version,
    kind: UpdateKind,
) -> Change {
    Change {
        package: target_package_for(releases, dep, &target),
        from: dep.current.clone(),
        to: target,
        kind,
        downgrade: true,
        direct: dep.direct,
        members: dep.members.clone(),
    }
}

pub(super) fn target_package(
    package: &PackageId,
    current_major: &MajorKey,
    target_major: &MajorKey,
) -> PackageId {
    let suffix = &target_major.0;
    let is_path_major = |key: &str| key.starts_with('/') || key.starts_with('.');
    let name = if current_major.0 != target_major.0
        && (is_path_major(&current_major.0) || is_path_major(suffix))
    {
        let prefix = package
            .name
            .strip_suffix(&current_major.0)
            .unwrap_or(&package.name);
        format!("{prefix}{suffix}")
    } else {
        package.name.clone()
    };
    PackageId::new(package.tool, name, package.registry.clone())
}
