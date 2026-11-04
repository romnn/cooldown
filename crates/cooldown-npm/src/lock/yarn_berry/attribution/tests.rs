//! The Berry join judged on real Yarn locks: each member owns exactly the copies Yarn binds for
//! its own declarations (checked against `yarn why`), and every declaration a rewrite, a stale
//! record, or an unestablished protocol may have moved is unproven rather than guessed.

#[cfg(unix)]
use super::super::super::yarn_berry_fixtures::LITERAL_PATHS;
use super::super::super::yarn_berry_fixtures::{
    BerryProject, EVERY_SOURCE, MEMBERS, NPM_PINS, ProjectFile, RESOLUTIONS, REWRITES,
    SCHEMA_FILES, SCHEMAS,
};
use super::super::{BerryLock, YarnBerry};
use super::{bindings, declared_names_index};
use crate::lock::{
    MemberIndex, MemberResolution, NameVersion, NodeLock, UnprovenDeclaration, WorkspaceDialect,
    WorkspaceManifests, YarnBindings,
};
use crate::manifest::workspaces::YarnEnvironment;
use camino::Utf8PathBuf;
use color_eyre::eyre;
use indoc::indoc;
use std::collections::{BTreeSet, HashSet};

/// A Berry project written into a temporary directory.
struct TempProject {
    /// Owns the temporary directory; dropping it deletes the project, so it must stay bound for
    /// as long as the project is read.
    _guard: tempfile::TempDir,
    root: Utf8PathBuf,
    lock: String,
}

impl TempProject {
    /// The workspace snapshot Yarn's configuration in the project alone establishes, no home rc
    /// file or `YARN_*` variable taking part.
    fn workspace(&self) -> WorkspaceManifests {
        WorkspaceManifests::read_in(
            &self.root,
            WorkspaceDialect::Berry,
            &YarnEnvironment::isolated(&self.root),
        )
    }

    /// The bindings of the project's lock through its snapshot.
    fn bindings(&self) -> eyre::Result<YarnBindings> {
        let workspace = self.workspace();
        if let Some(reason) = workspace.unavailable_reason() {
            eyre::bail!("the snapshot is unavailable: {reason}");
        }
        bindings(&BerryLock::read(&self.lock)?, &workspace)
            .ok_or_else(|| eyre::eyre!("a complete snapshot binds every member"))
    }
}

/// A project of `files` and the lock `lock`, every file written verbatim.
fn project(files: &[(&str, &str)], lock: &str) -> eyre::Result<TempProject> {
    let guard = tempfile::tempdir()?;
    let root = Utf8PathBuf::from_path_buf(guard.path().to_path_buf())
        .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
    for (rel, contents) in files.iter().copied().chain([("yarn.lock", lock)]) {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
    }
    Ok(TempProject {
        _guard: guard,
        root,
        lock: lock.to_string(),
    })
}

/// The files of `fixture`, with each `(path, from, to)` edit applied to the file at `path`.
fn files_of<'a>(
    files: &'a [ProjectFile],
    edits: &[(&str, &str, &str)],
) -> eyre::Result<Vec<(&'a str, String)>> {
    let mut edited: Vec<(&str, String)> = files
        .iter()
        .map(|ProjectFile { path, contents }| (*path, (*contents).to_string()))
        .collect();
    for (path, from, to) in edits {
        let Some((_, contents)) = edited.iter_mut().find(|(file, _)| file == path) else {
            eyre::bail!("no file {path}");
        };
        eyre::ensure!(contents.contains(from), "{path} holds no {from:?}");
        *contents = contents.replacen(from, to, 1);
    }
    Ok(edited)
}

/// The project `fixture` records, with `edits` applied to its files ([`files_of`]).
fn fixture_project(
    fixture: &BerryProject,
    edits: &[(&str, &str, &str)],
) -> eyre::Result<TempProject> {
    let files = files_of(fixture.files, edits)?;
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, contents)| (*path, contents.as_str()))
        .collect();
    project(&files, fixture.lock)
}

/// Every registry release of `lock` that `index` credits to a member, as
/// `name@version [members]`, with ` pinned` when every registry owner pins it exactly and
/// ` fixed` when an owner may not edit it.
fn owned_rows(lock: &str, index: &MemberIndex) -> eyre::Result<Vec<String>> {
    let releases: BTreeSet<(String, String)> = YarnBerry::parse(lock)?
        .into_iter()
        .map(|NameVersion { name, version, .. }| (name, version))
        .collect();
    Ok(releases
        .into_iter()
        .filter_map(|(name, version)| {
            let members = index.members_for(&name, &version);
            if members.is_empty() {
                return None;
            }
            let pinned = if index.is_exact_pinned(&name, &version) {
                " pinned"
            } else {
                ""
            };
            let fixed = if index.fixed_owners(&name, &version).is_empty() {
                ""
            } else {
                " fixed"
            };
            Some(format!(
                "{name}@{version} [{}]{pinned}{fixed}",
                members.join(",")
            ))
        })
        .collect())
}

/// Every unattributed name of `bindings` with its members and why, as `name: member (reason)`.
fn unattributed(bindings: &YarnBindings) -> Vec<String> {
    bindings
        .unattributed()
        .into_iter()
        .flat_map(|name| {
            name.declarations
                .into_iter()
                .map(move |UnprovenDeclaration { member, reason }| {
                    format!("{}: {member} ({reason:?})", name.name)
                })
        })
        .collect()
}

/// Each member owns the copy its own declaration binds, as `yarn why` reports it for the lock.
///
/// Members on three `chalk` lines each own their own, and `debug`'s newer `ms` copies stay
/// transitive beside `packages/b`'s pin.
/// An optional dependency, a same-name alias, Yarn's builtin `typescript` patch, and an unnamed
/// nested workspace bind registry copies; an alias of another package, a `file:` package, a
/// `workspace:` range, and a range a workspace satisfies bind none.
/// The registry `is-odd` beside a workspace of that name is owned but fixed in place.
/// `semver`, declared at two ranges by the root, is unproven, so neither copy of it is owned.
#[test]
fn each_member_owns_exactly_what_its_declarations_bind() -> eyre::Result<()> {
    let project = fixture_project(&MEMBERS, &[])?;
    let bindings = project.bindings()?;

    assert_eq!(
        owned_rows(&project.lock, &bindings.member_index(&HashSet::new()))?,
        [
            "chalk@4.1.0 [packages/a] pinned",
            "chalk@4.1.2 [.]",
            "chalk@5.6.2 [packages/b]",
            "debug@4.3.4 [packages/b] pinned",
            "debug@4.4.3 [packages/a]",
            "has-flag@4.0.0 [packages/a]",
            "is-number@6.0.0 [packages/b/nested/c]",
            "is-number@7.0.0 [packages/a]",
            "is-odd@3.0.1 [packages/b] fixed",
            "ms@2.0.0 [packages/b] pinned",
            "typescript@5.9.3 [.]",
        ]
    );
    assert_eq!(unattributed(&bindings), ["semver: . (ConflictingFields)"]);

    // Registry copies resolve, local and foreign bindings are unknown, and an undeclared name is
    // missing.
    for (member, name, expected) in [
        (".", "chalk", MemberResolution::Resolved("4.1.2")),
        (".", "typescript", MemberResolution::Resolved("5.9.3")),
        (".", "semver", MemberResolution::Unproven),
        ("packages/a", "chalk", MemberResolution::Resolved("4.1.0")),
        (
            "packages/a",
            "has-flag",
            MemberResolution::Resolved("4.0.0"),
        ),
        (
            "packages/a",
            "is-number",
            MemberResolution::Resolved("7.0.0"),
        ),
        ("packages/a", "is-odd", MemberResolution::Unknown),
        ("packages/a", "loc", MemberResolution::Unknown),
        ("packages/a", "my-alias", MemberResolution::Unknown),
        ("packages/a", "react", MemberResolution::Unknown),
        ("packages/a", "ms", MemberResolution::Missing),
        ("packages/b", "a", MemberResolution::Unknown),
        ("packages/b", "is-odd", MemberResolution::Unknown),
        ("packages/b", "ms", MemberResolution::Resolved("2.0.0")),
        ("packages/b", "debug", MemberResolution::Resolved("4.3.4")),
        (
            "packages/b/nested/c",
            "is-number",
            MemberResolution::Resolved("6.0.0"),
        ),
        ("packages/is-odd", "chalk", MemberResolution::Missing),
    ] {
        assert_eq!(
            bindings.resolution(member, name),
            expected,
            "{member} {name}"
        );
    }
    Ok(())
}

/// A declaration the root `resolutions` or a catalog may rewrite is unproven, even where the lock
/// keeps a key of its declared descriptor alive for another member: `packages/a` binds
/// `chalk@4.1.0`, yet the root's `chalk@npm:^4.1.0` key binds `4.1.2`.
/// A name no rewrite reaches stays proven beside it.
#[test]
fn rewritten_declarations_are_unproven() -> eyre::Result<()> {
    let project = fixture_project(&REWRITES, &[])?;
    let bindings = project.bindings()?;
    let index = bindings.member_index(&HashSet::new());

    assert_eq!(
        owned_rows(&project.lock, &index)?,
        ["has-flag@4.0.0 [packages/a]"]
    );
    assert_eq!(
        unattributed(&bindings),
        [
            "chalk: . (Resolutions)",
            "chalk: packages/a (Resolutions)",
            "react: . (Catalog)",
        ]
    );
    assert!(index.is_unattributed("chalk") && index.is_unattributed("react"));
    assert_eq!(
        bindings.resolution("packages/a", "chalk"),
        MemberResolution::Unproven
    );

    // A resolution reaching only a transitive name leaves every declaration proven.
    let transitive = fixture_project(&RESOLUTIONS, &[])?;
    let bindings = transitive.bindings()?;
    assert_eq!(
        owned_rows(&transitive.lock, &bindings.member_index(&HashSet::new()))?,
        ["chalk@4.1.2 [.]"]
    );
    assert_eq!(unattributed(&bindings), [] as [String; 0]);
    Ok(())
}

/// Local, git, tarball, `exec:`, and alias declarations bind no registry copy and taint nothing,
/// while an optional platform package, a dist-tag, a scoped package, and Yarn's builtin `resolve`
/// patch bind theirs.
#[test]
fn every_source_binds_only_its_registry_copies() -> eyre::Result<()> {
    let project = fixture_project(&EVERY_SOURCE, &[])?;
    let bindings = project.bindings()?;

    assert_eq!(
        owned_rows(&project.lock, &bindings.member_index(&HashSet::new()))?,
        [
            "@esbuild/darwin-arm64@0.25.0 [.] pinned",
            "@sindresorhus/is@4.6.0 [.]",
            "chalk@4.1.0 [packages/b] pinned",
            "chalk@4.1.2 [.,packages/a]",
            "is-number@7.0.0 [.]",
            "is-odd@3.0.1 [.]",
            "resolve@1.22.13 [.]",
        ]
    );
    assert_eq!(unattributed(&bindings), [] as [String; 0]);
    Ok(())
}

/// Every lockfile schema attributes the same workspace alike, the protocol-less ranges of the
/// older schemas included, where Yarn's `defaultProtocol` is the default `npm:`.
#[test]
fn every_schema_attributes_alike() -> eyre::Result<()> {
    let files: Vec<(&str, &str)> = SCHEMA_FILES
        .iter()
        .map(|ProjectFile { path, contents }| (*path, *contents))
        .collect();
    for schema in SCHEMAS {
        let project = project(&files, schema.lock)?;
        let bindings = project.bindings()?;
        assert_eq!(
            owned_rows(&project.lock, &bindings.member_index(&HashSet::new()))?,
            [
                "@sindresorhus/is@4.6.0 [.]",
                "chalk@4.1.0 [packages/b] pinned",
                "chalk@4.1.2 [.,packages/a]",
                "is-number@7.0.0 [.]",
                "resolve@1.22.13 [.]",
            ],
            "Yarn {}",
            schema.yarn
        );
        assert!(unattributed(&bindings).is_empty(), "Yarn {}", schema.yarn);
    }
    Ok(())
}

/// A lock of a protocol-less schema proves a bare declaration's protocol only where Yarn's
/// `defaultProtocol` is established as `npm:`.
///
/// Under `defaultProtocol: "file:"`, `packages/a`'s `chalk: 4.1.0` names a local directory, which
/// the schema-6 lock records as `chalk: 4.1.0` just as an npm range.
/// Read as `npm:4.1.0`, it would join `packages/b`'s explicit `npm:4.1.0` registry copy.
/// Synthetic: hand-written in the shape Yarn 3 writes, since its point is a configuration no
/// registry fixture can exercise.
#[test]
fn a_protocol_less_range_needs_the_npm_default_protocol() -> eyre::Result<()> {
    let lock = indoc! {r#"
        __metadata:
          version: 6
          cacheKey: 8

        "a@workspace:packages/a":
          version: 0.0.0-use.local
          resolution: "a@workspace:packages/a"
          dependencies:
            chalk: 4.1.0
          languageName: unknown
          linkType: soft

        "b@workspace:packages/b":
          version: 0.0.0-use.local
          resolution: "b@workspace:packages/b"
          dependencies:
            chalk: "npm:4.1.0"
          languageName: unknown
          linkType: soft

        "chalk@file:4.1.0::locator=a%40workspace%3Apackages%2Fa":
          version: 1.0.0
          resolution: "chalk@file:4.1.0#4.1.0::hash=1a2b3c&locator=a%40workspace%3Apackages%2Fa"
          languageName: node
          linkType: hard

        "chalk@npm:4.1.0":
          version: 4.1.0
          resolution: "chalk@npm:4.1.0"
          languageName: node
          linkType: hard

        "root@workspace:.":
          version: 0.0.0-use.local
          resolution: "root@workspace:."
          languageName: unknown
          linkType: soft
    "#};
    let manifests = [
        (
            "package.json",
            r#"{ "name": "root", "private": true, "workspaces": ["packages/*"] }"#,
        ),
        (
            "packages/a/package.json",
            r#"{ "name": "a", "version": "1.0.0", "dependencies": { "chalk": "4.1.0" } }"#,
        ),
        (
            "packages/b/package.json",
            r#"{ "name": "b", "version": "1.0.0", "dependencies": { "chalk": "npm:4.1.0" } }"#,
        ),
    ];

    // Under the `file:` protocol, the bare range is unproven, so no `chalk` copy is owned.
    let local = [
        manifests.as_slice(),
        &[(".yarnrc.yml", "defaultProtocol: \"file:\"\n")],
    ]
    .concat();
    let local_protocol = project(&local, lock)?;
    let bindings = local_protocol.bindings()?;
    assert_eq!(
        bindings.resolution("packages/a", "chalk"),
        MemberResolution::Unproven
    );
    assert_eq!(
        unattributed(&bindings),
        ["chalk: packages/a (DefaultProtocol)"]
    );
    assert_eq!(
        owned_rows(lock, &bindings.member_index(&HashSet::new()))?,
        [] as [String; 0]
    );

    // Under the default `npm:` protocol, the same range is the registry release both share.
    let registry = project(&manifests, lock)?;
    assert_eq!(
        owned_rows(
            &registry.lock,
            &registry.bindings()?.member_index(&HashSet::new())
        )?,
        ["chalk@4.1.0 [packages/a,packages/b] pinned"]
    );
    Ok(())
}

/// A declaration the lock records otherwise, no longer records, or never recorded is unproven,
/// and so is every declaration a resolution key cooldown cannot read may rewrite; a declaration
/// moved to another install field at the same range stays proven.
#[test]
fn stale_and_unreadable_declarations_are_unproven() -> eyre::Result<()> {
    // `packages/b` widened its `chalk` range after the lock was written.
    let widened = fixture_project(
        &MEMBERS,
        &[(
            "packages/b/package.json",
            "\"chalk\": \"^5.3.0\"",
            "\"chalk\": \"^5.4.0\"",
        )],
    )?;
    let bindings = widened.bindings()?;
    assert_eq!(unattributed(&bindings)[0], "chalk: packages/b (Stale)");
    let rows = owned_rows(&widened.lock, &bindings.member_index(&HashSet::new()))?;
    assert!(
        rows.iter().all(|row| !row.starts_with("chalk@")),
        "{rows:?}"
    );
    assert!(
        rows.contains(&"debug@4.4.3 [packages/a]".to_string()),
        "{rows:?}"
    );

    // `packages/b` dropped `ms`, which the lock still binds for it.
    let dropped = fixture_project(
        &MEMBERS,
        &[("packages/b/package.json", ",\n    \"ms\": \"2.0.0\"", "")],
    )?;
    assert!(unattributed(&dropped.bindings()?).contains(&"ms: packages/b (Stale)".to_string()));

    // `packages/b` moved its `debug` pin into `dependencies` at the same range.
    let moved = fixture_project(
        &MEMBERS,
        &[(
            "packages/b/package.json",
            "\"ms\": \"2.0.0\"\n  },\n  \"devDependencies\": {\n    \"debug\": \"4.3.4\"\n  }",
            "\"ms\": \"2.0.0\",\n    \"debug\": \"4.3.4\"\n  }",
        )],
    )?;
    assert_eq!(
        moved.bindings()?.resolution("packages/b", "debug"),
        MemberResolution::Resolved("4.3.4")
    );

    // A specifier that is no string, and a `patch:` reference
    let malformed = fixture_project(
        &MEMBERS,
        &[("packages/b/package.json", "\"ms\": \"2.0.0\"", "\"ms\": 2")],
    )?;
    assert!(
        unattributed(&malformed.bindings()?).contains(&"ms: packages/b (Malformed)".to_string())
    );
    let patched = fixture_project(
        &MEMBERS,
        &[(
            "packages/b/package.json",
            "\"ms\": \"2.0.0\"",
            "\"ms\": \"patch:ms@npm%3A2.0.0#./ms.patch\"",
        )],
    )?;
    assert!(unattributed(&patched.bindings()?).contains(&"ms: packages/b (Patch)".to_string()));

    // A lock recording the root's range under another protocol, as a non-default
    // `defaultProtocol` writes it
    let lock = MEMBERS.lock.replacen(
        "    chalk: \"npm:^4.1.0\"\n    semver",
        "    chalk: \"custom:^4.1.0\"\n    semver",
        1,
    );
    eyre::ensure!(lock != MEMBERS.lock, "the root records chalk");
    let files = files_of(MEMBERS.files, &[])?;
    let files: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, contents)| (*path, contents.as_str()))
        .collect();
    let respelled = project(&files, &lock)?;
    assert!(unattributed(&respelled.bindings()?).contains(&"chalk: . (Stale)".to_string()));

    // A resolution key Yarn refuses could target any package.
    let opaque = fixture_project(
        &MEMBERS,
        &[(
            "package.json",
            "\"private\": true,",
            "\"private\": true,\n  \"resolutions\": { \"**/debug\": \"4.3.4\" },",
        )],
    )?;
    let bindings = opaque.bindings()?;
    assert!(
        owned_rows(&opaque.lock, &bindings.member_index(&HashSet::new()))?.is_empty(),
        "every declared name is unproven"
    );
    assert!(
        unattributed(&bindings)
            .contains(&"is-number: packages/b/nested/c (Resolutions)".to_string())
    );
    Ok(())
}

/// A lock whose workspaces and the snapshot disagree proves nothing for the disagreeing member: a
/// workspace the lock no longer records leaves every declaration of the member unproven, and so
/// does a workspace the snapshot holds no manifest for.
#[test]
fn a_member_the_lock_or_the_snapshot_lacks_is_unproven() -> eyre::Result<()> {
    let project = fixture_project(&MEMBERS, &[])?;
    let workspace = project.workspace();

    // The lock lost the nested workspace's entry.
    let entry = indoc! {r#"
        "c-6c62a5@workspace:packages/b/nested/c":
          version: 0.0.0-use.local
          resolution: "c-6c62a5@workspace:packages/b/nested/c"
          dependencies:
            is-number: "npm:^6.0.0"
          languageName: unknown
          linkType: soft

    "#};
    let without = MEMBERS.lock.replacen(entry, "", 1);
    eyre::ensure!(
        without != MEMBERS.lock,
        "the lock records the nested workspace"
    );
    let unlocked = bindings(&BerryLock::read(&without)?, &workspace)
        .ok_or_else(|| eyre::eyre!("a complete snapshot"))?;
    assert_eq!(
        unattributed(&unlocked),
        [
            "is-number: packages/b/nested/c (NotLocked)",
            "semver: . (ConflictingFields)",
        ]
    );

    // The snapshot lacks a member the lock records, as one read from another lock would.
    let narrower = fixture_project(&MEMBERS, &[])?;
    std::fs::write(narrower.root.join("yarn.lock"), &without)?;
    let snapshot = narrower.workspace();
    let unlisted = bindings(&BerryLock::read(MEMBERS.lock)?, &snapshot)
        .ok_or_else(|| eyre::eyre!("a complete snapshot"))?;
    assert!(
        unattributed(&unlisted).contains(&"is-number: packages/b/nested/c (Unlisted)".to_string())
    );
    Ok(())
}

/// Without a usable snapshot, every name a workspace entry records is declared but owned by no
/// member, so every copy of it counts as direct, and an excluded workspace's names are left out.
#[test]
fn an_unavailable_snapshot_declares_every_recorded_name() -> eyre::Result<()> {
    let project = fixture_project(&MEMBERS, &[])?;
    std::fs::remove_file(project.root.join("packages/is-odd/package.json"))?;
    let workspace = project.workspace();
    assert!(!workspace.is_complete());
    let lock = BerryLock::read(&project.lock)?;
    assert!(bindings(&lock, &workspace).is_none());

    let index = declared_names_index(&lock, &HashSet::new());
    assert!(index.is_authoritative());
    assert_eq!(owned_rows(&project.lock, &index)?, [] as [String; 0]);
    for name in [
        "chalk",
        "semver",
        "typescript",
        "is-number",
        "my-alias",
        "ms",
    ] {
        assert!(index.is_unattributed(name), "{name}");
    }
    assert!(!index.is_unattributed("supports-color"));

    // Excluding `packages/b` drops `ms`, which only its entry records, while `is-number`, which
    // `packages/a` records too, stays.
    let excluded = HashSet::from(["packages/b".to_string()]);
    let index = declared_names_index(&lock, &excluded);
    assert!(!index.is_unattributed("ms"));
    assert!(index.is_unattributed("is-number"));

    // The adapter hook answers alike.
    let index = YarnBerry::member_sources_in(&project.lock, Some(&workspace), &HashSet::new());
    assert!(index.is_unattributed("chalk"));
    Ok(())
}

/// Workspace directories Yarn records verbatim, `%` and `#` included, are joined like any other.
#[cfg(unix)]
#[test]
fn literal_workspace_paths_are_joined() -> eyre::Result<()> {
    let project = fixture_project(&LITERAL_PATHS, &[])?;
    let bindings = project.bindings()?;
    assert_eq!(
        owned_rows(&project.lock, &bindings.member_index(&HashSet::new()))?,
        ["is-number@7.0.0 [packages/100%]"]
    );
    Ok(())
}

/// A same-name `npm:` reference pins the version inside it, whether spelled `npm:2.1.3` or
/// `npm:is-number@7.0.0`, while an alias of another package owns and pins nothing.
#[test]
fn same_name_npm_references_pin_their_version() -> eyre::Result<()> {
    let project = fixture_project(&NPM_PINS, &[])?;
    let bindings = project.bindings()?;

    assert_eq!(
        owned_rows(&project.lock, &bindings.member_index(&HashSet::new()))?,
        ["is-number@7.0.0 [.] pinned", "ms@2.1.3 [.] pinned"]
    );
    assert_eq!(
        bindings.resolution(".", "flag-alias"),
        MemberResolution::Unknown
    );

    // The pin guard captures the same versions, and never the alias.
    assert_eq!(YarnBerry::install_pin("ms", "npm:2.1.3"), Some("2.1.3"));
    assert_eq!(
        YarnBerry::install_pin("is-number", "npm:is-number@7.0.0"),
        Some("7.0.0")
    );
    assert_eq!(
        YarnBerry::install_pin("flag-alias", "npm:has-flag@4.0.0"),
        None
    );
    assert_eq!(YarnBerry::install_pin("ms", "npm:^2.1.3"), None);
    assert_eq!(YarnBerry::install_pin("ms", "2.1.3"), Some("2.1.3"));
    Ok(())
}
