//! [`EdgePolicy::Canonicalize`](cooldown_core::EdgePolicy::Canonicalize): cooldown's owned
//! normalization of ambiguous edges — bind every unambiguous crates.io edge to the **highest**
//! locked crates.io version satisfying the dependent's declared requirement, preferring candidates
//! whose declared `rust-version` is workspace-compatible. Unlike `preserve` this needs no pre-apply
//! snapshot and also heals a bad binding that predates the run. A successfully canonicalized edge
//! is a fixed point; a correction blocked by a safety guard remains a reported hold on later runs.
//!
//! This is a deliberate policy over the *existing* package set, not a re-implementation of cargo's
//! resolver (which would pick versions, not just bindings). It matches what a from-scratch resolve
//! binds in the common case — the resolver tries candidates highest-first, and coexisting locked
//! versions are semver-incompatible majors that never conflict — with one deliberate rule for MSRV:
//! candidates whose own `rust-version` exceeds the workspace minimum are deprioritized,
//! falling back to the highest satisfying candidate when no compatible one exists. That mirrors
//! cargo's `incompatible-rust-versions = "fallback"` behavior (the resolver-v3 default); a
//! workspace on resolver v1/v2 (default `allow`) or with a config override may see cargo fresh-bind
//! a higher, MSRV-incompatible version where this policy keeps the compatible one. The effective
//! resolver mode is deliberately not reconstructed from cargo config — the compatible-first choice
//! is based on declared `rust-version` metadata, is verified with `cargo metadata --locked`, and
//! can be opted out of via the edge policy itself.

use super::{EdgeRewrite, LockEdgeView, RequirementIndex};
use crate::version;

/// The corrective rewrites that bind each unambiguous crates.io edge of `lock` to its canonical
/// version: the highest satisfying in-lock crates.io version whose declared `rust-version` is
/// workspace-compatible — or, when no satisfying candidate is compatible by that metadata, the
/// highest satisfying one (cargo's fallback tier).
/// The orphan guard is applied by the caller via [`guard_rewrites`](super::guard_rewrites).
pub(crate) fn rebindings(
    lock: &LockEdgeView,
    requirements: &RequirementIndex<'_>,
) -> Vec<EdgeRewrite> {
    lock.bindings()
        .filter_map(|binding| {
            // Only edges currently bound to a crates.io version are candidates: a binding to a
            // git/path/alternate-registry version belongs to a requirement whose source the
            // metadata model does not carry, and a crates.io rebind could cross sources.
            if lock
                .dependency_source(binding.dependency, binding.bound)
                .as_deref()
                != Some(crate::cargocmd::CRATES_IO_SOURCE)
            {
                return None;
            }
            let admitted: Vec<&str> = lock
                .crates_io_versions(binding.dependency)
                .filter(|candidate| {
                    lock.dependency_source(binding.dependency, candidate)
                        .as_deref()
                        == Some(crate::cargocmd::CRATES_IO_SOURCE)
                        && requirements.admits(
                            binding.dependent,
                            binding.dependency,
                            binding.bound,
                            candidate,
                        )
                })
                .collect();
            let canonical = admitted
                .iter()
                .filter(|candidate| requirements.msrv_admits(binding.dependency, candidate))
                .max_by(|a, b| version::compare(a, b))
                .or_else(|| admitted.iter().max_by(|a, b| version::compare(a, b)))?;
            (*canonical != binding.bound).then(|| EdgeRewrite {
                dependent: binding.dependent.clone(),
                dependency: binding.dependency.to_string(),
                from: binding.bound.to_string(),
                to: (*canonical).to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CHURNED_LOCK, key, view};
    use super::*;
    use crate::cargocmd::{
        CRATES_IO_SOURCE, DeclaredRequirement, LockPackageId, ResolvedGraph, RustVersion,
    };
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

    /// The healing case `preserve` cannot cover: diesel's wide-range edge sits on 0.8.2 from a
    /// *previous* re-resolve (no pre-apply diff shows it), and canonicalize rebinds it to the
    /// highest satisfying locked version — what a fresh resolve picks.
    #[test]
    fn a_pre_existing_bad_binding_is_healed() {
        let lock = view(CHURNED_LOCK);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);

        let rewrites = rebindings(&lock, &RequirementIndex::new(&graph));

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

    /// An exact `=0.8.2` requirement makes 0.8.2 itself canonical: nothing to rewrite. The policy
    /// respects requirements, it does not blindly maximize.
    #[test]
    fn an_exact_requirement_keeps_the_pinned_binding() {
        let lock = view(CHURNED_LOCK);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", "=0.8.2")]);
        assert!(rebindings(&lock, &RequirementIndex::new(&graph)).is_empty());
    }

    /// A binding already at the canonical version is a fixed point.
    #[test]
    fn a_canonical_binding_produces_no_rewrite() {
        let healed = CHURNED_LOCK.replace(
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
        );
        let lock = view(&healed);
        let graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);
        assert!(rebindings(&lock, &RequirementIndex::new(&graph)).is_empty());
    }

    /// When several active requirements of one dependent resolve to the same concrete edge, the
    /// canonical version must satisfy them all.
    #[test]
    fn all_declared_requirements_must_admit_the_canonical_version() {
        let lock = view(CHURNED_LOCK);
        let graph = graph_with(&[
            ("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0"),
            ("diesel", "2.3.11", "uuid", "^0.8"),
        ]);
        // 1.24.0 fails `^0.8`, so 0.8.2 is canonical and the binding stays.
        assert!(rebindings(&lock, &RequirementIndex::new(&graph)).is_empty());
    }

    /// No declared requirement known (metadata gap): no license to rewrite.
    #[test]
    fn an_unknown_requirement_blocks_rebinding() {
        let lock = view(CHURNED_LOCK);
        let graph = graph_with(&[]);
        assert!(rebindings(&lock, &RequirementIndex::new(&graph)).is_empty());
    }

    /// The compatibility tier of cargo's MSRV fallback rule: with a workspace `rust-version`
    /// declared, a candidate whose own `rust-version` exceeds it is not preferred, so the
    /// canonical binding is the highest satisfying version with a workspace-compatible declared
    /// `rust-version` — never an override of a deliberate MSRV-aware downgrade.
    #[test]
    fn canonicalize_respects_the_workspace_msrv() {
        let lock = view(CHURNED_LOCK);
        let mut graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);
        graph.workspace_rust_version = Some(RustVersion::new(1, 60, 0));
        graph.rust_versions.insert(
            LockPackageId::new("uuid", "1.24.0", Some(CRATES_IO_SOURCE)),
            RustVersion::new(1, 63, 0),
        );

        // uuid 1.24.0 demands rustc 1.63 > the workspace's 1.60; the current 0.8.2 (no declared
        // MSRV, which cargo treats as compatible) stays canonical — nothing to rewrite.
        assert!(rebindings(&lock, &RequirementIndex::new(&graph)).is_empty());

        // Without a workspace MSRV the rule is inert and the highest satisfying version wins.
        graph.workspace_rust_version = None;
        assert_eq!(rebindings(&lock, &RequirementIndex::new(&graph)).len(), 1);
    }

    /// The fallback tier of the same rule: when no satisfying candidate has a compatible declared
    /// `rust-version`, cargo uses an incompatible one rather than failing — so canonicalization
    /// also falls back to the highest satisfying candidate instead of doing nothing.
    #[test]
    fn canonicalize_falls_back_when_no_candidate_is_msrv_compatible() {
        let lock = view(CHURNED_LOCK);
        let mut graph = graph_with(&[("diesel", "2.3.11", "uuid", ">=0.7.0, <2.0.0")]);
        graph.workspace_rust_version = Some(RustVersion::new(1, 60, 0));
        graph.rust_versions.insert(
            LockPackageId::new("uuid", "0.8.2", Some(CRATES_IO_SOURCE)),
            RustVersion::new(1, 62, 0),
        );
        graph.rust_versions.insert(
            LockPackageId::new("uuid", "1.24.0", Some(CRATES_IO_SOURCE)),
            RustVersion::new(1, 63, 0),
        );

        // Both candidates exceed the workspace MSRV: the compatible tier is empty, so the
        // fallback tier picks the highest satisfying version, 1.24.0.
        assert_eq!(
            rebindings(&lock, &RequirementIndex::new(&graph)),
            vec![EdgeRewrite {
                dependent: key("diesel", "2.3.11"),
                dependency: "uuid".to_string(),
                from: "0.8.2".to_string(),
                to: "1.24.0".to_string(),
            }]
        );
    }

    /// An edge bound to a non-crates.io version (a git copy of the crate) belongs to a requirement
    /// whose source the metadata model does not carry; proposing a crates.io rebind could cross
    /// sources, so the edge is left alone.
    #[test]
    fn a_non_crates_io_binding_is_not_canonicalized() {
        let lock_text = indoc::indoc! {r#"
            version = 4

            [[package]]
            name = "app"
            version = "0.1.0"
            dependencies = [
             "uuid 0.9.0",
            ]

            [[package]]
            name = "uuid"
            version = "0.9.0"
            source = "git+https://example.com/uuid#abcdef"

            [[package]]
            name = "uuid"
            version = "1.24.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#};
        let lock = view(lock_text);
        let graph = graph_with(&[("app", "0.1.0", "uuid", ">=0.7.0, <2.0.0")]);
        assert!(rebindings(&lock, &RequirementIndex::new(&graph)).is_empty());
    }
}
