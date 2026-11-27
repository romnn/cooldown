//! End-to-end convergence tests that drive the REAL Yarn Classic (the 1.22.22 the `yarn` pin in
//! `mise.toml` installs beside Yarn Berry) against `yarn.lock` fixtures generated on the fly in
//! temp dirs.
//!
//! They hold the Classic side of the `yarn` adapter to the contract the other suites hold their
//! adapters to: `outdated` reads the releases Yarn locked and plans what `upgrade` plans, and an
//! `upgrade` or `fix` lands every planned change exactly on cooldown's target, writes nothing but
//! the lock, moves nothing it did not report, leaves a lock Yarn's own frozen install accepts, and
//! converges, while its `--dry-run` disposes of every row as the real run does.
//! `check` asks Yarn for no verdict on a Classic lock, so it fails closed.
//! Only Linux confines the writes of the Yarn a landing runs, so elsewhere every landing test
//! asserts instead that cooldown holds its rows and leaves the lock ([`declined_off_linux`]).
//!
//! # Yarn Classic beside Yarn Berry
//!
//! The `yarn` on `PATH` is Yarn Berry, so the tests run Yarn Classic as `yarnpkg`, a name only
//! Yarn Classic installs, and point cooldown at it through its `COOLDOWN_YARN` override.
//! Every Yarn Classic process a test starts, cooldown's included, caches packages in the
//! fixture's own cache home rather than the user's (see [`cache_home`]).
//!
//! # Determinism
//!
//! Yarn Classic has no publish-date cutoff, so it locks each range at its newest release.
//! The fixtures therefore depend only on `chalk` 3 and 4, whose lines, and every package they
//! lock, published their last release years ago, and a lock that must hold an older release is
//! locked offline from a cache that holds only that release (see [`locked_behind`]).
//! [`FREEZE`] falls after `chalk` 4.1.0 (2020-06-09) and every package it locks, and before 4.1.1
//! (2021-04-21), so 4.1.0 is the matured target of `^4.0.0` from the older 4.0.0 and from the
//! too-fresh 4.1.2 alike.

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "the shared integration harness deliberately panics on invalid fixture setup or malformed command output"
)]
mod support;

use std::collections::{BTreeMap, BTreeSet};

use color_eyre::eyre;
use indoc::formatdoc;
use serde_json::Value;
use support::{CapturedOutput, ChangeVersions, Envelope, Fixture, project_files};

/// The Yarn Classic release `mise.toml` pins, which every fixture declares and runs.
const YARN: &str = "1.22.22";

/// The name the tests run Yarn Classic by: only Yarn Classic installs it, while `yarn` is Yarn
/// Berry.
const YARN_CLASSIC: &str = "yarnpkg";

/// The absolute cooldown cutoff every test but the transitive one evaluates under.
const FREEZE: &str = "2020-11-01";

/// The release of `chalk` that [`FREEZE`] matures `^4.0.0` to.
const CHALK_TARGET: &str = "4.1.0";

/// The cache home every Yarn Classic process a test starts, cooldown's included, is pointed at
/// through `XDG_CACHE_HOME`, so Yarn's default cache folder, the one a landing lets Yarn write,
/// is the fixture's own.
fn cache_home(fixture: &Fixture) -> eyre::Result<String> {
    let home = fixture.yarn_cache_home();
    home.to_str()
        .map(str::to_owned)
        .ok_or_else(|| eyre::eyre!("the cache home {} is not UTF-8", home.display()))
}

/// Runs Yarn Classic with `args` at the fixture root and requires it to succeed.
fn yarn(fixture: &Fixture, args: &[&str]) -> eyre::Result<CapturedOutput> {
    let home = cache_home(fixture)?;
    fixture
        .run_tool(YARN_CLASSIC, args, &[("XDG_CACHE_HOME", &home)])
        .require_success()
}

/// Runs `cooldown` with `args` and `--json` against the fixture, driving Yarn Classic.
fn cooldown(fixture: &Fixture, args: &[&str]) -> eyre::Result<Envelope> {
    let home = cache_home(fixture)?;
    Ok(fixture.cooldown_json_env(
        args,
        &[("XDG_CACHE_HOME", &home), ("COOLDOWN_YARN", YARN_CLASSIC)],
    ))
}

/// Off Linux, where cooldown confines no Yarn Classic install and so lands none, asserts that
/// `report` applied nothing and held its rows as not eligible, saying why, and that the lock is
/// still `lock_before`, and returns `true`, so a landing test ends there; on Linux it returns
/// `false`.
fn declined_off_linux(fixture: &Fixture, report: &Envelope, lock_before: &[u8]) -> bool {
    if cfg!(target_os = "linux") {
        return false;
    }
    assert_eq!(report.summary_applied(), 0, "{report:#?}");
    assert!(
        report.skipped_reasons().contains("not_eligible"),
        "{report:#?}"
    );
    assert!(
        report.items().iter().any(|item| item
            .pointer("/skipped/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("Linux Landlock"))),
        "{report:#?}"
    );
    assert_eq!(fixture.read_bytes("yarn.lock"), lock_before);
    true
}

/// Every file of the fixture's tool home outside its cache home, which Yarn's cache and
/// cooldown's lie in.
fn beside_the_caches(fixture: &Fixture) -> BTreeMap<String, Vec<u8>> {
    let cache_home = fixture.yarn_cache_home();
    let tool_home = cache_home.parent().unwrap_or(&cache_home);
    project_files(tool_home)
        .into_iter()
        .filter(|(path, _)| !path.starts_with("xdg-cache/"))
        .collect()
}

/// A root `package.json` that declares `chalk` at `range`.
fn chalk_project(range: &str) -> String {
    formatdoc! {r#"
        {{
          "name": "classic",
          "version": "1.0.0",
          "private": true,
          "packageManager": "yarn@{YARN}",
          "dependencies": {{ "chalk": "{range}" }}
        }}
    "#}
}

/// A Yarn Classic project of `manifests` (path and body), locked by Yarn Classic itself.
///
/// The install also links `node_modules`, which is removed, so the project starts as a checkout
/// does, with its manifests and its lock alone.
fn locked(manifests: &[(&str, &str)]) -> eyre::Result<Fixture> {
    let fixture = Fixture::new().tag_independent();
    let version = yarn(&fixture, &["--version"])?;
    eyre::ensure!(
        version.stdout_str().trim() == YARN,
        "`{YARN_CLASSIC}` runs Yarn {}, not the pinned Yarn {YARN}",
        version.stdout_str().trim()
    );
    write_manifests(&fixture, manifests);
    yarn(&fixture, &["install"])?;
    remove_installs(&fixture, manifests)?;
    Ok(fixture)
}

/// A Yarn Classic project of `manifests` whose lock binds each range to the release `seeds`
/// declare in its place, as a lock written before the newer releases of the range appeared.
///
/// Yarn Classic locks a range at its newest release unless it resolves offline, where it takes
/// the newest release its cache holds.
/// The seeds are therefore installed first, which leaves exactly their releases in the
/// fixture's private cache, and the manifests are then locked offline from scratch.
fn locked_behind(seeds: &[(&str, &str)], manifests: &[(&str, &str)]) -> eyre::Result<Fixture> {
    let fixture = locked(seeds)?;
    std::fs::remove_file(fixture.root().join("yarn.lock"))?;
    write_manifests(&fixture, manifests);
    yarn(&fixture, &["install", "--offline"])?;
    remove_installs(&fixture, manifests)?;
    Ok(fixture)
}

/// The project most tests share: `chalk@^4.0.0` locked at 4.0.0, below both the matured
/// [`CHALK_TARGET`] and the 4.1.2 its range admits.
fn chalk_behind() -> eyre::Result<Fixture> {
    locked_behind(
        &[("package.json", &chalk_project("4.0.0"))],
        &[("package.json", &chalk_project("^4.0.0"))],
    )
}

fn write_manifests(fixture: &Fixture, manifests: &[(&str, &str)]) {
    for (path, body) in manifests {
        fixture.write(path, body);
    }
}

/// Removes the `node_modules` folder an install linked beside each of `manifests`.
fn remove_installs(fixture: &Fixture, manifests: &[(&str, &str)]) -> eyre::Result<()> {
    for (path, _) in manifests {
        let installed = fixture.root().join(path).with_file_name("node_modules");
        if installed.exists() {
            std::fs::remove_dir_all(&installed)?;
        }
    }
    Ok(())
}

/// Every entry of a Yarn Classic `yarn.lock`, by each pattern that selects it (`chalk@^4.0.0`),
/// with the version it locks.
///
/// An entry opens with an unindented line listing its patterns, comma-separated and quoted where
/// they need it, and carries its version on a `version "…"` line indented by two spaces; its
/// dependencies are indented further.
fn lock_entries(lock: &[u8]) -> BTreeMap<String, String> {
    let mut entries = BTreeMap::new();
    let mut patterns: Vec<String> = Vec::new();
    for line in String::from_utf8_lossy(lock).lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            patterns = line
                .trim_end_matches(':')
                .split(", ")
                .map(|pattern| pattern.trim_matches('"').to_owned())
                .collect();
        } else if let Some(version) = line.strip_prefix("  version ") {
            for pattern in patterns.drain(..) {
                entries.insert(pattern, version.trim_matches('"').to_owned());
            }
        }
    }
    entries
}

/// The package a lock pattern selects, its name before the range's `@`.
fn package_of(pattern: &str) -> &str {
    pattern.rsplit_once('@').map_or(pattern, |(name, _)| name)
}

/// Every release Yarn's own `yarn list` reads from the lock, as `name@version`.
///
/// A child that only points at a copy listed elsewhere is a `shadow` named by its pattern
/// (`ansi-styles@^4.1.0`), so it is skipped.
fn yarn_releases(fixture: &Fixture) -> eyre::Result<BTreeSet<String>> {
    let listed = yarn(fixture, &["list", "--json"])?;
    let mut releases = BTreeSet::new();
    for line in listed.stdout_str().lines() {
        let entry: Value = serde_json::from_str(line)?;
        if entry.get("type").and_then(Value::as_str) != Some("tree") {
            continue;
        }
        let mut trees: Vec<&Value> = entry
            .pointer("/data/trees")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .collect();
        while let Some(tree) = trees.pop() {
            if tree.get("shadow").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(name) = tree.get("name").and_then(Value::as_str) {
                releases.insert(name.to_owned());
            }
            trees.extend(
                tree.get("children")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            );
        }
    }
    Ok(releases)
}

/// The `name@current` of every item `report` lists.
fn item_releases(report: &Envelope) -> BTreeSet<String> {
    report
        .items()
        .iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?;
            let current = item.get("current")?.as_str()?;
            Some(format!("{name}@{current}"))
        })
        .collect()
}

/// Whether the first item of `report` for `name` is a dependency a manifest declares.
fn is_direct(report: &Envelope, name: &str) -> Option<bool> {
    report.items().iter().find_map(|item| {
        if item.get("name").and_then(Value::as_str) != Some(name) {
            return None;
        }
        item.get("direct").and_then(Value::as_bool)
    })
}

fn is_applied(item: &Value) -> bool {
    item.get("applied").and_then(Value::as_bool) == Some(true)
}

/// What a report did with one row, so a dry run and a real run can be compared row by row.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Disposition {
    project: String,
    name: String,
    from: String,
    to: String,
    /// `applied`, the reason a held row was skipped for, or `error`.
    outcome: String,
}

/// The disposition of every row of `report`, sorted.
fn dispositions(report: &Envelope) -> Vec<Disposition> {
    let mut dispositions: Vec<Disposition> = report
        .items()
        .iter()
        .filter_map(|item| {
            let field = |key: &str| item.get(key)?.as_str().map(str::to_owned);
            let outcome = if is_applied(item) {
                "applied".to_owned()
            } else {
                item.pointer("/skipped/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("error")
                    .to_owned()
            };
            Some(Disposition {
                project: field("project")?,
                name: field("name")?,
                from: field("from")?,
                to: field("to")?,
                outcome,
            })
        })
        .collect();
    dispositions.sort();
    dispositions
}

/// Every project file but the lock, by `/`-separated path.
fn beside_the_lock(fixture: &Fixture) -> BTreeMap<String, Vec<u8>> {
    let mut files = project_files(fixture.root());
    files.remove("yarn.lock");
    files
}

/// The paths whose bytes differ between `before` and `after`, appeared or vanished ones included.
fn changed(before: &BTreeMap<String, Vec<u8>>, after: &BTreeMap<String, Vec<u8>>) -> Vec<String> {
    let paths: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    paths
        .into_iter()
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect()
}

/// Asserts that every selector both locks list binds what it bound before, unless a row `report`
/// applied moved its package from that release to the release it binds now.
///
/// A target may require other dependencies than the release it replaced, so a selector that
/// appears or vanishes is allowed; what each member binds is proven by Yarn's own install
/// ([`assert_members_install`]).
fn assert_bindings_moved_exactly(report: &Envelope, before: &[u8], after: &[u8]) {
    let moves: BTreeMap<String, String> = report
        .items()
        .iter()
        .filter(|item| is_applied(item))
        .map(|item| {
            let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
            (
                format!("{}@{}", field("name"), field("from")),
                field("to").to_owned(),
            )
        })
        .collect();
    let settled = lock_entries(after);
    for (selector, version) in lock_entries(before) {
        let Some(now) = settled.get(&selector) else {
            continue;
        };
        if *now != version {
            assert_eq!(
                moves.get(&format!("{}@{version}", package_of(&selector))),
                Some(now),
                "{selector} moved from {version} to {now}, which no applied row reports\n{}",
                String::from_utf8_lossy(after)
            );
        }
    }
}

/// Asserts what Yarn installs from the lock as it stands, from a clean state: its frozen install,
/// which fails on a lock the manifests need changed, succeeds without rewriting a byte of it, and
/// each member a direct row `report` applied names resolves the row's package at its target.
fn assert_members_install(fixture: &Fixture, report: &Envelope) -> eyre::Result<()> {
    let direct: Vec<&Value> = report
        .items()
        .iter()
        .filter(|item| is_applied(item) && item.get("direct") == Some(&Value::Bool(true)))
        .collect();
    let member_paths = |item: &Value| -> Vec<String> {
        let paths: Vec<String> = item
            .get("members")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|member| member.get("path")?.as_str().map(str::to_owned))
            .collect();
        if paths.is_empty() {
            vec![".".to_owned()]
        } else {
            paths
        }
    };
    for member in direct.iter().flat_map(|item| member_paths(item)) {
        let installed = fixture.root().join(&member).join("node_modules");
        if installed.exists() {
            std::fs::remove_dir_all(&installed)?;
        }
    }
    let root_install = fixture.root().join("node_modules");
    if root_install.exists() {
        std::fs::remove_dir_all(&root_install)?;
    }
    let published = fixture.read_bytes("yarn.lock");
    yarn(fixture, &["install", "--frozen-lockfile"])?;
    assert_eq!(
        fixture.read_bytes("yarn.lock"),
        published,
        "Yarn's frozen install must keep the published lock byte for byte"
    );
    for item in direct {
        let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
        let to = item.get("to").and_then(Value::as_str).unwrap_or_default();
        for member in member_paths(item) {
            assert_eq!(
                installed_version(fixture.root(), &member, name)?.as_deref(),
                Some(to),
                "{member} must install {name} at its target"
            );
        }
    }
    Ok(())
}

/// The version of `name` Node resolves from the member at `member`: the copy in the nearest
/// `node_modules` at or above it, up to the root.
fn installed_version(
    root: &std::path::Path,
    member: &str,
    name: &str,
) -> eyre::Result<Option<String>> {
    let mut directory = root.join(member);
    loop {
        let manifest = directory
            .join("node_modules")
            .join(name)
            .join("package.json");
        if manifest.is_file() {
            let installed: Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
            return Ok(installed
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned));
        }
        if directory == root || !directory.pop() {
            return Ok(None);
        }
    }
}

/// `check` cannot evaluate a Yarn Classic project yet: the adapter asks Yarn for no verdict on a
/// Classic lock, so it fails closed with `lock_unknown` alone, checks nothing, and leaves every
/// file as it was, although Yarn itself just wrote the lock.
///
/// This pins today's behavior; it flips once Yarn's own frozen install backs `check`.
#[test]
fn check_fails_closed_on_an_unverifiable_classic_lock() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = locked(&[("package.json", &chalk_project("^4.0.0"))])?;
    let before = project_files(fixture.root());

    let check = cooldown(&fixture, &["check", "--freeze", FREEZE])?;

    assert!(!check.ok(), "{check:#?}");
    assert_eq!(
        check.error_kinds(),
        BTreeSet::from(["lock_unknown".to_owned()]),
        "{check:#?}"
    );
    assert_eq!(check.summary_checked(), 0, "{check:#?}");
    assert_eq!(
        changed(&before, &project_files(fixture.root())),
        Vec::<String>::new()
    );
    Ok(())
}

/// `outdated` reads exactly the releases Yarn's own `yarn list` reads from the lock, `chalk` as
/// the dependency the manifest declares and every package it locks as a transitive one.
/// It agrees with `upgrade` on the plan: nothing caps `chalk` in this graph, so it is adoptable at
/// exactly the target `upgrade` plans for it, with newer releases still in cooldown past it.
/// Neither command writes a file.
#[test]
fn outdated_reads_yarns_lock_and_agrees_with_upgrade() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = chalk_behind()?;
    let before = project_files(fixture.root());

    let outdated = cooldown(
        &fixture,
        &["outdated", "--freeze", FREEZE, "--transitive", "--all"],
    )?;
    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE, "--dry-run"])?;

    assert_eq!(
        changed(&before, &project_files(fixture.root())),
        Vec::<String>::new()
    );
    // What cooldown reads
    assert_eq!(item_releases(&outdated), yarn_releases(&fixture)?);
    assert_eq!(is_direct(&outdated, "chalk"), Some(true), "{outdated:#?}");
    assert_eq!(
        is_direct(&outdated, "supports-color"),
        Some(false),
        "{outdated:#?}"
    );

    // The agreement contract every suite asserts
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");
    let held = upgrade.held_conflict_names();
    assert!(
        held.is_subset(&blocked),
        "every held candidate must be blocked by outdated\nheld={held:?}\nblocked={blocked:?}"
    );
    assert!(
        adoptable.is_disjoint(&held),
        "nothing outdated calls adoptable may be held by upgrade\nadoptable={adoptable:?}\nheld={held:?}"
    );

    if !cfg!(target_os = "linux") {
        assert!(blocked.contains("chalk"), "{outdated:#?}");
        return Ok(());
    }

    // The one plan
    assert!(
        adoptable.contains("chalk"),
        "nothing caps chalk, so it must be adoptable\n{outdated:#?}"
    );
    assert_eq!(
        outdated
            .item_field_str("chalk", "adoptableTarget")
            .as_deref(),
        Some(CHALK_TARGET),
        "{outdated:#?}"
    );
    assert_ne!(
        outdated.latest_version_for("chalk").as_deref(),
        Some(CHALK_TARGET),
        "a newer chalk must still be in cooldown at the freeze"
    );
    assert_eq!(
        upgrade
            .change_for("chalk")
            .map(|planned| planned.to)
            .as_deref(),
        Some(CHALK_TARGET),
        "upgrade must plan outdated's target\n{upgrade:#?}"
    );
    Ok(())
}

/// `upgrade` lands `chalk` at exactly its matured target 4.1.0, below the 4.1.2 its `^4.0.0`
/// admits, and writes nothing but the lock: the manifest keeps its range, and no install appears.
/// The lock moves exactly the reported row, Yarn's frozen install accepts it, and a second
/// `upgrade` applies nothing and leaves the lock byte for byte.
#[test]
fn upgrade_lands_the_exact_target_in_the_lock_alone_and_converges() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = chalk_behind()?;
    let lock_before = fixture.read_bytes("yarn.lock");
    let beside = beside_the_lock(&fixture);

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE])?;
    if declined_off_linux(&fixture, &upgrade, &lock_before) {
        return Ok(());
    }

    // Checked before Yarn runs again, since its frozen install links `node_modules`.
    assert_eq!(
        changed(&beside, &beside_the_lock(&fixture)),
        Vec::<String>::new(),
        "the upgrade must write nothing beside the lock\n{upgrade:#?}"
    );
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["chalk".to_owned()]),
        "{upgrade:#?}"
    );
    assert_eq!(
        upgrade.applied_change_for("chalk"),
        Some(ChangeVersions::new("4.0.0", CHALK_TARGET))
    );
    let lock_after = fixture.read_bytes("yarn.lock");
    assert_bindings_moved_exactly(&upgrade, &lock_before, &lock_after);
    assert_members_install(&fixture, &upgrade)?;

    let again = cooldown(&fixture, &["upgrade", "--freeze", FREEZE])?;
    assert_eq!(
        again.summary_applied(),
        0,
        "the second upgrade must be a no-op\n{again:#?}"
    );
    assert_eq!(fixture.read_bytes("yarn.lock"), lock_after);
    Ok(())
}

/// `fix` matures the too-fresh `chalk` 4.1.2 Yarn locked for `^4.0.0` down to its target 4.1.0,
/// reported as a downgrade, through the lock alone, which Yarn's frozen install accepts.
/// A second `fix` applies nothing and leaves the lock byte for byte.
#[test]
fn fix_matures_a_too_fresh_version_down_to_its_target_and_converges() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = locked(&[("package.json", &chalk_project("^4.0.0"))])?;
    let lock_before = fixture.read_bytes("yarn.lock");
    assert_eq!(
        lock_entries(&lock_before)
            .get("chalk@^4.0.0")
            .map(String::as_str),
        Some("4.1.2"),
        "Yarn locks the range at its newest release"
    );
    let beside = beside_the_lock(&fixture);

    let fixed = cooldown(&fixture, &["fix", "--freeze", FREEZE])?;
    if declined_off_linux(&fixture, &fixed, &lock_before) {
        return Ok(());
    }

    assert_eq!(
        changed(&beside, &beside_the_lock(&fixture)),
        Vec::<String>::new(),
        "the fix must write nothing beside the lock\n{fixed:#?}"
    );
    assert_eq!(
        fixed.applied_change_for("chalk"),
        Some(ChangeVersions::new("4.1.2", CHALK_TARGET)),
        "{fixed:#?}"
    );
    assert!(fixed.downgraded_names().contains("chalk"), "{fixed:#?}");
    let lock_after = fixture.read_bytes("yarn.lock");
    assert_bindings_moved_exactly(&fixed, &lock_before, &lock_after);
    assert_members_install(&fixture, &fixed)?;

    let again = cooldown(&fixture, &["fix", "--freeze", FREEZE])?;
    assert_eq!(
        again.summary_applied(),
        0,
        "the second fix must be a no-op\n{again:#?}"
    );
    assert_eq!(fixture.read_bytes("yarn.lock"), lock_after);
    Ok(())
}

/// `upgrade --dry-run` disposes of every row as the real run on an identical fixture does,
/// landing or holding alike, and leaves every project file byte-identical, while the real run
/// lands `chalk`.
#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let dry_fixture = chalk_behind()?;
    let before = project_files(dry_fixture.root());
    let tool_home = beside_the_caches(&dry_fixture);
    let dry = cooldown(&dry_fixture, &["upgrade", "--freeze", FREEZE, "--dry-run"])?;
    assert_eq!(
        changed(&before, &project_files(dry_fixture.root())),
        Vec::<String>::new()
    );
    assert_eq!(
        changed(&tool_home, &beside_the_caches(&dry_fixture)),
        Vec::<String>::new(),
        "the dry run must write nothing outside the project but the caches"
    );
    assert_eq!(dry.lock_status(), None, "--dry-run never re-locks");

    let real_fixture = chalk_behind()?;
    let lock_before = real_fixture.read_bytes("yarn.lock");
    let real = cooldown(&real_fixture, &["upgrade", "--freeze", FREEZE])?;
    if declined_off_linux(&real_fixture, &real, &lock_before) {
        assert_eq!(dispositions(&dry), dispositions(&real));
        return Ok(());
    }

    // The real run lands, so the two cannot merely agree on the same failure.
    assert_eq!(
        real.applied_names(),
        BTreeSet::from(["chalk".to_owned()]),
        "the real run must land chalk\n{real:#?}"
    );
    assert_eq!(
        dispositions(&dry),
        dispositions(&real),
        "the dry run must dispose of every row as the real run does\ndry={dry:#?}\nreal={real:#?}"
    );
    Ok(())
}

/// A dry run lands in a copy of the project, yet never lets Yarn cache inside the project itself:
/// with `XDG_CACHE_HOME` there, the landing caches in its scratch directory, as the real run
/// does, and still lands `chalk`.
#[test]
fn a_dry_run_never_caches_inside_the_project() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = chalk_behind()?;
    let caches = fixture.root().join(".cache");
    let caches = caches
        .to_str()
        .ok_or_else(|| eyre::eyre!("the cache home {} is not UTF-8", caches.display()))?;

    let dry = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE, "--dry-run"],
        &[("XDG_CACHE_HOME", caches), ("COOLDOWN_YARN", YARN_CLASSIC)],
    );

    assert!(
        !fixture.root().join(".cache").join("yarn").exists(),
        "the dry run cached inside the project\n{dry:#?}"
    );
    if cfg!(target_os = "linux") {
        assert_eq!(
            dry.applied_names(),
            BTreeSet::from(["chalk".to_owned()]),
            "{dry:#?}"
        );
    }
    Ok(())
}

/// A `.yarnrc` that keeps Yarn from writing the lock (`--install.pure-lockfile true`) leaves the
/// copy's lock marked as unwritten after Yarn exits successfully, so the landing fails, and the
/// project keeps every file as it was.
#[test]
fn a_lock_yarn_never_wrote_is_never_published() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = chalk_behind()?;
    fixture.write(".yarnrc", "--install.pure-lockfile true\n");
    let lock_before = fixture.read_bytes("yarn.lock");
    let before = project_files(fixture.root());

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE])?;

    assert_eq!(
        changed(&before, &project_files(fixture.root())),
        Vec::<String>::new(),
        "{upgrade:#?}"
    );
    if declined_off_linux(&fixture, &upgrade, &lock_before) {
        return Ok(());
    }
    assert_eq!(upgrade.summary_applied(), 0, "{upgrade:#?}");
    let detail = upgrade.skip_detail_for("chalk").unwrap_or_default();
    assert!(detail.contains("without writing yarn.lock"), "{upgrade:#?}");
    Ok(())
}

/// A `.yarnrc` that sends the install's `node_modules` outside the copy
/// (`--install.modules-folder`) gets Yarn's write there denied, so the run fails with Yarn's own
/// message, nothing appears there, and the project keeps every file as it was.
#[test]
fn a_landing_that_would_write_outside_its_copy_fails() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = chalk_behind()?;
    let cache_home = fixture.yarn_cache_home();
    let outside = cache_home.with_file_name("modules");
    let folder = serde_json::to_string(&outside.to_string_lossy())?;
    fixture.write(".yarnrc", &format!("--install.modules-folder {folder}\n"));
    let lock_before = fixture.read_bytes("yarn.lock");
    let before = project_files(fixture.root());

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE])?;

    assert_eq!(
        changed(&before, &project_files(fixture.root())),
        Vec::<String>::new(),
        "{upgrade:#?}"
    );
    assert!(!outside.exists(), "Yarn wrote outside its copy");
    if declined_off_linux(&fixture, &upgrade, &lock_before) {
        return Ok(());
    }
    assert_eq!(upgrade.summary_applied(), 0, "{upgrade:#?}");
    assert!(
        upgrade.items().iter().any(|item| item["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("EACCES: permission denied"))),
        "{upgrade:#?}"
    );
    Ok(())
}

/// A dependency only the lock names is a transitive one, which `outdated --transitive` reports as
/// no declared dependency.
/// Under a freeze before `ansi-styles` 4.3.0, which `chalk` locks through `ansi-styles@^4.1.0`,
/// `fix` matures `chalk` and plans to mature the transitive too, and whatever it does with the
/// transitive is reported: the lock moves exactly the applied rows, and Yarn accepts it.
#[test]
fn a_transitive_dependency_never_moves_unreported() -> eyre::Result<()> {
    // After `chalk` 4.1.0 and `supports-color` 7.2.0 (2020-08-28), before `ansi-styles` 4.3.0
    // (2020-10-04), whose matured predecessor is 4.2.1.
    const BEFORE_ANSI_STYLES: &str = "2020-09-01";
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = locked(&[("package.json", &chalk_project("^4.0.0"))])?;
    let lock_before = fixture.read_bytes("yarn.lock");

    let outdated = cooldown(
        &fixture,
        &["outdated", "--freeze", BEFORE_ANSI_STYLES, "--transitive"],
    )?;
    let fixed = cooldown(&fixture, &["fix", "--freeze", BEFORE_ANSI_STYLES])?;
    if declined_off_linux(&fixture, &fixed, &lock_before) {
        return Ok(());
    }

    assert_eq!(is_direct(&outdated, "chalk"), Some(true), "{outdated:#?}");
    assert_eq!(
        is_direct(&outdated, "ansi-styles"),
        Some(false),
        "{outdated:#?}"
    );
    assert_eq!(
        fixed.change_for("ansi-styles"),
        Some(ChangeVersions::new("4.3.0", "4.2.1")),
        "fix must plan to mature the too-fresh transitive\n{fixed:#?}"
    );
    assert_eq!(
        fixed.applied_change_for("chalk"),
        Some(ChangeVersions::new("4.1.2", CHALK_TARGET)),
        "{fixed:#?}"
    );
    assert_bindings_moved_exactly(&fixed, &lock_before, &fixture.read_bytes("yarn.lock"));
    assert_members_install(&fixture, &fixed)?;
    Ok(())
}

/// A target that requires a package its predecessor did not brings that package in at the
/// newest release its range admits, which may be too fresh: `string-width` 4.2.0, the target
/// [`FREEZE`] matures `^4.0.0` to, needs `strip-ansi@^6.0.0`, which Yarn resolves to 6.0.1, and
/// that to `ansi-regex@^5.0.0` at 5.0.1, both published in 2021.
/// The graph gate holds the landing rather than commit them: the lock gains neither release.
#[test]
fn a_target_that_brings_in_a_too_fresh_package_is_held() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let project = |range: &str| {
        formatdoc! {r#"
            {{
              "name": "classic-fresh-transitive",
              "private": true,
              "packageManager": "yarn@{YARN}",
              "dependencies": {{ "string-width": "{range}" }}
            }}
        "#}
    };
    let fixture = locked_behind(
        &[("package.json", &project("4.1.0"))],
        &[("package.json", &project("^4.0.0"))],
    )?;
    let lock_before = fixture.read_bytes("yarn.lock");
    assert_eq!(
        lock_entries(&lock_before)
            .get("string-width@^4.0.0")
            .map(String::as_str),
        Some("4.1.0"),
        "the fixture must start below the target"
    );

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE])?;
    if declined_off_linux(&fixture, &upgrade, &lock_before) {
        return Ok(());
    }

    let lock_after = fixture.read_bytes("yarn.lock");
    let fresh = ["strip-ansi@6.0.1", "ansi-regex@5.0.1"];
    let locked: BTreeSet<String> = lock_entries(&lock_after)
        .iter()
        .map(|(pattern, version)| format!("{}@{version}", package_of(pattern)))
        .collect();
    assert!(
        fresh.iter().all(|release| !locked.contains(*release)),
        "the lock must not gain a too-fresh release: {locked:?}\n{upgrade:#?}"
    );
    assert_eq!(
        upgrade.skipped_reasons_for("string-width"),
        BTreeSet::from(["transitive_in_cooldown".to_owned()]),
        "{upgrade:#?}"
    );
    assert_eq!(lock_after, lock_before, "{upgrade:#?}");
    Ok(())
}

/// A Classic lock records no workspace, so each member's dependencies are attributed through the
/// lock entry its own declaration selects: `packages/a`'s `chalk@^4.0.0` binds 4.0.0 and
/// `packages/b`'s `chalk@^3.0.0` binds 3.0.0.
/// `upgrade` lands `a`'s copy at its target and leaves `b`'s, whose range admits no newer
/// release, through the lock alone, which Yarn accepts.
/// The two copies cannot both be hoisted, so Yarn installs one in a member's own `node_modules`,
/// and none of it lands in the project.
#[test]
fn workspace_members_land_through_their_own_declarations() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let root = formatdoc! {r#"
        {{
          "name": "classic-workspace",
          "private": true,
          "packageManager": "yarn@{YARN}",
          "workspaces": ["packages/*"]
        }}
    "#};
    let member = |name: &str, range: &str| {
        formatdoc! {r#"
            {{ "name": "{name}", "version": "1.0.0", "dependencies": {{ "chalk": "{range}" }} }}
        "#}
    };
    let fixture = locked_behind(
        &[
            ("package.json", &root),
            ("packages/a/package.json", &member("a", "4.0.0")),
            ("packages/b/package.json", &member("b", "^3.0.0")),
        ],
        &[
            ("package.json", &root),
            ("packages/a/package.json", &member("a", "^4.0.0")),
            ("packages/b/package.json", &member("b", "^3.0.0")),
        ],
    )?;
    let lock_before = fixture.read_bytes("yarn.lock");
    let beside = beside_the_lock(&fixture);

    let outdated = cooldown(&fixture, &["outdated", "--freeze", FREEZE, "--all"])?;
    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE])?;
    if declined_off_linux(&fixture, &upgrade, &lock_before) {
        return Ok(());
    }

    assert_eq!(
        outdated.item_member_names("chalk", "4.0.0"),
        ["a"],
        "{outdated:#?}"
    );
    assert_eq!(
        outdated.item_member_names("chalk", "3.0.0"),
        ["b"],
        "{outdated:#?}"
    );
    assert_eq!(
        changed(&beside, &beside_the_lock(&fixture)),
        Vec::<String>::new(),
        "the upgrade must write nothing beside the lock\n{upgrade:#?}"
    );
    assert_eq!(
        upgrade.applied_change_for("chalk"),
        Some(ChangeVersions::new("4.0.0", CHALK_TARGET)),
        "{upgrade:#?}"
    );
    assert_bindings_moved_exactly(&upgrade, &lock_before, &fixture.read_bytes("yarn.lock"));
    assert_members_install(&fixture, &upgrade)?;
    Ok(())
}

/// Two selectors grouped under one entry move apart: the root's `supports-color@^7.0.0` and
/// `chalk`'s own `^7.1.0` share 7.1.0, and `upgrade` lands the root's at its target 7.2.0 while
/// `chalk`'s keeps 7.1.0.
#[test]
fn a_grouped_selector_moves_alone() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let project = |fields: &str| {
        formatdoc! {r#"
            {{ "name": "classic", "version": "1.0.0", "private": true, {fields} }}
        "#}
    };
    // The seed's resolution keeps 7.2.0 out of the cache, so both ranges lock 7.1.0.
    let fixture = locked_behind(
        &[(
            "package.json",
            &project(
                r#""dependencies": { "chalk": "4.0.0", "supports-color": "7.1.0" }, "resolutions": { "supports-color": "7.1.0" }"#,
            ),
        )],
        &[(
            "package.json",
            &project(r#""dependencies": { "chalk": "^4.0.0", "supports-color": "^7.0.0" }"#),
        )],
    )?;
    let lock_before = fixture.read_bytes("yarn.lock");
    let started = lock_entries(&lock_before);
    assert_eq!(
        [
            started.get("supports-color@^7.0.0"),
            started.get("supports-color@^7.1.0")
        ],
        [Some(&"7.1.0".to_owned()); 2],
        "both selectors share 7.1.0\n{}",
        String::from_utf8_lossy(&lock_before)
    );

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE])?;
    if declined_off_linux(&fixture, &upgrade, &lock_before) {
        return Ok(());
    }

    assert_eq!(
        upgrade.applied_change_for("supports-color"),
        Some(ChangeVersions::new("7.1.0", "7.2.0")),
        "{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("yarn.lock");
    let settled = lock_entries(&lock_after);
    assert_eq!(
        [
            settled.get("supports-color@^7.0.0").map(String::as_str),
            settled.get("supports-color@^7.1.0").map(String::as_str)
        ],
        [Some("7.2.0"), Some("7.1.0")],
        "only the root's selector moves\n{}",
        String::from_utf8_lossy(&lock_after)
    );
    assert_bindings_moved_exactly(&upgrade, &lock_before, &lock_after);
    assert_members_install(&fixture, &upgrade)?;
    Ok(())
}

/// `upgrade --build` lands `chalk` and then has Yarn install the lock it wrote: the build
/// succeeds, installs `chalk` at its target, and leaves the lock as the landing moved it.
#[test]
fn upgrade_build_installs_the_landed_lock() -> eyre::Result<()> {
    skip_if_missing!(YARN_CLASSIC, Ok(()));
    let fixture = chalk_behind()?;
    let lock_before = fixture.read_bytes("yarn.lock");

    let upgrade = cooldown(&fixture, &["upgrade", "--freeze", FREEZE, "--build"])?;
    if declined_off_linux(&fixture, &upgrade, &lock_before) {
        return Ok(());
    }

    assert_eq!(upgrade.build_ok(), Some(true), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_change_for("chalk"),
        Some(ChangeVersions::new("4.0.0", CHALK_TARGET)),
        "{upgrade:#?}"
    );
    assert_bindings_moved_exactly(&upgrade, &lock_before, &fixture.read_bytes("yarn.lock"));
    let installed: Value =
        serde_json::from_slice(&fixture.read_bytes("node_modules/chalk/package.json"))?;
    assert_eq!(
        installed.get("version").and_then(Value::as_str),
        Some(CHALK_TARGET),
        "the build must install the target"
    );
    Ok(())
}
