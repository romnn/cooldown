//! End-to-end tests that drive the REAL Yarn Berry (the `yarn = "4.18.1"` pin in `mise.toml`)
//! against fixtures generated on the fly in temp dirs.
//!
//! cooldown reads a Berry `yarn.lock` for its registry releases, attributes each to the workspace
//! members whose own declarations bind it, and lands exact targets in it, so these tests check
//! these invariants against Yarn itself:
//!
//! - the rows cooldown reads are exactly the npm releases Yarn's own `yarn info` reports;
//! - each row's members are exactly the workspaces `yarn why` binds it for through a declaration
//!   of its own name, and a declaration a rewrite or a plugin may have moved leaves its name
//!   direct but owned by no member;
//! - excluding a member drops exactly the rows only it owns;
//! - `upgrade` and `fix` land each target exactly where Yarn's own `yarn why` then binds it, below
//!   a range's maximum and downward too, moving no other binding, writing nothing but the lock,
//!   leaving a lock Yarn verifies current, and converging on a second run;
//! - a candidate Yarn rejects, such as one whose new transitive its age gate quarantines, is
//!   rolled back alone, while a young target cooldown admits lands through an exact approval;
//! - a Berry `yarn` never runs on a Classic lock, which it would silently migrate;
//! - `check` takes Yarn's own verdict on the lock from an offline run in a copy, so a current
//!   lock is evaluated, a stale one is refused unless waived, one Yarn cannot judge offline is
//!   refused outright, and the project stays byte for byte as it was;
//! - the age gate `sync` writes into `.yarnrc.yml` is the one Yarn applies: a release younger
//!   than it is quarantined when resolved afresh, and a release cooldown exempts or admits is not.
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

/// The environment that keeps Yarn's human output plain, which Yarn colours wherever it detects a
/// terminal or a CI that shows colours, such as GitHub Actions.
const PLAIN: [(&str, &str); 1] = [("YARN_ENABLE_COLORS", "0")];

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

/// The kinds of the lock diagnostics in the envelope's `section` array.
fn lock_kinds(envelope: &serde_json::Value, section: &str) -> Vec<String> {
    envelope
        .get(section)
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|diagnostic| diagnostic.get("kind")?.as_str())
        .filter(|kind| kind.contains("lock"))
        .map(ToString::to_string)
        .collect()
}

/// Every path below `root` with its kind, mode, modification time, and content or link target,
/// so a rewrite with identical bytes still shows.
///
/// The fixture's `.git` anchor is left out: cooldown keeps its own run leases there.
fn snapshot(root: &std::path::Path) -> eyre::Result<BTreeMap<String, String>> {
    let mut entries = BTreeMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path == root.join(".git") {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            let modified = metadata.modified()?;
            let shown = if metadata.is_symlink() {
                format!("link {}", std::fs::read_link(&path)?.display())
            } else if metadata.is_dir() {
                pending.push(path.clone());
                format!("dir {:?} {modified:?}", metadata.permissions())
            } else {
                let mut hasher = std::hash::DefaultHasher::new();
                std::hash::Hash::hash(&std::fs::read(&path)?, &mut hasher);
                format!(
                    "file {:?} {modified:?} {:x}",
                    metadata.permissions(),
                    std::hash::Hasher::finish(&hasher)
                )
            };
            let relative = path
                .strip_prefix(root)?
                .to_string_lossy()
                .replace('\\', "/");
            entries.insert(relative, shown);
        }
    }
    Ok(entries)
}

/// Copies the tree at `from` to `to`, links as links.
fn copy_tree(from: &std::path::Path, to: &std::path::Path) -> eyre::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// `lock` with its `__metadata` schema changed from `from` to `to`, whatever line ending Yarn
/// wrote, which is CRLF on Windows.
fn with_schema(lock: &str, from: &str, to: &str) -> String {
    let mut in_metadata = false;
    let mut changed = false;
    lock.split_inclusive('\n')
        .map(|line| {
            let content = line.trim_end_matches(['\r', '\n']);
            let ending = &line[content.len()..];
            if content == "__metadata:" {
                in_metadata = true;
            } else if in_metadata && !changed && content == format!("  version: {from}") {
                changed = true;
                return format!("  version: {to}{ending}");
            } else if !content.starts_with(' ') {
                in_metadata = false;
            }
            line.to_string()
        })
        .collect()
}

/// Runs `cooldown check --offline` with `extra`.
fn check(fixture: &Fixture, extra: &[&str]) -> eyre::Result<Run> {
    let mut args = vec!["check", "--offline"];
    args.extend_from_slice(extra);
    cooldown(fixture, &args)
}

/// A freshly seeded lock is current: `check` evaluates it with no lock diagnostic, and leaves
/// every file of the project, Yarn's project-local cache included, exactly as it was, down to
/// its modification time.
/// For a lock of an older schema, Yarn rewrites the `.yarnrc.yml` of the project it runs in and
/// re-resolves the lock's entries, which the offline run cannot, so the verdict is unknown, yet
/// the project's own rc file stays untouched too.
/// The control runs Yarn's own immutable install in place on a copy of the project, which the
/// same snapshot sees change.
#[test]
fn check_proves_a_seeded_lock_current_without_touching_the_project() -> eyre::Result<()> {
    let fixture = berry_workspace();
    fixture.write(
        ".yarnrc.yml",
        "enableTelemetry: false\nenableGlobalCache: false\n",
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    let before = snapshot(fixture.root())?;
    assert!(
        before.keys().any(|path| path.starts_with(".yarn/cache/")),
        "the seed filled the project-local cache: {before:?}"
    );

    let checked = check(&fixture, &[])?;

    assert!(
        lock_kinds(&checked.envelope, "errors").is_empty()
            && lock_kinds(&checked.envelope, "warnings").is_empty(),
        "{:#}",
        checked.envelope
    );
    assert_eq!(
        changed(&before, &snapshot(fixture.root())?),
        [] as [&str; 0]
    );

    // The schema Yarn 4.0 through 4.13 wrote, which Yarn 4.18.1 migrates the rc file for.
    let lock = String::from_utf8(fixture.read_bytes("yarn.lock"))?;
    let older = with_schema(&lock, "10", "8");
    assert_ne!(older, lock);
    fixture.write("yarn.lock", &older);
    let before = snapshot(fixture.root())?;
    let migrated = check(&fixture, &[])?;
    assert_eq!(
        lock_kinds(&migrated.envelope, "errors"),
        ["lock_unknown"],
        "{:#}",
        migrated.envelope
    );
    assert_eq!(
        changed(&before, &snapshot(fixture.root())?),
        [] as [&str; 0]
    );
    fixture.write("yarn.lock", &lock);

    // Control: the same install in place writes into the project.
    let control = tempfile::tempdir()?;
    copy_tree(fixture.root(), control.path())?;
    let copied = snapshot(control.path())?;
    // Resolve `yarn` as the harness does, since Windows installs it as `yarn.cmd`.
    let output = std::process::Command::new(cooldown_adapter_util::resolve_program("yarn"))
        .args(["install", "--immutable", "--mode=skip-build"])
        .current_dir(control.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_ne!(
        changed(&copied, &snapshot(control.path())?),
        [] as [&str; 0]
    );
    Ok(())
}

/// The paths whose snapshot entry differs between `before` and `after`, appeared or vanished
/// ones included.
fn changed<'a>(
    before: &'a BTreeMap<String, String>,
    after: &'a BTreeMap<String, String>,
) -> Vec<&'a str> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A member that drops a dependency makes the lock stale, which Yarn proves offline: `check`
/// refuses it, and `--allow-stale-lock` turns the refusal into a warning.
#[test]
fn a_dropped_member_dependency_is_stale_and_waivable() -> eyre::Result<()> {
    let fixture = berry_workspace();
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    fixture.write(
        "packages/app/package.json",
        r#"{ "name": "app", "version": "1.0.0", "dependencies": { "chalk": "4.1.0" } }"#,
    );
    let lock = fixture.read_bytes("yarn.lock");

    let refused = check(&fixture, &[])?;
    assert_eq!(
        lock_kinds(&refused.envelope, "errors"),
        ["stale_lock"],
        "{:#}",
        refused.envelope
    );
    assert_eq!(refused.code, Some(4));
    let detail = messages(&refused.envelope, "errors").join("\n");
    assert!(detail.contains("-    has-flag: \"npm:^4.0.0\""), "{detail}");

    let waived = check(&fixture, &["--allow-stale-lock"])?;
    assert!(
        lock_kinds(&waived.envelope, "errors").is_empty(),
        "{:#}",
        waived.envelope
    );
    assert_eq!(lock_kinds(&waived.envelope, "warnings"), ["stale_lock"]);
    assert_eq!(fixture.read_bytes("yarn.lock"), lock);
    Ok(())
}

/// A new dependency the lock does not record needs the registry, which the offline run never
/// reaches, so the lock's currency is unknown, and `--allow-stale-lock` does not waive that.
#[test]
fn a_dependency_needing_the_registry_is_unknown_and_not_waivable() -> eyre::Result<()> {
    let fixture = berry_workspace();
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    fixture.write(
        "packages/app/package.json",
        r#"{ "name": "app", "version": "1.0.0", "dependencies": { "chalk": "4.1.0", "has-flag": "^4.0.0", "is-odd": "^3.0.1" } }"#,
    );

    let refused = check(&fixture, &["--allow-stale-lock"])?;

    assert_eq!(
        lock_kinds(&refused.envelope, "errors"),
        ["lock_unknown"],
        "{:#}",
        refused.envelope
    );
    assert_eq!(refused.code, Some(4));
    let detail = messages(&refused.envelope, "errors").join("\n");
    assert!(detail.contains("reached no verdict offline"), "{detail}");
    Ok(())
}

/// A workspace below a `vendor` directory, and a `file:` folder whole, are inputs of the copy:
/// the seeded lock is current only with both, and a narrowed directory or a file added to the
/// folder makes it stale, since Yarn hashes the folder, permissions included, into its locator.
#[test]
fn every_workspace_and_local_folder_is_an_input() -> eyre::Result<()> {
    let fixture = berry_workspace();
    fixture
        .write(
            "package.json",
            &indoc::formatdoc! {r#"
                {{
                  "name": "berry-root",
                  "private": true,
                  "packageManager": "yarn@{YARN}",
                  "workspaces": ["packages/*", "packages/vendor/*"],
                  "dependencies": {{ "chalk": "^4.1.0", "folder": "file:./folder" }}
                }}
            "#},
        )
        .write(
            "packages/vendor/kept/package.json",
            r#"{ "name": "kept", "version": "1.0.0", "dependencies": { "has-flag": "^4.0.0" } }"#,
        )
        .write(
            "folder/package.json",
            r#"{ "name": "folder", "version": "1.0.0" }"#,
        )
        .write("folder/index.js", "module.exports = 1;\n")
        .write("folder/lib/util.js", "module.exports = 3;\n");
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;

    let current = check(&fixture, &[])?;
    assert!(
        lock_kinds(&current.envelope, "errors").is_empty(),
        "{:#}",
        current.envelope
    );

    // Yarn hashes a directory's permission bits too, so narrowing one makes the lock stale.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let lib = fixture.root().join("folder/lib");
        let mode = std::fs::metadata(&lib)?.permissions().mode();
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(0o700))?;
        let narrowed = check(&fixture, &[])?;
        std::fs::set_permissions(&lib, std::fs::Permissions::from_mode(mode))?;
        assert_ne!(mode & 0o777, 0o700);
        assert_eq!(
            lock_kinds(&narrowed.envelope, "errors"),
            ["stale_lock"],
            "{:#}",
            narrowed.envelope
        );
    }

    fixture.write("folder/extra.js", "module.exports = 2;\n");
    let stale = check(&fixture, &[])?;
    assert_eq!(
        lock_kinds(&stale.envelope, "errors"),
        ["stale_lock"],
        "{:#}",
        stale.envelope
    );
    Ok(())
}

/// An rc file above the project is copied to its mirrored position: a lock seeded under a
/// `packageExtensions` entry it declares is current, and removing the entry makes it stale.
#[test]
fn an_ancestor_rc_file_decides_the_verdict() -> eyre::Result<()> {
    let fixture = Fixture::new().tag_independent();
    fixture
        .write(
            "nested/project/package.json",
            &indoc::formatdoc! {r#"
                {{
                  "name": "nested",
                  "private": true,
                  "packageManager": "yarn@{YARN}",
                  "dependencies": {{ "has-flag": "^4.0.0" }}
                }}
            "#},
        )
        .write("nested/project/yarn.lock", "")
        .write(
            "nested/.yarnrc.yml",
            indoc! {r#"
                enableTelemetry: false
                packageExtensions:
                  "has-flag@*":
                    dependencies:
                      is-number: "^7.0.0"
            "#},
        );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    // Resolve `yarn` as the harness does, since Windows installs it as `yarn.cmd`.
    let seeded = std::process::Command::new(cooldown_adapter_util::resolve_program("yarn"))
        .args(["install", "--mode=update-lockfile"])
        .current_dir(fixture.root().join("nested/project"))
        .output()?;
    assert!(
        seeded.status.success(),
        "{}",
        String::from_utf8_lossy(&seeded.stdout)
    );

    let current = cooldown_in(&fixture, "nested/project")?;
    assert!(
        lock_kinds(&current.envelope, "errors").is_empty(),
        "{:#}",
        current.envelope
    );

    fixture.write("nested/.yarnrc.yml", "enableTelemetry: false\n");
    let stale = cooldown_in(&fixture, "nested/project")?;
    assert_eq!(
        lock_kinds(&stale.envelope, "errors"),
        ["stale_lock"],
        "{:#}",
        stale.envelope
    );
    Ok(())
}

/// Runs `cooldown check --offline --json` pinned to the fixture's `directory`.
fn cooldown_in(fixture: &Fixture, directory: &str) -> eyre::Result<Run> {
    let output = fixture.cooldown_in(Some(directory), &["check", "--offline", "--json"]);
    let envelope = serde_json::from_slice(&output.stdout).map_err(|err| {
        eyre::eyre!(
            "cooldown printed no JSON envelope ({err}); stderr: {}",
            output.stderr_str()
        )
    })?;
    Ok(Run {
        code: output.status.code(),
        envelope,
    })
}

/// An rc file Yarn would refuse to parse makes the lock's currency unknown, and nothing runs.
#[test]
fn a_malformed_rc_file_is_unknown() -> eyre::Result<()> {
    let fixture = berry_workspace();
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    fixture.write(".yarnrc.yml", "enableTelemetry false\n  nodeLinker: [\n");

    let refused = check(&fixture, &["--allow-stale-lock"])?;

    assert_eq!(
        lock_kinds(&refused.envelope, "errors"),
        ["lock_unknown"],
        "{:#}",
        refused.envelope
    );
    Ok(())
}

/// A Berry project in `dir` (`.` for the fixture root) declaring `dependencies`, with a comment and
/// a setting in its `.yarnrc.yml` and `config` as the repository's `cooldown.toml`.
///
/// No dependency here is a release the tests later resolve afresh, so a resolve that succeeds
/// does so through the age gate, never by reusing a locked copy.
fn age_gate_fixture(dir: &str, dependencies: &str, config: &str) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    let file = |name: &str| {
        if dir == "." {
            name.to_string()
        } else {
            format!("{dir}/{name}")
        }
    };
    fixture
        .write(
            &file("package.json"),
            &indoc::formatdoc! {r#"
                {{
                  "name": "berry-age-gate",
                  "private": true,
                  "packageManager": "yarn@{YARN}",
                  "dependencies": {dependencies}
                }}
            "#},
        )
        .write(
            &file(".yarnrc.yml"),
            "# Kept by every sync\nenableTelemetry: false\n",
        )
        .write("cooldown.toml", config);
    fixture
}

/// Seeds the lock of the project in `dir` with Yarn's own resolution, before any gate applies.
fn seed_lock_in(fixture: &Fixture, dir: &str) -> eyre::Result<()> {
    let lock = if dir == "." {
        "yarn.lock".to_string()
    } else {
        format!("{dir}/yarn.lock")
    };
    fixture.write(&lock, "");
    yarn_in(
        fixture,
        dir,
        "seed the Berry lock",
        &["install", "--mode=update-lockfile"],
    )?
    .require_success()?;
    Ok(())
}

/// Runs `yarn args` in the project in `dir`, its human output plain ([`PLAIN`]).
fn yarn_in(
    fixture: &Fixture,
    dir: &str,
    phase: &str,
    args: &[&str],
) -> eyre::Result<support::CapturedOutput> {
    let mut full = vec!["--cwd", dir];
    full.extend_from_slice(args);
    fixture.run_tool_traced(phase, "yarn", &full, &PLAIN)
}

/// Yarn's own reading of its setting `key` in the project in `dir`.
fn yarns_setting(fixture: &Fixture, dir: &str, key: &str) -> eyre::Result<serde_json::Value> {
    let output = yarn_in(
        fixture,
        dir,
        "read a Yarn setting",
        &["config", "get", key, "--json"],
    )?
    .require_success()?;
    Ok(serde_json::from_str(output.stdout_str().trim())?)
}

/// Whether Yarn refuses `args` in the project in `dir` because its age gate quarantines every
/// release they ask for; any other failure fails the test.
fn quarantined(fixture: &Fixture, dir: &str, args: &[&str]) -> eyre::Result<bool> {
    let output = yarn_in(fixture, dir, "resolve under the age gate", args)?;
    if output.status.success() {
        return Ok(false);
    }
    let text = format!("{}{}", output.stdout_str(), output.stderr_str());
    eyre::ensure!(
        text.contains("quarantined"),
        "yarn {args:?} failed for another reason than the age gate: {text}"
    );
    Ok(true)
}

/// Runs `cooldown sync --tool yarn` with `extra` and returns each item's status, requiring the
/// sync to succeed.
fn sync_statuses(fixture: &Fixture, extra: &[&str]) -> eyre::Result<Vec<String>> {
    let mut args = vec!["sync", "--tool", "yarn"];
    args.extend_from_slice(extra);
    let run = cooldown(fixture, &args)?;
    eyre::ensure!(run.code == Some(0), "sync failed: {:#}", run.envelope);
    Ok(run
        .envelope
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| Some(item.get("status")?.as_str()?.to_string()))
        .collect())
}

/// One hundred years in Yarn's minutes: older than every release, so the gate quarantines each
/// one nothing approves.
const CENTURY_MINUTES: u64 = 36_500 * 24 * 60;

/// The gate `sync` writes is the one Yarn applies, and a `latest` package is approved by it:
/// with the gate alone Yarn quarantines `has-flag@4.0.0` and changes nothing, and once cooldown
/// exempts `has-flag` the same resolve succeeds while every other package stays gated.
/// A dry run reports the write without making it, the rc file keeps its comment and setting, and
/// a second sync is unchanged.
#[test]
fn berry_sync_writes_the_age_gate_yarn_enforces() -> eyre::Result<()> {
    let fixture = age_gate_fixture(".", r#"{ "is-odd": "3.0.1" }"#, "min-age = \"36500d\"\n");
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_in(&fixture, ".")?;
    let rc_before = fixture.read_bytes(".yarnrc.yml");

    // A dry run reports the write and leaves the rc file alone.
    assert_eq!(sync_statuses(&fixture, &["--dry-run"])?, ["written"]);
    assert_eq!(fixture.read_bytes(".yarnrc.yml"), rc_before);

    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    let rc = String::from_utf8(fixture.read_bytes(".yarnrc.yml"))?;
    assert!(
        rc.starts_with("# Kept by every sync\nenableTelemetry: false\n"),
        "{rc}"
    );
    assert_eq!(
        yarns_setting(&fixture, ".", "npmMinimalAgeGate")?,
        serde_json::json!(CENTURY_MINUTES)
    );

    // The gate alone quarantines the release, and the failed resolve changes nothing.
    let manifest = fixture.read_bytes("package.json");
    let lock = fixture.read_bytes("yarn.lock");
    let add_has_flag = ["add", "has-flag@4.0.0", "--mode=update-lockfile"];
    assert!(quarantined(&fixture, ".", &add_has_flag)?);
    assert_eq!(fixture.read_bytes("package.json"), manifest);
    assert_eq!(fixture.read_bytes("yarn.lock"), lock);

    // Exempting `has-flag` approves it natively, and only it.
    fixture.write(
        "cooldown.toml",
        indoc! {r#"
            min-age = "36500d"

            [package."has-flag"]
            latest = true
        "#},
    );
    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    assert_eq!(
        yarns_setting(&fixture, ".", "npmPreapprovedPackages")?,
        serde_json::json!(["has-flag"])
    );
    assert!(!quarantined(&fixture, ".", &add_has_flag)?);
    assert!(quarantined(
        &fixture,
        ".",
        &["add", "is-number@7.0.0", "--mode=update-lockfile"]
    )?);

    assert_eq!(sync_statuses(&fixture, &[])?, ["unchanged"]);
    Ok(())
}

/// A young locked version cooldown admits is approved by its exact version, so the lock still
/// resolves afresh: without the admission Yarn quarantines the pinned `is-number@7.0.0` once the
/// lock is gone, and with it the same fresh resolve succeeds.
#[test]
fn berry_sync_approves_the_young_locked_versions_cooldown_admits() -> eyre::Result<()> {
    let fixture = age_gate_fixture(".", r#"{ "is-number": "7.0.0" }"#, "min-age = \"36500d\"\n");
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_in(&fixture, ".")?;
    let seeded = String::from_utf8(fixture.read_bytes("yarn.lock"))?;
    let resolve_afresh = |fixture: &Fixture| -> eyre::Result<bool> {
        fixture.write("yarn.lock", "");
        quarantined(fixture, ".", &["install", "--mode=update-lockfile"])
    };

    // The gate alone approves nothing, so a fresh resolve of the pin is quarantined.
    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    assert_eq!(
        yarns_setting(&fixture, ".", "npmPreapprovedPackages")?,
        serde_json::json!([])
    );
    assert!(resolve_afresh(&fixture)?);

    // A shorter window for `is-number` admits its locked version, which the admission sync reads
    // from the restored lock.
    fixture.write("yarn.lock", &seeded);
    fixture.write(
        "cooldown.toml",
        indoc! {r#"
            min-age = "36500d"

            [package."is-number"]
            min-age = "1d"
        "#},
    );
    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    assert_eq!(
        yarns_setting(&fixture, ".", "npmPreapprovedPackages")?,
        serde_json::json!(["is-number@7.0.0"])
    );
    assert!(!resolve_afresh(&fixture)?);
    Ok(())
}

/// Yarn adds the entries of every rc file above the project to its list, so the list `sync`
/// writes resets them: the `has-flag` an ancestor approves is approved before the sync and
/// quarantined after it.
#[test]
fn berry_sync_masks_inherited_approvals() -> eyre::Result<()> {
    let fixture = age_gate_fixture("app", r#"{ "is-odd": "3.0.1" }"#, "min-age = \"36500d\"\n");
    fixture.write(".yarnrc.yml", "npmPreapprovedPackages:\n  - \"has-flag\"\n");
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_in(&fixture, "app")?;
    assert_eq!(
        yarns_setting(&fixture, "app", "npmPreapprovedPackages")?,
        serde_json::json!(["has-flag"])
    );

    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    assert_eq!(
        yarns_setting(&fixture, "app", "npmPreapprovedPackages")?,
        serde_json::json!([])
    );
    assert!(quarantined(
        &fixture,
        "app",
        &["add", "has-flag@4.0.0", "--mode=update-lockfile"]
    )?);
    Ok(())
}

/// Yarn gates a configured scope by the scope's own gate, a one-day default unless set, so
/// `sync` sets the gate of the scope entry the project's rc file holds: before the sync Yarn
/// reads 1440 minutes for `@sindresorhus`, after it the written gate, with the scope's registry
/// untouched, and `@sindresorhus/is@4.6.0` is quarantined.
#[test]
fn berry_sync_gates_the_scopes_the_project_configures() -> eyre::Result<()> {
    let fixture = age_gate_fixture(".", r#"{ "is-odd": "3.0.1" }"#, "min-age = \"36500d\"\n");
    fixture.write(
        ".yarnrc.yml",
        indoc! {r#"
            enableTelemetry: false
            npmScopes:
              sindresorhus:
                npmRegistryServer: "https://registry.yarnpkg.com"
        "#},
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_in(&fixture, ".")?;
    let scope = |fixture: &Fixture| -> eyre::Result<serde_json::Value> {
        let scopes = yarns_setting(fixture, ".", "npmScopes")?;
        scopes
            .get("sindresorhus")
            .cloned()
            .ok_or_else(|| eyre::eyre!("Yarn reads no `sindresorhus` scope: {scopes}"))
    };
    assert_eq!(
        scope(&fixture)?.get("npmMinimalAgeGate"),
        Some(&serde_json::json!(1440))
    );

    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    let gated = scope(&fixture)?;
    assert_eq!(
        gated.get("npmMinimalAgeGate"),
        Some(&serde_json::json!(CENTURY_MINUTES))
    );
    assert_eq!(
        gated.get("npmRegistryServer"),
        Some(&serde_json::json!("https://registry.yarnpkg.com"))
    );
    assert!(quarantined(
        &fixture,
        ".",
        &["add", "@sindresorhus/is@4.6.0", "--mode=update-lockfile"]
    )?);
    Ok(())
}

/// An empty scope entry makes Yarn read the scope's defaults and ignore its entries in the rc
/// files above, so `sync` never fills it with a gate, which would restore the inherited registry:
/// Yarn routes `@sindresorhus` to its default registry before and after the sync, its gate stays
/// the one-day default, and the sync warns about it.
#[test]
fn berry_sync_leaves_an_empty_scope_entry_masking_its_inherited_one() -> eyre::Result<()> {
    let fixture = age_gate_fixture("app", r#"{ "is-odd": "3.0.1" }"#, "min-age = \"36500d\"\n");
    fixture
        .write(
            ".yarnrc.yml",
            indoc! {r#"
                npmScopes:
                  sindresorhus:
                    npmRegistryServer: "https://npm.corp.invalid"
            "#},
        )
        .write(
            "app/.yarnrc.yml",
            "enableTelemetry: false\nnpmScopes:\n  sindresorhus:\n",
        );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_in(&fixture, "app")?;
    let scope = |fixture: &Fixture| -> eyre::Result<serde_json::Value> {
        let scopes = yarns_setting(fixture, "app", "npmScopes")?;
        scopes
            .get("sindresorhus")
            .cloned()
            .ok_or_else(|| eyre::eyre!("Yarn reads no `sindresorhus` scope: {scopes}"))
    };
    let masked = scope(&fixture)?;
    assert_eq!(
        masked.get("npmRegistryServer"),
        Some(&serde_json::json!("https://registry.yarnpkg.com"))
    );

    let run = cooldown(&fixture, &["sync", "--tool", "yarn"])?;
    assert_eq!(run.code, Some(0), "{:#}", run.envelope);
    assert_eq!(scope(&fixture)?, masked);
    assert!(
        messages(&run.envelope, "warnings")
            .iter()
            .any(|message| message.contains("`npmScopes.sindresorhus`")
                && message.contains("one-day default")),
        "{:#}",
        run.envelope
    );
    Ok(())
}

/// The fixed date the landing fixtures' locks are seeded at: Yarn resolves every descriptor to
/// its newest release published before it, so the starting versions never drift.
const SEEDED_AT: &str = "2020-05-01T00:00:00Z";

/// The freeze the landing tests upgrade and fix under: after `chalk` 4.1.0 (2020-06-09),
/// `supports-color` 7.2.0 (2020-08-28), and `ansi-styles` 4.3.0 (2020-10-04), before `chalk`
/// 4.1.1 (2021-04-21) and any of their next majors.
const FROZEN_AT: &str = "2020-11-01";

/// A Berry project of `manifests` (path and JSON body, the root's `packageManager` and
/// `private` added) with `rc` as its `.yarnrc.yml`.
fn landing_fixture(manifests: &[(&str, &str)], rc: &str) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    for (path, body) in manifests {
        let contents = if *path == "package.json" {
            body.replacen(
                '{',
                &format!("{{\n  \"private\": true,\n  \"packageManager\": \"yarn@{YARN}\","),
                1,
            )
        } else {
            (*body).to_string()
        };
        fixture.write(path, &contents);
    }
    fixture.write(".yarnrc.yml", rc);
    fixture
}

/// Seeds the lock as Yarn resolves it under an age gate reaching back to [`SEEDED_AT`], with
/// `envs` set too.
fn seed_lock_at(fixture: &Fixture, envs: &[(&str, &str)]) -> eyre::Result<()> {
    let seeded_at: jiff::Timestamp = SEEDED_AT.parse()?;
    let minutes = jiff::Timestamp::now().duration_since(seeded_at).as_secs() / 60;
    let gate = minutes.to_string();
    let mut envs = envs.to_vec();
    envs.push(("YARN_NPM_MINIMAL_AGE_GATE", &gate));
    fixture.write("yarn.lock", "");
    fixture
        .run_tool_traced(
            "seed the Berry lock in the past",
            "yarn",
            &["install", "--mode=update-lockfile"],
            &envs,
        )?
        .require_success()?;
    Ok(())
}

/// One landing row of a mutation's envelope: what it planned and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Landed {
    from: String,
    to: String,
    applied: bool,
    /// The skip reason, when skipped.
    reason: Option<String>,
    message: Option<String>,
}

/// The row of `name` in a mutation's envelope.
fn landed(envelope: &serde_json::Value, name: &str) -> eyre::Result<Landed> {
    let item = envelope
        .get("items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|item| item.get("name").and_then(serde_json::Value::as_str) == Some(name))
        .ok_or_else(|| eyre::eyre!("no row for {name}: {envelope:#}"))?;
    let text = |pointer: &str| {
        item.pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
    };
    Ok(Landed {
        from: text("/from").unwrap_or_default(),
        to: text("/to").unwrap_or_default(),
        applied: item.get("applied").and_then(serde_json::Value::as_bool) == Some(true),
        reason: text("/skipped/reason"),
        message: text("/skipped/message"),
    })
}

/// The bindings of `name` Yarn's own `yarn why` reports, as `version → workspaces`.
fn bindings(fixture: &Fixture, name: &str) -> eyre::Result<Vec<(String, Vec<String>)>> {
    Ok(yarns_owners(fixture, name)?
        .into_iter()
        .map(|(version, owners)| (version, owners.into_iter().collect()))
        .collect())
}

/// `owners` as the shape [`bindings`] returns.
fn bound(owners: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    owners
        .iter()
        .map(|(version, members)| {
            (
                (*version).to_string(),
                members.iter().map(ToString::to_string).collect(),
            )
        })
        .collect()
}

/// Requires `check --offline` to find the lock current, with no lock diagnostic at all.
fn assert_current(fixture: &Fixture) -> eyre::Result<()> {
    let checked = check(fixture, &[])?;
    assert!(
        lock_kinds(&checked.envelope, "errors").is_empty()
            && lock_kinds(&checked.envelope, "warnings").is_empty(),
        "{:#}",
        checked.envelope
    );
    Ok(())
}

/// Every snapshot entry but the lock's.
fn beside_the_lock(root: &std::path::Path) -> eyre::Result<BTreeMap<String, String>> {
    let mut entries = snapshot(root)?;
    entries.remove("yarn.lock");
    Ok(entries)
}

/// `upgrade` lands each direct dependency at exactly its target: `chalk` at 4.1.0, the newest
/// release the freeze admits, below its range's maximum 4.1.2, and the root's `supports-color`
/// at 7.2.0 although `chalk` declares the very descriptor too, which Yarn would otherwise keep
/// bound to the old copy for both.
/// Only the lock changes, not even its modification time elsewhere, Yarn verifies it current,
/// and a second run applies nothing and leaves it byte for byte.
#[test]
fn berry_upgrade_lands_each_exact_target_and_converges() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[(
            "package.json",
            r#"{ "name": "berry-land", "dependencies": { "chalk": "^4.0.0", "supports-color": "^7.1.0" } }"#,
        )],
        "enableTelemetry: false\n",
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_at(&fixture, &[])?;
    assert_eq!(bindings(&fixture, "chalk")?, bound(&[("4.0.0", &["."])]));
    assert_eq!(
        bindings(&fixture, "supports-color")?,
        bound(&[("7.1.0", &["."])])
    );
    let before = beside_the_lock(fixture.root())?;

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FROZEN_AT])?;

    assert_eq!(upgrade.code, Some(0), "{:#}", upgrade.envelope);
    // Checked before Yarn runs again, since Yarn's own queries rewrite its install state.
    assert_eq!(
        changed(&before, &beside_the_lock(fixture.root())?),
        [] as [&str; 0]
    );
    for (name, from, to) in [
        ("chalk", "4.0.0", "4.1.0"),
        ("supports-color", "7.1.0", "7.2.0"),
    ] {
        let row = landed(&upgrade.envelope, name)?;
        assert!(
            row.applied && row.from == from && row.to == to,
            "{name}: {row:?}"
        );
    }
    assert_eq!(bindings(&fixture, "chalk")?, bound(&[("4.1.0", &["."])]));
    assert_eq!(
        bindings(&fixture, "supports-color")?,
        bound(&[("7.2.0", &["."])])
    );
    // `chalk`'s own `supports-color@^7.1.0` is the root's descriptor, so it moved with it.
    let releases = yarns_registry_releases(&fixture)?;
    assert!(
        !releases.contains("supports-color@7.1.0") && !releases.contains("chalk@4.0.0"),
        "{releases:?}"
    );
    assert_current(&fixture)?;

    let lock = fixture.read_bytes("yarn.lock");
    let again = cooldown(&fixture, &["upgrade", "--freeze", FROZEN_AT])?;
    assert_eq!(again.code, Some(0), "{:#}", again.envelope);
    assert!(
        again
            .envelope
            .get("items")
            .and_then(serde_json::Value::as_array)
            .is_none_or(|items| items
                .iter()
                .all(|item| item.get("applied") != Some(&serde_json::Value::Bool(true)))),
        "{:#}",
        again.envelope
    );
    assert_eq!(fixture.read_bytes("yarn.lock"), lock);
    Ok(())
}

/// `fix` lands a target below the locked release the same way: the freeze makes `chalk` 4.1.2
/// too young, so it moves down to 4.1.0 under the unchanged `^4.0.0`, and Yarn verifies the lock
/// current.
#[test]
fn berry_fix_lands_an_older_target() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[(
            "package.json",
            r#"{ "name": "berry-fix", "dependencies": { "chalk": "^4.0.0" } }"#,
        )],
        "enableTelemetry: false\n",
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock(&fixture)?;
    assert_eq!(bindings(&fixture, "chalk")?, bound(&[("4.1.2", &["."])]));
    let manifest = fixture.read_bytes("package.json");

    let fix = cooldown(&fixture, &["fix", "--freeze", FROZEN_AT])?;

    assert_eq!(fix.code, Some(0), "{:#}", fix.envelope);
    let row = landed(&fix.envelope, "chalk")?;
    assert!(
        row.applied && row.from == "4.1.2" && row.to == "4.1.0",
        "{row:?}"
    );
    assert_eq!(bindings(&fixture, "chalk")?, bound(&[("4.1.0", &["."])]));
    assert_eq!(fixture.read_bytes("package.json"), manifest);
    assert_current(&fixture)?;
    Ok(())
}

/// A landing moves only the descriptors its declaring members declare: the root's
/// `supports-color@^7.0.0` lands at 7.2.0 while `chalk`'s own `^7.1.0`, bound to the same copy
/// before, keeps 7.1.0, and a member's `chalk@^3.0.0` keeps its 3.0.0 while the other member's
/// `^4.0.0` lands.
#[test]
fn berry_landing_leaves_every_other_binding_alone() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[
            (
                "package.json",
                r#"{ "name": "berry-root", "workspaces": ["packages/*"], "dependencies": { "supports-color": "^7.0.0" } }"#,
            ),
            (
                "packages/a/package.json",
                r#"{ "name": "a", "version": "1.0.0", "dependencies": { "chalk": "^4.0.0" } }"#,
            ),
            (
                "packages/b/package.json",
                r#"{ "name": "b", "version": "1.0.0", "dependencies": { "chalk": "^3.0.0" } }"#,
            ),
        ],
        "enableTelemetry: false\n",
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_at(&fixture, &[])?;
    assert_eq!(
        bindings(&fixture, "chalk")?,
        bound(&[("3.0.0", &["packages/b"]), ("4.0.0", &["packages/a"])])
    );

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FROZEN_AT])?;

    assert_eq!(upgrade.code, Some(0), "{:#}", upgrade.envelope);
    assert_eq!(
        bindings(&fixture, "chalk")?,
        bound(&[("3.0.0", &["packages/b"]), ("4.1.0", &["packages/a"])])
    );
    assert_eq!(
        bindings(&fixture, "supports-color")?,
        bound(&[("7.2.0", &["."])])
    );
    let releases = yarns_registry_releases(&fixture)?;
    assert!(releases.contains("supports-color@7.1.0"), "{releases:?}");
    assert_current(&fixture)?;
    Ok(())
}

/// An excluded member's exact pin keeps its own binding while the root's range beside it lands;
/// an excluded member that declares the very descriptor the root lands would move with it, so
/// that candidate is held, naming the member, and the lock stays byte for byte.
#[test]
fn berry_excluded_members_keep_their_bindings() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[
            (
                "package.json",
                r#"{ "name": "berry-root", "workspaces": ["packages/*"], "dependencies": { "chalk": "^4.0.0" } }"#,
            ),
            (
                "packages/legacy/package.json",
                r#"{ "name": "legacy", "version": "1.0.0", "dependencies": { "chalk": "4.0.0" } }"#,
            ),
        ],
        "enableTelemetry: false\n",
    );
    fixture.write(
        "cooldown.toml",
        "[global]\nexclude-folders = [\"packages/legacy\"]\n",
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_at(&fixture, &[])?;
    assert_eq!(
        bindings(&fixture, "chalk")?,
        bound(&[("4.0.0", &[".", "packages/legacy"])])
    );

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FROZEN_AT])?;

    assert_eq!(upgrade.code, Some(0), "{:#}", upgrade.envelope);
    assert!(
        landed(&upgrade.envelope, "chalk")?.applied,
        "{:#}",
        upgrade.envelope
    );
    assert_eq!(
        bindings(&fixture, "chalk")?,
        bound(&[("4.0.0", &["packages/legacy"]), ("4.1.0", &["."])])
    );
    assert_current(&fixture)?;

    // The excluded member now shares the root's range.
    let fixture = landing_fixture(
        &[
            (
                "package.json",
                r#"{ "name": "berry-root", "workspaces": ["packages/*"], "dependencies": { "chalk": "^4.0.0" } }"#,
            ),
            (
                "packages/legacy/package.json",
                r#"{ "name": "legacy", "version": "1.0.0", "dependencies": { "chalk": "^4.0.0" } }"#,
            ),
        ],
        "enableTelemetry: false\n",
    );
    fixture.write(
        "cooldown.toml",
        "[global]\nexclude-folders = [\"packages/legacy\"]\n",
    );
    seed_lock_at(&fixture, &[])?;
    let lock = fixture.read_bytes("yarn.lock");

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FROZEN_AT])?;

    let row = landed(&upgrade.envelope, "chalk")?;
    assert!(
        !row.applied
            && row.reason.as_deref() == Some("not_eligible")
            && row
                .message
                .as_deref()
                .is_some_and(|message| message.contains("packages/legacy")),
        "{row:?}"
    );
    assert_eq!(fixture.read_bytes("yarn.lock"), lock);
    Ok(())
}

/// The age gate `sync` writes keeps Yarn from resolving young releases afresh while the locked
/// ones stay: a lock Yarn still verifies current after the sync is landed into, `chalk`'s target
/// lands through an approval of exactly that release, and `is-odd`'s, which newly needs an
/// `is-number` the century gate quarantines, is rolled back alone, its manifest range restored
/// and Yarn's quarantine named.
#[test]
fn berry_rejected_candidate_rolls_back_alone() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[(
            "package.json",
            r#"{ "name": "berry-gated", "dependencies": { "chalk": "^4.0.0", "is-odd": "^2.0.0" } }"#,
        )],
        "enableTelemetry: false\n",
    );
    fixture.write(
        "cooldown.toml",
        indoc! {r#"
            min-age = "36500d"

            [package."chalk"]
            latest = true

            [package."is-odd"]
            latest = true
        "#},
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_at(&fixture, &[])?;
    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    assert_current(&fixture)?;

    let upgrade = cooldown(&fixture, &["upgrade", "--major"])?;

    let chalk = landed(&upgrade.envelope, "chalk")?;
    assert!(chalk.applied, "{:#}", upgrade.envelope);
    assert_eq!(
        bindings(&fixture, "chalk")?,
        bound(&[(chalk.to.as_str(), &["."])])
    );
    let is_odd = landed(&upgrade.envelope, "is-odd")?;
    assert!(
        !is_odd.applied
            && is_odd.reason.as_deref() == Some("resolver_conflict")
            && is_odd
                .message
                .as_deref()
                .is_some_and(|message| message.contains("quarantined")),
        "{is_odd:?}"
    );
    assert_eq!(bindings(&fixture, "is-odd")?, bound(&[("2.0.0", &["."])]));
    let manifest: serde_json::Value = serde_json::from_slice(&fixture.read_bytes("package.json"))?;
    assert_eq!(
        manifest.pointer("/dependencies/is-odd"),
        Some(&serde_json::json!("^2.0.0"))
    );
    assert_current(&fixture)?;
    Ok(())
}

/// A target cooldown admits but the native gate would quarantine, here `chalk`'s newest major
/// through `--allow`, which `sync` never wrote, lands through a temporary approval of exactly
/// that release that the project's rc file never sees.
#[test]
fn berry_young_target_lands_through_an_exact_approval() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[(
            "package.json",
            r#"{ "name": "berry-young", "dependencies": { "chalk": "^4.0.0" } }"#,
        )],
        "enableTelemetry: false\n",
    );
    fixture.write("cooldown.toml", "min-age = \"36500d\"\n");
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_at(&fixture, &[])?;
    assert_eq!(sync_statuses(&fixture, &[])?, ["written"]);
    let rc = fixture.read_bytes(".yarnrc.yml");
    assert!(quarantined(
        &fixture,
        ".",
        &["add", "chalk@4.1.2", "--mode=update-lockfile"]
    )?);

    let upgrade = cooldown(&fixture, &["upgrade", "--major", "--allow", "chalk"])?;

    let row = landed(&upgrade.envelope, "chalk")?;
    assert!(row.applied, "{:#}", upgrade.envelope);
    assert_eq!(
        bindings(&fixture, "chalk")?,
        bound(&[(row.to.as_str(), &["."])])
    );
    assert_eq!(fixture.read_bytes(".yarnrc.yml"), rc);
    assert_current(&fixture)?;
    Ok(())
}

/// A landing leaves an npm alias of the package, a `file:` package with files of its own, and a
/// `link:` package bound as they were, and a dry run first reports the same landing and writes
/// nothing, its copy holding everything Yarn reads.
#[test]
fn berry_landing_leaves_aliases_and_local_sources() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[
            (
                "package.json",
                r#"{ "name": "berry-local", "dependencies": { "chalk": "^4.0.0", "chalk4": "npm:chalk@^4.0.0", "local": "file:./local", "linked": "link:./linked" } }"#,
            ),
            (
                "local/package.json",
                r#"{ "name": "local", "version": "1.0.0" }"#,
            ),
            (
                "linked/package.json",
                r#"{ "name": "linked", "version": "1.0.0" }"#,
            ),
        ],
        "enableTelemetry: false\n",
    );
    fixture.write("local/index.js", "module.exports = 1;\n");
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_at(&fixture, &[])?;
    // The entry's lines up to the blank line after it, whatever line ending Yarn wrote.
    let entry = |lock: &str, key: &str| -> Option<String> {
        let lines: Vec<&str> = lock
            .lines()
            .skip_while(|line| !line.starts_with(key))
            .take_while(|line| !line.trim().is_empty())
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
    };
    let lock = String::from_utf8(fixture.read_bytes("yarn.lock"))?;
    let local = entry(&lock, "\"local@file:");
    let linked = entry(&lock, "\"linked@link:");
    assert!(local.is_some() && linked.is_some(), "{lock}");
    let before = snapshot(fixture.root())?;

    let preview = cooldown(&fixture, &["upgrade", "--dry-run", "--freeze", FROZEN_AT])?;

    assert_eq!(preview.code, Some(0), "{:#}", preview.envelope);
    let planned = landed(&preview.envelope, "chalk")?;
    assert!(
        planned.from == "4.0.0" && planned.to == "4.1.0",
        "{planned:?}"
    );
    assert_eq!(
        changed(&before, &snapshot(fixture.root())?),
        [] as [&str; 0]
    );

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FROZEN_AT])?;

    assert!(
        landed(&upgrade.envelope, "chalk")?.applied,
        "{:#}",
        upgrade.envelope
    );
    assert_eq!(bindings(&fixture, "chalk")?, bound(&[("4.1.0", &["."])]));
    let output = fixture
        .run_tool_traced("ask Yarn why chalk", "yarn", &["why", "chalk"], &PLAIN)?
        .require_success()?;
    assert!(
        output
            .stdout_str()
            .contains("chalk@npm:4.0.0 (via npm:chalk@^4.0.0)"),
        "the alias keeps its copy: {}",
        output.stdout_str()
    );
    let landed_lock = String::from_utf8(fixture.read_bytes("yarn.lock"))?;
    assert_eq!(entry(&landed_lock, "\"local@file:"), local);
    assert_eq!(entry(&landed_lock, "\"linked@link:"), linked);
    assert_current(&fixture)?;
    Ok(())
}

/// A landing only rewrites the lock, so a project already linked by Plug'n'Play or into
/// `node_modules` keeps every artifact of that install as it was, and `--build` links the next
/// landed release with Yarn's immutable install.
#[test]
fn berry_landing_leaves_the_linked_install_until_the_build() -> eyre::Result<()> {
    for linker in ["pnp", "node-modules"] {
        let fixture = landing_fixture(
            &[(
                "package.json",
                r#"{ "name": "berry-linked", "dependencies": { "chalk": "^4.0.0" } }"#,
            )],
            &format!("enableTelemetry: false\nnodeLinker: {linker}\n"),
        );
        if !runs_pinned_yarn(&fixture) {
            return Ok(());
        }
        seed_lock_at(&fixture, &[])?;
        fixture
            .run_tool_traced("link the project", "yarn", &["install"], &[])?
            .require_success()?;
        let before = beside_the_lock(fixture.root())?;

        let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FROZEN_AT])?;

        assert!(
            landed(&upgrade.envelope, "chalk")?.applied,
            "{linker}: {:#}",
            upgrade.envelope
        );
        assert_eq!(
            changed(&before, &beside_the_lock(fixture.root())?),
            [] as [&str; 0],
            "{linker}"
        );

        let built = cooldown(&fixture, &["upgrade", "--freeze", "2021-06-01", "--build"])?;

        assert_eq!(built.code, Some(0), "{linker}: {:#}", built.envelope);
        let row = landed(&built.envelope, "chalk")?;
        assert!(row.applied && row.to == "4.1.1", "{linker}: {row:?}");
        let installed = fixture
            .run_tool_traced(
                "read the linked chalk",
                "yarn",
                &["node", "-p", "require('chalk/package.json').version"],
                &[],
            )?
            .require_success()?;
        assert_eq!(installed.stdout_str().trim(), "4.1.1", "{linker}");
    }
    Ok(())
}

/// Offline, a landing resolves from Yarn's warm metadata cache and mirror: a dry run online
/// warms them, then the real run lands with Yarn's network off.
/// With a cold global folder the same offline run cannot resolve the target, which skips the
/// candidate with Yarn's error and leaves the lock as it was, and no stale lock is reported.
#[test]
fn berry_landing_runs_offline_from_warm_caches() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[(
            "package.json",
            r#"{ "name": "berry-offline", "dependencies": { "chalk": "^4.0.0" } }"#,
        )],
        "enableTelemetry: false\n",
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    // A home of its own gives the run a default global folder nothing else shares.
    let home = tempfile::tempdir()?;
    let home = home.path().to_string_lossy().into_owned();
    let online = [("HOME", home.as_str())];
    seed_lock_at(&fixture, &online)?;
    let run = |args: &[&str], envs: &[(&str, &str)]| -> eyre::Result<serde_json::Value> {
        let mut full = args.to_vec();
        full.push("--json");
        let output = fixture.cooldown_in_env(None, &full, envs);
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let warmed = run(&["upgrade", "--dry-run", "--freeze", FROZEN_AT], &online)?;
    assert_eq!(landed(&warmed, "chalk")?.to, "4.1.0", "{warmed:#}");
    let lock = fixture.read_bytes("yarn.lock");

    // A cold global folder
    let cold = tempfile::tempdir()?;
    let cold = cold.path().to_string_lossy().into_owned();
    let offline_cold = [
        ("HOME", home.as_str()),
        ("YARN_ENABLE_OFFLINE_MODE", "1"),
        ("YARN_ENABLE_NETWORK", "0"),
        ("YARN_GLOBAL_FOLDER", cold.as_str()),
    ];
    let missed = run(
        &["upgrade", "--offline", "--freeze", FROZEN_AT],
        &offline_cold,
    )?;
    let row = landed(&missed, "chalk")?;
    assert!(
        !row.applied && row.reason.as_deref() == Some("resolver_conflict"),
        "{missed:#}"
    );
    assert!(
        lock_kinds(&missed, "errors").is_empty() && lock_kinds(&missed, "warnings").is_empty(),
        "{missed:#}"
    );
    assert_eq!(fixture.read_bytes("yarn.lock"), lock);

    // The warm default global folder
    let offline_warm = [
        ("HOME", home.as_str()),
        ("YARN_ENABLE_OFFLINE_MODE", "1"),
        ("YARN_ENABLE_NETWORK", "0"),
    ];
    let landed_offline = run(
        &["upgrade", "--offline", "--freeze", FROZEN_AT],
        &offline_warm,
    )?;
    assert!(
        landed(&landed_offline, "chalk")?.applied,
        "{landed_offline:#}"
    );
    assert_eq!(bindings(&fixture, "chalk")?, bound(&[("4.1.0", &["."])]));
    Ok(())
}

/// Yarn's default global folder under an `XDG_DATA_HOME` inside the project takes no write from
/// a landing: neither from a dry run, whose copy cannot see that the folder lies in the project
/// it copied, nor from the real run, which still lands the target.
#[test]
fn berry_landing_writes_no_global_folder_inside_the_project() -> eyre::Result<()> {
    let fixture = landing_fixture(
        &[(
            "package.json",
            r#"{ "name": "berry-xdg", "dependencies": { "chalk": "^4.0.0" } }"#,
        )],
        "enableTelemetry: false\n",
    );
    if !runs_pinned_yarn(&fixture) {
        return Ok(());
    }
    seed_lock_at(&fixture, &[])?;
    let data = fixture.root().join(".data");
    let data = data.to_string_lossy().into_owned();
    let envs = [("XDG_DATA_HOME", data.as_str())];
    let run = |args: &[&str]| -> eyre::Result<serde_json::Value> {
        let mut full = args.to_vec();
        full.push("--json");
        let output = fixture.cooldown_in_env(None, &full, &envs);
        Ok(serde_json::from_slice(&output.stdout)?)
    };
    let before = snapshot(fixture.root())?;

    let previewed = run(&["upgrade", "--dry-run", "--freeze", FROZEN_AT])?;
    assert_eq!(landed(&previewed, "chalk")?.to, "4.1.0", "{previewed:#}");
    assert_eq!(snapshot(fixture.root())?, before);

    let upgraded = run(&["upgrade", "--freeze", FROZEN_AT])?;
    assert!(landed(&upgraded, "chalk")?.applied, "{upgraded:#}");
    assert_eq!(beside_the_lock(fixture.root())?, {
        let mut entries = before;
        entries.remove("yarn.lock");
        entries
    });
    assert_eq!(bindings(&fixture, "chalk")?, bound(&[("4.1.0", &["."])]));
    Ok(())
}
