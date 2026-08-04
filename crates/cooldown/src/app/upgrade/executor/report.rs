use super::ViolationKey;
use crate::app::{SkippedInfo, UpgradeItem};
use cooldown_core::{Change, LockStatus, SkipReason, UpdateKind};
use std::collections::{HashMap, HashSet};

/// Collapses chronological version legs into the net rows present in the committed lock.
///
/// A transitive that moves from `1.0.44` to `1.0.46` and then reconciles to `1.0.45` must report
/// only `1.0.44` to `1.0.45`.
/// Contiguous chains keep coexisting version lines independent even when they share a package name
/// and registry.
/// A chain that returns to its starting version is removed because it has no net movement.
///
/// Direction is recomputed from the first leg and the violations that existed before the batch.
/// Classification is recomputed through the adapter when it understands the collapsed endpoints.
/// Only applied version rows for the selected project and tool are eligible.
pub(super) fn collapse_applied_legs(
    items: &mut Vec<UpgradeItem>,
    project: &str,
    tool: &str,
    prior_violations: &HashSet<ViolationKey>,
    classify_update_kind: impl Fn(&str, &str) -> Option<UpdateKind>,
) {
    let mut groups: HashMap<(String, Option<String>), Vec<usize>> = HashMap::new();
    for (idx, item) in items.iter().enumerate() {
        let applied =
            item.edge.is_none() && item.applied && item.skipped.is_none() && item.error.is_none();
        if applied && item.project == project && item.tool == tool {
            groups
                .entry((item.name.clone(), item.registry.clone()))
                .or_default()
                .push(idx);
        }
    }
    let mut remove: HashSet<usize> = HashSet::new();
    // Store each chain's first leg and its recomputed net fields.
    let mut retarget: Vec<(usize, String, bool, UpdateKind)> = Vec::new();
    for indices in groups.values() {
        // Link a leg to the chain whose current target equals the leg's source.
        let mut chains: Vec<Vec<usize>> = Vec::new();
        for &idx in indices {
            let Some(from) = items.get(idx).map(|item| item.from.clone()) else {
                continue;
            };
            let slot = chains.iter_mut().find(|chain| {
                chain
                    .last()
                    .and_then(|&tail| items.get(tail))
                    .is_some_and(|tail| tail.to == from)
            });
            match slot {
                Some(chain) => chain.push(idx),
                None => chains.push(vec![idx]),
            }
        }
        for chain in &chains {
            let (Some(&first), Some(&last)) = (chain.first(), chain.last()) else {
                continue;
            };
            if first == last {
                // A single-leg chain already represents its net movement.
                continue;
            }
            let (Some(head), Some(net_to)) = (
                items.get(first),
                items.get(last).map(|item| item.to.clone()),
            ) else {
                continue;
            };
            if head.from == net_to {
                // A chain that returns to its source has no committed movement.
                remove.extend(chain.iter().copied());
                continue;
            }
            let downgrade = head.downgrade
                || prior_violations
                    .iter()
                    .any(|violation| violation.matches_report_row(tool, head));
            let kind = classify_update_kind(&head.from, &net_to).unwrap_or(head.kind);
            retarget.push((first, net_to, downgrade, kind));
            remove.extend(chain.iter().skip(1).copied());
        }
    }
    for (first, net_to, downgrade, kind) in retarget {
        if let Some(head) = items.get_mut(first) {
            head.to = net_to;
            head.downgrade = downgrade;
            head.kind = kind;
        }
    }
    if remove.is_empty() {
        return;
    }
    *items = std::mem::take(items)
        .into_iter()
        .enumerate()
        .filter(|(idx, _)| !remove.contains(idx))
        .map(|(_, item)| item)
        .collect();
}

pub(super) const fn combine_lock_status(
    current: Option<LockStatus>,
    next: LockStatus,
) -> LockStatus {
    match (current, next) {
        (Some(LockStatus::Stale), _) | (_, LockStatus::Stale) => LockStatus::Stale,
        (Some(LockStatus::Unknown), _) | (_, LockStatus::Unknown) => LockStatus::Unknown,
        _ => LockStatus::Current,
    }
}

/// Builds the user-facing message for a skipped version change.
///
/// A resolver conflict caused by another package names that package because it is the actionable
/// reason the candidate cannot land.
/// Other skips retain the message owned by their typed reason.
pub(super) fn conflict_skip_message(
    reason: SkipReason,
    offending: Option<&str>,
    changed: &str,
) -> String {
    match (reason, offending) {
        // A self-conflict keeps the generic resolver message because it has no distinct blocker.
        (SkipReason::ResolverConflict, Some(offending)) if offending != changed => {
            format!("held: conflicts with {offending}")
        }
        // The adapter normally supplies the peer's verbatim range, making this a fallback.
        (SkipReason::PeerHeld, Some(offending)) => {
            format!("held: {offending} declares an incompatible peer range")
        }
        // Name the stuck transitive so the user knows what to baseline or wait out.
        (SkipReason::TransitiveInCooldown, Some(offending)) => {
            format!("{} ({offending})", reason.message())
        }
        _ => reason.message().to_string(),
    }
}

pub(super) fn plan_item(
    change: &Change,
    project: &str,
    tool: &str,
    applied: bool,
    skipped: Option<SkippedInfo>,
) -> UpgradeItem {
    UpgradeItem {
        name: change.package.name.clone(),
        tool: tool.to_string(),
        project: project.to_string(),
        direct: change.direct,
        downgrade: change.downgrade,
        members: change.members.clone(),
        registry: change.package.registry.clone(),
        from: change.from.to_string(),
        to: change.to.to_string(),
        kind: change.kind,
        applied,
        skipped,
        error: None,
        edge: None,
    }
}
