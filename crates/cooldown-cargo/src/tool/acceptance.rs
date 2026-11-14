//! The acceptance rule every lock-changing resolve of an apply keeps.
//!
//! Cargo moves more than it is asked to: a precise pin may float a dependency of its target to the
//! newest version the requirements admit, and an unlocked resolve may re-lock whatever a manifest
//! edit touched.
//! Each such step is judged against the lock it started from, and a step that breaks the rule is
//! refused and rolled back by the attempt that ran it:
//!
//! 1. Every declaration of a local package that is still in the lock keeps binding its exact
//!    node, unless it binds the node a planned change moves from, in which case it may follow that
//!    change to its target.
//! 2. Every planned target already reached stays reached.
//! 3. Every planned crate not yet reached stays at its original version or reaches its target.
//! 4. No crate an authored package declares gains a split it did not have, where requirers that
//!    shared a compatibility line no longer share one.
//!
//! A declared crate moved outside the plan is never gated as a planned direct change is, and a
//! planned crate drifting past its target can land a version younger than the cooldown permits.
//! A local package the step dropped from the lock, such as a path-patched crate the upgraded
//! graph no longer reaches, takes its declarations with it; workspace members always stay.

use super::{compatibility_line, new_direct_lines, reached};
use crate::index::CRATES_IO;
use crate::lockfile::{
    CRATES_IO_SOURCE, CargoLock, DeclaredEndpoint, DeclaredEndpoints, LockPackage, LockPackageId,
    LockedSlots, PackageKey, SlotKey,
};
use crate::version;
use cooldown_core::{Change, GeneratedMembers};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;

/// The lock one resolve step starts from, captured before the step edits any lock or manifest.
pub(super) struct ResolveBaseline {
    lock: CargoLock,
    declared: DeclaredEndpoints,
    slots: LockedSlots,
    versions: BTreeMap<SlotKey, String>,
}

impl ResolveBaseline {
    /// Captures the baseline of a step about to change `lock`.
    pub(super) fn capture(lock: &CargoLock, generated: &GeneratedMembers) -> Self {
        let slots = lock.crates_io_locked_slots();
        ResolveBaseline {
            lock: lock.clone(),
            declared: lock.declared_endpoints(generated),
            versions: lock.crates_io_locked_versions(),
            slots,
        }
    }

    /// Whether an authored package declares the crates.io node `name@version`.
    pub(super) fn declares_crates_io(&self, name: &str, version: &str) -> bool {
        self.declared.declares(name, version, CRATES_IO_SOURCE)
    }

    /// The first way the step that produced `after` breaks the rule for `plan`, or `None` when the
    /// step may be kept.
    pub(super) fn violation(
        &self,
        after: &CargoLock,
        generated: &GeneratedMembers,
        plan: &[Change],
    ) -> Option<Violation> {
        self.violations(after, generated, plan).next()
    }

    /// Every way the step that produced `after` breaks the rule for `plan`, first to last.
    ///
    /// The order is fixed: moved declarations in declaration order, then planned crates in plan
    /// order, then splits in identity order, so the first item is [`Self::violation`].
    /// The planned and split checks run only when the iterator reaches them.
    pub(super) fn violations<'s>(
        &'s self,
        after: &'s CargoLock,
        generated: &'s GeneratedMembers,
        plan: &'s [Change],
    ) -> impl Iterator<Item = Violation> + 's {
        self.declared_violations(after, generated, plan)
            .chain(std::iter::once(()).flat_map(move |()| self.planned_violations(after, plan)))
            .chain(std::iter::once(()).flat_map(move |()| self.split_violations(after, generated)))
    }

    /// Each crate an authored package declares that the step split, judged as the final guard
    /// judges the whole apply.
    ///
    /// Refusing the step holds only the candidate that split the crate, where the final guard
    /// could only refuse the apply as a whole.
    fn split_violations<'s>(
        &'s self,
        after: &'s CargoLock,
        generated: &'s GeneratedMembers,
    ) -> impl Iterator<Item = Violation> + 's {
        let declared = after.declared_endpoints(generated);
        new_direct_lines(Some(&self.lock), after, generated)
            .into_iter()
            .filter(move |(source, name)| {
                declared.iter().any(|declared| {
                    declared.endpoint.name == *name
                        && declared.endpoint.source() == source.as_deref()
                })
            })
            .map(move |identity| self.split(after, generated, &identity))
    }

    fn split(
        &self,
        after: &CargoLock,
        generated: &GeneratedMembers,
        identity: &(Option<String>, String),
    ) -> Violation {
        let (source, name) = identity;
        let of_identity = |package: &LockPackage| {
            package.name == *name && package.source.as_deref() == source.as_deref()
        };
        let old_lines: BTreeSet<String> = self
            .lock
            .package
            .iter()
            .filter(|package| of_identity(package))
            .filter_map(|package| package.version.as_deref().map(compatibility_line))
            .collect();
        let mut versions: Vec<String> = after
            .package
            .iter()
            .filter(|package| of_identity(package))
            .filter_map(|package| package.version.clone())
            .collect();
        versions.sort_by(|left, right| version::compare(left, right));
        let mut retainers = BTreeSet::new();
        let mut movers = BTreeSet::new();
        for edge in after.edges() {
            let (Some(version), Some(requirer_version)) =
                (&edge.endpoint.version, &edge.requirer.version)
            else {
                continue;
            };
            if !of_identity(edge.endpoint)
                || (edge.requirer.source.is_none() && generated.contains(&edge.requirer.name))
            {
                continue;
            }
            if old_lines.contains(&compatibility_line(version)) {
                retainers.insert(PackageKey::new(&edge.requirer.name, requirer_version));
            } else {
                movers.insert(edge.requirer.name.clone());
            }
        }
        Violation::DirectSplit {
            name: name.clone(),
            versions,
            retainers: retainers.into_iter().collect(),
            movers: movers.into_iter().collect(),
        }
    }

    fn declared_violations<'s>(
        &'s self,
        after: &CargoLock,
        generated: &GeneratedMembers,
        plan: &'s [Change],
    ) -> impl Iterator<Item = Violation> + 's {
        let declared_after = after.declared_endpoints(generated);
        // Cargo writes only the packages its resolve reaches, so a dependent absent from the lock
        // is one the step no longer resolves.
        let resolved: HashSet<LockPackageId> =
            after.package.iter().filter_map(LockPackage::id).collect();
        self.declared.iter().filter_map(move |moved| {
            if !resolved.contains(&moved.dependent)
                || declared_after.contains(moved)
                || planned_movement(plan, moved)
                    .is_some_and(|movement| declared_after.contains(&movement))
            {
                return None;
            }
            let node = &moved.endpoint;
            // The node the same dependent now binds under that name, when the step rebound it.
            let to = declared_after
                .iter()
                .find(|candidate| {
                    candidate.dependent == moved.dependent
                        && candidate.endpoint.name == node.name
                        && !self.declared.contains(candidate)
                })
                .map(|candidate| candidate.endpoint.version.clone());
            Some(Violation::DeclaredMoved {
                name: node.name.clone(),
                from: node.version.clone(),
                to,
            })
        })
    }

    fn planned_violations<'s>(
        &'s self,
        after: &'s CargoLock,
        plan: &'s [Change],
    ) -> impl Iterator<Item = Violation> + 's {
        let versions = after.crates_io_locked_versions();
        plan.iter().filter_map(move |planned| {
            let name = &planned.package.name;
            let (from, target) = (planned.from.as_str(), planned.to.as_str());
            if reached(&self.versions, planned) {
                (!reached(&versions, planned)).then(|| Violation::TargetDisplaced {
                    name: name.clone(),
                    target: target.to_string(),
                })
            } else if self.has_crates_io(name, from)
                && !after.has_crates_io_package(name, from)
                && !reached(&versions, planned)
            {
                Some(Violation::PlannedDrifted {
                    name: name.clone(),
                    from: from.to_string(),
                    target: target.to_string(),
                    landed: drifted_version(after, planned),
                })
            } else {
                None
            }
        })
    }

    fn has_crates_io(&self, name: &str, version: &str) -> bool {
        self.slots
            .get(&(name.to_string(), version::major_key(version).0))
            .is_some_and(|versions| versions.contains(version))
    }
}

/// The binding `declared` may become under the plan: the same dependent bound to the target of the
/// planned change that moves the exact node it binds now, or `None` when no change moves it.
///
/// Only the moved node's own bindings follow the change.
/// A dependent bound to another version of the crate keeps it even when that version shares the
/// target's compatibility line, and a plan for `syn` 2 leaves a declared `syn` 1 node protected.
fn planned_movement(plan: &[Change], declared: &DeclaredEndpoint) -> Option<DeclaredEndpoint> {
    let node = &declared.endpoint;
    let source = node.source()?;
    let registry = cooldown_core::redact::source_label(source);
    plan.iter()
        .find(|change| {
            change.package.name == node.name
                && change.package.registry.as_deref().unwrap_or(CRATES_IO) == registry
                && change.from.as_str() == node.version
        })
        .map(|change| DeclaredEndpoint {
            dependent: declared.dependent.clone(),
            endpoint: LockPackageId::new(&node.name, change.to.as_str(), Some(source)),
        })
}

/// The version a drifted planned crate landed at in its source or target compatibility line.
fn drifted_version(after: &CargoLock, planned: &Change) -> Option<String> {
    let lines = [
        version::major_key(planned.from.as_str()).0,
        version::major_key(planned.to.as_str()).0,
    ];
    after
        .crates_io_versions(&planned.package.name)
        .into_iter()
        .filter(|version| lines.contains(&version::major_key(version).0))
        .max_by(|left, right| version::compare(left, right))
        .map(str::to_string)
}

/// How a resolve step breaks the acceptance rule; its text becomes the held candidate's detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Violation {
    /// A declaration of a local package still in the lock stopped binding its node without
    /// following a planned change of that node to its target.
    DeclaredMoved {
        name: String,
        from: String,
        /// The version the declaration binds afterwards, when it still binds one.
        to: Option<String>,
    },
    /// A planned crate already at its target left it.
    TargetDisplaced { name: String, target: String },
    /// A planned crate still at its original version moved without reaching its target.
    PlannedDrifted {
        name: String,
        from: String,
        target: String,
        /// The version it drifted to, when one remains in its lines.
        landed: Option<String>,
    },
    /// A crate an authored package declares gained a split: requirers that shared a
    /// compatibility line no longer share one.
    DirectSplit {
        name: String,
        /// Every version of the crate afterwards, oldest first.
        versions: Vec<String>,
        /// The requirers still binding a line the crate had before.
        retainers: Vec<PackageKey>,
        /// The requirers binding a line the step added.
        movers: Vec<String>,
    },
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::DeclaredMoved { name, from, to } => {
                write!(formatter, "would move directly declared {name} {from}")?;
                match to {
                    Some(to) => write!(formatter, " to {to}"),
                    None => write!(formatter, " out of the lock"),
                }
            }
            Violation::TargetDisplaced { name, target } => {
                write!(
                    formatter,
                    "would move planned {name} off its target {target}"
                )
            }
            Violation::PlannedDrifted {
                name,
                from,
                target,
                landed,
            } => {
                write!(formatter, "would move planned {name} {from}")?;
                if let Some(landed) = landed {
                    write!(formatter, " to {landed}")?;
                }
                write!(formatter, " instead of its target {target}")
            }
            Violation::DirectSplit {
                name,
                versions,
                retainers,
                movers: _,
            } => {
                write!(
                    formatter,
                    "would resolve direct dependency {name} at another incompatible line ({}); ",
                    versions.join(" and ")
                )?;
                if retainers.is_empty() {
                    write!(formatter, "a dependency retains the old line")?;
                } else {
                    let retainers: Vec<String> = retainers
                        .iter()
                        .map(|retainer| format!("{} {}", retainer.name, retainer.version))
                        .collect();
                    write!(
                        formatter,
                        "the old line is retained by {}",
                        retainers.join(", ")
                    )?;
                }
                write!(
                    formatter,
                    "; align the retaining dependents' requirements and compatible releases, or keep this candidate held"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolveBaseline, Violation};
    use crate::index::CRATES_IO;
    use crate::lockfile::{CargoLock, PackageKey};
    use cooldown_core::{Change, GeneratedMembers, PackageId, UpdateKind, Version};
    use indoc::formatdoc;

    /// A lock of the authored `app` with `app_dependencies` as its entries, beside the crates.io
    /// `packages` given as `(name, version)`.
    fn lock(app_dependencies: &[&str], packages: &[(&str, &str)]) -> CargoLock {
        let entries = app_dependencies
            .iter()
            .map(|entry| format!("\"{entry}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let mut content = formatdoc! {r#"
            version = 4

            [[package]]
            name = "app"
            version = "0.1.0"
            dependencies = [{entries}]
        "#};
        for (name, version) in packages {
            content.push_str(&formatdoc! {r#"

                [[package]]
                name = "{name}"
                version = "{version}"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#});
        }
        CargoLock::parse(&content).expect("lock parses")
    }

    fn planned(name: &str, from: &str, to: &str) -> Change {
        Change {
            package: PackageId::new(crate::CARGO_ID, name, Some(CRATES_IO.to_string())),
            from: Version::new(from),
            to: Version::new(to),
            kind: UpdateKind::Patch,
            downgrade: false,
            direct: false,
            members: Vec::new(),
        }
    }

    fn violation(before: &CargoLock, after: &CargoLock, plan: &[Change]) -> Option<Violation> {
        let generated = GeneratedMembers::undeclared();
        ResolveBaseline::capture(before, &generated).violation(after, &generated, plan)
    }

    /// A lock of the authored `app` and the crates.io `holder`, which binds `dep` at
    /// `holder_binds`, with `dep` at each of `versions`.
    fn shared_lock(app_binds: &str, holder_binds: &str, versions: &[&str]) -> CargoLock {
        let mut content = formatdoc! {r#"
            version = 4

            [[package]]
            name = "app"
            version = "0.1.0"
            dependencies = ["{app_binds}", "holder"]

            [[package]]
            name = "holder"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            dependencies = ["{holder_binds}"]
        "#};
        for version in versions {
            content.push_str(&formatdoc! {r#"

                [[package]]
                name = "dep"
                version = "{version}"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#});
        }
        CargoLock::parse(&content).expect("lock parses")
    }

    /// A planned move of a declared crate that leaves a dependency on the shared old line splits
    /// the crate, and the step is refused with the dependency named.
    #[test]
    fn a_step_splitting_a_declared_crate_is_refused() {
        let plan = [planned("dep", "1.0.0", "2.0.0")];
        let before = shared_lock("dep", "dep", &["1.0.0"]);
        let split = shared_lock("dep 2.0.0", "dep 1.0.0", &["1.0.0", "2.0.0"]);
        let refused = violation(&before, &split, &plan);
        assert_eq!(
            refused,
            Some(Violation::DirectSplit {
                name: "dep".to_owned(),
                versions: vec!["1.0.0".to_owned(), "2.0.0".to_owned()],
                retainers: vec![PackageKey::new("holder", "1.0.0")],
                movers: vec!["app".to_owned()],
            })
        );
        assert!(
            refused
                .map(|refused| refused.to_string())
                .is_some_and(|detail| detail.contains("the old line is retained by holder 1.0.0"))
        );

        // Moving together keeps the line shared, so the step is kept.
        let together = shared_lock("dep", "dep", &["2.0.0"]);
        assert_eq!(violation(&before, &together, &plan), None);
    }

    /// The issue #55 shape: landing the planned pest family must not drag the `psm` the
    /// workspace declares itself, while the same move of a merely transitive `psm` is collateral.
    #[test]
    fn a_declared_node_outside_the_plan_keeps_its_identity() {
        let plan = [planned("pest", "2.9.2", "2.9.3")];
        let before = lock(&["pest", "psm"], &[("pest", "2.9.2"), ("psm", "0.1.32")]);
        let after = lock(&["pest", "psm"], &[("pest", "2.9.3"), ("psm", "0.1.24")]);
        assert_eq!(
            violation(&before, &after, &plan),
            Some(Violation::DeclaredMoved {
                name: "psm".to_owned(),
                from: "0.1.32".to_owned(),
                to: Some("0.1.24".to_owned()),
            })
        );

        let transitive_before = lock(&["pest"], &[("pest", "2.9.2"), ("psm", "0.1.32")]);
        let transitive_after = lock(&["pest"], &[("pest", "2.9.3"), ("psm", "0.1.24")]);
        assert_eq!(
            violation(&transitive_before, &transitive_after, &plan),
            None
        );
    }

    /// A declared crate the plan itself moves is judged by its target, not by its identity.
    #[test]
    fn a_planned_declared_node_may_reach_its_target() {
        let plan = [planned("pest", "2.9.2", "2.9.3")];
        let before = lock(&["pest"], &[("pest", "2.9.2")]);
        let after = lock(&["pest"], &[("pest", "2.9.3")]);
        assert_eq!(violation(&before, &after, &plan), None);
    }

    /// A planned target an earlier step reached may not be displaced by a later one.
    #[test]
    fn a_reached_target_stays_reached() {
        let plan = [
            planned("alpha", "1.0.0", "1.1.0"),
            planned("beta", "1.0.0", "1.1.0"),
        ];
        let before = lock(&[], &[("alpha", "1.1.0"), ("beta", "1.0.0")]);
        let displaced = lock(&[], &[("alpha", "1.2.0"), ("beta", "1.1.0")]);
        assert_eq!(
            violation(&before, &displaced, &plan),
            Some(Violation::TargetDisplaced {
                name: "alpha".to_owned(),
                target: "1.1.0".to_owned(),
            })
        );
        let kept = lock(&[], &[("alpha", "1.1.0"), ("beta", "1.1.0")]);
        assert_eq!(violation(&before, &kept, &plan), None);
    }

    /// A planned crate still at its original version either stays there or reaches its target:
    /// `pest_derive`'s pin may not float the planned `pest` past its own target.
    #[test]
    fn an_unreached_planned_crate_stays_or_reaches_its_target() {
        let plan = [
            planned("pest", "2.9.2", "2.9.3"),
            planned("pest_derive", "2.9.2", "2.9.3"),
        ];
        let before = lock(&[], &[("pest", "2.9.2"), ("pest_derive", "2.9.2")]);
        let floated = lock(&[], &[("pest", "2.9.5"), ("pest_derive", "2.9.3")]);
        let drift = violation(&before, &floated, &plan);
        assert_eq!(
            drift,
            Some(Violation::PlannedDrifted {
                name: "pest".to_owned(),
                from: "2.9.2".to_owned(),
                target: "2.9.3".to_owned(),
                landed: Some("2.9.5".to_owned()),
            })
        );
        assert_eq!(
            drift.map(|drift| drift.to_string()).as_deref(),
            Some("would move planned pest 2.9.2 to 2.9.5 instead of its target 2.9.3")
        );

        let stayed = lock(&[], &[("pest", "2.9.2"), ("pest_derive", "2.9.3")]);
        assert_eq!(violation(&before, &stayed, &plan), None);
        let reached = lock(&[], &[("pest", "2.9.3"), ("pest_derive", "2.9.3")]);
        assert_eq!(violation(&before, &reached, &plan), None);
    }

    /// The protected set is concrete: a workspace that declares `syn` 1 while `syn` 2 is only
    /// transitive protects the 1.x node alone, and a plan for `syn` 2 does not release `syn` 1.
    #[test]
    fn a_declared_line_is_protected_apart_from_a_transitive_namesake() {
        let before = lock(&["syn 1.0.109"], &[("syn", "1.0.109"), ("syn", "2.0.50")]);
        let transitive_moved = lock(&["syn 1.0.109"], &[("syn", "1.0.109"), ("syn", "2.0.60")]);
        assert_eq!(violation(&before, &transitive_moved, &[]), None);

        let declared_moved = lock(&["syn 1.0.100"], &[("syn", "1.0.100"), ("syn", "2.0.60")]);
        let expected = Some(Violation::DeclaredMoved {
            name: "syn".to_owned(),
            from: "1.0.109".to_owned(),
            to: Some("1.0.100".to_owned()),
        });
        assert_eq!(violation(&before, &declared_moved, &[]), expected);
        assert_eq!(
            violation(
                &before,
                &declared_moved,
                &[planned("syn", "2.0.50", "2.0.60")]
            ),
            expected
        );
    }

    /// A generated member's entries mirror what the authored members reach, so a node only a
    /// generated member lists is not protected.
    #[test]
    fn a_generated_members_entries_are_not_declarations() {
        let projection = |version: &str| {
            let content = formatdoc! {r#"
                version = 4

                [[package]]
                name = "hack"
                version = "0.1.0"
                dependencies = ["dep"]

                [[package]]
                name = "dep"
                version = "{version}"
                source = "registry+https://github.com/rust-lang/crates.io-index"
            "#};
            CargoLock::parse(&content).expect("lock parses")
        };
        let generated = GeneratedMembers::declared(vec!["hack".to_owned()]);
        let baseline = ResolveBaseline::capture(&projection("1.0.0"), &generated);
        assert_eq!(
            baseline.violation(&projection("1.1.0"), &generated, &[]),
            None
        );
        let authored = GeneratedMembers::undeclared();
        let baseline = ResolveBaseline::capture(&projection("1.0.0"), &authored);
        assert!(
            baseline
                .violation(&projection("1.1.0"), &authored, &[])
                .is_some()
        );
    }
}
