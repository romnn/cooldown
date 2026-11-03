use super::*;
use crate::lock::yarn_fixtures::{self, YarnWorkspace};
use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;
use indoc::{formatdoc, indoc};

/// The workspace snapshot of `manifests`, each a project-relative path and its contents, read
/// from a temporary directory.
fn workspace_of(manifests: &[(&str, &str)]) -> eyre::Result<WorkspaceManifests> {
    let guard = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    for (rel, contents) in manifests {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    let workspace = WorkspaceManifests::read(&root);
    if let Some(reason) = workspace.unavailable_reason() {
        eyre::bail!("the fixture workspace is unavailable: {reason}");
    }
    Ok(workspace)
}

/// The bindings a [`YarnWorkspace`] fixture's lock records for its manifests.
fn fixture_bindings(fixture: &YarnWorkspace) -> eyre::Result<YarnBindings> {
    let workspace = workspace_of(fixture.manifests)?;
    YarnBindings::read(fixture.lock, &workspace)
        .ok_or_else(|| eyre::eyre!("the fixture lock is unreadable"))
}

/// The bindings of `lock` for a single-package root declaring `manifest`.
fn root_bindings(manifest: &str, lock: &str) -> eyre::Result<Option<YarnBindings>> {
    let workspace = workspace_of(&[("package.json", manifest)])?;
    Ok(YarnBindings::read(lock, &workspace))
}

/// What the root of `root_bindings(manifest, lock)` binds `name` to, as an owned value.
fn root_resolution(manifest: &str, lock: &str, name: &str) -> eyre::Result<Resolution> {
    let bindings =
        root_bindings(manifest, lock)?.ok_or_else(|| eyre::eyre!("the lock is unreadable"))?;
    Ok(Resolution::of(bindings.resolution(".", name)))
}

/// A [`MemberResolution`] without its borrow, for comparing across fixtures.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Resolution {
    Resolved(String),
    Missing,
    Unknown,
}

impl Resolution {
    fn of(resolution: MemberResolution<'_>) -> Self {
        match resolution {
            MemberResolution::Resolved(version) => Resolution::Resolved(version.to_string()),
            MemberResolution::Missing => Resolution::Missing,
            MemberResolution::Unknown => Resolution::Unknown,
        }
    }
}

fn resolved(version: &str) -> Resolution {
    Resolution::Resolved(version.to_string())
}

/// A one-entry lock resolving `descriptors` (the key line, verbatim) to `chalk@version`.
fn chalk_lock(descriptors: &str, version: &str) -> String {
    formatdoc! {r#"
        # yarn lockfile v1


        {descriptors}:
          version "{version}"
          resolved "https://registry.yarnpkg.com/chalk/-/chalk-{version}.tgz"
    "#}
}

/// Two members pinning different versions of one package each bind their own entry, and the
/// root's caret binds the entry it shares with a member's pin; a linked sibling, an alias, and an
/// undeclared name bind no copy.
#[test]
fn members_bind_the_entry_their_own_selector_names() -> eyre::Result<()> {
    let bindings = fixture_bindings(&yarn_fixtures::PINNED_MEMBERS)?;
    let resolution = |member: &str, name: &str| Resolution::of(bindings.resolution(member, name));

    // Each member's own copy
    assert_eq!(resolution("packages/a", "chalk"), resolved("4.1.0"));
    assert_eq!(resolution("packages/b", "chalk"), resolved("4.1.2"));
    assert_eq!(resolution(".", "chalk"), resolved("4.1.2"));
    assert_eq!(
        resolution("packages/b", "@sindresorhus/is"),
        resolved("4.0.0")
    );

    // A sibling Yarn links, an alias to another package, and a name no field declares
    assert_eq!(resolution("packages/a", "pkg-b"), Resolution::Unknown);
    assert_eq!(resolution("packages/a", "isnum"), Resolution::Unknown);
    assert_eq!(resolution("packages/a", "is-number"), Resolution::Missing);

    // A member the workspace does not list
    assert_eq!(resolution("packages/c", "chalk"), Resolution::Unknown);
    Ok(())
}

/// A name the root `resolutions` can rewrite is unknown for every member, bare and nested keys
/// alike, while an unrelated name stays attributable; trivial and tag selectors read as Yarn
/// records them, an optional pin outranks a dev range, and a sibling's name is unknown even where
/// the registry served another request for it.
#[test]
fn resolutions_siblings_and_local_sources_bind_no_registry_copy() -> eyre::Result<()> {
    let bindings = fixture_bindings(&yarn_fixtures::RESOLUTIONS_AND_SOURCES)?;
    let resolution = |member: &str, name: &str| Resolution::of(bindings.resolution(member, name));

    // Rewritten by `is-number` and by `pkg-a/**/@sindresorhus/is`
    assert_eq!(resolution(".", "is-number"), Resolution::Unknown);
    assert_eq!(
        resolution("tools/group/c", "is-number"),
        Resolution::Unknown
    );
    assert_eq!(
        resolution("packages/a", "@sindresorhus/is"),
        Resolution::Unknown
    );

    // Untouched by any resolution
    assert_eq!(resolution(".", "has-flag"), resolved("5.0.1"));
    assert_eq!(resolution("tools/group/c", "has-flag"), resolved("5.0.1"));
    assert_eq!(resolution(".", "ansi-regex"), resolved("6.5.0"));
    assert_eq!(resolution(".", "color-name"), resolved("1.1.3"));

    // A local directory, and a member's name served from the registry
    assert_eq!(resolution("packages/a", "local-thing"), Resolution::Unknown);
    assert_eq!(resolution("packages/a", "ms"), Resolution::Unknown);
    Ok(())
}

/// A remote tarball, a link, and a GitHub shorthand bind no registry copy, even where the tarball
/// is a registry tarball, while a `||` union and a comparator set bind theirs under the quoted keys
/// Yarn writes.
#[test]
fn source_selectors_bind_no_registry_copy() -> eyre::Result<()> {
    let bindings = fixture_bindings(&yarn_fixtures::SINGLE_PACKAGE_SOURCES)?;
    let resolution = |name: &str| Resolution::of(bindings.resolution(".", name));

    assert_eq!(resolution("ms"), Resolution::Unknown);
    assert_eq!(resolution("linked"), Resolution::Unknown);
    assert_eq!(resolution("is-number"), Resolution::Unknown);
    assert_eq!(resolution("color-name"), resolved("1.1.4"));
    assert_eq!(resolution("has-flag"), resolved("4.0.0"));
    Ok(())
}

/// The descriptor is looked up exactly: a copy that satisfies the member's range under another
/// selector never stands in for it.
#[test]
fn a_satisfying_copy_under_another_selector_is_no_binding() -> eyre::Result<()> {
    let manifest = r#"{ "dependencies": { "chalk": "^4.1.1" } }"#;
    assert_eq!(
        root_resolution(manifest, &chalk_lock("chalk@^4.1.0", "4.1.2"), "chalk")?,
        Resolution::Unknown
    );
    assert_eq!(
        root_resolution(
            manifest,
            &chalk_lock("chalk@^4.1.0, chalk@^4.1.1", "4.1.2"),
            "chalk"
        )?,
        resolved("4.1.2")
    );
    Ok(())
}

/// Yarn's normalization keeps one selector per name, the first nontrivial specifier in optional,
/// regular, dev order, so a pin hidden beside a range is judged at the range's copy, a trivial
/// `*` defers to a pin beside it, and a specifier that is no string binds nothing.
#[test]
fn one_effective_selector_names_every_install_fields_copy() -> eyre::Result<()> {
    let lock = indoc! {r#"
        # yarn lockfile v1


        chalk@4.1.0:
          version "4.1.0"

        chalk@^4.1.0:
          version "4.1.2"
    "#};
    for (manifest, expected) in [
        (
            r#"{ "dependencies": { "chalk": "^4.1.0" }, "devDependencies": { "chalk": "4.1.0" } }"#,
            resolved("4.1.2"),
        ),
        (
            r#"{ "devDependencies": { "chalk": "^4.1.0" }, "optionalDependencies": { "chalk": "4.1.0" } }"#,
            resolved("4.1.0"),
        ),
        (
            r#"{ "dependencies": { "chalk": "*" }, "devDependencies": { "chalk": "4.1.0" } }"#,
            resolved("4.1.0"),
        ),
        (
            r#"{ "dependencies": { "chalk": "" }, "devDependencies": { "chalk": "^4.1.0" } }"#,
            resolved("4.1.2"),
        ),
        (
            r#"{ "dependencies": { "chalk": 4 }, "devDependencies": { "chalk": "4.1.0" } }"#,
            Resolution::Unknown,
        ),
        (
            r#"{ "peerDependencies": { "chalk": "^4.1.0" } }"#,
            Resolution::Unknown,
        ),
    ] {
        assert_eq!(
            root_resolution(manifest, lock, "chalk")?,
            expected,
            "{manifest}"
        );
    }
    Ok(())
}

/// Keys read as Yarn's own parser reads them: grouped, quoted or not, scoped, JSON-escaped, and
/// with a byte-order mark and `\r\n` line endings.
#[test]
fn descriptors_read_as_yarns_parser_reads_them() -> eyre::Result<()> {
    let manifest = r#"{ "dependencies": { "@scope/pkg": "^1.0.0", "quoted": ">=1.0.0 <2", "chalk": "~4.1.0" } }"#;
    let lock = indoc! {r#"
        # yarn lockfile v1


        "@scope/pkg@^1.0.0", "@scope/pkg@^1.0.1":
          version "1.0.2"
          resolved "https://registry.yarnpkg.com/@scope/pkg/-/pkg-1.0.2.tgz#0123abcd"

        "quoted@>=1.0.0 \u003c2":
          version "1.5.0"

        chalk@^4.0.0, chalk@~4.1.0, "chalk@4.x":
          version "4.1.2"
    "#};
    let crlf = format!("\u{feff}{}", lock.replace('\n', "\r\n"));
    for lock in [lock.to_string(), crlf] {
        assert_eq!(
            root_resolution(manifest, &lock, "@scope/pkg")?,
            resolved("1.0.2")
        );
        assert_eq!(
            root_resolution(manifest, &lock, "quoted")?,
            resolved("1.5.0")
        );
        assert_eq!(
            root_resolution(manifest, &lock, "chalk")?,
            resolved("4.1.2")
        );
    }
    Ok(())
}

/// A descriptor recorded twice with different entries, or with a plain value, has no single
/// answer, while an identical repeat is harmless; a bare-name key may serve any selector.
#[test]
fn ambiguous_descriptors_bind_no_copy() -> eyre::Result<()> {
    let manifest = r#"{ "dependencies": { "chalk": "^4.1.0" } }"#;
    let twice = |second: &str| {
        formatdoc! {r#"
            # Synthetic: Yarn never writes one descriptor twice.
            chalk@^4.1.0:
              version "4.1.2"

            chalk@^4.1.0:
              version "{second}"
        "#}
    };
    assert_eq!(
        root_resolution(manifest, &twice("4.1.1"), "chalk")?,
        Resolution::Unknown
    );
    assert_eq!(
        root_resolution(manifest, &twice("4.1.2"), "chalk")?,
        resolved("4.1.2")
    );
    let aliased = indoc! {r#"
        # Synthetic: a descriptor whose value is another descriptor, the legacy shape.
        chalk@^4.1.0 "chalk@^4.1.2"
    "#};
    assert_eq!(
        root_resolution(manifest, aliased, "chalk")?,
        Resolution::Unknown
    );
    let bare = format!(
        "{}\nchalk:\n  version \"4.1.0\"\n",
        chalk_lock("chalk@^4.1.0", "4.1.2")
    );
    assert_eq!(
        root_resolution(manifest, &bare, "chalk")?,
        Resolution::Unknown
    );
    Ok(())
}

/// What the root of a single-package `manifest` binds `name` to under `lock`: the resolution the
/// guard and the landing check read, and the version of the copy its selector identifies.
fn root_binding(
    manifest: &str,
    lock: &str,
    name: &str,
) -> eyre::Result<(Resolution, Option<String>)> {
    let bindings =
        root_bindings(manifest, lock)?.ok_or_else(|| eyre::eyre!("the lock is unreadable"))?;
    Ok((
        Resolution::of(bindings.resolution(".", name)),
        bindings.bound_version(".", name).map(str::to_string),
    ))
}

/// A `resolved` URL proves a registry copy only as a registry tarball of the very version, on the
/// public registry or a mirror serving its layout, a scope's slash written either way; an absent
/// one proves nothing against it.
/// Any other URL leaves the copy identified but unproven, and an entry naming another package, or
/// a field holding no string, identifies none.
#[test]
fn registry_identity_needs_a_registry_tarball() -> eyre::Result<()> {
    let manifest = r#"{ "dependencies": { "chalk": "^4.1.0", "@scope/pkg": "^1.0.0" } }"#;
    let lock = |chalk_fields: &str| {
        formatdoc! {r#"
            # Synthetic: entries in Yarn's shape with the fields under test.
            chalk@^4.1.0:
              {chalk_fields}

            "@scope/pkg@^1.0.0":
              version "1.0.0"
              resolved "https://registry.npmjs.org/@scope%2fpkg/-/pkg-1.0.0.tgz"
        "#}
    };
    let proven = (resolved("4.1.2"), Some("4.1.2".to_string()));
    let unproven = (Resolution::Unknown, Some("4.1.2".to_string()));
    let unidentified = (Resolution::Unknown, None);
    for (fields, expected) in [
        // Registry tarballs, a mirror's included, and no URL at all
        (
            r#"version "4.1.2"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz#abc""#,
            proven.clone(),
        ),
        (
            r#"version "4.1.2"
  resolved "https://npm.example.com/api/npm/remote/chalk/-/chalk-4.1.2.tgz""#,
            proven.clone(),
        ),
        (r#"version "4.1.2""#, proven.clone()),
        (
            r#"version "4.1.2"
  resolved "https://mirror.example/npm/chalk/-/chalk-4.1.2.tgz?token=abc""#,
            proven.clone(),
        ),
        // Other URLs, one spelling the layout only in its query
        (
            r#"version "4.1.2"
  resolved "https://mirror.example/download?file=/chalk/-/chalk-4.1.2.tgz""#,
            unproven.clone(),
        ),
        (
            r#"version "4.1.2"
  resolved "https://example.com/chalk-4.1.2.tgz""#,
            unproven.clone(),
        ),
        (
            r#"version "4.1.2"
  resolved "https://registry.yarnpkg.com/chalk/-/chalk-4.1.1.tgz""#,
            unproven.clone(),
        ),
        (
            r#"version "4.1.2"
  resolved "https://example.com/notchalk/-/chalk-4.1.2.tgz""#,
            unproven.clone(),
        ),
        // Another package, and fields holding no string
        (
            r#"name "other"
  version "4.1.2""#,
            unidentified.clone(),
        ),
        (
            r#"name true
  version "4.1.2""#,
            unidentified.clone(),
        ),
        (
            r#"version "4.1.2"
  resolved true"#,
            unidentified.clone(),
        ),
        (
            r#"version "4.1.2"
  resolved 7"#,
            unidentified.clone(),
        ),
        (
            r#"version "4.1.2"
  resolved:
    url "https://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz""#,
            unidentified.clone(),
        ),
        ("version true", unidentified.clone()),
    ] {
        let lock = lock(fields);
        assert_eq!(root_binding(manifest, &lock, "chalk")?, expected, "{lock}");
        assert_eq!(
            root_resolution(manifest, &lock, "@scope/pkg")?,
            resolved("1.0.0")
        );
    }
    Ok(())
}

/// Only an `http(s)` URL naming its host carries a registry path, and only its parsed path is
/// read: a layout in the query or the fragment proves nothing, while a query or a fragment beside
/// a registry path leaves it intact.
#[test]
fn registry_tarballs_are_recognized_by_their_path_alone() {
    for (resolved, expected) in [
        // Registry paths, a mirror's prefix, query, and fragment included
        ("https://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz", true),
        ("http://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz", true),
        (
            "https://mirror.example/npm/chalk/-/chalk-4.1.2.tgz?token=abc",
            true,
        ),
        (
            "https://mirror.example/npm/chalk/-/chalk-4.1.2.tgz#abc123",
            true,
        ),
        (
            "https://user:secret@mirror.example:8443/chalk/-/chalk-4.1.2.tgz",
            true,
        ),
        // The layout outside the path
        (
            "https://mirror.example/download?file=/chalk/-/chalk-4.1.2.tgz",
            false,
        ),
        (
            "https://mirror.example/download#/chalk/-/chalk-4.1.2.tgz",
            false,
        ),
        // No host after the scheme
        ("https:///chalk/-/chalk-4.1.2.tgz", false),
        ("https:////mirror.example/chalk/-/chalk-4.1.2.tgz", false),
        ("https:/mirror.example/chalk/-/chalk-4.1.2.tgz", false),
        ("https://", false),
        ("https://exa mple/chalk/-/chalk-4.1.2.tgz", false),
        // Another scheme
        ("ftp://mirror.example/chalk/-/chalk-4.1.2.tgz", false),
        ("file:///chalk/-/chalk-4.1.2.tgz", false),
        ("git+https://mirror.example/chalk/-/chalk-4.1.2.tgz", false),
        ("/chalk/-/chalk-4.1.2.tgz", false),
        // Another package or version under the layout
        ("https://mirror.example/notchalk/-/chalk-4.1.2.tgz", false),
        ("https://mirror.example/chalk/-/chalk-4.1.1.tgz", false),
    ] {
        assert_eq!(
            is_registry_tarball("chalk", "4.1.2", resolved),
            expected,
            "{resolved}"
        );
    }
    for resolved in [
        "https://registry.npmjs.org/@scope/pkg/-/pkg-1.0.0.tgz",
        "https://registry.npmjs.org/@scope%2fpkg/-/pkg-1.0.0.tgz",
        "https://registry.npmjs.org/@scope%2Fpkg/-/pkg-1.0.0.tgz",
    ] {
        assert!(
            is_registry_tarball("@scope/pkg", "1.0.0", resolved),
            "{resolved}"
        );
    }
    assert!(!is_registry_tarball(
        "@scope/pkg",
        "1.0.0",
        "https://registry.npmjs.org/download?file=/@scope/pkg/-/pkg-1.0.0.tgz"
    ));
}

/// A `version`, `name`, or `resolved` field holding no string leaves the copy unidentified on
/// every branch that would otherwise hold it fixed in place, so no owner is credited through it,
/// though the declaration stays in the name inventory.
#[test]
fn a_field_holding_no_string_identifies_no_copy_on_any_branch() -> eyre::Result<()> {
    let workspace = workspace_of(&[
        (
            "package.json",
            indoc! {r#"
                {
                  "name": "root",
                  "version": "1.0.0",
                  "private": true,
                  "workspaces": ["packages/*"],
                  "dependencies": {
                    "chalk": "^4.1.0",
                    "local-thing": "file:./local",
                    "is-number": "github:jonschlinkert/is-number#7.0.0",
                    "pkg-b": "^2.0.0"
                  },
                  "resolutions": { "chalk": "4.1.2" }
                }
            "#},
        ),
        (
            "local/package.json",
            r#"{ "name": "local-thing", "version": "0.1.0" }"#,
        ),
        (
            "packages/b/package.json",
            r#"{ "name": "pkg-b", "version": "1.0.0" }"#,
        ),
    ])?;
    let well_formed = r#"version "VERSION""#;
    let malformed = [
        r#"version "VERSION"
  resolved true"#,
        r#"version "VERSION"
  resolved 7"#,
        r#"version "VERSION"
  resolved:
    url "https://registry.yarnpkg.com/x/-/x-1.0.0.tgz""#,
        r#"name true
  version "VERSION""#,
        r#"name:
    value "x"
  version "VERSION""#,
        "version true",
    ];
    for (name, descriptor, version, reason) in [
        ("chalk", "chalk@^4.1.0", "4.1.2", FixedBinding::Resolutions),
        (
            "local-thing",
            r#""local-thing@file:./local""#,
            "0.1.0",
            FixedBinding::LocalPath,
        ),
        (
            "is-number",
            r#""is-number@github:jonschlinkert/is-number#7.0.0""#,
            "7.0.0",
            FixedBinding::OwnSource,
        ),
        (
            "pkg-b",
            "pkg-b@^2.0.0",
            "2.0.0",
            FixedBinding::WorkspaceName,
        ),
    ] {
        let lock_of = |fields: &str| {
            formatdoc! {"
                # Synthetic: one entry in Yarn's shape with the fields under test.
                {descriptor}:
                  {fields}
            ", fields = fields.replace("VERSION", version)}
        };
        let index_of = |lock: &str| -> eyre::Result<(YarnBindings, MemberIndex)> {
            let bindings = YarnBindings::read(lock, &workspace)
                .ok_or_else(|| eyre::eyre!("the lock is unreadable: {lock}"))?;
            let index = bindings.member_index(&HashSet::new());
            Ok((bindings, index))
        };

        // The branch the well-formed entry takes
        let (bindings, index) = index_of(&lock_of(well_formed))?;
        assert_eq!(bindings.bound_version(".", name), Some(version), "{name}");
        assert_eq!(
            index.fixed_owners(name, version),
            [FixedOwner {
                member: ".".to_string(),
                reason
            }],
            "{name}"
        );

        for fields in malformed {
            let lock = lock_of(fields);
            let (bindings, index) = index_of(&lock)?;
            assert_eq!(bindings.bound_version(".", name), None, "{lock}");
            assert_eq!(
                Resolution::of(bindings.resolution(".", name)),
                Resolution::Unknown,
                "{lock}"
            );
            assert_eq!(
                index.members_for(name, version),
                Vec::<String>::new(),
                "{lock}"
            );
            assert_eq!(index.fixed_owners(name, version), [], "{lock}");
            assert!(index.declares(name), "{lock}");
        }
    }
    Ok(())
}

/// Anything Yarn's own reader refuses attributes nothing: a Berry lock, a merge conflict, a newer
/// lockfile version, an odd indentation, an invalid token, or an unterminated string.
#[test]
fn locks_yarns_reader_refuses_are_unreadable() -> eyre::Result<()> {
    let manifest = r#"{ "dependencies": { "chalk": "^4.1.0" } }"#;
    let readable = chalk_lock("chalk@^4.1.0", "4.1.2");
    assert!(root_bindings(manifest, &readable)?.is_some());
    for refused in [
        indoc! {r#"
            __metadata:
              version: 8

            "chalk@npm:^4.1.0":
              version: 4.1.2
        "#}
        .to_string(),
        readable.replace(
            "chalk@^4.1.0:",
            "<<<<<<< HEAD\nchalk@^4.1.0:\n=======\n>>>>>>> other",
        ),
        readable.replace("yarn lockfile v1", "yarn lockfile v2"),
        readable.replace("  version", "   version"),
        readable.replace("  version", "\tversion"),
        readable.replace("\"4.1.2\"", "\"4.1.2"),
        (0..=MAX_DEPTH + 1)
            .flat_map(|level| ["  ".repeat(level), "a:\n".to_string()])
            .collect(),
    ] {
        assert!(root_bindings(manifest, &refused)?.is_none(), "{refused}");
    }
    Ok(())
}

/// The workspace snapshot must be complete: a lock read without every member attributes nothing.
#[test]
fn an_incomplete_workspace_attributes_nothing() -> eyre::Result<()> {
    let guard = tempfile::tempdir()?;
    let root = Utf8Path::from_path(guard.path())
        .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
    std::fs::write(
        root.join("package.json"),
        r#"{ "private": true, "workspaces": ["packages/{a,b}"] }"#,
    )?;
    let workspace = WorkspaceManifests::read(root);
    assert!(!workspace.is_complete());
    assert!(YarnBindings::read(&chalk_lock("chalk@^4.1.0", "4.1.2"), &workspace).is_none());
    Ok(())
}

/// The index attributes each copy to the members whose selector binds it, pins a copy only when
/// every install declaration of every owner is exact, keeps unproven declarations in the name
/// inventory without a copy, and recomputes all of it without excluded members.
#[test]
fn the_member_index_owns_each_copy_through_its_selectors() -> eyre::Result<()> {
    let bindings = fixture_bindings(&yarn_fixtures::PINNED_MEMBERS)?;
    let index = bindings.member_index(&HashSet::new());
    assert!(index.is_authoritative());

    // Owners
    assert_eq!(index.members_for("chalk", "4.1.0"), ["packages/a"]);
    assert_eq!(index.members_for("chalk", "4.1.2"), [".", "packages/b"]);
    assert_eq!(
        index.members_for("is-number", "6.0.0"),
        Vec::<String>::new()
    );

    // The root's caret beside its dev pin leaves the shared copy unpinned; a lone pin pins its own.
    assert!(index.is_exact_pinned("chalk", "4.1.0"));
    assert!(!index.is_exact_pinned("chalk", "4.1.2"));
    assert!(index.is_exact_pinned("@sindresorhus/is", "4.0.0"));

    // Unproven declarations are declared, but own nothing.
    assert!(index.declares("pkg-b"));
    assert!(index.declares("isnum"));
    assert_eq!(
        index.declaring_members("chalk"),
        [".", "packages/a", "packages/b"]
    );

    // Excluding the root leaves the shared copy to the member pinning it.
    let excluded = bindings.member_index(&HashSet::from([".".to_string()]));
    assert_eq!(excluded.members_for("chalk", "4.1.2"), ["packages/b"]);
    assert!(excluded.is_exact_pinned("chalk", "4.1.2"));
    assert!(!excluded.declares("is-number"));
    Ok(())
}

/// A copy an unproven binding identifies stays the member's own, held fixed with the reason no
/// edit of its declaration is proven to move it, and neither pinned nor editable through it.
#[test]
fn the_member_index_holds_unproven_bindings_as_fixed_owners() -> eyre::Result<()> {
    let owner = |member: &str, reason: FixedBinding| FixedOwner {
        member: member.to_string(),
        reason,
    };

    let workspace = fixture_bindings(&yarn_fixtures::RESOLUTIONS_AND_SOURCES)?;
    let index = workspace.member_index(&HashSet::new());
    for (name, version, owners) in [
        (
            "is-number",
            "7.0.0",
            vec![
                owner(".", FixedBinding::Resolutions),
                owner("tools/group/c", FixedBinding::Resolutions),
            ],
        ),
        (
            "@sindresorhus/is",
            "4.6.0",
            vec![owner("packages/a", FixedBinding::Resolutions)],
        ),
        (
            "local-thing",
            "0.1.0",
            vec![owner("packages/a", FixedBinding::LocalPath)],
        ),
        (
            "ms",
            "2.1.3",
            vec![owner("packages/a", FixedBinding::WorkspaceName)],
        ),
    ] {
        let members: Vec<String> = owners
            .iter()
            .map(|FixedOwner { member, .. }| member.clone())
            .collect();
        assert_eq!(
            index.fixed_owners(name, version),
            owners,
            "{name}@{version}"
        );
        assert_eq!(
            index.members_for(name, version),
            members,
            "{name}@{version}"
        );
        assert!(
            index.editable_members_for(name, version).is_empty(),
            "{name}@{version}"
        );
        assert!(!index.is_exact_pinned(name, version), "{name}@{version}");
    }
    // The nested resolution's own entry is no member's copy.
    assert_eq!(
        index.members_for("@sindresorhus/is", "4.0.1"),
        Vec::<String>::new()
    );
    assert_eq!(
        index.editable_members_for("has-flag", "5.0.1"),
        [".", "tools/group/c"]
    );

    let single = fixture_bindings(&yarn_fixtures::SINGLE_PACKAGE_SOURCES)?;
    let index = single.member_index(&HashSet::new());
    for (name, version, reason) in [
        ("ms", "2.1.2", FixedBinding::OwnSource),
        ("is-number", "7.0.0", FixedBinding::OwnSource),
        ("linked", "0.0.0", FixedBinding::LocalPath),
    ] {
        assert_eq!(
            index.fixed_owners(name, version),
            [owner(".", reason)],
            "{name}"
        );
        assert!(
            index.editable_members_for(name, version).is_empty(),
            "{name}"
        );
    }
    Ok(())
}

/// The tokenizer swallows a blank line after a newline as Yarn's does, and reads `true`, `false`,
/// and digits as plain values.
#[test]
fn the_tokenizer_follows_yarns_cases() {
    assert_eq!(
        tokenize("a:\n\n  b true\n"),
        Some(vec![
            Token::Text("a".to_string()),
            Token::Colon,
            Token::Newline,
            Token::Indent(1),
            Token::Text("b".to_string()),
            Token::Boolean,
            Token::Newline,
            Token::Eof,
        ])
    );
    assert_eq!(
        tokenize(r#""a\"b" 12"#),
        Some(vec![
            Token::Text("a\"b".to_string()),
            Token::Number,
            Token::Eof
        ])
    );
    assert_eq!(tokenize("@scope/pkg@1:"), None);
}
