//! [`EdgePolicy::Preserve`](cooldown_core::EdgePolicy::Preserve) keeps an upgrade scoped to the
//! relationships it reports.
//! It restores an eligible edge the re-resolve rebound between two *still-coexisting* versions to
//! its pre-apply binding — the churn cargo's incremental resolver introduces when a wide declared
//! range (diesel's `uuid = ">=0.7, <2.0"`) admits several locked versions and preference order
//! happens to land on a different one than last time.
//!
//! A binding change that reflects a *planned* move is never restored, structurally.
//! A same-major move replaces the old version in the lock, so the existence guard skips its absent
//! earlier target.
//! A cross-major move only lands after the manifest requirement was widened, so the requirement
//! guard rejects its earlier target.
//! What remains is exactly the gratuitous rebinding between versions that both still exist and
//! still satisfy the declared range.

use super::{EdgeRewrite, LockEdgeView, RequirementIndex};

/// Whether the lock pair contains an addressable crates.io rebind that might need restoration.
///
/// This source-only preflight deliberately does not decide whether the earlier target still
/// satisfies the declared requirement; metadata is fetched only after a potential move exists.
pub(crate) fn has_potential_restoration(before: &LockEdgeView, after: &LockEdgeView) -> bool {
    after.bindings().any(|binding| {
        let Some(earlier) = before.binding(binding.dependent, binding.dependency) else {
            return false;
        };
        earlier != binding.bound
            && after
                .dependency_source(binding.dependency, binding.bound)
                .as_deref()
                == Some(crate::cargocmd::CRATES_IO_SOURCE)
            && after
                .dependency_source(binding.dependency, earlier)
                .as_deref()
                == Some(crate::cargocmd::CRATES_IO_SOURCE)
    })
}

/// The corrective rewrites that restore churned bindings of `after` back to their `before` state.
///
/// Ambiguous pairs are already absent from the views.
/// The orphan guard is applied by the caller via [`guard_rewrites`](super::guard_rewrites).
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
            // Preserve never crosses package sources.
            // The metadata requirement does not encode the source constraint, so both the
            // resolver-produced and restored endpoints must be positively identified as crates.io
            // packages by the lock itself.
            if after
                .dependency_source(binding.dependency, binding.bound)
                .as_deref()
                != Some(crate::cargocmd::CRATES_IO_SOURCE)
            {
                return None;
            }
            // The pre-apply binding must still be a locked crates.io version — a vanished version
            // means the slot legitimately moved (a planned or collateral version change), not churn.
            if after
                .dependency_source(binding.dependency, earlier)
                .as_deref()
                != Some(crate::cargocmd::CRATES_IO_SOURCE)
            {
                return None;
            }
            // And it must still satisfy every requirement the dependent declares (post-widen
            // metadata): a cross-major planned move changed the requirement, and restoring the old
            // binding would violate it.
            if !requirements.admits(
                binding.dependent,
                binding.dependency,
                binding.bound,
                earlier,
            ) {
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
    use crate::cargocmd::{CRATES_IO_SOURCE, DeclaredRequirement, LockPackageId, ResolvedGraph};
    use indoc::indoc;
    use std::collections::{HashMap, HashSet};

    fn graph_with(requirements: &[(&str, &str, &str, &str)]) -> ResolvedGraph {
        let mut declared_requirements: HashMap<LockPackageId, Vec<DeclaredRequirement>> =
            HashMap::new();
        for (name, version, dependency, requirement) in requirements {
            for resolved_version in ["0.8.2", "1.24.0"] {
                declared_requirements
                    .entry(LockPackageId::new(
                        *name,
                        *version,
                        (*name != "app").then_some(CRATES_IO_SOURCE),
                    ))
                    .or_default()
                    .push(DeclaredRequirement {
                        dependency: (*dependency).to_string(),
                        resolved: LockPackageId::new(
                            *dependency,
                            resolved_version,
                            Some(CRATES_IO_SOURCE),
                        ),
                        requirement: (*requirement).to_string(),
                    });
            }
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
    /// the apply; the re-resolve collapsed it onto the coexisting 0.8.2.
    /// Preserve restores it.
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

    #[test]
    fn a_source_change_is_not_restored_as_a_crates_io_rebind() {
        let before_text = indoc::indoc! {r#"
            version = 4

            [[package]]
            name = "app"
            version = "0.1.0"
            dependencies = [
             "foo 1.0.0",
            ]

            [[package]]
            name = "foo"
            version = "0.9.0"
            source = "git+https://example.com/foo#abcdef"

            [[package]]
            name = "foo"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#};
        let after_text = before_text.replace("\"foo 1.0.0\",", "\"foo 0.9.0\",");
        let graph = graph_with(&[("app", "0.1.0", "foo", ">=0.9, <2")]);

        assert!(
            restorations(
                &view(before_text),
                &view(&after_text),
                &RequirementIndex::new(&graph),
            )
            .is_empty(),
            "a source-changing move is outside the crates.io edge policy"
        );
    }

    #[test]
    fn git_dependent_joins_metadata_and_lock_source_spellings() {
        let before = indoc! {r#"
            version = 4

            [[package]]
            name = "dep"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "dep"
            version = "1.1.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "ort"
            version = "2.0.0"
            source = "git+https://example.com/ort?branch=chore%2Fort-rc-12#abcdef"
            dependencies = [
             "dep 1.0.0",
            ]
        "#};
        let after = before.replace("\"dep 1.0.0\",", "\"dep 1.1.0\",");
        let graph = crate::cargocmd::Cargo::build_graph_from_json(indoc! {r#"
            {
                "packages": [
                    {
                        "id": "git+https://example.com/ort?branch=chore%2Fort-rc-12#abcdef",
                        "name": "ort",
                        "version": "2.0.0",
                        "source": "git+https://example.com/ort?branch=chore/ort-rc-12#abcdef",
                        "dependencies": [{"name": "dep", "req": "^1"}]
                    },
                    {
                        "id": "dep 1.1.0 (registry+https://github.com/rust-lang/crates.io-index)",
                        "name": "dep",
                        "version": "1.1.0",
                        "source": "registry+https://github.com/rust-lang/crates.io-index",
                        "dependencies": []
                    }
                ],
                "workspace_members": [],
                "workspace_root": "/workspace",
                "resolve": {"nodes": [{
                    "id": "git+https://example.com/ort?branch=chore%2Fort-rc-12#abcdef",
                    "deps": [{
                        "name": "dep",
                        "pkg": "dep 1.1.0 (registry+https://github.com/rust-lang/crates.io-index)"
                    }]
                }]}
            }
        "#});

        let rewrites = restorations(&view(before), &view(&after), &RequirementIndex::new(&graph));

        assert_eq!(rewrites.len(), 1);
        assert_eq!(
            rewrites[0].dependent.source(),
            Some("git+https://example.com/ort?branch=chore%2Fort-rc-12#abcdef")
        );
        assert_eq!(
            (rewrites[0].from.as_str(), rewrites[0].to.as_str()),
            ("1.1.0", "1.0.0")
        );
    }
}
