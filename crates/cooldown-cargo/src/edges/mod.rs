//! Resolved lock **edge bindings**: the shared model behind the `--cargo-edge-policy` enforcement.
//!
//! A `Cargo.lock` records more than which package versions exist: each `[[package]]` block's
//! `dependencies` array binds every edge to a concrete coexisting version (`"uuid 0.8.2"` vs
//! `"uuid 1.24.0"`). When a dependent's declared range admits several locked versions — diesel's
//! `uuid = ">=0.7.0, <2.0.0"` beside both a `0.8` and a `1.x` line — cargo's *incremental*
//! re-resolves (`cargo update -p … --precise`) can rebind such an edge between them depending on
//! graph-traversal order, while a from-scratch resolve deterministically picks the highest
//! satisfying version. The rebinding is build-affecting (`rustc` receives the other copy as
//! `--extern`) yet invisible at the per-version level, and `cargo metadata --locked` accepts either
//! binding as long as no package is orphaned — so nothing downstream catches it.
//!
//! The submodules split the pipeline: [`lock_view`] parses each lock's bindings and reference
//! structure, the policy modules ([`preserve`], [`canonicalize`]) compute corrective rewrites over
//! them, [`rewrite`] filters those rewrites through the safety guards and applies them as targeted
//! textual surgery that leaves the rest of the lock byte-identical, and [`observe`] diffs the final
//! bindings per `[[package]]` block so a rebind that no policy corrected is still reported.

pub(crate) mod canonicalize;
pub(crate) mod enforce;
mod lock_view;
mod observe;
pub(crate) mod preserve;
mod rewrite;

pub(crate) use lock_view::LockEdgeView;
pub(crate) use observe::binding_changes;
pub(crate) use rewrite::{guard_rewrites, rewrite_lock_text};

use crate::cargocmd::{CRATES_IO_SOURCE, ResolvedGraph};
use crate::version;

pub(crate) use crate::cargocmd::{LockPackageId, PackageKey};

/// One corrective binding rewrite a policy proposes: rebind `dependent`'s edge to `dependency`
/// from its current bound version to `to`. `from` is the binding as the (post-apply) lock has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EdgeRewrite {
    /// The dependent package whose edge is rewritten.
    pub(crate) dependent: LockPackageId,
    /// The depended-on crate name.
    pub(crate) dependency: String,
    /// The bound version being replaced (as present in the lock the rewrite applies to).
    pub(crate) from: String,
    /// The bound version to bind instead.
    pub(crate) to: String,
}

/// A binding that differs between two locks for a `[[package]]` block present in both — the raw
/// observation the report surfaces when no policy corrected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BindingChange {
    /// The dependent package whose edge moved.
    pub(crate) dependent: LockPackageId,
    /// The depended-on crate name.
    pub(crate) dependency: String,
    /// The complete binding target in the earlier lock.
    pub(crate) before: LockPackageId,
    /// The complete binding target in the later lock.
    pub(crate) after: LockPackageId,
    /// Extra context when versions alone understate the source-bearing move.
    pub(crate) detail: Option<String>,
}

/// The declared version requirements each locked package imposes on its dependencies, read from
/// `cargo metadata` (so a mid-apply manifest widening is already reflected).
pub(crate) struct RequirementIndex<'a> {
    graph: &'a ResolvedGraph,
}

impl<'a> RequirementIndex<'a> {
    pub(crate) fn new(graph: &'a ResolvedGraph) -> Self {
        RequirementIndex { graph }
    }

    /// Whether every active requirement mapped to the current lock edge admits `candidate`.
    /// A dependent or concrete edge the metadata does not know yields `false` because no known
    /// requirement means no license to rewrite.
    pub(crate) fn admits(
        &self,
        dependent: &LockPackageId,
        dependency: &str,
        current: &str,
        candidate: &str,
    ) -> bool {
        let Some(requirements) = self.graph.declared_requirements.get(dependent) else {
            return false;
        };
        let target = LockPackageId::new(dependency, current, Some(CRATES_IO_SOURCE));
        let mut matched = false;
        for requirement in requirements.iter().filter(|requirement| {
            requirement.dependency == dependency && requirement.resolved == target
        }) {
            matched = true;
            if !version::version_in_range(&requirement.requirement, candidate) {
                return false;
            }
        }
        matched
    }

    /// Whether metadata identifies an active declaration for this concrete lock edge.
    pub(crate) fn identifies(
        &self,
        dependent: &LockPackageId,
        dependency: &str,
        current: &str,
    ) -> bool {
        let target = LockPackageId::new(dependency, current, Some(CRATES_IO_SOURCE));
        self.graph
            .declared_requirements
            .get(dependent)
            .is_some_and(|requirements| {
                requirements.iter().any(|requirement| {
                    requirement.dependency == dependency && requirement.resolved == target
                })
            })
    }

    /// Whether binding `dependency` at `candidate` respects the workspace's declared MSRV: a
    /// candidate whose own `rust-version` exceeds the workspace minimum is not *preferred*. This is
    /// the compatibility tier of cargo's `incompatible-rust-versions = "fallback"` rule (the
    /// resolver-v3 default); [`canonicalize`] applies the fallback tier itself — when no compatible
    /// candidate satisfies the requirement, the highest satisfying one is used regardless. A
    /// candidate declaring no `rust-version` imposes nothing, and a workspace without one disables
    /// the rule entirely.
    pub(crate) fn msrv_admits(&self, dependency: &str, candidate: &str) -> bool {
        let Some(workspace) = self.graph.workspace_rust_version else {
            return true;
        };
        match self.graph.rust_versions.get(&LockPackageId::new(
            dependency,
            candidate,
            Some(CRATES_IO_SOURCE),
        )) {
            Some(required) => *required <= workspace,
            None => true,
        }
    }
}

/// A policy-proposed rewrite a safety guard withheld, with the reason — reported as a
/// [`Held`](cooldown_core::EdgeBindingAction::Held) row so the withheld correction is never silent.
pub(crate) struct RejectedRewrite {
    /// The withheld rewrite (its `from` binding stays in the lock).
    pub(crate) rewrite: EdgeRewrite,
    /// Why the guard withheld it.
    pub(crate) reason: String,
}

/// The guard verdict over a policy's proposed rewrites: what may be applied and what was withheld.
pub(crate) struct GuardedRewrites {
    /// Rewrites that passed every guard, in deterministic order.
    pub(crate) accepted: Vec<EdgeRewrite>,
    /// Rewrites a guard withheld, each with its reason.
    pub(crate) rejected: Vec<RejectedRewrite>,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::lock_view::LockEdgeView;
    use super::{LockPackageId, PackageKey};
    use crate::cargocmd::CRATES_IO_SOURCE;
    use crate::lockfile::CargoLock;
    use indoc::indoc;

    pub(crate) const CHURNED_LOCK: &str = indoc! {r#"
        version = 4

        [[package]]
        name = "app"
        version = "0.1.0"
        dependencies = [
         "diesel",
         "uuid 0.8.2",
         "uuid 1.24.0",
        ]

        [[package]]
        name = "diesel"
        version = "2.3.11"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "aa"
        dependencies = [
         "itoa",
         "uuid 0.8.2",
        ]

        [[package]]
        name = "itoa"
        version = "1.0.11"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "bb"

        [[package]]
        name = "uuid"
        version = "0.8.2"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "cc"

        [[package]]
        name = "uuid"
        version = "1.24.0"
        source = "registry+https://github.com/rust-lang/crates.io-index"
        checksum = "dd"
    "#};

    pub(crate) fn view(lock_text: &str) -> LockEdgeView {
        LockEdgeView::from_lock(&CargoLock::parse(lock_text).expect("lock parses"))
    }

    pub(crate) fn key(name: &str, version: &str) -> LockPackageId {
        LockPackageId::new(name, version, Some(CRATES_IO_SOURCE))
    }

    pub(crate) fn path_key(name: &str, version: &str) -> LockPackageId {
        LockPackageId::new(name, version, None::<String>)
    }

    pub(crate) fn package_key(name: &str, version: &str) -> PackageKey {
        PackageKey::new(name, version)
    }
}
