//! The identity of a planned [`Change`] for landed/held bookkeeping. The upgrade executor decides
//! "did my planned change land?" and the resilient-apply recovery decides "which sibling is held?"
//! with the **same** key, so the two can never disagree on what counts as one change — a change
//! recovery drops must always resurface as a held row, never vanish behind an accepted sibling.

use cooldown_core::{Change, MemberRef};

pub(crate) type MemberTargetKey = (String, String);
pub(crate) type ChangeTargetKey = (String, Option<String>, String, Vec<MemberTargetKey>);

pub(crate) fn change_target_key(change: &Change) -> ChangeTargetKey {
    change_target_key_parts(
        &change.package.name,
        change.package.registry.as_deref(),
        change.to.as_str(),
        change.direct,
        &change.members,
    )
}

/// [`change_target_key`] over borrowed parts, for callers holding a report row (e.g. an
/// `UpgradeItem`) rather than a [`Change`] value.
pub(crate) fn change_target_key_parts(
    name: &str,
    registry: Option<&str>,
    target: &str,
    direct: bool,
    source_members: &[MemberRef],
) -> ChangeTargetKey {
    // Two members upgrading the same crate to the same target from different current versions are
    // distinct direct changes that share `(name, registry, to)`. Keying them member-blind lets the
    // member-aware `target_reached` collapse them, masking a held member behind an applied one or
    // recording the held one as both applied and skipped. Transitive members are attribution context,
    // not separate editable targets, so only direct changes include members in the key.
    let mut members: Vec<MemberTargetKey> = if direct {
        source_members.iter().map(member_key).collect()
    } else {
        Vec::new()
    };
    members.sort();
    members.dedup();
    (
        name.to_string(),
        registry.map(str::to_string),
        target.to_string(),
        members,
    )
}

fn member_key(member: &MemberRef) -> MemberTargetKey {
    (member.name.clone(), member.path.clone())
}
