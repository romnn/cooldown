//! The write half of edge enforcement: the safety guards a policy's proposed rewrites must pass,
//! and the targeted textual surgery that applies the survivors while leaving every other byte of
//! the lock identical.

use super::{
    EdgeRewrite, GuardedRewrites, LockEdgeView, LockPackageId, PackageKey, RejectedRewrite,
};
use std::collections::HashMap;

/// Filters policy-proposed rewrites through the shared safety guards, in deterministic order. A
/// rewrite is withheld when the rewrite set's **net** effect would leave a still-locked version
/// with no reference (an orphan `cargo metadata --locked` rejects). Withheld rewrites are returned,
/// not dropped — the caller reports them.
pub(crate) fn guard_rewrites(
    view: &LockEdgeView,
    mut rewrites: Vec<EdgeRewrite>,
) -> GuardedRewrites {
    rewrites.sort_by(|a, b| {
        a.dependent
            .cmp(&b.dependent)
            .then_with(|| a.dependency.cmp(&b.dependency))
    });
    let mut rejected = Vec::new();
    let mut candidates = rewrites;

    // Orphan validation over the NET effect of the whole candidate set: a version's final
    // reference count is its current count minus the edges rebound away plus the edges rebound
    // onto it. Validating rewrite-by-rewrite in application order would reject a legitimate swap —
    // two dependents exchanging two single-reference versions leave both final counts at one.
    // When a version's final count does reach zero, the last candidate (in sorted order) draining
    // it is withheld and the remainder re-validated, so the outcome is deterministic and only
    // genuine orphaners fall.
    loop {
        let mut finals: HashMap<PackageKey, i64> = view
            .refcounts
            .iter()
            .map(|(key, count)| (key.clone(), i64::try_from(*count).unwrap_or(i64::MAX)))
            .collect();
        for rewrite in &candidates {
            *finals
                .entry(PackageKey::new(&*rewrite.dependency, &*rewrite.from))
                .or_default() -= 1;
            *finals
                .entry(PackageKey::new(&*rewrite.dependency, &*rewrite.to))
                .or_default() += 1;
        }
        let orphaned: Option<PackageKey> = candidates
            .iter()
            .map(|rewrite| PackageKey::new(&*rewrite.dependency, &*rewrite.from))
            .filter(|from_key| finals.get(from_key).copied().unwrap_or(0) <= 0)
            .min();
        let Some(orphan) = orphaned else {
            break;
        };
        let Some(position) = candidates.iter().rposition(|rewrite| {
            rewrite.dependency == orphan.name && rewrite.from == orphan.version
        }) else {
            // `orphan` was drawn from `candidates`, so a draining rewrite always exists.
            break;
        };
        let rewrite = candidates.remove(position);
        let reason = format!(
            "rebinding away from {} {} would orphan its last lock reference",
            rewrite.dependency, rewrite.from
        );
        rejected.push(RejectedRewrite { rewrite, reason });
    }
    GuardedRewrites {
        accepted: candidates,
        rejected,
    }
}

/// Applies `rewrites` to the lock's verbatim text as targeted line surgery, leaving every other
/// byte identical. Entries keep their position: only the version component changes, and entries of
/// one name sort adjacently regardless of version, so the array stays cargo-sorted.
///
/// Returns `None` when any rewrite's entry line cannot be found in its dependent's block — the
/// text then no longer matches the parsed view, and half-applied surgery must not be written.
pub(crate) fn rewrite_lock_text(lock_text: &str, rewrites: &[EdgeRewrite]) -> Option<String> {
    let mut wanted: HashMap<(LockPackageId, String), (String, bool)> = rewrites
        .iter()
        .map(|rewrite| {
            (
                (
                    rewrite.dependent.clone(),
                    format!("\"{} {}\",", rewrite.dependency, rewrite.from),
                ),
                (format!("\"{} {}\",", rewrite.dependency, rewrite.to), false),
            )
        })
        .collect();

    let mut text = String::with_capacity(lock_text.len());
    let mut block_name: Option<String> = None;
    let mut block_version: Option<String> = None;
    let mut block_source: Option<String> = None;
    for segment in lock_text.split_inclusive('\n') {
        let (line, terminator) = if let Some(line) = segment.strip_suffix("\r\n") {
            (line, "\r\n")
        } else if let Some(line) = segment.strip_suffix('\n') {
            (line, "\n")
        } else {
            (segment, "")
        };
        let trimmed = line.trim();
        if trimmed == "[[package]]" {
            block_name = None;
            block_version = None;
            block_source = None;
        } else if let Some(rest) = trimmed.strip_prefix("name = ") {
            block_name = Some(rest.trim_matches('"').to_string());
        } else if let Some(rest) = trimmed.strip_prefix("version = ") {
            block_version = Some(rest.trim_matches('"').to_string());
        } else if let Some(rest) = trimmed.strip_prefix("source = ") {
            block_source = Some(rest.trim_matches('"').to_string());
        } else if let (Some(name), Some(version)) = (&block_name, &block_version)
            && let dependent = LockPackageId::new(name, version, block_source.as_deref())
            && let Some((replacement, seen)) =
                wanted.get_mut(&(dependent.clone(), trimmed.to_string()))
        {
            let leading_len = line.len() - line.trim_start().len();
            let trailing_start = line.trim_end().len();
            text.push_str(&line[..leading_len]);
            text.push_str(replacement);
            text.push_str(&line[trailing_start..]);
            text.push_str(terminator);
            *seen = true;
            continue;
        }
        text.push_str(line);
        text.push_str(terminator);
    }
    if wanted.values().any(|(_, seen)| !seen) {
        return None;
    }
    Some(text)
}

/// Groups rewrites whose dependency-version endpoints overlap.
///
/// A component is the smallest retry unit that preserves reciprocal swaps and other refcount-
/// balanced corrections which may be invalid when verified one edge at a time.
pub(crate) fn rewrite_components(mut rewrites: Vec<EdgeRewrite>) -> Vec<Vec<EdgeRewrite>> {
    let mut components = Vec::new();
    while !rewrites.is_empty() {
        let mut component = vec![rewrites.remove(0)];
        while let Some(position) = rewrites
            .iter()
            .position(|candidate| component.iter().any(|member| connected(member, candidate)))
        {
            component.push(rewrites.remove(position));
        }
        components.push(component);
    }
    components
}

fn connected(left: &EdgeRewrite, right: &EdgeRewrite) -> bool {
    left.dependency == right.dependency
        && [left.from.as_str(), left.to.as_str()]
            .iter()
            .any(|endpoint| *endpoint == right.from || *endpoint == right.to)
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CHURNED_LOCK, key, path_key, view};
    use super::*;
    use indoc::indoc;

    #[test]
    fn guard_withholds_a_rewrite_that_would_orphan_the_from_version() {
        let view = view(CHURNED_LOCK);
        // `uuid 1.24.0` has exactly one reference (app's renamed dep). Rebinding it away would
        // orphan the 1.24.0 block, so the guard withholds the rewrite — with a reported reason,
        // never a silent drop.
        let orphaning = guard_rewrites(
            &view,
            vec![EdgeRewrite {
                dependent: path_key("app", "0.1.0"),
                dependency: "uuid".to_string(),
                from: "1.24.0".to_string(),
                to: "0.8.2".to_string(),
            }],
        );
        assert!(orphaning.accepted.is_empty());
        assert_eq!(orphaning.rejected.len(), 1);
        assert!(
            orphaning.rejected[0].reason.contains("orphan"),
            "the withheld reason names the orphan guard: {}",
            orphaning.rejected[0].reason
        );

        // `uuid 0.8.2` has two references; rebinding diesel's edge keeps app's, so it passes.
        let passing = guard_rewrites(
            &view,
            vec![EdgeRewrite {
                dependent: key("diesel", "2.3.11"),
                dependency: "uuid".to_string(),
                from: "0.8.2".to_string(),
                to: "1.24.0".to_string(),
            }],
        );
        assert_eq!(passing.accepted.len(), 1);
        assert!(passing.rejected.is_empty());
    }

    #[test]
    fn guard_validates_net_refcounts_so_rewrites_cannot_collectively_orphan() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "a"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            dependencies = [
             "shared 1.0.0",
            ]

            [[package]]
            name = "b"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            dependencies = [
             "shared 1.0.0",
            ]

            [[package]]
            name = "shared"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "shared"
            version = "1.1.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#};
        // `shared 1.1.0` starts unreferenced only in this synthetic shape; the point is the two
        // rewrites together would take `shared 1.0.0` from 2 references to 0. Exactly one may pass.
        let view = view(lock);
        let rewrite = |dependent: &str| EdgeRewrite {
            dependent: key(dependent, "1.0.0"),
            dependency: "shared".to_string(),
            from: "1.0.0".to_string(),
            to: "1.1.0".to_string(),
        };
        let guarded = guard_rewrites(&view, vec![rewrite("a"), rewrite("b")]);
        assert_eq!(
            guarded.accepted.len(),
            1,
            "keeping both rewrites would orphan shared 1.0.0"
        );
        assert_eq!(guarded.accepted[0].dependent, key("a", "1.0.0"));
        assert_eq!(guarded.rejected.len(), 1);
    }

    /// A reciprocal swap of two single-reference versions is a valid atomic set: every final
    /// reference count equals the initial one. Sequential per-rewrite validation would reject the
    /// first leg (its source momentarily looks like a last reference); net validation accepts both.
    #[test]
    fn guard_accepts_a_reciprocal_swap() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "a"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            dependencies = [
             "shared 1.0.0",
            ]

            [[package]]
            name = "b"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            dependencies = [
             "shared 1.1.0",
            ]

            [[package]]
            name = "shared"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "shared"
            version = "1.1.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
        "#};
        let view = view(lock);
        let swap = vec![
            EdgeRewrite {
                dependent: key("a", "1.0.0"),
                dependency: "shared".to_string(),
                from: "1.0.0".to_string(),
                to: "1.1.0".to_string(),
            },
            EdgeRewrite {
                dependent: key("b", "1.0.0"),
                dependency: "shared".to_string(),
                from: "1.1.0".to_string(),
                to: "1.0.0".to_string(),
            },
        ];
        let guarded = guard_rewrites(&view, swap);
        assert_eq!(guarded.accepted.len(), 2, "a swap orphans nothing");
        assert!(guarded.rejected.is_empty());
    }

    /// Source-bearing dependent identities let surgery target one of two same-name, same-version
    /// blocks without touching its twin.
    #[test]
    fn rewrite_targets_one_source_distinct_twin() {
        let lock = indoc! {r#"
            version = 4

            [[package]]
            name = "dep"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "dep"
            version = "2.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "twin"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"
            dependencies = [
             "dep 1.0.0",
            ]

            [[package]]
            name = "twin"
            version = "1.0.0"
            source = "git+https://example.com/twin#abcdef"
            dependencies = [
             "dep 1.0.0",
            ]
        "#};
        let twins = view(lock);
        let guarded = guard_rewrites(
            &twins,
            vec![EdgeRewrite {
                dependent: key("twin", "1.0.0"),
                dependency: "dep".to_string(),
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
            }],
        );
        assert_eq!(guarded.accepted.len(), 1);
        let rewritten = rewrite_lock_text(lock, &guarded.accepted).expect("rewrite twin");
        assert!(
            rewritten.contains(indoc! {r#"
                source = "registry+https://github.com/rust-lang/crates.io-index"
                dependencies = [
                 "dep 2.0.0",
                ]
            "#}),
            "the crates.io twin is rewritten: {rewritten}"
        );
        assert!(
            rewritten.contains(indoc! {r#"
                source = "git+https://example.com/twin#abcdef"
                dependencies = [
                 "dep 1.0.0",
                ]
            "#}),
            "the git twin remains unchanged: {rewritten}"
        );
    }

    #[test]
    fn rewrite_lock_text_touches_exactly_the_target_line() {
        let rewritten = rewrite_lock_text(
            CHURNED_LOCK,
            &[EdgeRewrite {
                dependent: key("diesel", "2.3.11"),
                dependency: "uuid".to_string(),
                from: "0.8.2".to_string(),
                to: "1.24.0".to_string(),
            }],
        )
        .expect("the entry line exists");
        // Only diesel's entry moved; app's own `"uuid 0.8.2"` entry is untouched.
        assert_eq!(
            rewritten.matches("\"uuid 0.8.2\",").count(),
            1,
            "app's entry must survive"
        );
        assert_eq!(rewritten.matches("\"uuid 1.24.0\",").count(), 2);
        assert!(
            rewritten.ends_with('\n'),
            "the trailing newline is preserved"
        );
        // Every line except the single rewritten entry is byte-identical.
        let differing: Vec<(&str, &str)> = CHURNED_LOCK
            .lines()
            .zip(rewritten.lines())
            .filter(|(before, after)| before != after)
            .collect();
        assert_eq!(differing, vec![(" \"uuid 0.8.2\",", " \"uuid 1.24.0\",")]);
        assert_eq!(CHURNED_LOCK.lines().count(), rewritten.lines().count());
    }

    #[test]
    fn rewrite_lock_text_refuses_a_missing_entry() {
        assert!(
            rewrite_lock_text(
                CHURNED_LOCK,
                &[EdgeRewrite {
                    dependent: key("diesel", "2.3.11"),
                    dependency: "uuid".to_string(),
                    from: "9.9.9".to_string(),
                    to: "1.24.0".to_string(),
                }],
            )
            .is_none(),
            "surgery must be all-or-nothing"
        );
    }

    #[test]
    fn rewrite_lock_text_preserves_crlf_and_trailing_whitespace() {
        let crlf = CHURNED_LOCK.replace('\n', "\r\n");
        let rewritten = rewrite_lock_text(
            &crlf,
            &[EdgeRewrite {
                dependent: key("diesel", "2.3.11"),
                dependency: "uuid".to_string(),
                from: "0.8.2".to_string(),
                to: "1.24.0".to_string(),
            }],
        )
        .expect("rewrite CRLF lock");
        assert_eq!(
            rewritten.matches("\r\n").count(),
            crlf.matches("\r\n").count()
        );
        assert!(!rewritten.replace("\r\n", "").contains('\n'));

        let trailing = CHURNED_LOCK.replacen(
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",   ",
            1,
        );
        let rewritten = rewrite_lock_text(
            &trailing,
            &[EdgeRewrite {
                dependent: key("diesel", "2.3.11"),
                dependency: "uuid".to_string(),
                from: "0.8.2".to_string(),
                to: "1.24.0".to_string(),
            }],
        )
        .expect("rewrite trailing whitespace");
        assert!(rewritten.contains(" \"uuid 1.24.0\",   \n"));
    }

    #[test]
    fn rewrite_components_keep_reciprocal_endpoints_together() {
        let rewrite = |dependent: &str, dependency: &str, from: &str, to: &str| EdgeRewrite {
            dependent: key(dependent, "1.0.0"),
            dependency: dependency.to_string(),
            from: from.to_string(),
            to: to.to_string(),
        };
        let components = rewrite_components(vec![
            rewrite("a", "shared", "1.0.0", "2.0.0"),
            rewrite("b", "shared", "2.0.0", "1.0.0"),
            rewrite("c", "other", "1.0.0", "2.0.0"),
        ]);

        assert_eq!(components.len(), 2);
        assert_eq!(components[0].len(), 2, "the reciprocal swap stays atomic");
        assert_eq!(components[1].len(), 1, "the unrelated rewrite isolates");
    }
}
