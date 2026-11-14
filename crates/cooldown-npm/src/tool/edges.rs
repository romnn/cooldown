//! Temporary override selectors that reach one copy of a name resolved at several versions.
//!
//! A `name@range` override matches every dependency request whose range intersects its own, so
//! with several copies of a name a request such as `^6 || ^7` can pull another copy's edge onto
//! the target.
//! A parent selector `parent@<version>>name` instead rewrites only the requests of the manifests
//! named `parent` whose version satisfies `<version>`, which the pre-apply lock's edges into the
//! copy enumerate exactly ([`CopyEdges::selectors`]).

use crate::lock::{BindingContext, CopyBinding};
use cooldown_core::Change;
use std::collections::{BTreeSet, HashMap};

/// The planned targets of each `(name, from)` line of a plan, direct and transitive alike.
pub(super) type PlannedTargets = HashMap<(String, String), BTreeSet<String>>;

/// One dependent package's requirement of a name, as the pre-apply lock records it.
#[derive(Debug)]
struct Requirement {
    /// The dependent's version, peer suffix stripped.
    parent_version: String,
    /// The version the requirement binds.
    bound: String,
}

/// The pre-apply lock's edges, indexed for addressing one copy through its dependents.
#[derive(Debug, Default)]
pub(super) struct CopyEdges {
    /// The edges into each `(name, version)` copy.
    into_copy: HashMap<(String, String), Vec<CopyBinding>>,
    /// Every dependent package record's requirement under each `(parent name, slot)`: the
    /// manifests a `parent@<version>>slot` selector can reach.
    by_parent_slot: HashMap<(String, String), Vec<Requirement>>,
}

impl CopyEdges {
    /// Indexes `bindings`, the pre-apply lock's [`copy_bindings`](crate::lock::NodeLock::copy_bindings).
    pub(super) fn new(bindings: Vec<CopyBinding>) -> Self {
        let mut edges = Self::default();
        for binding in bindings {
            if let BindingContext::Package { name, version, .. } = &binding.context {
                edges
                    .by_parent_slot
                    .entry((name.clone(), binding.slot.clone()))
                    .or_default()
                    .push(Requirement {
                        parent_version: version.clone(),
                        bound: binding.version.clone(),
                    });
            }
            edges
                .into_copy
                .entry((binding.name.clone(), binding.version.clone()))
                .or_default()
                .push(binding);
        }
        edges
    }

    /// The parent-selector override keys that move exactly `change`'s copy, one per dependent
    /// package version, or why no set of selectors reaches that copy alone.
    ///
    /// A selector names its parent by version, so it also matches every record of that parent a
    /// version cannot tell apart: another peer context of the same version, and a parent
    /// resolved from a tarball, git, or `file:` source, whose manifest version the lock does not
    /// record.
    /// Each such record must bind the planned copy too, or the selector would move another copy's
    /// edge.
    ///
    /// A dependent with its own planned move (`planned`) is matched at its target as well, so the
    /// requirement still reaches the copy when the dependent moves in the same resolve.
    /// A target another copy's selector could match too is left out: one already recorded with a
    /// requirement of another copy, or one another version of the dependent, binding another copy,
    /// moves to as well.
    pub(super) fn selectors(
        &self,
        change: &Change,
        planned: &PlannedTargets,
    ) -> Result<Vec<String>, String> {
        let name = change.package.name.as_str();
        let from = change.from.as_str();
        let Some(edges) = self.into_copy.get(&(name.to_string(), from.to_string())) else {
            return Err(format!("no dependent in the lock binds the {from} copy"));
        };
        let mut selectors = BTreeSet::new();
        for edge in edges {
            let (parent, parent_version) = match &edge.context {
                BindingContext::Importer(path) => {
                    return Err(format!("importer `{path}` requires it directly"));
                }
                BindingContext::Package { name, version, .. } => (name, version),
            };
            if edge.slot != name {
                return Err(format!(
                    "{parent}@{parent_version} requires it through the alias `{}`",
                    edge.slot
                ));
            }
            if !is_registry_version(parent_version) {
                return Err(format!(
                    "{parent} {parent_version} has no registry version to select it by"
                ));
            }
            let requirements = self
                .by_parent_slot
                .get(&(parent.clone(), name.to_string()))
                .map(Vec::as_slice)
                .unwrap_or_default();
            if let Some(other) = requirements.iter().find(|requirement| {
                (requirement.parent_version == *parent_version
                    || !is_registry_version(&requirement.parent_version))
                    && requirement.bound != from
            }) {
                return Err(format!(
                    "{parent}@{} binds it at {}, which a selector for {parent}@{parent_version} \
                     would move too",
                    other.parent_version, other.bound
                ));
            }
            let mut versions = vec![parent_version.clone()];
            let targets = planned
                .get(&(parent.clone(), parent_version.clone()))
                .into_iter()
                .flatten();
            for target in targets {
                // The target is taken when a record of it binds another copy, or when another
                // version of the parent binding another copy moves to it too: that copy's selector
                // would then match the same manifest.
                let taken = requirements.iter().any(|requirement| {
                    requirement.bound != from
                        && (requirement.parent_version == *target
                            || planned
                                .get(&(parent.clone(), requirement.parent_version.clone()))
                                .is_some_and(|targets| targets.contains(target)))
                });
                if is_registry_version(target) && !taken {
                    versions.push(target.clone());
                }
            }
            selectors.insert(format!("{parent}@{}>{name}", versions.join(" || ")));
        }
        Ok(selectors.into_iter().collect())
    }
}

/// The planned targets of every change in `changes`, by the `(name, from)` line it moves.
pub(super) fn planned_targets<'a>(changes: impl IntoIterator<Item = &'a Change>) -> PlannedTargets {
    let mut targets = PlannedTargets::new();
    for change in changes {
        targets
            .entry((
                change.package.name.clone(),
                change.from.as_str().to_string(),
            ))
            .or_default()
            .insert(change.to.as_str().to_string());
    }
    targets
}

/// Whether `version` is a registry version, which a selector's semver range can name: a tarball,
/// git, or `file:` source reference (`github.com/acme/p/<commit>`, `https://…`) is not.
fn is_registry_version(version: &str) -> bool {
    crate::version::parse(version).is_some()
}
