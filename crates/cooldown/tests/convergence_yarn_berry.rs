//! End-to-end tests that drive the REAL Yarn Berry (the `yarn = "4.18.1"` pin in `mise.toml`)
//! against fixtures generated on the fly in temp dirs.
//!
//! cooldown reads a Berry `yarn.lock` for its registry releases and attributes each to the
//! workspace members whose own declarations bind it, but does not mutate a Berry project yet, so
//! these tests check these invariants against Yarn itself:
//!
//! - the rows cooldown reads are exactly the npm releases Yarn's own `yarn info` reports;
//! - each row's members are exactly the workspaces `yarn why` binds it for through a declaration
//!   of its own name, and a declaration a rewrite or a plugin may have moved leaves its name
//!   direct but owned by no member;
//! - excluding a member drops exactly the rows only it owns;
//! - `upgrade` refuses a Berry project without touching any of its files;
//! - a Berry `yarn` never runs on a Classic lock, which it would silently migrate.
//!
//! Each fixture declares `packageManager: "yarn@4.18.1"` and checks the version `yarn` reports
//! in it, so a Yarn Classic or another Berry release on `PATH` skips the tests locally and fails
//! them in CI instead of passing them against the wrong Yarn.
//! The seeded lock resolves the registry's current releases; the assertions compare cooldown with
//! Yarn's own reading of the same lock, never with hard-coded versions.

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "the shared integration harness deliberately panics on invalid fixture setup or malformed command output"
)]
mod support;

use color_eyre::eyre;
use indoc::indoc;
use std::collections::{BTreeMap, BTreeSet};
use support::Fixture;

/// The Yarn release the fixtures declare and require.
const YARN: &str = "4.18.1";

/// Whether the `yarn` that runs in `fixture` is [`YARN`]; anything else skips the test locally and
/// fails it in CI, where the pin must be provisioned.
fn runs_pinned_yarn(fixture: &Fixture) -> bool {
    if !support::tool_on_path("yarn") {
        assert!(
            std::env::var_os("CI").is_none(),
            "`yarn` not found on PATH even though CI must provision every integration tool"
        );
        eprintln!("skipping: `yarn` not found on PATH (provision it via the repo `mise.toml`)");
        return false;
    }
    let output = fixture.run_tool("yarn", &["--version"], &[]);
    let version = output.stdout_str().trim().to_string();
    if output.status.success() && version == YARN {
        return true;
    }
    assert!(
        std::env::var_os("CI").is_none(),
        "`yarn --version` in the fixture reported {version:?} (status {:?}), not the pinned Yarn {YARN}: {}",
        output.status.code(),
        output.stderr_str()
    );
    eprintln!("skipping: `yarn` here is not Yarn {YARN} (reported {version:?})");
    false
}

/// A Berry workspace covering a scoped package, an npm alias, a package Yarn wraps in its builtin
/// patch, a `link:` dependency, and a member with dependencies of its own.
fn berry_workspace() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture
        .write(
            "package.json",
            &indoc::formatdoc! {r#"
                {{
                  "name": "berry-root",
                  "private": true,
                  "packageManager": "yarn@{YARN}",
                  "workspaces": ["packages/*"],
                  "dependencies": {{
                    "@sindresorhus/is": "^4.0.0",
                    "chalk": "^4.1.0",
                    "isnum": "npm:is-number@6.0.0",
                    "resolve": "^1.22.0",
                    "local": "link:./local"
                  }}
                }}
            "#},
        )
        .write(
            "packages/app/package.json",
            indoc! {r#"
                { "name": "app", "version": "1.0.0", "dependencies": { "chalk": "4.1.0", "has-flag": "^4.0.0" } }
            "#},
        )
        .write("local/package.json", r#"{ "name": "local", "version": "1.0.0" }"#)
        .write(".yarnrc.yml", "enableTelemetry: false\n");
    fixture
}

/// Writes an empty `yarn.lock` and has Yarn resolve it without linking anything.
fn seed_lock(fixture: &Fixture) -> eyre::Result<()> {
    fixture.write("yarn.lock", "");
    fixture
        .run_tool_traced(
            "seed the Berry lock",
            "yarn",
            &["install", "--mode=update-lockfile"],
            &[],
        )?
        .require_success()?;
    Ok(())
}

/// Every npm release Yarn's own `yarn info --all --recursive --json` reports for the project, as
/// `name@version`.
fn yarns_registry_releases(fixture: &Fixture) -> eyre::Result<BTreeSet<String>> {
    let output = fixture
        .run_tool_traced(
            "list Yarn's packages",
            "yarn",
            &["info", "--all", "--recursive", "--json"],
            &[],
        )?
        .require_success()?;
    let mut releases = BTreeSet::new();
    for line in output.stdout_str().lines() {
        let entry: serde_json::Value = serde_json::from_str(line)?;
        let Some(locator) = entry.get("value").and_then(serde_json::Value::as_str) else {
            continue;
        };
        // A locator splits after its package name, so a scope's leading `@` is skipped first.
        let name_end = locator
            .get(1..)
            .and_then(|rest| rest.find('@'))
            .map(|index| index + 1)
            .ok_or_else(|| eyre::eyre!("locator {locator} names no reference"))?;
        let (name, reference) = locator.split_at(name_end);
        if let Some(version) = reference.strip_prefix("@npm:") {
            releases.insert(format!("{name}@{version}"));
        }
    }
    Ok(releases)
}

/// The JSON envelope `cooldown` printed and its exit code.
struct Run {
    code: Option<i32>,
    envelope: serde_json::Value,
}

/// Runs `cooldown` with `args` and `--json`.
fn cooldown(fixture: &Fixture, args: &[&str]) -> eyre::Result<Run> {
    let mut full = args.to_vec();
    full.push("--json");
    let output = fixture.cooldown(&full);
    let envelope = serde_json::from_slice(&output.stdout).map_err(|err| {
        eyre::eyre!(
            "cooldown {args:?} printed no JSON envelope ({err}); stderr: {}",
            output.stderr_str()
        )
    })?;
    Ok(Run {
        code: output.status.code(),
        envelope,
    })
}

/// The `name@current` of every reported item.
fn item_releases(envelope: &serde_json::Value) -> BTreeSet<String> {
    envelope
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?;
            let current = item.get("current")?.as_str()?;
            Some(format!("{name}@{current}"))
        })
        .collect()
}

/// The `message` of every diagnostic in the envelope's `section` array.
fn messages(envelope: &serde_json::Value, section: &str) -> Vec<String> {
    envelope
        .get(section)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|diagnostic| diagnostic.get("message")?.as_str())
        .map(ToString::to_string)
        .collect()
}

/// The message of every error the envelope reports, the run's own and each item's.
fn error_messages(envelope: &serde_json::Value) -> Vec<String> {
    let mut errors = messages(envelope, "errors");
    errors.extend(
        envelope
            .get("items")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.pointer("/error/message")?.as_str())
            .map(ToString::to_string),
    );
    errors
}

/// cooldown reads exactly the npm releases Yarn itself resolves: the alias under its target's
/// name, the patched package's inner release once, and no workspace or `link:` package.
/// Reading changes nothing on disk.
#[test]
fn berry_inventory_matches_yarns_own_registry_releases() -> eyre::Result<()> {
    let fixture = berry_workspace();
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    let lock = fixture.read_bytes("yarn.lock");
    let yarns = yarns_registry_releases(&fixture)?;
    assert!(
        yarns
            .iter()
            .any(|release| release.starts_with("is-number@6."))
            && yarns.iter().any(|release| release.starts_with("resolve@")),
        "the fixture resolves the alias and the patched package: {yarns:?}"
    );

    let outdated = cooldown(
        &fixture,
        &["outdated", "--offline", "--transitive", "--all"],
    )?;

    assert_eq!(outdated.code, Some(0), "{:#}", outdated.envelope);
    assert_eq!(item_releases(&outdated.envelope), yarns);
    // The lock proves every member's binding, so the run warns of no lost attribution.
    assert!(
        messages(&outdated.envelope, "warnings")
            .iter()
            .all(|message| !message.contains("cannot prove") && !message.contains("attributes no")),
        "{:#}",
        outdated.envelope
    );
    assert_eq!(
        fixture.read_bytes("yarn.lock"),
        lock,
        "reading wrote the lock"
    );
    Ok(())
}

/// `upgrade` refuses a Berry project before running Yarn, with an explicit error and no change to
/// its lock or manifests, in a dry run and a real one alike.
#[test]
fn berry_upgrade_is_refused_without_touching_the_project() -> eyre::Result<()> {
    let fixture = berry_workspace();
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    let lock = fixture.read_bytes("yarn.lock");
    let manifest = fixture.read_bytes("package.json");
    let member = fixture.read_bytes("packages/app/package.json");

    // `chalk@^4` has matured majors beyond it, so `--major` plans a change for the apply to refuse.
    for args in [
        ["upgrade", "--major", "--dry-run"].as_slice(),
        ["upgrade", "--major"].as_slice(),
    ] {
        let upgrade = cooldown(&fixture, args)?;
        assert_eq!(upgrade.code, Some(4), "{args:?}: {:#}", upgrade.envelope);
        let errors = error_messages(&upgrade.envelope);
        assert!(
            !errors.is_empty()
                && errors
                    .iter()
                    .all(|message| message.contains("Yarn Berry (v2+) lockfile")
                        && message.contains("cannot change yet")),
            "{args:?}: {:#}",
            upgrade.envelope
        );
        assert_eq!(fixture.read_bytes("yarn.lock"), lock, "{args:?}");
        assert_eq!(fixture.read_bytes("package.json"), manifest, "{args:?}");
        assert_eq!(
            fixture.read_bytes("packages/app/package.json"),
            member,
            "{args:?}"
        );
    }
    Ok(())
}

/// A Classic `yarn.lock` that a Yarn Berry executable would run is never handed to it, since its
/// first install would migrate the lock: `upgrade` refuses, naming the executable or the
/// declaration, and the Classic lock stays byte for byte.
///
/// The lock is a hand-written Classic entry for the one dependency, enough for cooldown to plan
/// a change against it.
#[test]
fn a_berry_yarn_never_migrates_a_classic_lock() -> eyre::Result<()> {
    let classic = indoc! {r#"
        # THIS IS AN AUTOGENERATED FILE. DO NOT EDIT THIS FILE DIRECTLY.
        # yarn lockfile v1


        chalk@^4.1.0:
          version "4.1.2"
          resolved "https://registry.yarnpkg.com/chalk/-/chalk-4.1.2.tgz#4f0faa9d7a8a5f2a5c41ab1d5ee3d0b5e5e5e5e5"
    "#};
    // The root declares no Yarn, so only the executable `yarn --version` reaches tells; then it
    // declares the pinned Berry release, which is refused before Yarn even runs.
    for declares_berry in [false, true] {
        let fixture = Fixture::new().tag_independent();
        let package_manager = if declares_berry {
            format!(r#""packageManager": "yarn@{YARN}","#)
        } else {
            String::new()
        };
        fixture
            .write(
                "package.json",
                &indoc::formatdoc! {r#"
                    {{
                      "name": "classic-app",
                      "version": "1.0.0",
                      {package_manager}
                      "dependencies": {{ "chalk": "^4.1.0" }}
                    }}
                "#},
            )
            .write(".yarnrc.yml", "enableTelemetry: false\n")
            .write("yarn.lock", classic);
        if !runs_pinned_yarn(&fixture) {
            return Ok(());
        }

        let upgrade = cooldown(&fixture, &["upgrade", "--major"])?;

        assert_eq!(upgrade.code, Some(4), "{:#}", upgrade.envelope);
        let expected = if declares_berry {
            format!("\"packageManager\": \"yarn@{YARN}\"")
        } else {
            format!("runs Yarn {YARN}")
        };
        let errors = error_messages(&upgrade.envelope);
        assert!(
            !errors.is_empty()
                && errors.iter().all(|message| message
                    .contains("yarn.lock is a Yarn Classic (v1) lockfile")
                    && message.contains(&expected)
                    && message.contains("does not migrate a lock")),
            "declares Berry: {declares_berry}: {:#}",
            upgrade.envelope
        );
        assert_eq!(fixture.read_bytes("yarn.lock"), classic.as_bytes());
    }
    Ok(())
}

/// A Berry workspace whose members bind every kind of declaration a Berry lock records per
/// workspace: three `chalk` lines (the root's exact pin shared with `packages/a`'s caret,
/// `packages/old`'s own pin, and `packages/b`'s major), `debug`'s newer `ms` copies beside
/// `packages/b`'s pin, an optional dependency, a same-name alias, an alias of another package, a
/// `file:` package, a `workspace:` range, a range a workspace satisfies beside the registry's
/// release of that name, Yarn's builtin `typescript` patch, an unnamed nested workspace, and the
/// root's `semver` declared at two ranges.
fn berry_members() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture
        .write(
            "package.json",
            &indoc::formatdoc! {r#"
                {{
                  "name": "berry-members",
                  "private": true,
                  "packageManager": "yarn@{YARN}",
                  "workspaces": ["packages/*"],
                  "dependencies": {{ "chalk": "4.1.2", "semver": "^7.5.0" }},
                  "devDependencies": {{ "semver": "7.5.4", "typescript": "^5.4.0" }}
                }}
            "#},
        )
        .write(
            "packages/a/package.json",
            indoc! {r#"
                {
                  "name": "a",
                  "version": "1.0.0",
                  "dependencies": {
                    "chalk": "^4.1.0",
                    "debug": "^4.3.0",
                    "has-flag": "npm:has-flag@^4.0.0",
                    "is-odd": "^1.0.0",
                    "loc": "file:../../vendor/loc",
                    "my-alias": "npm:ms@^2.1.0"
                  },
                  "optionalDependencies": { "is-number": "^7.0.0" },
                  "peerDependencies": { "react": "^18.0.0" }
                }
            "#},
        )
        .write(
            "packages/b/package.json",
            indoc! {r#"
                {
                  "name": "b",
                  "version": "1.2.0",
                  "workspaces": ["nested/*"],
                  "dependencies": {
                    "a": "workspace:^",
                    "chalk": "^5.3.0",
                    "is-odd": "^3.0.1",
                    "ms": "2.0.0"
                  },
                  "devDependencies": { "debug": "4.3.4" }
                }
            "#},
        )
        .write(
            "packages/b/nested/c/package.json",
            r#"{ "dependencies": { "is-number": "^6.0.0" } }"#,
        )
        .write(
            "packages/is-odd/package.json",
            r#"{ "name": "is-odd", "version": "1.0.0" }"#,
        )
        .write(
            "packages/old/package.json",
            r#"{ "name": "old", "version": "1.0.0", "dependencies": { "chalk": "4.1.0" } }"#,
        )
        .write(
            "vendor/loc/package.json",
            r#"{ "name": "loc", "version": "1.0.0" }"#,
        )
        .write(".yarnrc.yml", "enableTelemetry: false\n");
    fixture
}

/// One reported row: its release, whether it is direct, and the paths of its members.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    name: String,
    current: String,
    direct: bool,
    members: BTreeSet<String>,
}

/// Every reported row; an item without `members` belongs to no member.
fn rows(envelope: &serde_json::Value) -> eyre::Result<Vec<Row>> {
    let items = envelope
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| eyre::eyre!("the envelope lists no items: {envelope:#}"))?;
    items
        .iter()
        .map(|item| {
            let row = || -> Option<Row> {
                Some(Row {
                    name: item.get("name")?.as_str()?.to_string(),
                    current: item.get("current")?.as_str()?.to_string(),
                    direct: item.get("direct")?.as_bool()?,
                    members: item
                        .get("members")
                        .and_then(serde_json::Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|member| Some(member.get("path")?.as_str()?.to_string()))
                        .collect(),
                })
            };
            row().ok_or_else(|| {
                eyre::eyre!("an item lacks its name, release or directness: {item:#}")
            })
        })
        .collect()
}

/// Every reported row by its `name@current`, which names one row only.
fn rows_by_release(rows: Vec<Row>) -> eyre::Result<BTreeMap<String, Row>> {
    let mut by_release = BTreeMap::new();
    for row in rows {
        let release = format!("{}@{}", row.name, row.current);
        if let Some(earlier) = by_release.insert(release, row) {
            eyre::bail!("two rows report {}@{}", earlier.name, earlier.current);
        }
    }
    Ok(by_release)
}

/// The row of `release`, as `name@current`.
fn row<'a>(rows: &'a BTreeMap<String, Row>, release: &str) -> eyre::Result<&'a Row> {
    rows.get(release)
        .ok_or_else(|| eyre::eyre!("no row reports {release}: {rows:?}"))
}

/// The `heldBy` of the row `name@current`, if any.
fn held_by(envelope: &serde_json::Value, name: &str, current: &str) -> Option<String> {
    envelope
        .get("items")?
        .as_array()?
        .iter()
        .find(|item| {
            item.get("name").and_then(serde_json::Value::as_str) == Some(name)
                && item.get("current").and_then(serde_json::Value::as_str) == Some(current)
        })?
        .get("heldBy")?
        .as_str()
        .map(str::to_string)
}

/// A locator or descriptor split after its package name: the name and what follows its `@`.
fn split_after_name(text: &str) -> Option<(&str, &str)> {
    // A scope's leading `@` belongs to the name.
    let name_end = text.get(1..)?.find('@')? + 1;
    let (name, rest) = text.split_at(name_end);
    Some((name, rest.get(1..)?))
}

/// The registry version of `name` a locator Yarn reports resolves, through Yarn's builtin patch
/// and a peer-dependency virtual instance too, or `None` for a locator of any other package or
/// source.
fn locator_version(name: &str, locator: &str) -> Option<String> {
    let (locator_name, reference) = split_after_name(locator)?;
    if locator_name != name {
        return None;
    }
    // `virtual:<hash>#<reference>` is one peer context of the release `<reference>` locates.
    let reference = match reference.strip_prefix("virtual:") {
        Some(virtual_reference) => virtual_reference.split_once('#')?.1,
        None => reference,
    };
    if let Some(version) = reference.strip_prefix("npm:") {
        return Some(version.to_string());
    }
    // `patch:<name>@npm%3A<version>#<patch>::<parameters>`
    let source = reference.strip_prefix("patch:")?.split('#').next()?;
    let source = source.replace("%3A", ":");
    let (inner, version) = split_after_name(&source)?;
    (inner == name)
        .then(|| version.strip_prefix("npm:").map(str::to_string))
        .flatten()
}

/// For each version of `name`, the path of every workspace Yarn's own `yarn why` binds it for
/// through a declaration of `name` itself, a builtin patch's inner release included.
///
/// A virtual instance of a workspace is the workspace seen from a dependent and is left out, and
/// so is a dependency an alias of another name binds.
fn yarns_owners(fixture: &Fixture, name: &str) -> eyre::Result<BTreeMap<String, BTreeSet<String>>> {
    let output = fixture
        .run_tool_traced(
            "ask Yarn why the package is installed",
            "yarn",
            &["why", name, "--json"],
            &[],
        )?
        .require_success()?;
    let mut owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in output.stdout_str().lines() {
        let entry: serde_json::Value = serde_json::from_str(line)?;
        let Some(dependent) = entry.get("value").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some((_, reference)) = split_after_name(dependent) else {
            continue;
        };
        let Some(path) = reference.strip_prefix("workspace:") else {
            continue;
        };
        let children = entry
            .get("children")
            .and_then(serde_json::Value::as_object)
            .into_iter()
            .flatten();
        for (_, child) in children {
            let field = |key: &str| child.get(key).and_then(serde_json::Value::as_str);
            let (Some(descriptor), Some(locator)) = (field("descriptor"), field("locator")) else {
                continue;
            };
            if split_after_name(descriptor).map(|(declared, _)| declared) != Some(name) {
                continue;
            }
            if let Some(version) = locator_version(name, locator) {
                owners.entry(version).or_default().insert(path.to_string());
            }
        }
    }
    Ok(owners)
}

/// cooldown reports exactly the releases Yarn's own inventory lists, and each one's members are
/// exactly the workspaces `yarn why` binds it for through a declaration of its own name, a row
/// being direct exactly when it has a member.
/// The root's `semver`, declared at two ranges, is the one name the lock does not prove: its row
/// is direct but owned by no member, and the run names it.
#[test]
fn berry_members_match_yarns_own_bindings() -> eyre::Result<()> {
    let fixture = berry_members();
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;

    let outdated = cooldown(
        &fixture,
        &["outdated", "--offline", "--transitive", "--all"],
    )?;
    assert_eq!(outdated.code, Some(0), "{:#}", outdated.envelope);
    let rows = rows_by_release(rows(&outdated.envelope)?)?;
    let yarns = yarns_registry_releases(&fixture)?;
    assert_eq!(rows.keys().cloned().collect::<BTreeSet<_>>(), yarns);

    // Ownership, release by release over Yarn's own inventory
    let names: BTreeSet<&str> = yarns
        .iter()
        .filter_map(|release| split_after_name(release).map(|(name, _)| name))
        .collect();
    for name in names {
        let owners = yarns_owners(&fixture, name)?;
        for (release, row) in rows.iter().filter(|(_, row)| row.name == name) {
            if name == "semver" {
                assert!(row.direct && row.members.is_empty(), "{row:?}");
                continue;
            }
            let expected = owners.get(&row.current).cloned().unwrap_or_default();
            assert_eq!(
                row.members, expected,
                "{release} against yarn why {owners:?}"
            );
            assert_eq!(row.direct, !row.members.is_empty(), "{row:?}");
        }
    }

    // The rows each kind of declaration owns are among them.
    let members = |release: &str| -> eyre::Result<BTreeSet<String>> {
        Ok(row(&rows, release)?.members.clone())
    };
    let paths = |paths: &[&str]| -> BTreeSet<String> {
        paths.iter().map(|path| (*path).to_string()).collect()
    };
    assert_eq!(members("chalk@4.1.2")?, paths(&[".", "packages/a"]));
    assert_eq!(members("chalk@4.1.0")?, paths(&["packages/old"]));
    assert_eq!(members("is-number@6.0.0")?, paths(&["packages/b/nested/c"]));
    assert_eq!(members("ms@2.0.0")?, paths(&["packages/b"]));
    assert_eq!(members("semver@7.5.4")?, paths(&[]));

    let warnings = messages(&outdated.envelope, "warnings");
    assert!(
        warnings
            .iter()
            .any(|message| message
                .contains("semver in . (its install fields declare different ranges)")),
        "{warnings:?}"
    );
    Ok(())
}

/// Root `resolutions` and a catalog rewrite declarations the lock still records as written: Yarn
/// binds `packages/a`'s `chalk` caret to another copy than the root's same caret, whose lock key
/// survives.
/// cooldown credits neither `chalk` copy nor the catalog's `react` to any member, keeps them
/// direct, and names both, while `has-flag`, which nothing rewrites, stays `packages/a`'s.
#[test]
fn berry_rewritten_names_stay_unattributed() -> eyre::Result<()> {
    let fixture = Fixture::new().tag_independent();
    fixture
        .write(
            "package.json",
            &indoc::formatdoc! {r#"
                {{
                  "name": "berry-rewrites",
                  "private": true,
                  "packageManager": "yarn@{YARN}",
                  "workspaces": ["packages/*"],
                  "dependencies": {{ "chalk": "^4.1.0", "react": "catalog:" }},
                  "resolutions": {{ "a/chalk": "4.1.0" }}
                }}
            "#},
        )
        .write(
            "packages/a/package.json",
            r#"{ "name": "a", "version": "1.0.0", "dependencies": { "chalk": "^4.1.0", "has-flag": "^4.0.0" } }"#,
        )
        .write(
            ".yarnrc.yml",
            "enableTelemetry: false\ncatalog:\n  react: ^18.2.0\n",
        );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;

    // The hazard is real: Yarn binds the two members' identical carets to different copies.
    let chalk = yarns_owners(&fixture, "chalk")?;
    let copy_of = |member: &str| {
        chalk
            .iter()
            .find(|(_, members)| members.contains(member))
            .map(|(version, _)| version.clone())
    };
    assert_eq!(copy_of("packages/a").as_deref(), Some("4.1.0"), "{chalk:?}");
    assert!(
        copy_of(".").is_some_and(|version| version != "4.1.0"),
        "{chalk:?}"
    );

    let outdated = cooldown(
        &fixture,
        &["outdated", "--offline", "--transitive", "--all"],
    )?;
    assert_eq!(outdated.code, Some(0), "{:#}", outdated.envelope);
    let rows = rows_by_release(rows(&outdated.envelope)?)?;
    assert_eq!(
        rows.keys().cloned().collect::<BTreeSet<_>>(),
        yarns_registry_releases(&fixture)?
    );

    // Both `chalk` copies Yarn binds and the catalog's `react` are direct but owned by no member.
    let mut unattributed: Vec<&Row> = chalk
        .keys()
        .map(|version| row(&rows, &format!("chalk@{version}")))
        .collect::<eyre::Result<_>>()?;
    unattributed.extend(rows.values().filter(|row| row.name == "react"));
    assert_eq!(unattributed.len(), 3, "{rows:?}");
    for row in unattributed {
        assert!(row.direct && row.members.is_empty(), "{row:?}");
    }

    // `has-flag`, which nothing rewrites, is `packages/a`'s, as Yarn binds it.
    let has_flag = row(&rows, "has-flag@4.0.0")?;
    assert_eq!(has_flag.members, BTreeSet::from(["packages/a".to_string()]));
    assert_eq!(
        Some(&has_flag.members),
        yarns_owners(&fixture, "has-flag")?.get("4.0.0")
    );

    let warnings = messages(&outdated.envelope, "warnings");
    assert!(
        warnings.iter().any(|message| {
            message.contains("chalk in ., packages/a (the root `resolutions` field may rewrite it)")
                && message.contains("react in . (it is a `catalog:` reference)")
        }),
        "{warnings:?}"
    );
    Ok(())
}

/// Excluding a member drops exactly the rows only it owns and recomputes the rest from the
/// members left: the root's exact `chalk` pin, which `packages/a`'s caret shares, holds the copy
/// once `packages/a` is excluded.
#[test]
fn berry_excluded_member_drops_only_its_copies() -> eyre::Result<()> {
    let fixture = berry_members();
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    let read = |args: &[&str]| -> eyre::Result<serde_json::Value> {
        let run = cooldown(&fixture, args)?;
        assert_eq!(run.code, Some(0), "{args:?}: {:#}", run.envelope);
        Ok(run.envelope)
    };
    let offline = ["outdated", "--offline", "--transitive", "--all"];
    let online = ["outdated", "--transitive", "--all"];
    let shared = rows(&read(&offline)?)?
        .into_iter()
        .find(|row| row.name == "chalk" && row.members.contains("packages/a"))
        .ok_or_else(|| eyre::eyre!("packages/a shares a chalk copy"))?;
    assert_eq!(
        shared.members,
        BTreeSet::from([".".to_string(), "packages/a".to_string()])
    );
    let unexcluded = read(&online)?;
    assert_ne!(
        held_by(&unexcluded, "chalk", &shared.current).as_deref(),
        Some("pinned"),
        "the caret sharing the copy leaves it unpinned: {unexcluded:#}"
    );
    let before = rows(&read(&offline)?)?;

    fixture.write(
        "cooldown.toml",
        "[global]\nexclude-folders = [\"packages/a\"]\n",
    );
    let after = rows(&read(&offline)?)?;

    // Every row `packages/a` owns alone is gone, and every other row keeps its other members.
    let mut expected: Vec<Row> = before
        .iter()
        .filter(|row| row.members != BTreeSet::from(["packages/a".to_string()]))
        .cloned()
        .map(|mut row| {
            row.members.remove("packages/a");
            row
        })
        .collect();
    expected.sort_by(|left, right| (&left.name, &left.current).cmp(&(&right.name, &right.current)));
    let mut after_sorted = after.clone();
    after_sorted
        .sort_by(|left, right| (&left.name, &left.current).cmp(&(&right.name, &right.current)));
    assert_eq!(after_sorted, expected);
    assert!(
        before.len() > after.len(),
        "packages/a owns rows of its own: {before:?}"
    );

    // The root's exact pin now holds the shared copy alone.
    let excluded = read(&online)?;
    assert_eq!(
        held_by(&excluded, "chalk", &shared.current).as_deref(),
        Some("pinned"),
        "{excluded:#}"
    );
    Ok(())
}

/// A configured plugin may rewrite any declaration, so the run warns that it attributes no
/// dependency to a member: every row of a name a workspace declares is direct but owned by no
/// member, while purely transitive rows stay transitive.
#[test]
fn a_berry_plugin_makes_every_binding_unproven() -> eyre::Result<()> {
    let fixture = berry_members();
    fixture
        .write(
            ".yarn/plugins/noop.cjs",
            "module.exports = { name: `noop`, factory: () => ({}) };\n",
        )
        .write(
            ".yarnrc.yml",
            "enableTelemetry: false\nplugins:\n  - path: .yarn/plugins/noop.cjs\n    spec: noop\n",
        );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;

    let outdated = cooldown(
        &fixture,
        &["outdated", "--offline", "--transitive", "--all"],
    )?;
    assert_eq!(outdated.code, Some(0), "{:#}", outdated.envelope);
    let rows = rows(&outdated.envelope)?;
    assert!(rows.iter().all(|row| row.members.is_empty()), "{rows:?}");
    for name in ["chalk", "debug", "ms", "semver", "typescript", "is-number"] {
        let copies: Vec<&Row> = rows.iter().filter(|row| row.name == name).collect();
        assert!(
            !copies.is_empty() && copies.iter().all(|row| row.direct),
            "{name}: {rows:?}"
        );
    }
    let transitive: Vec<&Row> = rows
        .iter()
        .filter(|row| row.name == "ansi-styles")
        .collect();
    assert!(
        !transitive.is_empty() && transitive.iter().all(|row| !row.direct),
        "{rows:?}"
    );
    let warnings = messages(&outdated.envelope, "warnings");
    assert!(
        warnings.iter().any(
            |message| message.contains(".yarnrc.yml configures Yarn plugins")
                && message.contains("so cooldown attributes no dependency to a workspace member")
        ),
        "{warnings:?}"
    );
    Ok(())
}
