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

/// The identity of a planned change for **report provenance** — security blocks and
/// advisory-rollback notes: [`change_target_key`] plus the *source* version.
///
/// The landed/held bookkeeping above is deliberately source-blind so recovery can collapse
/// siblings onto one target; provenance must not be. Two coexisting transitive copies of a
/// package converging on one target are distinct report rows, and an advisory can affect one
/// copy's current version without affecting the other's — a source-blind key would stamp the
/// one row's security evidence onto both.
pub(crate) type ChangeProvenanceKey = (String, ChangeTargetKey);

pub(crate) fn change_provenance_key(change: &Change) -> ChangeProvenanceKey {
    (change.from.as_str().to_string(), change_target_key(change))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::{PackageId, ToolId, UpdateKind, Version};

    /// Two coexisting copies of one transitive converging on the same target share the
    /// (deliberately source-blind) target key but keep distinct provenance keys, so security
    /// evidence recorded for one copy's move never labels the other's row.
    #[test]
    fn provenance_keys_distinguish_source_versions_that_target_keys_collapse() {
        let change = |from: &str| Change {
            package: PackageId {
                tool: ToolId("cargo"),
                name: "widget".to_string(),
                registry: None,
            },
            from: Version::new(from),
            to: Version::new("2.0.0"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: false,
            members: Vec::new(),
        };
        let (affected, clean) = (change("1.0.0"), change("1.5.0"));
        assert_eq!(change_target_key(&affected), change_target_key(&clean));
        assert_ne!(
            change_provenance_key(&affected),
            change_provenance_key(&clean)
        );
    }
}
