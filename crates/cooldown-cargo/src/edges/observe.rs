//! The observation diff: which edge bindings moved between two locks, computed over the raw
//! per-block qualified-entry multisets so that moves among entries too ambiguous to *rewrite*
//! (renamed multi-version deps, source-suffixed entries, twin blocks sharing one identity) are
//! still reported.

use super::lock_view::remainder_version;
use super::{BindingChange, LockEdgeView};

/// The elements of `left` minus `right` as multisets; both slices are version-sorted.
fn multiset_difference(left: &[String], right: &[String]) -> Vec<String> {
    let mut remaining: Vec<&String> = right.iter().collect();
    left.iter()
        .filter(
            |version| match remaining.iter().position(|candidate| candidate == version) {
                Some(index) => {
                    remaining.swap_remove(index);
                    false
                }
                None => true,
            },
        )
        .cloned()
        .collect()
}

/// Bindings that moved between `before` and `after` for `[[package]]` blocks present in both locks
/// under the same `(name, version, source)` identity — the rebinds an apply produced. Compares the
/// per-block qualified-entry multisets verbatim (source suffixes included), so a rebind among
/// entries too ambiguous to rewrite is still observed, and twin blocks sharing a `(name, version)`
/// identity are diffed independently instead of merged. Vacated and adopted entries are paired in
/// version order; a pair whose entries differ only in source (same bound version, another origin)
/// is reported with the source move in `detail`. Excluded as *not* rebinds: a block whose own
/// identity changed (its dependency list was legitimately re-resolved), an entry that appeared or
/// vanished without a counterpart, and a pair whose endpoint versions do not exist in **both**
/// locks — an edge merely following a slot-level version change the report already carries as an
/// applied row. A genuine rebinding moves between versions that coexist throughout.
pub(crate) fn binding_changes(before: &LockEdgeView, after: &LockEdgeView) -> Vec<BindingChange> {
    let mut changes = Vec::new();
    for ((block, dependency), after_entries) in &after.qualified {
        let key = (block.clone(), dependency.clone());
        let Some(before_entries) = before.qualified.get(&key) else {
            continue;
        };
        if before_entries == after_entries {
            continue;
        }
        let vacated = multiset_difference(before_entries, after_entries);
        let adopted = multiset_difference(after_entries, before_entries);
        for (from_entry, to_entry) in vacated.into_iter().zip(adopted) {
            let from = remainder_version(&from_entry).to_string();
            let to = remainder_version(&to_entry).to_string();
            if !after.has_version(dependency, &from) || !before.has_version(dependency, &to) {
                continue;
            }
            let detail = (from == to)
                .then(|| format!("the entry's source changed: \"{from_entry}\" → \"{to_entry}\""));
            changes.push(BindingChange {
                dependent: super::PackageKey::new(&*block.name, &*block.version),
                dependency: dependency.clone(),
                before: from,
                after: to,
                detail,
            });
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::super::tests::{CHURNED_LOCK, key, view};
    use super::*;

    #[test]
    fn binding_changes_report_moves_and_skip_changed_dependents() {
        let before = view(CHURNED_LOCK);
        let after_text = CHURNED_LOCK.replace(
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 0.8.2\",",
            "checksum = \"aa\"\ndependencies = [\n \"itoa\",\n \"uuid 1.24.0\",",
        );
        assert_ne!(after_text, CHURNED_LOCK, "the fixture edit must apply");
        let after = view(&after_text);
        let changes = binding_changes(&before, &after);
        assert_eq!(
            changes,
            vec![BindingChange {
                dependent: key("diesel", "2.3.11"),
                dependency: "uuid".to_string(),
                before: "0.8.2".to_string(),
                after: "1.24.0".to_string(),
                detail: None,
            }]
        );

        // A dependent whose own version changed is not compared: its list was re-resolved.
        let moved_dependent = after_text.replace(
            "name = \"diesel\"\nversion = \"2.3.11\"",
            "name = \"diesel\"\nversion = \"2.3.12\"",
        );
        let after_moved = view(&moved_dependent);
        assert!(binding_changes(&before, &after_moved).is_empty());
    }

    /// The observation diff must also see edges the policies cannot rewrite: `app` holds `uuid`
    /// through TWO renamed entries (an ambiguous pair with no `binding`), yet a move of one of them
    /// still surfaces via the qualified-entry multisets.
    #[test]
    fn binding_changes_observe_an_ambiguous_multi_entry_move() {
        let before = view(CHURNED_LOCK);
        let after_text = CHURNED_LOCK.replace(
            " \"uuid 0.8.2\",\n \"uuid 1.24.0\",\n]",
            " \"uuid 0.8.2\",\n \"uuid 0.8.2\",\n]",
        );
        assert_ne!(after_text, CHURNED_LOCK, "the fixture edit must apply");
        let after = view(&after_text);

        let changes = binding_changes(&before, &after);
        assert_eq!(
            changes,
            vec![BindingChange {
                dependent: key("app", "0.1.0"),
                dependency: "uuid".to_string(),
                before: "1.24.0".to_string(),
                after: "0.8.2".to_string(),
                detail: None,
            }],
            "an ambiguous pair's move must still be observed"
        );
        // The pair remains untouchable for rewrites all the same.
        assert_eq!(after.binding(&key("app", "0.1.0"), "uuid"), None);
    }

    /// Twin blocks sharing one `(name, version)` identity are diffed per block: swapping their
    /// bindings leaves the *merged* version multiset identical, but the per-block multisets each
    /// move, so both rebinds are observed.
    #[test]
    fn binding_changes_observe_a_twin_identity_swap() {
        let twins_before = indoc::indoc! {r#"
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
             "dep 2.0.0",
            ]
        "#};
        let twins_after = twins_before
            .replace(
                "source = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\n \"dep 1.0.0\",",
                "source = \"registry+https://github.com/rust-lang/crates.io-index\"\ndependencies = [\n \"dep 2.0.0\",",
            )
            .replace(
                "source = \"git+https://example.com/twin#abcdef\"\ndependencies = [\n \"dep 2.0.0\",",
                "source = \"git+https://example.com/twin#abcdef\"\ndependencies = [\n \"dep 1.0.0\",",
            );
        assert_ne!(twins_after, twins_before, "the fixture edit must apply");

        let mut changes = binding_changes(&view(twins_before), &view(&twins_after));
        changes.sort_by(|a, b| a.before.cmp(&b.before));
        assert_eq!(
            changes,
            vec![
                BindingChange {
                    dependent: key("twin", "1.0.0"),
                    dependency: "dep".to_string(),
                    before: "1.0.0".to_string(),
                    after: "2.0.0".to_string(),
                    detail: None,
                },
                BindingChange {
                    dependent: key("twin", "1.0.0"),
                    dependency: "dep".to_string(),
                    before: "2.0.0".to_string(),
                    after: "1.0.0".to_string(),
                    detail: None,
                },
            ],
            "each twin block's rebind must be observed independently"
        );
    }

    /// A same-version entry swap between two sources changes the code `rustc` receives even though
    /// the bound version is identical; the pair is reported with the source move in `detail`.
    #[test]
    fn binding_changes_observe_a_same_version_source_swap() {
        let before_text = indoc::indoc! {r#"
            version = 4

            [[package]]
            name = "app"
            version = "0.1.0"
            dependencies = [
             "foo 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)",
            ]

            [[package]]
            name = "foo"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "foo"
            version = "1.0.0"
            source = "git+https://example.com/foo#abcdef"
        "#};
        let after_text = before_text.replace(
            " \"foo 1.0.0 (registry+https://github.com/rust-lang/crates.io-index)\",",
            " \"foo 1.0.0 (git+https://example.com/foo#abcdef)\",",
        );
        assert_ne!(after_text, before_text, "the fixture edit must apply");

        let changes = binding_changes(&view(before_text), &view(&after_text));
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].before, "1.0.0");
        assert_eq!(changes[0].after, "1.0.0");
        let detail = changes[0].detail.as_deref().unwrap_or_default();
        assert!(
            detail.contains("git+https://example.com/foo"),
            "the source move must be spelled out: {detail}"
        );
    }
}
