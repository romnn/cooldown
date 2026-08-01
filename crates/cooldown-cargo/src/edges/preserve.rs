//! [`EdgePolicy::Preserve`](cooldown_core::EdgePolicy::Preserve): an upgrade touches only what it
//! reports. Any edge the re-resolve rebound between two *still-coexisting* versions is restored to
//! its pre-apply binding — the churn cargo's incremental resolver introduces when a wide declared
//! range (diesel's `uuid = ">=0.7, <2.0"`) admits several locked versions and preference order
//! happens to land on a different one than last time.
//!
//! A binding change that reflects a *planned* move is never restored, structurally: a same-major
//! move replaces the old version in the lock (the old binding target no longer exists, so the
//! existence guard skips it), and a cross-major move only lands after the manifest requirement was
//! widened (the old binding no longer satisfies the post-widen requirement, so the requirement
//! guard skips it). What remains is exactly the gratuitous rebinding between versions that both
//! still exist and both still satisfy the declared range.

use super::{EdgeRewrite, LockEdgeView, RequirementIndex};

/// The corrective rewrites that restore churned bindings of `after` back to their `before` state.
/// Ambiguous pairs are already absent from the views; the orphan guard is applied by the caller
/// via [`guard_rewrites`](super::guard_rewrites).
pub(crate) fn restorations(
    before: &LockEdgeView,
    after: &LockEdgeView,
    requirements: &RequirementIndex<'_>,
) -> Vec<EdgeRewrite> {
    after
        .bindings()
        .filter_map(|binding| {
            let earlier = before.binding(binding.dependent, binding.dependency)?;
            if earlier == binding.bound {
                return None;
            }
            // The pre-apply binding must still be a locked crates.io version — a vanished version
            // means the slot legitimately moved (a planned or collateral version change), not churn.
            if !after.has_crates_io_version(binding.dependency, earlier) {
                return None;
            }
            // And it must still satisfy every requirement the dependent declares (post-widen
            // metadata): a cross-major planned move changed the requirement, and restoring the old
            // binding would violate it.
            if !requirements.admits(binding.dependent, binding.dependency, earlier) {
                return None;
            }
            Some(EdgeRewrite {
                dependent: binding.dependent.clone(),
                dependency: binding.dependency.to_string(),
                from: binding.bound.to_string(),
                to: earlier.to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CHURNED_LOCK, key, view};
    use super::*;
    use crate::cargocmd::{DeclaredRequirement, PackageKey, ResolvedGraph};
    use std::collections::{HashMap, HashSet};

    fn graph_with(requirements: &[(&str, &str, &str, &str)]) -> ResolvedGraph {
        let mut declared_requirements: HashMap<PackageKey, Vec<DeclaredRequirement>> =
            HashMap::new();
        for (name, version, dependency, requirement) in requirements {
            declared_requirements
                .entry(PackageKey::new(*name, *version))
                .or_default()
                .push(DeclaredRequirement {
                    dependency: (*dependency).to_string(),
                    requirement: (*requirement).to_string(),
                });
        }
        ResolvedGraph {
            packages: HashMap::new(),
            roots: HashSet::new(),
            edges: HashMap::new(),
            exact_pins: HashSet::new(),
            graph_ceilings: HashSet::new(),
            ceiling_requirers: HashMap::new(),
            graph_floors: HashMap::new(),
            declared_bounds: HashMap::new(),
            declared_requirements,
            rust_versions: HashMap::new(),
            workspace_rust_version: None,
        }
    }

    /// The luup regression shape: diesel's wide `uuid >=0.7,<2.0` edge was bound to 1.24.0 before
    /// the apply; the re-resolve collapsed it onto the coexisting 0.8.2. Preserve restores it.
    #[test]
    fn a_churned_wide_range_binding_is_restored() {
        let after = view(CHURNED_LOCK);
        let before_text = CHURNED_LOCK.replace(
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
        );
        assert_ne!(before_text, CHURNED_LOCK);
        let before = view(&before_text);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);

        let rewrites = restorations(&before, &after, &RequirementIndex::new(&graph));

        assert_eq!(
            rewrites,
            vec![EdgeRewrite {
                dependent: key("diesel", "2.3.11"),
                dependency: "uuid".to_string(),
                from: "0.8.2".to_string(),
                to: "1.24.0".to_string(),
            }]
        );
    }

    #[test]
    fn an_unchanged_binding_is_left_alone() {
        let before = view(CHURNED_LOCK);
        let after = view(CHURNED_LOCK);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);
        assert!(restorations(&before, &after, &RequirementIndex::new(&graph)).is_empty());
    }

    /// A planned same-major move (`uuid 1.20.0` → `1.24.0`) replaces the version in the lock: the
    /// old binding target no longer exists afterwards, so the edge that followed the slot must NOT
    /// be "restored".
    #[test]
    fn a_planned_slot_move_is_not_restored() {
        // Before: diesel bound to the 1.x line at 1.20.0 (and the 1.x package is 1.20.0).
        let before_text = CHURNED_LOCK
            .replace(
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
            )
            .replace("1.24.0", "1.20.0");
        let before = view(&before_text);
        // After: the planned move replaced 1.20.0 with 1.24.0 and diesel's edge followed.
        let after_text = CHURNED_LOCK.replace(
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
        );
        let after = view(&after_text);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);

        // diesel's binding moved 1.20.0 → 1.24.0, but 1.20.0 is gone from the lock: not churn.
        assert!(restorations(&before, &after, &RequirementIndex::new(&graph)).is_empty());
    }

    /// A cross-major planned move rewrote the manifest requirement; the old binding still exists
    /// (another consumer keeps it) but no longer satisfies the requirement — not churn either.
    #[test]
    fn a_widened_requirement_blocks_restoration() {
        let before_text = CHURNED_LOCK.replace(
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
        );
        // Before: diesel bound 0.8.2; after: bound 1.24.0; requirement now demands ^1.
        let before = view(CHURNED_LOCK);
        let after = view(&before_text);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", "^1")]);

        assert!(restorations(&before, &after, &RequirementIndex::new(&graph)).is_empty());
    }

    /// No declared requirement is known for the edge (metadata gap): no license to rewrite.
    #[test]
    fn an_unknown_requirement_blocks_restoration() {
        let after = view(CHURNED_LOCK);
        let before_text = CHURNED_LOCK.replace(
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
        );
        let before = view(&before_text);
        let graph = graph_with(&[]);

        assert!(restorations(&before, &after, &RequirementIndex::new(&graph)).is_empty());
    }
}
