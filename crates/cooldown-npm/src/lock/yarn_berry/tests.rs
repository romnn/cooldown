use super::super::yarn_berry_fixtures::{EVERY_SOURCE, LITERAL_PATHS, RESOLUTIONS, SCHEMAS};
use super::{
    BerryLock, Descriptor, Ident, Locator, ReferenceParts, YarnBerry, combined_key_descriptors,
    is_semver_range, is_tag, is_workspace_path, legacy_patch_inner, percent_decode,
};
use crate::lock::{LockOrigin, NameVersion, NodeLock, Yarn, is_yarn_berry_lock};
use color_eyre::eyre;
use cooldown_core::CoreError;
use indoc::{formatdoc, indoc};
use std::collections::BTreeSet;

/// The `(name, version)` of every row, sorted.
fn releases(rows: &[NameVersion]) -> BTreeSet<(String, String)> {
    rows.iter()
        .map(|NameVersion { name, version, .. }| (name.clone(), version.clone()))
        .collect()
}

/// `pairs` as a sorted set of owned `(name, version)` rows.
fn expected(pairs: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    pairs
        .iter()
        .map(|(name, version)| ((*name).to_string(), (*version).to_string()))
        .collect()
}

/// The message of a [`CoreError::LockUnreadable`], or a failure naming what was returned instead.
fn unreadable_message(content: &str) -> eyre::Result<String> {
    match BerryLock::read(content) {
        Err(CoreError::LockUnreadable(message)) => Ok(message),
        Err(err) => eyre::bail!("expected an unreadable lock, got {err:?}"),
        Ok(lock) => eyre::bail!("expected an unreadable lock, read {lock:?}"),
    }
}

/// The registry releases [`SCHEMAS`] resolves, which every schema family records alike.
const SCHEMA_RELEASES: &[(&str, &str)] = &[
    ("@sindresorhus/is", "4.6.0"),
    ("ansi-styles", "4.3.0"),
    ("chalk", "4.1.0"),
    ("chalk", "4.1.2"),
    ("color-convert", "2.0.1"),
    ("color-name", "1.1.4"),
    ("es-errors", "1.3.0"),
    ("function-bind", "1.1.2"),
    ("has-flag", "4.0.0"),
    ("hasown", "2.0.4"),
    ("is-core-module", "2.17.0"),
    ("is-number", "6.0.0"),
    ("is-number", "7.0.0"),
    ("path-parse", "1.0.7"),
    ("resolve", "1.22.13"),
    ("supports-color", "7.2.0"),
    ("supports-preserve-symlinks-flag", "1.0.0"),
];

/// Only npm locators are registry rows: workspaces, the `link:`, `portal:`, `file:`, `exec:`,
/// git, and tarball URL entries, and the patched `resolve` are left out, even where they record a
/// version, a checksum, and a hard link.
/// Their own registry dependencies are entries of their own and stay in, and the patch's inner
/// npm release is read like any other.
#[test]
fn every_source_inventories_only_its_npm_releases() -> eyre::Result<()> {
    let rows = YarnBerry::parse(EVERY_SOURCE.lock)?;

    assert_eq!(
        releases(&rows),
        expected(&[
            ("@esbuild/darwin-arm64", "0.25.0"),
            ("@sindresorhus/is", "4.6.0"),
            ("ansi-styles", "4.3.0"),
            ("chalk", "4.1.0"),
            ("chalk", "4.1.2"),
            ("color-convert", "2.0.1"),
            ("color-name", "1.1.4"),
            ("es-errors", "1.3.0"),
            ("function-bind", "1.1.2"),
            ("has-flag", "4.0.0"),
            ("hasown", "2.0.4"),
            ("is-core-module", "2.17.0"),
            ("is-number", "6.0.0"),
            ("is-number", "7.0.0"),
            ("is-odd", "3.0.1"),
            ("path-parse", "1.0.7"),
            ("resolve", "1.22.13"),
            ("supports-color", "7.2.0"),
            ("supports-preserve-symlinks-flag", "1.0.0"),
        ])
    );
    // One row per release, so no release appears twice.
    assert_eq!(rows.len(), 19);
    // A checksum and a hard link prove no registry, so every row's archive comes only from the
    // registry Yarn's configuration routes it to.
    assert!(
        rows.iter()
            .all(|row| row.origin == crate::lock::LockOrigin::ConfiguredRegistry),
        "{rows:?}"
    );
    Ok(())
}

/// Each workspace the lock records is a member path, the root as `.`, whatever it declares.
#[test]
fn workspaces_are_member_paths() {
    assert_eq!(
        YarnBerry::member_paths(EVERY_SOURCE.lock)
            .into_iter()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([
            ".".to_string(),
            "packages/a".to_string(),
            "packages/b".to_string()
        ])
    );
    assert_eq!(
        YarnBerry::member_paths(RESOLUTIONS.lock),
        std::collections::HashSet::from([".".to_string()])
    );
}

/// A combined key binds each of its descriptors to the one locator, a caret and a member's exact
/// pin alike, and a dist-tag keeps its `npm:` spelling.
#[test]
fn combined_keys_bind_every_descriptor() -> eyre::Result<()> {
    let lock = BerryLock::read(EVERY_SOURCE.lock)?;
    let descriptors_of = |locator: &str| -> eyre::Result<BTreeSet<String>> {
        let package = lock
            .packages
            .iter()
            .find(|package| package.locator.to_string() == locator)
            .ok_or_else(|| eyre::eyre!("no entry resolves to {locator}"))?;
        Ok(package
            .descriptors
            .iter()
            .map(ToString::to_string)
            .collect())
    };

    assert_eq!(
        descriptors_of("chalk@npm:4.1.2")?,
        BTreeSet::from([
            "chalk@npm:4.1.2".to_string(),
            "chalk@npm:^4.1.0".to_string()
        ])
    );
    assert_eq!(
        descriptors_of("is-odd@npm:3.0.1")?,
        BTreeSet::from([
            "is-odd@npm:^3.0.1".to_string(),
            "is-odd@npm:latest".to_string()
        ])
    );
    // The alias's descriptor names its own package before the target's.
    assert_eq!(
        descriptors_of("is-number@npm:6.0.0")?,
        BTreeSet::from([
            "is-number@npm:^6.0.0".to_string(),
            "isnum@npm:is-number@6.0.0".to_string()
        ])
    );
    assert_eq!(
        descriptors_of("pkg-b@workspace:packages/b")?,
        BTreeSet::from([
            "pkg-b@workspace:^".to_string(),
            "pkg-b@workspace:packages/b".to_string()
        ])
    );
    Ok(())
}

/// A release only an npm alias resolves to is alias-only, while one a descriptor of its own
/// package also binds — `is-odd`'s `is-number@^6.0.0` beside the root's alias — is a genuine copy.
#[test]
fn a_release_only_an_alias_binds_is_alias_only() -> eyre::Result<()> {
    let Some(schema) = SCHEMAS.last() else {
        eyre::bail!("no schema fixture");
    };
    let aliased = YarnBerry::alias_only_versions(schema.lock);
    assert!(aliased.contains("is-number", "6.0.0"));
    assert!(!aliased.contains("is-number", "7.0.0"));
    assert!(!aliased.contains("isnum", "6.0.0"));

    let shared = YarnBerry::alias_only_versions(EVERY_SOURCE.lock);
    assert!(!shared.contains("is-number", "6.0.0"));
    Ok(())
}

/// Every schema family a released Yarn writes reads to the same releases, workspaces, and alias,
/// whatever its checksum, patch, and protocol spellings.
#[test]
fn every_schema_family_reads_alike() -> eyre::Result<()> {
    for schema in SCHEMAS {
        let context = format!("Yarn {} (schema {})", schema.yarn, schema.schema);
        assert!(
            schema
                .lock
                .contains(&format!("__metadata:\n  version: {}\n", schema.schema)),
            "{context}: the fixture's schema"
        );
        let rows = YarnBerry::parse(schema.lock)?;
        assert_eq!(releases(&rows), expected(SCHEMA_RELEASES), "{context}");
        assert_eq!(
            YarnBerry::member_paths(schema.lock)
                .into_iter()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                ".".to_string(),
                "packages/a".to_string(),
                "packages/b".to_string()
            ]),
            "{context}"
        );
        assert!(
            YarnBerry::alias_only_versions(schema.lock).contains("is-number", "6.0.0"),
            "{context}"
        );
    }
    Ok(())
}

/// A schema up to 6 gets the `npm:` protocol back on a protocol-less semver key, as Yarn's reader
/// restores it, and a builtin patch of a protocol-less inner range gets Yarn's own rewrite.
/// A later schema's keys are read as written.
#[test]
fn protocol_less_keys_of_old_schemas_are_normalized_as_yarn_does() -> eyre::Result<()> {
    let schema_of = |yarn: &str| {
        SCHEMAS
            .iter()
            .find(|schema| schema.yarn == yarn)
            .ok_or_else(|| eyre::eyre!("no Yarn {yarn} fixture"))
    };
    let descriptors_of = |lock: &BerryLock| -> BTreeSet<String> {
        lock.packages
            .iter()
            .filter(|package| package.locator.ident.name == "resolve")
            .flat_map(|package| package.descriptors.iter().map(ToString::to_string))
            .collect()
    };

    // Yarn 2 keys `resolve@^1.22.0` and its patch without the protocol.
    let yarn2 = BerryLock::read(schema_of("2.4.3")?.lock)?;
    assert_eq!(
        descriptors_of(&yarn2),
        BTreeSet::from([
            "resolve@npm:^1.22.0".to_string(),
            "resolve@npm%3A^1.22.0#builtin<compat/resolve>".to_string()
        ])
    );
    // Yarn 3 keys the plain range with it, but still not the patch's inner range.
    let yarn3 = BerryLock::read(schema_of("3.8.7")?.lock)?;
    assert_eq!(
        descriptors_of(&yarn3),
        BTreeSet::from([
            "resolve@npm:^1.22.0".to_string(),
            "resolve@npm%3A^1.22.0#~builtin<compat/resolve>".to_string()
        ])
    );
    // Yarn 4 writes every key in the form its reader expects.
    let yarn4 = BerryLock::read(schema_of("4.0.2")?.lock)?;
    assert_eq!(
        descriptors_of(&yarn4),
        BTreeSet::from([
            "resolve@npm:^1.22.0".to_string(),
            "resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>".to_string()
        ])
    );
    Ok(())
}

/// A dependency whose descriptor the root `resolutions` rewrote has no key of its own, which the
/// lock still reads.
#[test]
fn a_resolution_rewritten_dependency_reads() -> eyre::Result<()> {
    let rows = YarnBerry::parse(RESOLUTIONS.lock)?;
    assert!(
        releases(&rows).contains(&("supports-color".to_string(), "7.1.0".to_string())),
        "{rows:?}"
    );
    Ok(())
}

/// An npm locator whose archive comes from another URL is still the registry release it names,
/// its origin that URL.
///
/// Synthetic: Yarn binds `__archiveUrl` only for a registry whose tarball URL breaks npm's
/// convention, which the public registry never does; the spelling follows `bindLocator` in Yarn
/// 4.18.1's `NpmSemverResolver`.
#[test]
fn an_archive_url_parameter_keeps_the_release() -> eyre::Result<()> {
    let lock = indoc! {r#"
        __metadata:
          version: 10
          cacheKey: 10c0

        "private-pkg@npm:^1.0.0":
          version: 1.0.0
          resolution: "private-pkg@npm:1.0.0::__archiveUrl=https%3A%2F%2Fnpm.example.com%2Fprivate-pkg%2F1.0.0.tgz"
          languageName: node
          linkType: hard
    "#};
    let rows = YarnBerry::parse(lock)?;
    assert_eq!(releases(&rows), expected(&[("private-pkg", "1.0.0")]));
    assert_eq!(
        rows.iter().map(|row| &row.origin).collect::<Vec<_>>(),
        [&LockOrigin::Url(
            "https://npm.example.com/private-pkg/1.0.0.tgz".to_string()
        )]
    );
    Ok(())
}

/// Bindings other than one decodable `__archiveUrl` leave a registry locator without origin
/// evidence, since Yarn's fetchers may then fetch it from anywhere.
///
/// Synthetic: the bindings are spelled as Yarn's `bindLocator` writes them.
#[test]
fn only_a_lone_archive_url_binding_is_origin_evidence() -> eyre::Result<()> {
    let origin = |resolution: &str| -> eyre::Result<LockOrigin> {
        let lock = formatdoc! {r#"
            __metadata:
              version: 10
              cacheKey: 10c0

            "pkg@npm:^1.0.0":
              version: 1.0.0
              resolution: "{resolution}"
              languageName: node
              linkType: hard
        "#};
        let rows = YarnBerry::parse(&lock)?;
        let [row] = rows.as_slice() else {
            eyre::bail!("one row for {resolution}: {rows:?}")
        };
        Ok(row.origin.clone())
    };

    // No binding, a lone archive URL, and a `+` the query string reads as a space
    assert_eq!(origin("pkg@npm:1.0.0")?, LockOrigin::ConfiguredRegistry);
    assert_eq!(
        origin(
            "pkg@npm:1.0.0::__archiveUrl=https%3A%2F%2Fregistry.npmjs.org%2Fpkg%2F-%2Fpkg-1.0.0.tgz"
        )?,
        LockOrigin::Url("https://registry.npmjs.org/pkg/-/pkg-1.0.0.tgz".to_string())
    );
    assert_eq!(
        origin("pkg@npm:1.0.0::__archiveUrl=https%3A%2F%2Fx.example%2Fa+b.tgz")?,
        LockOrigin::Url("https://x.example/a b.tgz".to_string())
    );

    // A second binding, another binding, and a malformed escape
    for resolution in [
        "pkg@npm:1.0.0::__archiveUrl=https%3A%2F%2Fregistry.npmjs.org%2Fp.tgz&other=1",
        "pkg@npm:1.0.0::locator=root%40workspace%3A.",
        "pkg@npm:1.0.0::__archiveUrl=https%3A%2F%2Fregistry.npmjs.org%2F%zz",
    ] {
        assert_eq!(origin(resolution)?, LockOrigin::Unrecorded, "{resolution}");
    }
    Ok(())
}

/// The metadata key may be quoted or flow-mapped, its version quoted, and the file may start
/// with a byte-order mark or use CRLF line endings.
#[test]
fn metadata_spellings_read_alike() -> eyre::Result<()> {
    let entry = indoc! {r#"
        "chalk@npm:^4.1.0":
          version: 4.1.2
          resolution: "chalk@npm:4.1.2"
          languageName: node
          linkType: hard
    "#};
    for metadata in [
        "__metadata:\n  version: 10\n  cacheKey: 10c0\n",
        "\"__metadata\":\n  version: \"10\"\n",
        "'__metadata':\n  version: 10\n",
        "__metadata: { version: 10, cacheKey: 10c0 }\n",
        "\u{feff}__metadata:\n  version: 10\n",
        // Spellings that name the key only once YAML decodes it
        "\"\\x5f_metadata\":\n  version: 10\n",
        "\"\\u005F_metadata\":\n  version: 10\n",
        "? __metadata\n: { version: 10 }\n",
    ] {
        let lock = format!("{metadata}\n{entry}");
        assert_eq!(
            releases(&YarnBerry::parse(&lock)?),
            expected(&[("chalk", "4.1.2")]),
            "{metadata:?}"
        );
        let crlf = lock.replace('\n', "\r\n");
        assert_eq!(
            releases(&YarnBerry::parse(&crlf)?),
            expected(&[("chalk", "4.1.2")]),
            "{metadata:?} with CRLF"
        );
    }
    Ok(())
}

/// A metadata key whose value is null or malformed still marks the lock as Berry, so it is refused
/// by the Berry reader and never read as an empty Classic graph, whether Yarn's line spelling or
/// only the decoded document names the key.
#[test]
fn a_null_metadata_value_still_marks_berry() -> eyre::Result<()> {
    for lock in [
        "__metadata: null\n",
        "__metadata: ~\n",
        "__metadata: 10\n",
        "{ \"__metadata\": null }\n",
        "? __metadata\n: null\n",
        "\"\\x5f_metadata\": null\n",
    ] {
        assert!(is_yarn_berry_lock(lock), "{lock:?}");
        unreadable_message(lock)?;
        assert!(
            matches!(Yarn::parse(lock), Err(CoreError::LockUnreadable(_))),
            "{lock:?}"
        );
    }
    Ok(())
}

/// A metadata key with its value on the same line marks a Berry lock even when the rest of the
/// document is no valid YAML, so the lock fails on the Berry reader instead of reading as an
/// empty Classic graph.
#[test]
fn inline_metadata_marks_an_otherwise_malformed_lock_berry() -> eyre::Result<()> {
    for lock in [
        "__metadata: { version: 10 }\nbroken: [unclosed\n",
        "__metadata: null\nbroken: [unclosed\n",
        "\"__metadata\": { version: 10, cacheKey: 10c0 }\nbroken: [unclosed\n",
        "'__metadata': ~ # pinned\nbroken: [unclosed\n",
        "? __metadata\n: { version: 10 }\nbroken: [unclosed\n",
    ] {
        assert!(is_yarn_berry_lock(lock), "{lock:?}");
        unreadable_message(lock)?;
        assert!(
            matches!(Yarn::parse(lock), Err(CoreError::LockUnreadable(_))),
            "{lock:?}"
        );
    }
    Ok(())
}

/// A schema no released Yarn wrote, or none at all, is refused by name rather than read under
/// another schema's rules.
#[test]
fn unsupported_schemas_are_refused() -> eyre::Result<()> {
    for version in ["3", "7", "11", "x"] {
        let lock = format!("__metadata:\n  version: {version}\n");
        let message = unreadable_message(&lock)?;
        assert!(
            message.contains(&format!(
                "unsupported Yarn Berry lockfile version {version}"
            )),
            "{message}"
        );
    }
    let message = unreadable_message("__metadata:\n  cacheKey: 10c0\n")?;
    assert!(message.contains("no `version`"), "{message}");
    let message = unreadable_message("__metadata:\n  version: 10\n__metadata:\n  version: 10\n")?;
    assert!(message.contains("`__metadata` twice"), "{message}");
    Ok(())
}

/// How a malformed case edits the real lock.
enum Edit {
    /// Replaces the first occurrence of `from` with `to`.
    Replace {
        from: &'static str,
        to: &'static str,
    },
    /// Appends a top-level entry after the last one.
    Append(&'static str),
}

/// One malformed edit of the real Yarn 4.18.1 lock and the detail its refusal names.
struct MalformedCase {
    case: &'static str,
    edit: Edit,
    detail: &'static str,
}

/// The entry fields Yarn records for the registry release `chalk@4.1.0`.
const CHALK_FIELDS: &str = "  version: 4.1.0\n  resolution: \"chalk@npm:4.1.0\"\n";

/// The fields that end `chalk@4.1.0`'s entry, before the next entry's key.
const CHALK_TAIL: &str = "  languageName: node\n  linkType: hard\n\n\"chalk@npm:4.1.2";

/// Each case changes one field of [`EVERY_SOURCE`]'s lock.
const MALFORMED: &[MalformedCase] = &[
    MalformedCase {
        case: "unparsable YAML",
        edit: Edit::Append("\"broken: [unclosed\n"),
        detail: "yarn.lock:",
    },
    MalformedCase {
        case: "a missing resolution",
        edit: Edit::Replace {
            from: CHALK_FIELDS,
            to: "  version: 4.1.0\n",
        },
        detail: "records no `resolution`",
    },
    MalformedCase {
        case: "a resolution that is no locator",
        edit: Edit::Replace {
            from: "resolution: \"chalk@npm:4.1.0\"",
            to: "resolution: \"chalk\"",
        },
        detail: "which is no locator",
    },
    MalformedCase {
        case: "a missing link type",
        edit: Edit::Replace {
            from: CHALK_TAIL,
            to: "  languageName: node\n\n\"chalk@npm:4.1.2",
        },
        detail: "records no `linkType`",
    },
    MalformedCase {
        case: "a version disagreeing with the locator",
        edit: Edit::Replace {
            from: CHALK_FIELDS,
            to: "  version: 4.1.1\n  resolution: \"chalk@npm:4.1.0\"\n",
        },
        detail: "records the version 4.1.1",
    },
    MalformedCase {
        case: "a version that is no string",
        edit: Edit::Replace {
            from: CHALK_FIELDS,
            to: "  version: [4, 1, 0]\n  resolution: \"chalk@npm:4.1.0\"\n",
        },
        detail: "yarn.lock:",
    },
    MalformedCase {
        case: "a soft-linked npm release",
        edit: Edit::Replace {
            from: CHALK_TAIL,
            to: "  languageName: node\n  linkType: soft\n\n\"chalk@npm:4.1.2",
        },
        detail: "links it softly",
    },
    MalformedCase {
        case: "an npm release of another language",
        edit: Edit::Replace {
            from: CHALK_TAIL,
            to: "  languageName: python\n  linkType: hard\n\n\"chalk@npm:4.1.2",
        },
        detail: "records the language `python`",
    },
    MalformedCase {
        case: "an npm locator without a version",
        edit: Edit::Replace {
            from: "\"chalk@npm:4.1.0\"\n",
            to: "\"chalk@npm:^4.1.0\"\n",
        },
        detail: "which is malformed",
    },
    MalformedCase {
        case: "a checksum Yarn never writes",
        edit: Edit::Replace {
            from: "checksum: 10c0/3787bd",
            to: "checksum: 10c0/not-hex-3787bd",
        },
        detail: "records the checksum",
    },
    MalformedCase {
        case: "a key that is no descriptor",
        edit: Edit::Replace {
            from: "\"chalk@npm:4.1.0\":\n",
            to: "\"chalk\":\n",
        },
        detail: "which is no descriptor",
    },
    MalformedCase {
        case: "a workspace outside the project",
        edit: Edit::Replace {
            from: "pkg-a@workspace:packages/a\"\n",
            to: "pkg-a@workspace:../a\"\n",
        },
        detail: "which is malformed",
    },
    MalformedCase {
        case: "a dependency that is no string",
        edit: Edit::Replace {
            from: "    ansi-styles: \"npm:^4.1.0\"\n",
            to: "    ansi-styles: { range: \"npm:^4.1.0\" }\n",
        },
        detail: "yarn.lock:",
    },
    MalformedCase {
        case: "an entry that is no mapping",
        edit: Edit::Append("\"left-over@npm:1.0.0\": dangling\n"),
        detail: "yarn.lock:",
    },
];

/// A malformed Berry lock stays on the Berry error path: it is refused naming `yarn.lock`, never
/// read as an empty or smaller graph.
#[test]
fn malformed_berry_locks_are_refused() -> eyre::Result<()> {
    let real = EVERY_SOURCE.lock;
    for MalformedCase { case, edit, detail } in MALFORMED {
        let lock = match edit {
            Edit::Replace { from, to } => real.replacen(from, to, 1),
            Edit::Append(entry) => format!("{real}\n{entry}"),
        };
        assert_ne!(lock, real, "{case}: the edit applies to the fixture");
        let message = unreadable_message(&lock)?;
        assert!(
            message.starts_with("yarn.lock:") && message.contains(detail),
            "{case}: {message}"
        );
        // The adapter's parse is the same refusal, so a malformed lock never reads as empty.
        assert!(YarnBerry::parse(&lock).is_err(), "{case}");
    }
    Ok(())
}

/// A descriptor two entries bind to different locators is ambiguous and refused, while a key
/// written twice with the same entry, as a hand merge leaves it, reads once.
#[test]
fn conflicting_descriptor_bindings_are_refused() -> eyre::Result<()> {
    let real = EVERY_SOURCE.lock;
    // Bind `chalk@npm:^4.1.0` to 4.1.0 as well as to 4.1.2.
    let conflicting = real.replacen(
        "\"chalk@npm:4.1.0\":\n",
        "\"chalk@npm:4.1.0, chalk@npm:^4.1.0\":\n",
        1,
    );
    let message = unreadable_message(&conflicting)?;
    assert!(
        message.contains("chalk@npm:^4.1.0 resolves to both"),
        "{message}"
    );

    // One locator recorded twice with different fields.
    let chalk = indoc! {r#"
        "chalk@npm:4.1.0":
          version: 4.1.0
          resolution: "chalk@npm:4.1.0"
          languageName: node
          linkType: hard
    "#};
    let message = unreadable_message(&format!("{real}\n{chalk}"))?;
    assert!(
        message.contains("chalk@npm:4.1.0 is recorded twice"),
        "{message}"
    );

    // The same entry twice reads as once.
    let start = real
        .find("\"chalk@npm:4.1.0\":\n")
        .ok_or_else(|| eyre::eyre!("no chalk 4.1.0 entry"))?;
    let end = real
        .get(start..)
        .and_then(|rest| rest.find("\n\n"))
        .map(|length| start + length)
        .ok_or_else(|| eyre::eyre!("no end of the chalk 4.1.0 entry"))?;
    let entry = real
        .get(start..end)
        .ok_or_else(|| eyre::eyre!("no chalk 4.1.0 entry"))?;
    let duplicated = format!("{real}\n{entry}\n");
    assert_eq!(
        releases(&YarnBerry::parse(&duplicated)?),
        releases(&YarnBerry::parse(real)?)
    );
    Ok(())
}

/// The Berry reader refuses a Classic lock, and the Classic reader still reads its own lock while
/// refusing a Berry one, so neither format is ever read as the other.
#[test]
fn each_reader_keeps_to_its_own_format() -> eyre::Result<()> {
    let classic = indoc! {r#"
        # THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
        # yarn lockfile v1


        lodash@^4.17.21:
          version "4.17.21"
          resolved "https://registry.yarnpkg.com/lodash/-/lodash-4.17.21.tgz#abc"
    "#};
    let message = unreadable_message(classic)?;
    assert!(message.contains("Yarn Classic (v1) lockfile"), "{message}");
    assert_eq!(
        releases(&Yarn::parse(classic)?),
        expected(&[("lodash", "4.17.21")])
    );
    assert!(matches!(
        Yarn::parse(EVERY_SOURCE.lock),
        Err(CoreError::LockUnreadable(_))
    ));
    Ok(())
}

/// A descriptor or locator splits after its package name, so a scope, an alias's target, and a
/// patch's inner descriptor stay whole, unlike a split at the last `@`.
#[test]
fn descriptors_split_after_the_package_name() {
    let descriptor = |scope: Option<&str>, name: &str, range: &str| Descriptor {
        ident: Ident {
            scope: scope.map(str::to_string),
            name: name.to_string(),
        },
        range: range.to_string(),
    };
    assert_eq!(
        Descriptor::parse("@sindresorhus/is@npm:^4.0.0"),
        Some(descriptor(Some("sindresorhus"), "is", "npm:^4.0.0"))
    );
    assert_eq!(
        Descriptor::parse("isnum@npm:is-number@6.0.0"),
        Some(descriptor(None, "isnum", "npm:is-number@6.0.0"))
    );
    assert_eq!(
        Descriptor::parse("alias@npm:@scope/pkg@^1.0.0"),
        Some(descriptor(None, "alias", "npm:@scope/pkg@^1.0.0"))
    );
    assert_eq!(
        Descriptor::parse("resolve@patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>"),
        Some(descriptor(
            None,
            "resolve",
            "patch:resolve@npm%3A^1.22.0#optional!builtin<compat/resolve>"
        ))
    );

    // Forms Yarn's strict pattern refuses
    for refused in [
        "lodash",
        "lodash@",
        "@scope@1.0.0",
        "@/name@1.0.0",
        "@scope/@1.0.0",
        "a/b@1.0.0",
        "lodash@unknown",
        "lodash@1.0.0\n",
    ] {
        assert!(Descriptor::parse(refused).is_none(), "{refused:?}");
        assert!(Locator::parse(refused).is_none(), "{refused:?}");
    }
    assert!(Ident::parse("@scope/name").is_some());
    assert!(Ident::parse("name@1.0.0").is_none());
}

/// A combined key splits at each comma with only the spaces beside it removed.
#[test]
fn combined_keys_split_like_yarn() {
    assert_eq!(
        combined_key_descriptors("a@npm:1 ,  b@npm:2,c@npm:3").collect::<Vec<_>>(),
        ["a@npm:1", "b@npm:2", "c@npm:3"]
    );
    assert_eq!(
        combined_key_descriptors(" a@npm:1 ").collect::<Vec<_>>(),
        [" a@npm:1 "]
    );
    assert_eq!(
        combined_key_descriptors("a@npm:1,").collect::<Vec<_>>(),
        ["a@npm:1", ""]
    );
}

/// The protocol Yarn restores goes on a semver range or a dist-tag, never on another protocol, a
/// path, or a git shorthand.
#[test]
fn only_semver_ranges_and_tags_take_the_npm_protocol() {
    // Ranges node-semver accepts
    for range in [
        "^1.22.0",
        "~1.1.4",
        "1.2.3",
        "1.x",
        "*",
        ">=1.2.3 <2",
        ">= 1.2.3",
        "^1.0.0 || ^2.0.0",
        "1.0.0 - 2.0.0",
        "~> 1.2",
        "v1.2.3",
        "1.2.3-beta.1+build.5",
    ] {
        assert!(is_semver_range(range), "{range}");
    }
    // Neither ranges nor needing the check
    for range in [
        "npm:^1.0.0",
        "01.2.3",
        "1.2-beta",
        "1.2.3-",
        "^",
        ">=",
        "1.2.3.4",
        "user/repo",
        "latest",
    ] {
        assert!(!is_semver_range(range), "{range}");
    }
    assert!(is_tag("latest"));
    assert!(is_tag("next-1.x"));
    assert!(!is_tag("v2"));
    assert!(!is_tag("user/repo"));
    assert!(!is_tag(""));
}

/// Yarn's builtin-patch rewrite applies only to an unscoped inner range without the protocol.
#[test]
fn legacy_patch_rewrite_matches_yarns_pattern() {
    assert_eq!(
        legacy_patch_inner("patch:resolve@^1.22.0#~builtin<compat/resolve>"),
        Some("^1.22.0#~builtin<compat/resolve>")
    );
    assert_eq!(
        legacy_patch_inner("patch:resolve@npm%3A^1.22.0#~builtin<compat/resolve>"),
        None
    );
    assert_eq!(legacy_patch_inner("patch:resolve@npm:^1.22.0#x"), None);
    assert_eq!(legacy_patch_inner("patch:@scope/pkg@^1.0.0#x"), None);
    assert_eq!(legacy_patch_inner("npm:^1.0.0"), None);
}

/// A reference splits into its protocol, source, and selector as Yarn's `parseRange` splits it,
/// its `::` bindings dropped.
#[test]
fn references_split_like_parse_range() {
    let parts = |protocol, source, selector| ReferenceParts {
        protocol,
        source,
        selector,
    };
    assert_eq!(
        ReferenceParts::split("npm:4.1.2"),
        parts(Some("npm:"), "4.1.2", None)
    );
    assert_eq!(
        ReferenceParts::split("npm:1.0.0::__archiveUrl=https%3A%2F%2Fexample.com"),
        parts(Some("npm:"), "1.0.0", None)
    );
    assert_eq!(
        ReferenceParts::split(
            "patch:resolve@npm%3A1.22.13#optional!builtin<compat/resolve>::version=1.22.13"
        ),
        parts(
            Some("patch:"),
            "resolve@npm%3A1.22.13",
            Some("optional!builtin<compat/resolve>")
        )
    );
    assert_eq!(
        ReferenceParts::split("https://github.com/stevemao/left-pad.git#commit=eb115f2"),
        parts(
            Some("https:"),
            "//github.com/stevemao/left-pad.git",
            Some("commit=eb115f2")
        )
    );
    // A `#` before any colon leaves the reference without a protocol.
    assert_eq!(
        ReferenceParts::split("a#b:c"),
        parts(None, "a", Some("b:c"))
    );
}

/// Percent escapes decode as `decodeURIComponent` decodes them, and a malformed one is refused.
#[test]
fn percent_escapes_decode_strictly() {
    assert_eq!(
        percent_decode("1.0.0%2Bbuild").as_deref(),
        Some("1.0.0+build")
    );
    assert_eq!(
        percent_decode("1.0.0+build").as_deref(),
        Some("1.0.0+build")
    );
    assert_eq!(percent_decode("%zz"), None);
    assert_eq!(percent_decode("%+1"), None);
    assert_eq!(percent_decode("%2"), None);
    assert_eq!(percent_decode("%ff"), None);
}

/// Yarn writes each workspace's directory verbatim after `workspace:`, so a `%`, `#`, or `::` in
/// a directory name stays part of its path, in the locator and in a descriptor's range alike.
#[test]
fn workspace_paths_are_read_literally() -> eyre::Result<()> {
    assert_eq!(
        YarnBerry::member_paths(LITERAL_PATHS.lock)
            .into_iter()
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([".", "packages/100%", "packages/a#b", "packages/c::d"].map(str::to_string))
    );
    assert_eq!(
        releases(&YarnBerry::parse(LITERAL_PATHS.lock)?),
        expected(&[("is-number", "7.0.0")])
    );
    let lock = BerryLock::read(LITERAL_PATHS.lock)?;
    let descriptors: BTreeSet<String> = lock
        .packages
        .iter()
        .flat_map(|package| package.descriptors.iter().map(ToString::to_string))
        .collect();
    for descriptor in [
        "hash@workspace:packages/a#b",
        "percent@workspace:*",
        "percent@workspace:packages/100%",
        "colons@workspace:packages/c::d",
    ] {
        assert!(
            descriptors.contains(descriptor),
            "{descriptor}: {descriptors:?}"
        );
    }
    Ok(())
}

/// A workspace path stays inside the project.
#[test]
fn workspace_paths_stay_inside_the_project() {
    for path in [
        ".",
        "packages/a",
        "a",
        "packages/@scope/b",
        "packages/100%",
        "packages/a#b",
        "packages/c::d",
        "packages/C:a",
    ] {
        assert!(is_workspace_path(path), "{path}");
    }
    for path in [
        "",
        "..",
        "../a",
        "packages/../../a",
        "/abs",
        "packages//a",
        "./a",
        "packages\\a",
        "C:/a",
        "c:a",
    ] {
        assert!(!is_workspace_path(path), "{path}");
    }
}
