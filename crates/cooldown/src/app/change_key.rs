//! The identity of one [`Change`] for landed and held bookkeeping and report provenance.
//!
//! The upgrade executor decides "did my planned change land?" and the resilient-apply recovery
//! decides "which candidate is held?" with the **same** key, so the two can never disagree on
//! what counts as one change.
//! A change recovery drops resurfaces as a held row, never hidden behind an accepted sibling,
//! unless the committed subset itself moved it onto its target.
//! The executor also keys security evidence, advisory notes, and fallback targets by it.
//!
//! The key includes the version a change moves *from*.
//! Coexisting copies of one package converging on one target are distinct changes: a hold on one
//! copy must never hide or borrow another copy's landing, and an advisory affecting one copy's
//! current version must never label the other copy's row.

use crate::app::UpgradeItem;
use cooldown_core::{Change, MemberRef, PackageId};

/// The identity of one change: its package, the versions it moves from and to, and, for a direct
/// change, the workspace members that declare it.
///
/// Two members upgrading the same crate to the same target from different current versions are
/// distinct direct changes, and so are two copies of one package converging on one target.
/// Transitive members are attribution context, not separate editable targets, so only a direct
/// change's members are part of its identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ChangeKey {
    name: String,
    registry: Option<String>,
    from: String,
    to: String,
    /// Sorted and deduplicated, so the order an adapter lists members in never splits a change.
    members: Vec<MemberKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct MemberKey {
    name: String,
    path: String,
}

/// The fields of a change or report row that make up its [`ChangeKey`].
struct KeyParts<'a> {
    name: &'a str,
    registry: Option<&'a str>,
    from: &'a str,
    to: &'a str,
    direct: bool,
    members: &'a [MemberRef],
}

impl ChangeKey {
    pub(crate) fn of(change: &Change) -> Self {
        Self::from_parts(&KeyParts {
            name: &change.package.name,
            registry: change.package.registry.as_deref(),
            from: change.from.as_str(),
            to: change.to.as_str(),
            direct: change.direct,
            members: &change.members,
        })
    }

    /// The key of the version change a report row describes.
    pub(crate) fn of_item(item: &UpgradeItem) -> Self {
        Self::from_parts(&KeyParts {
            name: &item.name,
            registry: item.registry.as_deref(),
            from: &item.from,
            to: &item.to,
            direct: item.direct,
            members: &item.members,
        })
    }

    /// The key with the declaring members left out, which matches a direct change against an
    /// adapter's collateral row for the same move: that row is transitive and unattributed, so its
    /// key never carries members.
    pub(crate) fn member_blind(mut self) -> Self {
        self.members.clear();
        self
    }

    /// The key of the same move reported under `package`'s name.
    pub(crate) fn reported_as(mut self, package: &PackageId) -> Self {
        self.name.clone_from(&package.name);
        self.registry.clone_from(&package.registry);
        self
    }

    fn from_parts(parts: &KeyParts<'_>) -> Self {
        let mut members: Vec<MemberKey> = if parts.direct {
            parts
                .members
                .iter()
                .map(|member| MemberKey {
                    name: member.name.clone(),
                    path: member.path.clone(),
                })
                .collect()
        } else {
            Vec::new()
        };
        members.sort();
        members.dedup();
        ChangeKey {
            name: parts.name.to_string(),
            registry: parts.registry.map(str::to_string),
            from: parts.from.to_string(),
            to: parts.to.to_string(),
            members,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::{PackageId, ToolId, UpdateKind, Version};

    fn change(from: &str, direct: bool, members: &[(&str, &str)]) -> Change {
        Change {
            package: PackageId {
                tool: ToolId("cargo"),
                name: "widget".to_string(),
                registry: None,
            },
            from: Version::new(from),
            to: Version::new("2.0.0"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct,
            members: members
                .iter()
                .map(|(name, path)| MemberRef {
                    name: (*name).to_string(),
                    path: (*path).to_string(),
                })
                .collect(),
        }
    }

    /// Two coexisting copies of one transitive converging on the same target are two changes, so a
    /// hold on one copy can never be read as the other's landing.
    #[test]
    fn copies_converging_on_one_target_are_distinct_changes() {
        let (held, landed) = (change("1.0.0", false, &[]), change("1.5.0", false, &[]));

        assert_ne!(ChangeKey::of(&held), ChangeKey::of(&landed));
    }

    /// Members split direct changes, in any listed order, but never transitive ones, whose members
    /// only attribute the copy.
    #[test]
    fn only_a_direct_change_is_split_by_its_declaring_members() {
        let (a, b) = (("app-a", "crates/app-a"), ("app-b", "crates/app-b"));

        // Direct
        assert_ne!(
            ChangeKey::of(&change("1.0.0", true, &[a])),
            ChangeKey::of(&change("1.0.0", true, &[b]))
        );
        assert_eq!(
            ChangeKey::of(&change("1.0.0", true, &[a, b])),
            ChangeKey::of(&change("1.0.0", true, &[b, a, b]))
        );

        // Transitive
        assert_eq!(
            ChangeKey::of(&change("1.0.0", false, &[a])),
            ChangeKey::of(&change("1.0.0", false, &[b]))
        );

        // Member-blind
        assert_eq!(
            ChangeKey::of(&change("1.0.0", true, &[a])).member_blind(),
            ChangeKey::of(&change("1.0.0", false, &[]))
        );
    }
}
