use super::*;
use crate::lock::{Bun, Npm, Pnpm, Yarn};
use crate::manifest::RewrittenDeclaration;
use crate::registry::NPM;
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use cooldown_core::{PackageId, UpdateKind, Version};
use indoc::{formatdoc, indoc};

/// The expected [`violated`] result when nothing is violated.
const NO_VIOLATIONS: [&str; 0] = [];

/// The protected pins of the given `(member, manifest)` pairs, as the journal would yield them.
fn protected(manifests: &[(&str, &str)]) -> eyre::Result<ProtectedPins> {
    let mut declarations = Vec::new();
    for (member, content) in manifests {
        declarations.extend(manifest::exact_install_declarations(member, content)?);
    }
    declarations.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(ProtectedPins { declarations })
}

fn key(member: &str, field: &'static str, dependency: &str) -> DeclarationKey {
    DeclarationKey {
        member: member.to_string(),
        field,
        dependency: dependency.to_string(),
    }
}

/// Every new violation of `settled` against `baseline`, each as `member field dependency`.
fn violated<L: NodeLock>(
    pins: &ProtectedPins,
    baseline: Option<&str>,
    settled: &str,
    authorized: &AuthorizedPins,
) -> Vec<String> {
    pins.violations_between::<L>(baseline, settled, authorized)
        .iter()
        .map(|violation| {
            let DeclarationKey {
                member,
                field,
                dependency,
            } = &violation.key;
            format!("{member} {field} {dependency}")
        })
        .collect()
}

/// A pnpm lock whose importers each declare `chalk` as `(path, specifier, version)`, with a
/// `packages:` entry per listed version.
fn pnpm_lock(importers: &[(&str, &str, &str)], packages: &[&str]) -> String {
    let importers = importers
        .iter()
        .map(|(path, specifier, version)| {
            format!(
                "  {path}: {{dependencies: {{chalk: {{specifier: '{specifier}', version: '{version}'}}}}}}"
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let packages = packages
        .iter()
        .map(|version| format!("  chalk@{version}: {{resolution: {{integrity: sha512-x}}}}"))
        .collect::<Vec<_>>()
        .join("\n");
    formatdoc! {"
        lockfileVersion: '9.0'

        importers:
        {importers}

        packages:
        {packages}
    "}
}

/// The #13 shape: a caret pnpm records beside an exact pin in a second install field.
const TWO_FIELD_MANIFEST: &str = indoc! {r#"
    {
      "name": "a",
      "dependencies": { "chalk": "^4.1.0" },
      "devDependencies": { "chalk": "4.1.0" }
    }
"#};

fn change(name: &str, direct: bool, downgrade: bool) -> Change {
    Change {
        package: PackageId::new(Pnpm::ID, name, Some(NPM.to_string())),
        from: Version::new("1.0.0"),
        to: Version::new("1.1.0"),
        kind: UpdateKind::Minor,
        downgrade,
        direct,
        members: Vec::new(),
    }
}

fn rewrite_of(key: DeclarationKey, replacement: &str) -> ManifestRewrite {
    ManifestRewrite {
        modified: vec![Utf8PathBuf::from("package.json")],
        rewritten: vec![RewrittenDeclaration {
            key,
            replacement: replacement.to_string(),
        }],
    }
}

/// Every install field contributes its exact pins, bare or equals-prefixed, while ranges, aliases
/// and peer contracts do not.
#[test]
fn exact_pins_are_read_from_every_install_field() -> eyre::Result<()> {
    let content = indoc! {r#"
        {
          "dependencies": { "bare": "1.2.3", "caret": "^1.2.3", "alias": "npm:other@1.2.3" },
          "devDependencies": { "equals": "=1.2.3", "spaced": "= 1.2.3" },
          "optionalDependencies": { "optional": "2.0.0-rc.1" },
          "peerDependencies": { "peer": "1.0.0" }
        }
    "#};

    let declarations = manifest::exact_install_declarations("apps/a", content)?;

    let read: Vec<String> = declarations
        .iter()
        .map(|ExactDeclaration { key, pin }| format!("{} {} {pin}", key.field, key.dependency))
        .collect();
    assert_eq!(
        read,
        [
            "dependencies bare 1.2.3",
            "devDependencies equals =1.2.3",
            "devDependencies spaced = 1.2.3",
            "optionalDependencies optional 2.0.0-rc.1",
        ]
    );
    assert!(
        declarations
            .iter()
            .all(|declaration| declaration.key.manifest() == "apps/a/package.json")
    );
    Ok(())
}

/// A pin in a second install field, which pnpm's per-importer specifier hides, is judged at its
/// importer: a sibling's resolve moving the copy past it is a new violation naming the field.
#[test]
fn a_moved_second_field_pin_is_a_new_violation() -> eyre::Result<()> {
    let pins = protected(&[("apps/a", TWO_FIELD_MANIFEST)])?;
    let baseline = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.0")], &["4.1.0"]);
    let settled = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.2")], &["4.1.2"]);

    let violations =
        pins.violations_between::<Pnpm>(Some(&baseline), &settled, &AuthorizedPins::default());

    assert_eq!(
        refusal(&violations),
        "the resolve moved an exact pin it was not asked to rewrite: chalk resolves to 4.1.2 for \
         apps/a/package.json, whose devDependencies pins 4.1.0"
    );
    Ok(())
}

/// Two owners, and two fields of one owner, are separate declarations, each judged on its own.
#[test]
fn owners_and_fields_are_judged_separately() -> eyre::Result<()> {
    let both_fields = indoc! {r#"
        { "dependencies": { "chalk": "4.1.0" }, "devDependencies": { "chalk": "=4.1.0" } }
    "#};
    let pins = protected(&[("apps/a", both_fields), ("apps/b", TWO_FIELD_MANIFEST)])?;
    let baseline = pnpm_lock(
        &[("apps/a", "4.1.0", "4.1.0"), ("apps/b", "^4.1.0", "4.1.0")],
        &["4.1.0"],
    );
    // Only b's copy moves, so a's two pins stay satisfied.
    let settled = pnpm_lock(
        &[("apps/a", "4.1.0", "4.1.0"), ("apps/b", "^4.1.0", "4.1.2")],
        &["4.1.0", "4.1.2"],
    );

    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &settled, &AuthorizedPins::default()),
        ["apps/b devDependencies chalk"]
    );
    Ok(())
}

/// A violation the baseline already had at the same version is grandfathered, while a moved bad
/// version is new, and an old mismatch in one importer does not cover a new one in another.
#[test]
fn only_an_unchanged_violation_is_grandfathered() -> eyre::Result<()> {
    let pins = protected(&[
        ("apps/a", TWO_FIELD_MANIFEST),
        ("apps/b", TWO_FIELD_MANIFEST),
    ])?;
    let baseline = pnpm_lock(
        &[("apps/a", "^4.1.0", "4.1.1"), ("apps/b", "^4.1.0", "4.1.0")],
        &["4.1.0", "4.1.1"],
    );
    let none = AuthorizedPins::default();

    // Unchanged
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &baseline, &none),
        NO_VIOLATIONS
    );

    // An old mismatch in a beside a new one in b
    let spread = pnpm_lock(
        &[("apps/a", "^4.1.0", "4.1.1"), ("apps/b", "^4.1.0", "4.1.2")],
        &["4.1.1", "4.1.2"],
    );
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &spread, &none),
        ["apps/b devDependencies chalk"]
    );

    // A changed bad version
    let moved = pnpm_lock(
        &[("apps/a", "^4.1.0", "4.1.2"), ("apps/b", "^4.1.0", "4.1.0")],
        &["4.1.0", "4.1.2"],
    );
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &moved, &none),
        ["apps/a devDependencies chalk"]
    );
    Ok(())
}

/// A newer transitive copy of the pinned name beside the importer's unchanged direct copy moves no
/// pin: the newest copy is never the evidence.
#[test]
fn a_newer_transitive_duplicate_is_not_a_violation() -> eyre::Result<()> {
    let pins = protected(&[("apps/a", TWO_FIELD_MANIFEST)])?;
    let baseline = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.0")], &["4.1.0"]);
    let settled = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.0")], &["4.1.0", "4.1.2"]);

    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &settled, &AuthorizedPins::default()),
        NO_VIOLATIONS
    );
    Ok(())
}

/// A pnpm version recorded with its peer context still matches the pin it was resolved for.
#[test]
fn a_peer_suffixed_resolution_matches_its_pin() -> eyre::Result<()> {
    let pins = protected(&[("apps/a", TWO_FIELD_MANIFEST)])?;
    let lock = pnpm_lock(
        &[("apps/a", "^4.1.0", "4.1.0(supports-color@7.2.0)")],
        &["4.1.0"],
    );

    assert_eq!(
        violated::<Pnpm>(&pins, Some(&lock), &lock, &AuthorizedPins::default()),
        NO_VIOLATIONS
    );
    assert_eq!(
        pins.observe::<Pnpm>(Some(&lock))
            .get(&key("apps/a", "devDependencies", "chalk")),
        &Observed::Resolved("4.1.0".to_string())
    );
    Ok(())
}

/// npm judges each member at the copy its own lookup reaches: a member-local copy shadows a newer
/// hoisted one, while a hoisted copy that moved off the member's pin is a violation.
#[test]
fn npm_judges_the_copy_each_member_resolves() -> eyre::Result<()> {
    let pins = protected(&[
        ("apps/a", TWO_FIELD_MANIFEST),
        (".", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#),
    ])?;
    let lock = |hoisted: &str, nested: Option<&str>| {
        let nested = nested.map_or(String::new(), |version| {
            format!(r#""apps/a/node_modules/chalk": {{ "version": "{version}" }},"#)
        });
        formatdoc! {r#"
            {{
              "name": "root",
              "lockfileVersion": 3,
              "packages": {{
                "": {{ "name": "root", "workspaces": ["apps/a"] }},
                "apps/a": {{ "name": "a", "version": "0.1.0" }},
                {nested}
                "node_modules/a": {{ "resolved": "apps/a", "link": true }},
                "node_modules/chalk": {{ "version": "{hoisted}" }}
              }}
            }}
        "#}
    };
    let baseline = lock("4.1.0", None);
    let none = AuthorizedPins::default();

    // The root moves while a keeps its own copy.
    let nested = lock("4.1.2", Some("4.1.0"));
    assert_eq!(
        violated::<Npm>(&pins, Some(&baseline), &nested, &none),
        [". devDependencies chalk"]
    );

    // Both resolve the moved hoisted copy.
    let hoisted = lock("4.1.2", None);
    assert_eq!(
        violated::<Npm>(&pins, Some(&baseline), &hoisted, &none),
        [". devDependencies chalk", "apps/a devDependencies chalk"]
    );
    Ok(())
}

/// A binding that disappears breaks a required pin but not an optional one, and a pin the baseline
/// never bound may stay unbound but not resolve off its pin.
#[test]
fn a_missing_binding_breaks_only_a_bound_required_pin() -> eyre::Result<()> {
    let required = protected(&[("apps/a", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#)])?;
    let optional = protected(&[(
        "apps/a",
        r#"{ "optionalDependencies": { "chalk": "4.1.0" } }"#,
    )])?;
    let bound = pnpm_lock(&[("apps/a", "4.1.0", "4.1.0")], &["4.1.0"]);
    let unbound = pnpm_lock(&[("apps/b", "4.1.0", "4.1.0")], &["4.1.0"]);
    let wrong = pnpm_lock(&[("apps/a", "4.1.0", "4.1.2")], &["4.1.2"]);
    let none = AuthorizedPins::default();

    // Resolved to missing
    assert_eq!(
        violated::<Pnpm>(&required, Some(&bound), &unbound, &none),
        ["apps/a devDependencies chalk"]
    );
    assert_eq!(
        violated::<Pnpm>(&optional, Some(&bound), &unbound, &none),
        NO_VIOLATIONS
    );

    // Missing to missing
    assert_eq!(
        violated::<Pnpm>(&required, Some(&unbound), &unbound, &none),
        NO_VIOLATIONS
    );

    // Missing to a wrong version, including from no lock at all
    assert_eq!(
        violated::<Pnpm>(&required, Some(&unbound), &wrong, &none),
        ["apps/a devDependencies chalk"]
    );
    assert_eq!(
        violated::<Pnpm>(&required, None, &wrong, &none),
        ["apps/a devDependencies chalk"]
    );
    assert_eq!(
        violated::<Pnpm>(&required, None, &bound, &none),
        NO_VIOLATIONS
    );
    Ok(())
}

/// Under `--rewrite` the change's own rewritten pin is judged against the replacement cooldown
/// wrote, while an unrelated pin beside it stays protected.
#[test]
fn an_authorized_rewrite_exempts_only_its_own_declaration() -> eyre::Result<()> {
    let manifest = indoc! {r#"
        { "dependencies": { "chalk": "4.1.0" }, "devDependencies": { "ansi": "5.0.0" } }
    "#};
    let pins = protected(&[("apps/a", manifest)])?;
    let lock = |chalk: &str, ansi: &str| {
        formatdoc! {"
            lockfileVersion: '9.0'

            importers:

              apps/a:
                dependencies:
                  chalk:
                    specifier: {chalk}
                    version: {chalk}
                devDependencies:
                  ansi:
                    specifier: 5.0.0
                    version: {ansi}
        "}
    };
    let baseline = lock("4.1.0", "5.0.0");
    let mut authorized = AuthorizedPins::default();
    authorized.record(
        &change("chalk", true, false),
        RewriteMode::Always,
        false,
        &rewrite_of(key("apps/a", "dependencies", "chalk"), "4.1.2"),
    );

    // The rewritten pin landed at its replacement.
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &lock("4.1.2", "5.0.0"), &authorized),
        NO_VIOLATIONS
    );

    // The replacement is the contract, so another version still violates it.
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &lock("4.1.1", "5.0.0"), &authorized),
        ["apps/a dependencies chalk"]
    );

    // The unrelated pin is not exempted.
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &lock("4.1.2", "5.0.1"), &authorized),
        ["apps/a devDependencies ansi"]
    );
    Ok(())
}

/// Only the user's `--rewrite`, or `rewrite_pins` for a downgrade, authorizes a direct change's pin
/// edit.
#[test]
fn only_user_authority_records_a_rewrite() {
    let rewrite = rewrite_of(key(".", "dependencies", "chalk"), "1.1.0");
    let recorded = |change: &Change, mode: RewriteMode, rewrite_pins: bool| {
        let mut authorized = AuthorizedPins::default();
        authorized.record(change, mode, rewrite_pins, &rewrite);
        !authorized.replacements.is_empty()
    };

    assert!(recorded(
        &change("chalk", true, false),
        RewriteMode::Always,
        false
    ));
    assert!(recorded(
        &change("chalk", true, true),
        RewriteMode::Auto,
        true
    ));
    assert!(!recorded(
        &change("chalk", true, false),
        RewriteMode::Auto,
        true
    ));
    assert!(!recorded(
        &change("chalk", true, true),
        RewriteMode::Auto,
        false
    ));
    assert!(!recorded(
        &change("chalk", false, false),
        RewriteMode::Always,
        false
    ));
}

/// Equality follows npm: an equals pin with whitespace and a resolution with build metadata still
/// match, and a violation whose build metadata alone changed is grandfathered.
#[test]
fn equality_ignores_spacing_equals_and_build_metadata() -> eyre::Result<()> {
    let pins = protected(&[("apps/a", r#"{ "devDependencies": { "chalk": "= 1.2.3" } }"#)])?;
    let none = AuthorizedPins::default();
    let built = pnpm_lock(
        &[("apps/a", "= 1.2.3", "1.2.3+build.a")],
        &["1.2.3+build.a"],
    );
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&built), &built, &none),
        NO_VIOLATIONS
    );
    let moved = pnpm_lock(&[("apps/a", "= 1.2.3", "1.2.4")], &["1.2.4"]);
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&built), &moved, &none),
        ["apps/a devDependencies chalk"]
    );

    let was = pnpm_lock(
        &[("apps/a", "= 1.2.3", "1.2.4+build.a")],
        &["1.2.4+build.a"],
    );
    let now = pnpm_lock(
        &[("apps/a", "= 1.2.3", "1.2.4+build.b")],
        &["1.2.4+build.b"],
    );
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&was), &now, &none),
        NO_VIOLATIONS
    );
    Ok(())
}

/// Locks without per-member attribution report every pin unknown and violate nothing, rather than
/// guessing from the name's newest copy.
#[test]
fn unattributed_locks_protect_nothing() -> eyre::Result<()> {
    let pins = protected(&[(".", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#)])?;
    let none = AuthorizedPins::default();
    let yarn = |version: &str| {
        formatdoc! {r#"
            chalk@^4.1.0:
              version "{version}"
              resolved "https://registry.yarnpkg.com/chalk/-/chalk-{version}.tgz"
        "#}
    };
    assert_eq!(
        violated::<Yarn>(&pins, Some(&yarn("4.1.0")), &yarn("4.1.2"), &none),
        NO_VIOLATIONS
    );
    assert_eq!(
        violated::<Yarn>(&pins, None, &yarn("4.1.2"), &none),
        NO_VIOLATIONS
    );
    assert_eq!(
        pins.observe::<Yarn>(Some(&yarn("4.1.2")))
            .get(&key(".", "devDependencies", "chalk")),
        &Observed::Unknown
    );
    // A version 0 bun.lock resolves member dependencies through the workspace's package row, which
    // the reader does not model, so it attributes nothing either.
    let bun_v0 = |version: &str| {
        formatdoc! {r#"
            {{
              "lockfileVersion": 0,
              "workspaces": {{ "": {{ "devDependencies": {{ "chalk": "4.1.0" }} }} }},
              "packages": {{ "chalk": ["chalk@{version}", "", {{}}, "sha512-x"] }}
            }}
        "#}
    };
    assert_eq!(
        violated::<Bun>(&pins, Some(&bun_v0("4.1.0")), &bun_v0("4.1.2"), &none),
        NO_VIOLATIONS
    );
    let npm_v1 = |version: &str| {
        formatdoc! {r#"
            {{ "lockfileVersion": 1, "dependencies": {{ "chalk": {{ "version": "{version}" }} }} }}
        "#}
    };
    assert_eq!(
        violated::<Npm>(&pins, Some(&npm_v1("4.1.0")), &npm_v1("4.1.2"), &none),
        NO_VIOLATIONS
    );
    Ok(())
}

/// A `bun.lock` whose root declares `chalk` at a caret and whose member `packages/a` (named
/// `pkg-a`) declares it in `field` at `specifier`, the root key holding `root` and the member's
/// nested key holding `nested` when given.
fn bun_lock(field: &str, specifier: &str, root: Option<&str>, nested: Option<&str>) -> String {
    let entry = |key: &str, version: Option<&str>| {
        version.map_or(String::new(), |version| {
            format!(r#""{key}": ["chalk@{version}", "", {{}}, "sha512-x"],"#)
        })
    };
    let root = entry("chalk", root);
    let nested = entry("pkg-a/chalk", nested);
    formatdoc! {r#"
        {{
          "lockfileVersion": 2,
          "workspaces": {{
            "": {{ "name": "root", "dependencies": {{ "chalk": "^4.1.0" }} }},
            "packages/a": {{ "name": "pkg-a", "{field}": {{ "chalk": "{specifier}" }} }},
          }},
          "packages": {{
            {root}
            "pkg-a": ["pkg-a@workspace:packages/a"],
            {nested}
          }},
        }}
    "#}
}

/// Bun judges each member at the copy its own package key binds: a member's nested copy holds its
/// pin beside a newer root copy, while the root copy it falls back to moving off the pin is a
/// violation.
#[test]
fn bun_judges_the_copy_each_member_binds() -> eyre::Result<()> {
    let pins = protected(&[(
        "packages/a",
        r#"{ "devDependencies": { "chalk": "4.1.0" } }"#,
    )])?;
    let none = AuthorizedPins::default();
    let lock =
        |root: &str, nested: Option<&str>| bun_lock("devDependencies", "4.1.0", Some(root), nested);
    let baseline = lock("4.1.0", None);
    assert_eq!(
        pins.observe::<Bun>(Some(&baseline))
            .get(&key("packages/a", "devDependencies", "chalk")),
        &Observed::Resolved("4.1.0".to_string())
    );

    // The root moves while the member keeps its own nested copy, so the newest copy is not the
    // evidence.
    assert_eq!(
        violated::<Bun>(&pins, Some(&baseline), &lock("4.1.2", Some("4.1.0")), &none),
        NO_VIOLATIONS
    );

    // The member's binding follows the moved root copy.
    assert_eq!(
        violated::<Bun>(&pins, Some(&baseline), &lock("4.1.2", None), &none),
        ["packages/a devDependencies chalk"]
    );

    // A nested copy that moves off the pin is a violation even with the root copy in place.
    assert_eq!(
        violated::<Bun>(&pins, Some(&baseline), &lock("4.1.0", Some("4.1.1")), &none),
        ["packages/a devDependencies chalk"]
    );
    Ok(())
}

/// The grandfathering and authorization rules apply to Bun bindings as they do to pnpm's: an
/// unchanged old violation stays, a moved one is new, and an authorized rewrite is judged against
/// its replacement.
#[test]
fn bun_pins_follow_grandfathering_and_authorization() -> eyre::Result<()> {
    let pins = protected(&[(
        "packages/a",
        r#"{ "devDependencies": { "chalk": "4.1.0" } }"#,
    )])?;
    let none = AuthorizedPins::default();
    let lock = |root: &str| bun_lock("devDependencies", "4.1.0", Some(root), None);

    // Grandfathered
    assert_eq!(
        violated::<Bun>(&pins, Some(&lock("4.1.2")), &lock("4.1.2"), &none),
        NO_VIOLATIONS
    );
    assert_eq!(
        violated::<Bun>(&pins, Some(&lock("4.1.2")), &lock("4.1.3"), &none),
        ["packages/a devDependencies chalk"]
    );

    // Authorized
    let mut authorized = AuthorizedPins::default();
    authorized.record(
        &change("chalk", true, false),
        RewriteMode::Always,
        false,
        &rewrite_of(key("packages/a", "devDependencies", "chalk"), "4.1.2"),
    );
    assert_eq!(
        violated::<Bun>(&pins, Some(&lock("4.1.0")), &lock("4.1.2"), &authorized),
        NO_VIOLATIONS
    );
    assert_eq!(
        violated::<Bun>(&pins, Some(&lock("4.1.0")), &lock("4.1.1"), &authorized),
        ["packages/a devDependencies chalk"]
    );
    Ok(())
}

/// Only a dependency the member's lock entry stops declaring, or an optional one no key holds, is
/// missing.
/// A required one no key holds is a lock Bun refuses to load, and a peer is bound by version, so
/// both are unknown.
#[test]
fn bun_absence_is_missing_only_where_the_lock_proves_it() -> eyre::Result<()> {
    let required = protected(&[(
        "packages/a",
        r#"{ "devDependencies": { "chalk": "4.1.0" } }"#,
    )])?;
    let optional = protected(&[(
        "packages/a",
        r#"{ "optionalDependencies": { "chalk": "4.1.0" } }"#,
    )])?;
    let none = AuthorizedPins::default();
    let observed = |pins: &ProtectedPins, field: &'static str, lock: &str| {
        pins.observe::<Bun>(Some(lock))
            .get(&key("packages/a", field, "chalk"))
            .clone()
    };
    let bound = bun_lock("devDependencies", "4.1.0", Some("4.1.0"), None);

    // A required dependency without a key
    let unkeyed = bun_lock("devDependencies", "4.1.0", None, None);
    assert_eq!(
        observed(&required, "devDependencies", &unkeyed),
        Observed::Unknown
    );
    assert_eq!(
        violated::<Bun>(&required, Some(&bound), &unkeyed, &none),
        NO_VIOLATIONS
    );

    // An optional dependency without a key
    let unresolved = bun_lock("optionalDependencies", "4.1.0", None, None);
    assert_eq!(
        observed(&optional, "optionalDependencies", &unresolved),
        Observed::Missing
    );
    let installed = bun_lock("optionalDependencies", "4.1.0", Some("4.1.0"), None);
    assert_eq!(
        violated::<Bun>(&optional, Some(&installed), &unresolved, &none),
        NO_VIOLATIONS
    );

    // A dropped declaration
    let dropped = bound.replace(
        r#""devDependencies": { "chalk": "4.1.0" }"#,
        r#""devDependencies": { "other": "1.0.0" }"#,
    );
    assert_ne!(dropped, bound);
    assert_eq!(
        observed(&required, "devDependencies", &dropped),
        Observed::Missing
    );
    assert_eq!(
        violated::<Bun>(&required, Some(&bound), &dropped, &none),
        ["packages/a devDependencies chalk"]
    );

    // A peer declaration of the pinned name binds by version, not by key
    let peer = bun_lock("peerDependencies", "^4.0.0", Some("4.1.2"), None);
    assert_eq!(
        observed(&required, "devDependencies", &peer),
        Observed::Unknown
    );
    assert_eq!(
        violated::<Bun>(&required, Some(&bound), &peer, &none),
        NO_VIOLATIONS
    );
    Ok(())
}

/// The baseline's manifests, the root's and every member's, are the protected set, read from the
/// bytes captured before any mutation.
#[test]
fn pins_are_read_from_the_baseline_manifests() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = Utf8Path::from_path(directory.path())
        .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
    std::fs::create_dir_all(root.join("apps/a"))?;
    std::fs::write(
        root.join("package.json"),
        r#"{ "dependencies": { "root-pin": "1.0.0" } }"#,
    )?;
    std::fs::write(root.join("apps/a/package.json"), TWO_FIELD_MANIFEST)?;
    let baseline = MutationBaseline::read(
        root,
        [
            Utf8PathBuf::from("pnpm-lock.yaml"),
            Utf8PathBuf::from("package.json"),
            Utf8Path::new("apps/a").join("package.json"),
        ],
    )?;
    // A later edit is not the baseline.
    std::fs::write(root.join("apps/a/package.json"), "{}")?;

    let pins = ProtectedPins::from_baseline(&baseline)?;

    assert_eq!(
        declared(&pins),
        [
            "package.json root-pin 1.0.0",
            "apps/a/package.json chalk 4.1.0"
        ]
    );
    Ok(())
}

/// Each protected declaration as `manifest dependency pin`.
fn declared(pins: &ProtectedPins) -> Vec<String> {
    pins.declarations
        .iter()
        .map(|ExactDeclaration { key, pin }| format!("{} {} {pin}", key.manifest(), key.dependency))
        .collect()
}

/// A member manifest that is a symbolic or a hard link, which a journaled write refuses, is still
/// read into the baseline, so its pins stay protected.
#[cfg(unix)]
#[test]
fn linked_member_manifests_are_read_into_the_baseline() -> eyre::Result<()> {
    let directory = tempfile::tempdir()?;
    let root = Utf8Path::from_path(directory.path())
        .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
    std::fs::create_dir_all(root.join("apps/symlinked"))?;
    std::fs::create_dir_all(root.join("apps/hardlinked"))?;
    std::fs::create_dir_all(root.join("shared"))?;
    std::fs::write(root.join("shared/symlinked.json"), TWO_FIELD_MANIFEST)?;
    std::fs::write(
        root.join("shared/hardlinked.json"),
        r#"{ "devDependencies": { "ansi": "5.0.0" } }"#,
    )?;
    std::os::unix::fs::symlink(
        root.join("shared/symlinked.json"),
        root.join("apps/symlinked/package.json"),
    )?;
    std::fs::hard_link(
        root.join("shared/hardlinked.json"),
        root.join("apps/hardlinked/package.json"),
    )?;

    let baseline = MutationBaseline::read(
        root,
        [
            Utf8Path::new("apps/symlinked").join("package.json"),
            Utf8Path::new("apps/hardlinked").join("package.json"),
        ],
    )?;
    let pins = ProtectedPins::from_baseline(&baseline)?;

    assert_eq!(
        declared(&pins),
        [
            "apps/hardlinked/package.json ansi 5.0.0",
            "apps/symlinked/package.json chalk 4.1.0"
        ]
    );
    Ok(())
}

/// node-semver reads a `v`-prefixed version, alone or after a spaced `=`, as the same exact pin,
/// so every spelling is protected, matched, and grandfathered as the bare version.
#[test]
fn v_prefixed_pins_are_exact_pins() -> eyre::Result<()> {
    for spelling in ["v4.1.0", "=v4.1.0", "= v4.1.0"] {
        assert_eq!(crate::version::exact_pin_version(spelling), Some("4.1.0"));
        let manifest = formatdoc! {r#"
            {{ "dependencies": {{ "chalk": "^4.1.0" }}, "devDependencies": {{ "chalk": "{spelling}" }} }}
        "#};
        let pins = protected(&[("apps/a", &manifest)])?;
        let baseline = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.0")], &["4.1.0"]);
        let none = AuthorizedPins::default();

        // On its pin
        assert_eq!(
            violated::<Pnpm>(&pins, Some(&baseline), &baseline, &none),
            NO_VIOLATIONS,
            "{spelling}"
        );

        // Moved past the pin hidden beside the caret
        let moved = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.2")], &["4.1.2"]);
        assert_eq!(
            violated::<Pnpm>(&pins, Some(&baseline), &moved, &none),
            ["apps/a devDependencies chalk"],
            "{spelling}"
        );

        // An unchanged mismatch stays grandfathered under a replacement spelled another way.
        let mismatched = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.1")], &["4.1.1"]);
        let mut respelled = AuthorizedPins::default();
        respelled.record(
            &change("chalk", true, false),
            RewriteMode::Always,
            false,
            &rewrite_of(key("apps/a", "devDependencies", "chalk"), "4.1.0"),
        );
        assert_eq!(
            violated::<Pnpm>(&pins, Some(&mismatched), &mismatched, &respelled),
            NO_VIOLATIONS,
            "{spelling}"
        );
    }
    for invalid in ["==4.1.0", "vv4.1.0", "v=4.1.0", "^v4.1.0"] {
        assert_eq!(
            crate::version::exact_pin_version(invalid),
            None,
            "{invalid}"
        );
    }
    Ok(())
}

/// A binding pnpm records without a registry version of the pinned name, a local reference or an
/// alias to another package, cannot be judged and violates nothing, while an importer that no
/// longer lists the dependency at all is still missing it.
#[test]
fn only_a_dropped_binding_is_missing() -> eyre::Result<()> {
    let pins = protected(&[("apps/a", TWO_FIELD_MANIFEST)])?;
    let baseline = pnpm_lock(&[("apps/a", "^4.1.0", "4.1.0")], &["4.1.0"]);
    let none = AuthorizedPins::default();
    let observed = |version: &str| {
        let lock = pnpm_lock(&[("apps/a", "^4.1.0", version)], &[]);
        pins.observe::<Pnpm>(Some(&lock))
            .get(&key("apps/a", "devDependencies", "chalk"))
            .clone()
    };

    // Local references
    for local in ["link:../chalk", "file:../chalk", "workspace:*"] {
        assert_eq!(observed(local), Observed::Unknown, "{local}");
        let settled = pnpm_lock(&[("apps/a", "^4.1.0", local)], &[]);
        assert_eq!(
            violated::<Pnpm>(&pins, Some(&baseline), &settled, &none),
            NO_VIOLATIONS,
            "{local}"
        );
    }

    // Aliases: another package is unknown, the pinned name itself is judged by its version.
    for alias in ["other@1.0.0", "npm:other@1.0.0", "@scope/other@4.1.0"] {
        assert_eq!(observed(alias), Observed::Unknown, "{alias}");
    }
    assert_eq!(
        observed("chalk@4.1.0(supports-color@7.2.0)"),
        Observed::Resolved("4.1.0".to_string())
    );
    assert_eq!(
        observed("npm:chalk@4.1.2"),
        Observed::Resolved("4.1.2".to_string())
    );

    // A dropped binding
    let dropped = pnpm_lock(&[("apps/b", "^4.1.0", "4.1.0")], &["4.1.0"]);
    assert_eq!(
        pins.observe::<Pnpm>(Some(&dropped))
            .get(&key("apps/a", "devDependencies", "chalk")),
        &Observed::Missing
    );
    assert_eq!(
        violated::<Pnpm>(&pins, Some(&baseline), &dropped, &none),
        ["apps/a devDependencies chalk"]
    );
    Ok(())
}

/// A v2 or v3 npm lock of a root and its member `apps/a`, holding the given extra `packages`
/// entries (each a `"key": { … }` pair) beside the root, member, and member-link entries.
fn npm_tree(lockfile_version: u8, entries: &[String]) -> String {
    let entries = entries.join(",\n    ");
    formatdoc! {r#"
        {{
          "name": "root",
          "lockfileVersion": {lockfile_version},
          "packages": {{
            "": {{ "name": "root", "workspaces": ["apps/a"] }},
            "apps/a": {{ "name": "a", "version": "0.1.0" }},
            "node_modules/a": {{ "resolved": "apps/a", "link": true }},
            {entries}
          }}
        }}
    "#}
}

/// The `packages` entry installing `version` at `key`, with `name` recorded when given, as npm
/// records an alias (`"chalk": "npm:other@2.0.0"` installs `other` at `node_modules/chalk`).
fn npm_entry(key: &str, name: Option<&str>, version: &str) -> String {
    let name = name.map_or(String::new(), |name| format!(r#""name": "{name}", "#));
    format!(r#""{key}": {{ {name}"version": "{version}" }}"#)
}

/// What `lock` resolves the root's and `apps/a`'s `chalk` pins to.
fn npm_observed(pins: &ProtectedPins, lock: &str) -> [Observed; 2] {
    let observed = pins.observe::<Npm>(Some(lock));
    [
        observed.get(&key(".", "devDependencies", "chalk")).clone(),
        observed
            .get(&key("apps/a", "devDependencies", "chalk"))
            .clone(),
    ]
}

/// An npm alias installs another package at the alias's own slot and records that package's
/// `name` there, so the slot binds no version of the pinned name: newly aliasing a pinned name, or
/// moving the alias target, is unknown and violates nothing, in v2 and v3 locks alike, while an
/// entry whose `name` is the slot's own (or absent) is the genuine copy and is still judged.
#[test]
fn an_npm_alias_binds_no_version_of_the_pinned_name() -> eyre::Result<()> {
    let pins = protected(&[
        (".", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#),
        ("apps/a", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#),
    ])?;
    let none = AuthorizedPins::default();
    let hoisted =
        |name: Option<&str>, version: &str| npm_entry("node_modules/chalk", name, version);
    for lockfile_version in [2, 3] {
        let tree = |entries: &[String]| npm_tree(lockfile_version, entries);
        let genuine = tree(&[hoisted(None, "4.1.0")]);
        for target in ["other", "@scope/other"] {
            let aliased = tree(&[hoisted(Some(target), "2.0.0")]);
            assert_eq!(
                npm_observed(&pins, &aliased),
                [Observed::Unknown, Observed::Unknown],
                "v{lockfile_version} {target}"
            );

            // A newly introduced alias
            assert_eq!(
                violated::<Npm>(&pins, Some(&genuine), &aliased, &none),
                NO_VIOLATIONS,
                "v{lockfile_version} {target}"
            );

            // A moved alias target
            let moved = tree(&[hoisted(Some(target), "2.1.0")]);
            assert_eq!(
                violated::<Npm>(&pins, Some(&aliased), &moved, &none),
                NO_VIOLATIONS,
                "v{lockfile_version} {target}"
            );
        }

        // A `name` equal to the slot's is the genuine copy, judged as one.
        let named = tree(&[hoisted(Some("chalk"), "4.1.2")]);
        assert_eq!(
            npm_observed(&pins, &named),
            [
                Observed::Resolved("4.1.2".to_string()),
                Observed::Resolved("4.1.2".to_string())
            ]
        );
        assert_eq!(
            violated::<Npm>(&pins, Some(&genuine), &named, &none),
            [". devDependencies chalk", "apps/a devDependencies chalk"]
        );

        // A genuine copy moved off the pin still violates, and a dropped one is missing.
        let moved = tree(&[hoisted(None, "4.1.2")]);
        assert_eq!(
            violated::<Npm>(&pins, Some(&genuine), &moved, &none),
            [". devDependencies chalk", "apps/a devDependencies chalk"]
        );
        let dropped = tree(&[npm_entry("node_modules/ansi", None, "5.0.0")]);
        assert_eq!(
            npm_observed(&pins, &dropped),
            [Observed::Missing, Observed::Missing]
        );
        assert_eq!(
            violated::<Npm>(&pins, Some(&genuine), &dropped, &none),
            [". devDependencies chalk", "apps/a devDependencies chalk"]
        );
    }
    Ok(())
}

/// A member's nested alias occupies the member's own slot, so it shadows the genuine hoisted copy:
/// the member binds no version of the name, rather than the ancestor's copy the alias hides, while
/// the root still resolves that ancestor and is judged by it.
#[test]
fn a_nested_npm_alias_shadows_the_genuine_ancestor() -> eyre::Result<()> {
    let pins = protected(&[
        (".", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#),
        ("apps/a", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#),
    ])?;
    let none = AuthorizedPins::default();
    let lock = |hoisted: &str| {
        npm_tree(
            3,
            &[
                npm_entry("apps/a/node_modules/chalk", Some("other"), "4.1.2"),
                npm_entry("node_modules/chalk", None, hoisted),
            ],
        )
    };
    let baseline = npm_tree(3, &[npm_entry("node_modules/chalk", None, "4.1.0")]);

    assert_eq!(
        npm_observed(&pins, &lock("4.1.0")),
        [Observed::Resolved("4.1.0".to_string()), Observed::Unknown]
    );
    assert_eq!(
        violated::<Npm>(&pins, Some(&baseline), &lock("4.1.0"), &none),
        NO_VIOLATIONS
    );
    assert_eq!(
        violated::<Npm>(&pins, Some(&baseline), &lock("4.1.2"), &none),
        [". devDependencies chalk"]
    );
    Ok(())
}

/// A hoisted workspace link is judged by its target's `name`: a link to the member that is the
/// pinned package binds its version, while a link at the pinned name's slot to another member is
/// an alias and binds none.
#[test]
fn an_npm_workspace_link_is_judged_by_its_targets_name() -> eyre::Result<()> {
    let pins = protected(&[(".", r#"{ "devDependencies": { "chalk": "4.1.0" } }"#)])?;
    let linked = |target_name: &str, version: &str| {
        npm_tree(
            3,
            &[
                npm_entry("packages/chalk", Some(target_name), version),
                r#""node_modules/chalk": { "resolved": "packages/chalk", "link": true }"#
                    .to_string(),
            ],
        )
    };
    let observed = |lock: &str| {
        pins.observe::<Npm>(Some(lock))
            .get(&key(".", "devDependencies", "chalk"))
            .clone()
    };

    assert_eq!(
        observed(&linked("chalk", "4.1.0")),
        Observed::Resolved("4.1.0".to_string())
    );
    assert_eq!(observed(&linked("other", "4.1.2")), Observed::Unknown);
    assert_eq!(
        violated::<Npm>(
            &pins,
            Some(&linked("chalk", "4.1.0")),
            &linked("other", "4.1.2"),
            &AuthorizedPins::default()
        ),
        NO_VIOLATIONS
    );
    Ok(())
}
