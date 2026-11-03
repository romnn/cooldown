//! End-to-end tests that drive the REAL Yarn Berry (the `yarn = "4.18.1"` pin in `mise.toml`)
//! against fixtures generated on the fly in temp dirs.
//!
//! cooldown reads a Berry `yarn.lock` for its registry releases but does not mutate a Berry
//! project yet, so these tests check three invariants against Yarn itself:
//!
//! - the rows cooldown reads are exactly the npm releases Yarn's own `yarn info` reports;
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
use std::collections::BTreeSet;
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
    // The workspace's members are not attributed, which the run says.
    assert!(
        messages(&outdated.envelope, "warnings")
            .iter()
            .any(|message| message.contains("workspace members cooldown does not attribute yet")),
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
