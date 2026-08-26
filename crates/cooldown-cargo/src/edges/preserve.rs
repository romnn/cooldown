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
use crate::version;

/// Whether the lock pair contains an addressable crates.io rebind that might need restoration.
///
/// This source-only preflight deliberately does not decide whether the earlier target still
/// satisfies the declared requirement; metadata is fetched only after a potential move exists.
/// Beside the both-versions-coexist churn shape, it also admits the vanished-target shape
/// [`line_successor`] restores: the earlier binding is gone from the later lock while the edge
/// landed on a different release line that still coexists with the vanished line's successor.
pub(crate) fn has_potential_restoration(before: &LockEdgeView, after: &LockEdgeView) -> bool {
    after.bindings().any(|binding| {
        let Some(earlier) = before.binding(binding.dependent, binding.dependency) else {
            return false;
        };
        if earlier == binding.bound
            || after
                .dependency_source(binding.dependency, binding.bound)
                .as_deref()
                != Some(crate::cargocmd::CRATES_IO_SOURCE)
        {
            return false;
        }
        if after
            .dependency_source(binding.dependency, earlier)
            .as_deref()
            == Some(crate::cargocmd::CRATES_IO_SOURCE)
        {
            return true;
        }
        line_successor(before, after, binding.dependency, earlier, binding.bound).is_some()
    })
}

/// The unique surviving same-line successor of a vanished binding target, when the edge crossed
/// release lines.
///
/// `earlier` (the before-lock binding) must have been a crates.io version and be absent from
/// `after`; the successor is the one crates.io version in `after` sharing `earlier`'s
/// compatibility line ([`version::major_key`]) while `bound` (the post-apply binding) sits on a
/// different line. A genuine replacement is new to the after-lock — it could not have coexisted
/// with `earlier` on the same cargo line — so a candidate already present in the before-lock is
/// rejected as a bystander. That guard carries the `0.0.x` corner: [`version::major_key`] maps
/// every `0.0.z` to the one key `"0.0"` although cargo treats each `0.0.z` as its own line and
/// may lock several side by side, so a pre-existing `0.0.z` node (kept alive by some other
/// dependent) could otherwise masquerade as the successor. More than one surviving candidate
/// (again only possible under `"0.0"`) is ambiguous, and restoration abstains.
fn line_successor(
    before: &LockEdgeView,
    after: &LockEdgeView,
    dependency: &str,
    earlier: &str,
    bound: &str,
) -> Option<String> {
    if before.dependency_source(dependency, earlier).as_deref()
        != Some(crate::cargocmd::CRATES_IO_SOURCE)
    {
        return None;
    }
    let earlier_line = version::major_key(earlier);
    if version::major_key(bound) == earlier_line {
        // The edge stayed on the vanished target's own line: that is the planned move landing,
        // not a cross-line collapse.
        return None;
    }
    let mut successors = after
        .crates_io_versions(dependency)
        .filter(|candidate| version::major_key(candidate) == earlier_line)
        .filter(|candidate| {
            !before
                .crates_io_versions(dependency)
                .any(|existing| existing == *candidate)
        });
    let successor = successors.next()?;
    if successors.next().is_some() {
        return None;
    }
    Some(successor.to_string())
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
            // means the slot legitimately moved (a planned or collateral version change), not
            // churn. But the slot's move does not license a *line* change: when several versions
            // of the name still coexist, cargo's incremental resolver can land the dependent's
            // edge on a different release line than the vanished target's replacement (diesel's
            // `uuid >=0.7, <2` rebound from the vanished `1.25.0` onto the surviving `0.8.2`
            // while `1.24.0` took the slot). Rebinding to the unique same-line successor keeps
            // the downgrade line-continuous; the requirement guard below still applies.
            if after
                .dependency_source(binding.dependency, earlier)
                .as_deref()
                != Some(crate::cargocmd::CRATES_IO_SOURCE)
            {
                let successor =
                    line_successor(before, after, binding.dependency, earlier, binding.bound)?;
                if !requirements.admits(
                    binding.dependent,
                    binding.dependency,
                    binding.bound,
                    &successor,
                ) {
                    return None;
                }
                return Some(EdgeRewrite {
                    dependent: binding.dependent.clone(),
                    dependency: binding.dependency.to_string(),
                    from: binding.bound.to_string(),
                    to: successor,
                });
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
        graph_with_resolved(requirements, &["0.8.2", "1.24.0"])
    }

    /// [`graph_with`] with explicit resolved-edge versions, for fixtures whose current binding is
    /// not one of the uuid fixture's two: `admits` matches a declaration only when it resolves to
    /// the edge's *current* bound, so the versions here must include it.
    fn graph_with_resolved(
        requirements: &[(&str, &str, &str, &str)],
        resolved_versions: &[&str],
    ) -> ResolvedGraph {
        let mut declared_requirements: HashMap<LockPackageId, Vec<DeclaredRequirement>> =
            HashMap::new();
        for (name, version, dependency, requirement) in requirements {
            for resolved_version in resolved_versions.iter().copied() {
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
            hold_edges: HashMap::new(),
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

    /// The luup fix-collapse shape: diesel's wide `uuid >=0.7,<2.0` edge was bound to `1.25.0`, a
    /// planned downgrade replaced that node with `1.24.0`, and cargo's incremental re-resolve
    /// rebound the edge onto the coexisting `0.8.2` line instead of the successor.
    /// The vanished target licenses no line change: preserve rebinds to the `1.x` successor.
    #[test]
    fn a_vanished_downgrade_target_rebinds_to_its_line_successor() {
        // Before: diesel bound to the 1.x line at 1.25.0 (the 1.x node then in the lock).
        let before_text = CHURNED_LOCK
            .replace(
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
            )
            .replace("1.24.0", "1.25.0");
        let before = view(&before_text);
        // After: the downgrade landed 1.24.0 in the slot and the edge collapsed onto 0.8.2.
        let after = view(CHURNED_LOCK);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);

        assert!(has_potential_restoration(&before, &after));
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

    /// The vanished line has no surviving successor (the downgrade removed the 1.x node
    /// entirely): the edge legitimately collapsed onto the only remaining version.
    #[test]
    fn a_vanished_line_without_a_successor_is_left_alone() {
        let before_text = CHURNED_LOCK
            .replace(
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
            )
            .replace("1.24.0", "1.25.0");
        let before = view(&before_text);
        let after_text = CHURNED_LOCK.replace(" \"uuid 1.24.0\",\n", "").replace(
            indoc! {r#"

                    [[package]]
                    name = "uuid"
                    version = "1.24.0"
                    source = "registry+https://github.com/rust-lang/crates.io-index"
                    checksum = "dd"
                "#},
            "\n",
        );
        assert!(!after_text.contains("1.24.0"));
        let after = view(&after_text);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);

        assert!(restorations(&before, &after, &RequirementIndex::new(&graph)).is_empty());
    }

    /// The `0.0.x` corner: [`version::major_key`] collapses every `0.0.z` to one `"0.0"` key,
    /// but cargo treats each `0.0.z` as its own line and can lock several side by side — so a
    /// pre-existing `0.0.z` bystander (kept alive by another dependent) must never be chosen as
    /// the vanished target's "successor". A genuine replacement is new to the after-lock.
    #[test]
    fn a_pre_existing_zero_zero_bystander_is_not_a_successor() {
        // Before: widget bound to nano 0.0.5; nano 0.0.3 coexists as another dependent's node.
        let before = view(indoc! {r#"
            version = 4

            [[package]]
            name = "widget"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "aa"
            dependencies = [
             "nano 0.0.5",
            ]

            [[package]]
            name = "nano"
            version = "0.0.3"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "bb"

            [[package]]
            name = "nano"
            version = "0.0.5"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "cc"

            [[package]]
            name = "nano"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "dd"
        "#});
        // After: 0.0.5 vanished, widget's edge collapsed onto the 1.x line; the surviving 0.0.3
        // is the untouched bystander, not a replacement.
        let after_bystander = view(indoc! {r#"
            version = 4

            [[package]]
            name = "widget"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "aa"
            dependencies = [
             "nano 1.0.0",
            ]

            [[package]]
            name = "nano"
            version = "0.0.3"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "bb"

            [[package]]
            name = "nano"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "dd"
        "#});
        // The dependent's declared range admits every nano version in play, so a rewrite the
        // guard failed to veto would pass `admits` — the restorations assertion below is only
        // meaningful with this requirement in the graph (an empty graph rejects everything).
        let graph = graph_with_resolved(&[("widget", "1.0.0", "nano", ">=0.0.3, <2")], &["1.0.0"]);

        assert!(!has_potential_restoration(&before, &after_bystander));
        assert!(restorations(&before, &after_bystander, &RequirementIndex::new(&graph)).is_empty());

        // Counterfactual guarding against a vacuous pass: the same shape with a genuinely new
        // `0.0.z` node (0.0.4, absent from the before-lock) is a real successor candidate.
        let after_replacement = view(indoc! {r#"
            version = 4

            [[package]]
            name = "widget"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "aa"
            dependencies = [
             "nano 1.0.0",
            ]

            [[package]]
            name = "nano"
            version = "0.0.4"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "ee"

            [[package]]
            name = "nano"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "dd"
        "#});
        assert!(has_potential_restoration(&before, &after_replacement));
        assert_eq!(
            restorations(&before, &after_replacement, &RequirementIndex::new(&graph)),
            vec![EdgeRewrite {
                dependent: key("widget", "1.0.0"),
                dependency: "nano".to_string(),
                from: "1.0.0".to_string(),
                to: "0.0.4".to_string(),
            }],
            "the genuinely new same-key node is restored — proving the bystander case above \
             abstained on the guard, not on the requirement"
        );
    }

    /// Two surviving `"0.0"`-key candidates are ambiguous (only reachable in the `0.0.x` corner,
    /// where cargo locks several `0.0.z` versions side by side): restoration abstains.
    #[test]
    fn two_zero_zero_survivors_are_ambiguous_and_abstain() {
        let before = view(indoc! {r#"
            version = 4

            [[package]]
            name = "widget"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "aa"
            dependencies = [
             "nano 0.0.5",
            ]

            [[package]]
            name = "nano"
            version = "0.0.5"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "cc"

            [[package]]
            name = "nano"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "dd"
        "#});
        // After: 0.0.5 vanished and TWO new 0.0.z nodes appeared beside the 1.x landing.
        let after = view(indoc! {r#"
            version = 4

            [[package]]
            name = "widget"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "aa"
            dependencies = [
             "nano 1.0.0",
            ]

            [[package]]
            name = "nano"
            version = "0.0.3"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "bb"

            [[package]]
            name = "nano"
            version = "0.0.4"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "ee"

            [[package]]
            name = "nano"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            checksum = "dd"
        "#});
        // As in the bystander test: the requirement admits both survivors, so the emptiness
        // below can only come from the ambiguity abstention itself.
        let graph = graph_with_resolved(&[("widget", "1.0.0", "nano", ">=0.0.3, <2")], &["1.0.0"]);

        assert!(!has_potential_restoration(&before, &after));
        assert!(restorations(&before, &after, &RequirementIndex::new(&graph)).is_empty());
    }

    /// The successor no longer satisfies the dependent's requirement (a narrowed range that only
    /// admits the old line): the collapse is the resolver's only valid answer.
    #[test]
    fn a_successor_outside_the_requirement_is_not_forced() {
        let before_text = CHURNED_LOCK
            .replace(
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
                "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
            )
            .replace("1.24.0", "1.25.0");
        let before = view(&before_text);
        let after = view(CHURNED_LOCK);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", "^0.8")]);

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
