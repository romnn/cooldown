//! Transitive advance of a name resolved at several copies: each copy is addressed through the
//! parent selectors of its own dependents, and held only when no selector reaches it alone.

use super::*;
use crate::lock::Pnpm;
use color_eyre::eyre;
use indoc::indoc;

/// A change to `name` moving `from` to `to`.
fn change(name: &str, from: &str, to: &str, direct: bool) -> Change {
    Change {
        package: PackageId::new(Pnpm::ID, name, Some(NPM.to_string())),
        from: Version::new(from),
        to: Version::new(to),
        kind: UpdateKind::Minor,
        downgrade: false,
        direct,
        members: Vec::new(),
    }
}

fn transitive(name: &str, from: &str, to: &str) -> Change {
    change(name, from, to, false)
}

fn plan(changes: Vec<Change>) -> Plan {
    Plan {
        changes,
        ..Plan::default()
    }
}

/// What a plan's transitive candidates get against a pre-apply lock ([`advance`]).
struct Advance {
    verdicts: HashMap<AdvanceKey, TransitiveAdvance>,
    /// The temporary override entries ([`transitive_override_pins`]).
    pins: Vec<(String, String)>,
}

/// The verdicts and override pins `plan` gets against the pre-apply `lock`, with no importer
/// excluded.
fn advance(plan: &Plan, lock: &str) -> eyre::Result<Advance> {
    let verdicts = transitive_advance_verdicts::<Pnpm>(plan, Some(lock), &HashSet::new())?;
    let pins = transitive_override_pins(plan, &verdicts);
    Ok(Advance { verdicts, pins })
}

fn pin(key: &str, target: &str) -> (String, String) {
    (key.to_string(), target.to_string())
}

/// The hold detail of `change`'s verdict, `None` when it was handed to the resolver.
fn hold_detail(
    verdicts: &HashMap<AdvanceKey, TransitiveAdvance>,
    change: &Change,
) -> Option<String> {
    let hold = verdicts.get(&advance_key(change))?.hold_skip(change)?;
    assert_eq!(hold.reason, SkipReason::MultiVersionHeld, "{hold:?}");
    hold.detail
}

/// The issue's `ansi-regex` shape: `strip-ansi` 7.2.0 binds the 6.2.2 copy through `^6.2.2`, and
/// `strip-ansi` 6.0.1, under `wrap-ansi`, binds the 5.0.1 copy.
const ANSI_REGEX: &str = indoc! {"
    lockfileVersion: '9.0'

    importers:

      .:
        dependencies:
          strip-ansi:
            specifier: 7.2.0
            version: 7.2.0
          wrap-ansi:
            specifier: 7.0.0
            version: 7.0.0

    packages:

      ansi-regex@5.0.1:
        resolution: {integrity: sha512-a}

      ansi-regex@6.2.2:
        resolution: {integrity: sha512-b}

      strip-ansi@6.0.1:
        resolution: {integrity: sha512-c}

      strip-ansi@7.2.0:
        resolution: {integrity: sha512-d}

      wrap-ansi@7.0.0:
        resolution: {integrity: sha512-e}

    snapshots:

      ansi-regex@5.0.1: {}

      ansi-regex@6.2.2: {}

      strip-ansi@6.0.1:
        dependencies:
          ansi-regex: 5.0.1

      strip-ansi@7.2.0:
        dependencies:
          ansi-regex: 6.2.2

      wrap-ansi@7.0.0:
        dependencies:
          strip-ansi: 6.0.1
"};

/// The 6.2.2 copy advances alone: its only dependent is selected by name and version, so the
/// override cannot reach `strip-ansi` 6.0.1's requirement of the 5.0.1 copy.
#[test]
fn one_copy_of_a_duplicated_name_advances_through_its_own_dependent() -> eyre::Result<()> {
    let regex = transitive("ansi-regex", "6.2.2", "6.5.0");
    let plan = plan(vec![regex.clone()]);

    let Advance { verdicts, pins } = advance(&plan, ANSI_REGEX)?;

    assert_eq!(pins, [pin("strip-ansi@7.2.0>ansi-regex", "6.5.0")]);
    assert_eq!(hold_detail(&verdicts, &regex), None);
    Ok(())
}

/// A dependent with its own planned move is selected at its target too, so the override still
/// reaches the copy when the dependent's new version is resolved in the same run.
#[test]
fn a_moving_dependent_is_selected_at_its_target_too() -> eyre::Result<()> {
    let plan = plan(vec![
        transitive("ansi-regex", "6.2.2", "6.5.0"),
        change("strip-ansi", "7.2.0", "7.3.0", true),
    ]);

    let Advance { pins, .. } = advance(&plan, ANSI_REGEX)?;

    assert_eq!(pins, [pin("strip-ansi@7.2.0 || 7.3.0>ansi-regex", "6.5.0")]);
    Ok(())
}

/// The issue's Babel shape: a helper resolved on the 7 and 8 lines, the 7 copy bound by two
/// plugins (one under a peer context), the 8 copy by the plugin's own 8 line.
const BABEL_HELPER: &str = indoc! {"
    lockfileVersion: '9.0'

    importers:

      .:
        dependencies:
          '@babel/plugin-a':
            specifier: 7.29.0
            version: 7.29.0
          '@babel/plugin-b':
            specifier: 7.28.0
            version: 7.28.0(@babel/core@7.29.0)
          plugin-a-next:
            specifier: npm:@babel/plugin-a@8.0.0
            version: '@babel/plugin-a@8.0.0'

    packages:

      '@babel/core@7.29.0':
        resolution: {integrity: sha512-a}

      '@babel/helper-x@7.29.0':
        resolution: {integrity: sha512-b}

      '@babel/helper-x@8.0.0':
        resolution: {integrity: sha512-c}

      '@babel/plugin-a@7.29.0':
        resolution: {integrity: sha512-d}

      '@babel/plugin-a@8.0.0':
        resolution: {integrity: sha512-e}

      '@babel/plugin-b@7.28.0':
        resolution: {integrity: sha512-f}

    snapshots:

      '@babel/core@7.29.0': {}

      '@babel/helper-x@7.29.0': {}

      '@babel/helper-x@8.0.0': {}

      '@babel/plugin-a@7.29.0':
        dependencies:
          '@babel/helper-x': 7.29.0

      '@babel/plugin-a@8.0.0':
        dependencies:
          '@babel/helper-x': 8.0.0

      '@babel/plugin-b@7.28.0(@babel/core@7.29.0)':
        dependencies:
          '@babel/core': 7.29.0
          '@babel/helper-x': 7.29.0
"};

/// Both major lines take their own in-range update: each copy's selectors name only the
/// dependents that bind it, so the 7 and 8 targets never compete for one request.
#[test]
fn two_major_lines_each_advance_through_their_own_dependents() -> eyre::Result<()> {
    let seven = transitive("@babel/helper-x", "7.29.0", "7.29.12");
    let eight = transitive("@babel/helper-x", "8.0.0", "8.0.10");
    let plan = plan(vec![seven.clone(), eight.clone()]);

    let Advance { verdicts, pins } = advance(&plan, BABEL_HELPER)?;

    assert_eq!(
        pins,
        [
            pin("@babel/plugin-a@7.29.0>@babel/helper-x", "7.29.12"),
            pin("@babel/plugin-a@8.0.0>@babel/helper-x", "8.0.10"),
            pin("@babel/plugin-b@7.28.0>@babel/helper-x", "7.29.12"),
        ]
    );
    assert_eq!(hold_detail(&verdicts, &seven), None);
    assert_eq!(hold_detail(&verdicts, &eight), None);
    Ok(())
}

/// A name resolved at several copies whose dependents include `parent_records`, each a snapshot
/// key and the version of `foo` it binds; a registry `bar@1.0.0` binds `foo` 1.0.0 too.
fn foo_lock(parent_records: &[(&str, &str)]) -> String {
    // Each record is indented to sit under `snapshots:`, since interpolation keeps the inserted
    // text's own indentation.
    let snapshots: Vec<String> = parent_records
        .iter()
        .flat_map(|(key, bound)| {
            indoc::formatdoc! {"

                '{key}':
                  dependencies:
                    foo: {bound}
            "}
            .lines()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
        })
        .collect();
    let snapshots = snapshots.join("\n");
    indoc::formatdoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              bar:
                specifier: 1.0.0
                version: 1.0.0

        packages:

          bar@1.0.0:
            resolution: {{integrity: sha512-a}}

          foo@1.0.0:
            resolution: {{integrity: sha512-b}}

          foo@2.0.0:
            resolution: {{integrity: sha512-c}}

        snapshots:

          bar@1.0.0:
            dependencies:
              foo: 1.0.0

          foo@1.0.0: {{}}

          foo@2.0.0: {{}}
        {snapshots}
    "}
}

/// Every record a selector for `parent@version` would also match must bind the planned copy:
/// another peer context of the same version, or a parent from a git source whose manifest
/// version the lock does not record, binding the other copy holds the move.
#[test]
fn a_selector_that_would_reach_another_copy_holds_the_move() -> eyre::Result<()> {
    let foo = transitive("foo", "1.0.0", "1.1.0");
    let plan = plan(vec![foo.clone()]);
    let cases = [
        (
            vec![("bar@1.0.0(react@18.0.0)", "2.0.0")],
            "bar@1.0.0 binds it at 2.0.0",
        ),
        (
            vec![("bar@git+ssh://git@github.com/acme/bar.git#abc", "2.0.0")],
            "bar@git+ssh://git@github.com/acme/bar.git#abc binds it at 2.0.0",
        ),
        (
            vec![("bar@github.com/acme/bar/abc", "2.0.0")],
            "bar@github.com/acme/bar/abc binds it at 2.0.0",
        ),
    ];
    for (records, blocker) in cases {
        let lock = foo_lock(&records);

        let Advance { verdicts, pins } = advance(&plan, &lock)?;

        assert_eq!(pins, [], "{records:?}");
        let detail = hold_detail(&verdicts, &foo).unwrap_or_default();
        assert!(
            detail.contains("1.0.0, 2.0.0") && detail.contains(blocker),
            "{records:?}: {detail}"
        );
    }

    // Records that bind the planned copy, or that a selector cannot match, leave it addressable.
    let lock = foo_lock(&[("bar@1.0.0(react@18.0.0)", "1.0.0"), ("baz@1.0.0", "2.0.0")]);
    let Advance { pins, .. } = advance(&plan, &lock)?;
    assert_eq!(pins, [pin("bar@1.0.0>foo", "1.1.0")]);
    Ok(())
}

/// A dependent's target that another copy's selector could match too is left out of the
/// selector, which keeps matching the dependent's current version only.
#[test]
fn a_dependent_target_binding_another_copy_is_not_selected() -> eyre::Result<()> {
    let lock = foo_lock(&[("bar@1.1.0", "2.0.0")]);
    let plan = plan(vec![
        transitive("foo", "1.0.0", "1.1.0"),
        change("bar", "1.0.0", "1.1.0", true),
    ]);

    let Advance { pins, .. } = advance(&plan, &lock)?;

    assert_eq!(pins, [pin("bar@1.0.0>foo", "1.1.0")]);

    // Two versions of the dependent, each binding its own copy, converging on one target: neither
    // copy's selector claims the shared target.
    let lock = foo_lock(&[("bar@2.0.0", "2.0.0")]);
    let converging = Plan {
        changes: vec![
            transitive("foo", "1.0.0", "1.1.0"),
            transitive("foo", "2.0.0", "2.1.0"),
            change("bar", "1.0.0", "3.0.0", true),
            change("bar", "2.0.0", "3.0.0", true),
        ],
        ..Plan::default()
    };

    let Advance { pins, .. } = advance(&converging, &lock)?;

    assert_eq!(
        pins,
        [pin("bar@1.0.0>foo", "1.1.0"), pin("bar@2.0.0>foo", "2.1.0")]
    );
    Ok(())
}

/// An edge no parent selector can address alone holds the move: a requirement through an npm
/// alias, and a requirement by a parent with no registry version to select it by.
#[test]
fn an_unaddressable_edge_holds_the_move() -> eyre::Result<()> {
    let alias = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              bar:
                specifier: 1.0.0
                version: 1.0.0

        packages:

          bar@1.0.0:
            resolution: {integrity: sha512-a}

          foo@1.0.0:
            resolution: {integrity: sha512-b}

          foo@2.0.0:
            resolution: {integrity: sha512-c}

        snapshots:

          bar@1.0.0:
            dependencies:
              foo: 2.0.0
              foo-cjs: foo@1.0.0

          foo@1.0.0: {}

          foo@2.0.0: {}
    "};
    let foo = transitive("foo", "1.0.0", "1.1.0");
    let plan = plan(vec![foo.clone()]);

    let Advance { verdicts, pins } = advance(&plan, alias)?;
    assert_eq!(pins, []);
    let detail = hold_detail(&verdicts, &foo).unwrap_or_default();
    assert!(
        detail.contains("bar@1.0.0 requires it through the alias `foo-cjs`"),
        "{detail}"
    );

    let git_parent = foo_lock(&[("qux@https://example.com/qux.tgz", "1.0.0")]);
    let Advance { verdicts, pins } = advance(&plan, &git_parent)?;
    assert_eq!(pins, []);
    let detail = hold_detail(&verdicts, &foo).unwrap_or_default();
    assert!(
        detail.contains("qux https://example.com/qux.tgz has no registry version"),
        "{detail}"
    );
    Ok(())
}

/// A workspace manifest declaring the name could match a parent selector by its own name and
/// version, which the lock does not record, so an importer the run excludes declaring the name
/// holds every copy.
#[test]
fn an_excluded_importer_declaring_the_name_holds_every_copy() -> eyre::Result<()> {
    let lock = indoc! {"
        lockfileVersion: '9.0'

        importers:

          .:
            dependencies:
              bar:
                specifier: 1.0.0
                version: 1.0.0

          pkgs/legacy:
            dependencies:
              foo:
                specifier: ^2.0.0
                version: 2.0.0

        packages:

          bar@1.0.0:
            resolution: {integrity: sha512-a}

          foo@1.0.0:
            resolution: {integrity: sha512-b}

          foo@2.0.0:
            resolution: {integrity: sha512-c}

        snapshots:

          bar@1.0.0:
            dependencies:
              foo: 1.0.0

          foo@1.0.0: {}

          foo@2.0.0: {}
    "};
    let foo = transitive("foo", "1.0.0", "1.1.0");
    let plan = plan(vec![foo.clone()]);
    let excluded = HashSet::from(["pkgs/legacy".to_string()]);

    let verdicts = transitive_advance_verdicts::<Pnpm>(&plan, Some(lock), &excluded)?;

    assert_eq!(transitive_override_pins(&plan, &verdicts), []);
    let detail = hold_detail(&verdicts, &foo).unwrap_or_default();
    assert!(
        detail.contains("an importer the run excludes declares it"),
        "{detail}"
    );
    Ok(())
}

/// The age repair withholds the starting pin only of the copy a change moves: an untouched
/// sibling copy of the same name keeps its permission, or pnpm's native gate would reject the
/// unchanged copy and fail the repair.
#[test]
fn the_age_repair_keeps_the_permission_of_an_untouched_sibling_copy() {
    let violation = |version: &str| cooldown_core::BaselineViolation {
        package: PackageId::new(Pnpm::ID, "foo", None),
        version: Version::new(version),
    };
    let plan = Plan {
        changes: vec![transitive("foo", "1.0.2", "1.0.1")],
        baseline_violations: vec![violation("1.0.2"), violation("2.0.2")],
        ..Plan::default()
    };

    let exclusions = minimum_age_repair_exclusions(&plan, Vec::new());

    assert!(crate::exclusions::exempts(&exclusions, "foo", "1.0.1"));
    assert!(crate::exclusions::exempts(&exclusions, "foo", "2.0.2"));
    assert!(!crate::exclusions::exempts(&exclusions, "foo", "1.0.2"));
}

/// Two copies on one major line, `semver` 7.3.8 under `a` and 7.5.4 under `b`, as both the
/// pre-apply and the settled lock record them: the settlement reverted both advances.
const SAME_LINE_REVERTED: &str = indoc! {"
    lockfileVersion: '9.0'

    importers:

      .:
        dependencies:
          a:
            specifier: 1.0.0
            version: 1.0.0
          b:
            specifier: 1.0.0
            version: 1.0.0

    packages:

      a@1.0.0:
        resolution: {integrity: sha512-a}

      b@1.0.0:
        resolution: {integrity: sha512-b}

      semver@7.3.8:
        resolution: {integrity: sha512-c}

      semver@7.5.4:
        resolution: {integrity: sha512-d}

    snapshots:

      a@1.0.0:
        dependencies:
          semver: 7.3.8

      b@1.0.0:
        dependencies:
          semver: 7.5.4

      semver@7.3.8: {}

      semver@7.5.4: {}
"};

/// A reverted advance's hold names where its own copy settled and the dependent that kept it, not
/// another copy on the same major line: the 7.5.4 copy is held at 7.5.4 by `b`, though 7.3.8 is
/// the name's first version on the line.
#[test]
fn a_reverted_advance_names_where_its_own_copy_settled() -> eyre::Result<()> {
    let higher = transitive("semver", "7.5.4", "7.6.2");
    let plan = plan(vec![transitive("semver", "7.3.8", "7.6.2"), higher.clone()]);
    let Advance { verdicts, .. } = advance(&plan, SAME_LINE_REVERTED)?;
    let ledger = CopyLedger::read::<Pnpm>(Some(SAME_LINE_REVERTED), SAME_LINE_REVERTED)?;
    let members = crate::lock::MemberIndex::default();

    let verdict = settled_verdict::<Pnpm>(
        &higher,
        &verdicts,
        &ledger,
        &HashMap::new(),
        &MemberLocks {
            before: &members,
            after: &members,
        },
    );

    let Settled::Short(shortfall) = verdict else {
        eyre::bail!("a reverted advance falls short of its target");
    };
    let hold = graph_ceiling_hold::<Pnpm>(&higher, SAME_LINE_REVERTED, &shortfall)?;
    assert_eq!(
        hold.offending.map(|package| package.name).as_deref(),
        Some("b")
    );
    let detail = hold.detail.unwrap_or_default();
    assert!(detail.contains("holds it at 7.5.4"), "{detail}");
    Ok(())
}

/// `a` and `b` share a `semver` 7.3.8 copy beside `c`'s 7.0.0 copy.
const SPLIT_BEFORE: &str = indoc! {"
    lockfileVersion: '9.0'

    importers:

      .:
        dependencies:
          a:
            specifier: 1.0.0
            version: 1.0.0
          b:
            specifier: ^1.0.0
            version: 1.0.0
          c:
            specifier: 1.0.0
            version: 1.0.0

    packages:

      a@1.0.0:
        resolution: {integrity: sha512-a}

      b@1.0.0:
        resolution: {integrity: sha512-b}

      c@1.0.0:
        resolution: {integrity: sha512-c}

      semver@7.0.0:
        resolution: {integrity: sha512-d}

      semver@7.3.8:
        resolution: {integrity: sha512-e}

    snapshots:

      a@1.0.0:
        dependencies:
          semver: 7.3.8

      b@1.0.0:
        dependencies:
          semver: 7.3.8

      c@1.0.0:
        dependencies:
          semver: 7.0.0

      semver@7.0.0: {}

      semver@7.3.8: {}
"};

/// The settled [`SPLIT_BEFORE`]: `a` kept 7.3.8, while `b` moved to 1.1.0 and settled at 7.5.4.
const SPLIT_AFTER: &str = indoc! {"
    lockfileVersion: '9.0'

    importers:

      .:
        dependencies:
          a:
            specifier: 1.0.0
            version: 1.0.0
          b:
            specifier: ^1.0.0
            version: 1.1.0
          c:
            specifier: 1.0.0
            version: 1.0.0

    packages:

      a@1.0.0:
        resolution: {integrity: sha512-a}

      b@1.1.0:
        resolution: {integrity: sha512-f}

      c@1.0.0:
        resolution: {integrity: sha512-c}

      semver@7.0.0:
        resolution: {integrity: sha512-d}

      semver@7.3.8:
        resolution: {integrity: sha512-e}

      semver@7.5.4:
        resolution: {integrity: sha512-g}

    snapshots:

      a@1.0.0:
        dependencies:
          semver: 7.3.8

      b@1.1.0:
        dependencies:
          semver: 7.5.4

      c@1.0.0:
        dependencies:
          semver: 7.0.0

      semver@7.0.0: {}

      semver@7.3.8: {}

      semver@7.5.4: {}
"};

/// A copy whose dependents settled at several versions short of the target is held with each
/// dependent's own version, never the name's other copy on the line (`c`'s 7.0.0).
#[test]
fn a_copy_settled_at_several_versions_names_each_dependent() -> eyre::Result<()> {
    let shared = transitive("semver", "7.3.8", "7.6.2");
    let plan = plan(vec![shared.clone()]);
    let Advance { verdicts, .. } = advance(&plan, SPLIT_BEFORE)?;
    let ledger = CopyLedger::read::<Pnpm>(Some(SPLIT_BEFORE), SPLIT_AFTER)?;
    let members = crate::lock::MemberIndex::default();

    let verdict = settled_verdict::<Pnpm>(
        &shared,
        &verdicts,
        &ledger,
        &HashMap::new(),
        &MemberLocks {
            before: &members,
            after: &members,
        },
    );

    let Settled::Held(hold) = verdict else {
        eyre::bail!("a copy split across versions is held with its edges' detail");
    };
    let detail = hold.detail.unwrap_or_default();
    assert!(
        detail.contains("a@1.0.0 still resolves semver 7.3.8")
            && detail.contains("b@1.1.0 still resolves semver 7.5.4")
            && !detail.contains("7.0.0"),
        "{detail}"
    );
    Ok(())
}
