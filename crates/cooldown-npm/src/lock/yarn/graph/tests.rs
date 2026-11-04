use super::*;
use crate::lock::yarn_fixtures;
use crate::lock::{NodeLock, Yarn};
use cooldown_core::CoreError;
use indoc::{formatdoc, indoc};

/// Each row of `rows` as `name@version`, sorted.
fn releases(rows: &[NameVersion]) -> Vec<String> {
    let mut releases: Vec<String> = rows
        .iter()
        .map(|NameVersion { name, version, .. }| format!("{name}@{version}"))
        .collect();
    releases.sort();
    releases
}

/// Every alias form Yarn writes reads as its target's release, under the target's own name and
/// with the entry's registry origin, and a quoted key holding a comma stays one selector.
#[test]
fn aliases_read_as_their_targets_release() -> Result<(), CoreError> {
    let rows = Yarn::parse(yarn_fixtures::ALIASES.lock)?;

    // The foreign, scoped, and same-name aliases each name their target, and the local
    // directory `dir,other@1.0.0` names only `pkg`.
    // `color-name@1.1.4` appears twice, once for the alias's entry and once for the genuine one.
    assert_eq!(
        releases(&rows),
        [
            "@sindresorhus/is@4.6.0",
            "ansi-styles@4.3.0",
            "chalk@4.1.2",
            "color-convert@2.0.1",
            "color-name@1.1.4",
            "color-name@1.1.4",
            "has-flag@4.0.0",
            "is-number@6.0.0",
            "is-number@7.0.0",
            "ms@2.1.3",
            "pkg@0.3.0",
            "supports-color@7.2.0",
        ]
    );

    // An alias's row keeps the registry tarball it was fetched from, so its advisory identity is
    // the target's.
    let origin_of = |name: &str, version: &str| {
        rows.iter()
            .find(|row| row.name == name && row.version == version)
            .map(|row| row.origin.clone())
    };
    assert_eq!(
        origin_of("is-number", "6.0.0"),
        Some(LockOrigin::Url(
            "https://registry.yarnpkg.com/is-number/-/is-number-6.0.0.tgz".into()
        ))
    );
    assert_eq!(
        origin_of("@sindresorhus/is", "4.6.0"),
        Some(LockOrigin::Url(
            "https://registry.yarnpkg.com/@sindresorhus/is/-/is-4.6.0.tgz".into()
        ))
    );
    assert_eq!(origin_of("pkg", "0.3.0"), Some(LockOrigin::Unrecorded));
    Ok(())
}

/// A release only a foreign alias installs is alias-only, while a genuine registry copy of the
/// same release, in the alias's entry or its own, gives it back to its name.
#[test]
fn alias_only_versions_hold_out_what_only_aliases_install() {
    let aliased = Yarn::alias_only_versions(yarn_fixtures::ALIASES.lock);

    // Foreign aliases, a scoped alias name, and a scoped target
    for (name, version) in [
        ("is-number", "6.0.0"),
        ("ms", "2.1.3"),
        ("@sindresorhus/is", "4.6.0"),
    ] {
        assert!(aliased.contains(name, version), "{name}@{version}");
    }

    // The genuine copy of the alias target's name at another major, a release `chalk` resolves
    // in an entry of its own, and a same-name alias sharing the genuine range's entry
    for (name, version) in [
        ("is-number", "7.0.0"),
        ("color-name", "1.1.4"),
        ("has-flag", "4.0.0"),
    ] {
        assert!(!aliased.contains(name, version), "{name}@{version}");
    }
}

/// A registry range of the release's own name that the root `resolutions` reroutes to GitHub
/// resolves no registry copy, so the alias's release stays alias-only.
#[test]
fn a_rerouted_own_name_range_restores_no_alias_target() {
    let aliased = Yarn::alias_only_versions(yarn_fixtures::RESOLVED_TO_GIT.lock);

    assert!(aliased.contains("is-number", "7.0.0"));
}

/// Only an entry whose fields are what Yarn writes proves a registry copy of its own name: a
/// `resolved` or `name` holding no string leaves the alias's release alias-only, while an entry
/// recording no `resolved` at all restores it.
#[test]
fn a_malformed_entry_restores_no_alias_target() {
    let lock_with = |fields: &str| {
        formatdoc! {r#"
            # yarn lockfile v1


            "alias@npm:target@^1.0.0":
              version "1.0.0"
              resolved "https://registry.yarnpkg.com/target/-/target-1.0.0.tgz#abc"

            target@^1.0.0:
              version "1.0.0"
            {fields}
        "#}
    };

    // Synthetic: Yarn writes every field as a string.
    for fields in ["  resolved true", "  name true"] {
        let aliased = Yarn::alias_only_versions(&lock_with(fields));
        assert!(aliased.contains("target", "1.0.0"), "{fields}");
    }
    let aliased = Yarn::alias_only_versions(&lock_with(""));
    assert!(!aliased.contains("target", "1.0.0"));
}

/// A same-name selector of a registry protocol restores its release whichever registry protocol
/// spells it.
#[test]
fn a_same_name_registry_protocol_restores_its_release() {
    // Synthetic: Yarn Classic resolves `yarn:` through its second registry, like `npm:`.
    let lock = indoc! {r#"
        # yarn lockfile v1


        "alias@yarn:target@^1.0.0", "target@yarn:target@^1.0.0":
          version "1.0.0"
          resolved "https://registry.yarnpkg.com/target/-/target-1.0.0.tgz#abc"

        "other@yarn:left-pad@^1.0.0":
          version "1.3.0"
          resolved "https://registry.yarnpkg.com/left-pad/-/left-pad-1.3.0.tgz#abc"
    "#};

    let aliased = Yarn::alias_only_versions(lock);

    assert!(!aliased.contains("target", "1.0.0"));
    assert!(aliased.contains("left-pad", "1.3.0"));
}

/// Each selector names the package Yarn's pattern split and registry resolver read from it.
#[test]
fn selectors_decode_as_yarn_decodes_them() {
    let decoded = |key: &str, recorded: Option<&str>| {
        Selector::decode(key, recorded).map(
            |Selector {
                 package,
                 aliased,
                 registry,
             }| (package, aliased, registry),
        )
    };
    let selector = |package: &str, aliased: bool, registry: bool| {
        Some((package.to_string(), aliased, registry))
    };

    // Plain, scoped, and bare selectors
    assert_eq!(
        decoded("lodash@^4.17.0", None),
        selector("lodash", false, true)
    );
    assert_eq!(
        decoded("@babel/core@^7.0.0", None),
        selector("@babel/core", false, true)
    );
    assert_eq!(decoded("chalk", None), selector("chalk", false, true));
    assert_eq!(decoded("chalk@", None), selector("chalk", false, true));

    // Aliases, with and without a target range
    assert_eq!(
        decoded("isnum@npm:is-number@^6.0.0", None),
        selector("is-number", true, true)
    );
    assert_eq!(
        decoded("@acme/ms@npm:ms@^2.1.0", None),
        selector("ms", true, true)
    );
    assert_eq!(
        decoded("sis@npm:@sindresorhus/is@^4.0.0", None),
        selector("@sindresorhus/is", true, true)
    );
    assert_eq!(
        decoded("isnum@npm:is-number", None),
        selector("is-number", true, true)
    );
    assert_eq!(
        decoded("sis@npm:@sindresorhus/is", None),
        selector("@sindresorhus/is", true, true)
    );
    assert_eq!(
        decoded("has-flag@npm:has-flag@^4.0.0", None),
        selector("has-flag", false, true)
    );
    assert_eq!(
        decoded("pad@yarn:left-pad@^1.0.0", None),
        selector("left-pad", true, true)
    );

    // An alias names its target whatever name the entry records.
    assert_eq!(
        decoded("isnum@npm:is-number@^6.0.0", Some("isnum")),
        selector("is-number", true, true)
    );

    // Sources of their own
    assert_eq!(
        decoded("pkg@file:./dir,other@1.0.0", None),
        selector("pkg", false, false)
    );
    assert_eq!(
        decoded("is-number@github:jonschlinkert/is-number#7.0.0", None),
        selector("is-number", false, false)
    );

    // A recorded name that differs from the selector's installs another package under it.
    assert_eq!(
        decoded("ghost@^1.0.0", Some("actual")),
        selector("actual", true, true)
    );
    assert_eq!(decoded("", None), None);
    assert_eq!(decoded("isnum@npm:", None), None);
}

/// A selector a later entry lists again belongs to that entry alone, as in Yarn's reader, so an
/// entry left without a selector yields no row.
#[test]
fn a_later_entry_owns_a_repeated_selector() -> Result<(), CoreError> {
    // Synthetic: Yarn never writes a selector twice.
    let lock = indoc! {r#"
        # yarn lockfile v1


        lodash@^4.17.0:
          version "4.17.20"

        lodash@^4.17.0, lodash@^4.17.21:
          version "4.17.21"
    "#};

    assert_eq!(releases(&Yarn::parse(lock)?), ["lodash@4.17.21"]);
    Ok(())
}

/// A lock Yarn's own reader refuses is unreadable rather than read line by line.
#[test]
fn a_lock_yarn_refuses_is_unreadable() {
    let lock = indoc! {r#"
        # yarn lockfile v1


        chalk@^4.1.0:
          version "4.1.2"
    "#};
    let conflicted = formatdoc! {r#"
        {lock}
        <<<<<<< HEAD
        ms@^2.0.0:
          version "2.1.3"
        =======
        >>>>>>> other
    "#};

    for refused in [
        lock.replace("yarn lockfile v1", "yarn lockfile v2"),
        conflicted,
    ] {
        assert!(
            matches!(Yarn::parse(&refused), Err(CoreError::LockUnreadable(_))),
            "{refused}"
        );
    }
}
