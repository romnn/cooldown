use super::{PlanMode, ViolationKey};
use crate::app::TransitiveGate;
use cooldown_core::{
    BaselineViolation, Change, DepScope, Dependency, MajorKey, PackageId, Release, ResolveContext,
    UpdateKind, Version,
};
use std::collections::HashSet;

/// Selects the dependency scope that supplies upgrade or downgrade candidates.
///
/// `upgrade` moves only direct requirements forward because indirect movement is resolver-owned
/// collateral.
/// `fix` walks the resolved graph unless `--transitive hide` explicitly narrows it to direct
/// dependencies.
pub(super) const fn candidate_scope(mode: PlanMode) -> DepScope {
    match mode {
        PlanMode::Upgrade
        | PlanMode::Fix {
            transitive: TransitiveGate::Hide,
            ..
        } => DepScope::Direct,
        PlanMode::Fix { .. } => DepScope::Graph,
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
    violations: &HashSet<ViolationKey>,
) -> Vec<BaselineViolation> {
    let mut baseline = violations
        .iter()
        .map(|violation| BaselineViolation {
            package: violation.package.clone(),
            version: Version::new(&violation.version),
        })
        .collect::<Vec<_>>();
    baseline.sort_by(|a, b| {
        a.package
            .cmp(&b.package)
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
