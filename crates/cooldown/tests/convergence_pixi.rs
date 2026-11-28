//! End-to-end convergence tests that drive the REAL pixi (the release `mise.toml` pins) against
//! `pixi.toml` and `pixi.lock` fixtures generated on the fly in temp dirs.
//!
//! They hold the pixi adapter to the contract the other suites hold their adapters to: `outdated`
//! reads the packages pixi locked and plans what `upgrade` plans, and an `upgrade` or `fix` lands
//! every planned change exactly on cooldown's target, writes nothing but the lock, moves nothing
//! it did not report, adopts no build uploaded after the cutoff, leaves a lock pixi's own
//! `pixi lock --check` accepts, and converges, while its `--dry-run` disposes of every row as the
//! real run does.
//! `check` asks pixi for no verdict on the lock, so it fails closed.
//! Every pixi process a test starts, cooldown's included, keeps its home and caches in the
//! fixture's tool home and reads no ambient config (see the harness's `tool_env`), so nothing
//! reaches the user's `~/.pixi` or `~/.cache`.
//!
//! # Determinism
//!
//! Every test pins the cooldown window with `--freeze`, and the conda-forge and PyPI history
//! before that instant is immutable, so cooldown's targets reproduce forever.
//! pixi has a publish-date cutoff of its own, the workspace's `exclude-newer`, which seeds each
//! lock: pixi locks the manifest under `exclude-newer` at the seed instant, and the cutoff then
//! leaves the manifest, which pixi still finds the lock current for, as for a lock written then.
//! Every fixture locks conda-forge alone, and the host platform alone unless a test is about
//! several.
//!
//! # The fixtures
//!
//! `pcre2` is a small C library that conda-forge builds for every platform the suite runs on, and
//! that depends on `bzip2` and `libzlib` besides the platform's C runtime.
//! [`FREEZE`] falls after `pcre2` 10.42 (2023-10-29), whose builds all date from that day, and
//! before 10.43 (2024-02-16), so 10.42 is the matured target of `>=10.40,<11` from the older 10.40
//! and from the too-fresh 10.43 alike.
//! `libzlib` 1.3.1 and the `zlib` that pins it exactly were first built on 2024-02-29 and rebuilt
//! on 2024-05-28, so the locked build can be younger than its version.

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "the shared integration harness deliberately panics on invalid fixture setup or malformed command output"
)]
mod support;

use std::collections::{BTreeMap, BTreeSet};

use color_eyre::eyre;
use indoc::{formatdoc, indoc};
use serde_json::Value;
use support::{CapturedOutput, ChangeVersions, Envelope, Fixture, project_files};

/// The program the tests run pixi by.
const PIXI: &str = "pixi";

/// The pixi release `mise.toml` pins, which every fixture runs.
const PIXI_VERSION: &str = "0.85.0";

/// The absolute cooldown cutoff most tests evaluate under.
const FREEZE: &str = "2024-01-15T00:00:00Z";

/// The release of `pcre2` that [`FREEZE`] matures `>=10.40,<11` to.
const PCRE2_TARGET: &str = "10.42";

/// A seed instant before `pcre2` 10.42, at which pixi locks 10.40.
const SEED_BEHIND: &str = "2023-06-01T00:00:00Z";

/// A seed instant after `pcre2` 10.43, at which pixi locks that too-fresh release beside `libzlib`
/// 1.3 (2024-01-09), which [`FREEZE`] has matured.
const SEED_TOO_FRESH: &str = "2024-02-20T00:00:00Z";

/// A seed instant after `libzlib` 1.3.1 (2024-02-29), at which pixi locks it beside `pcre2` 10.43.
const SEED_LATER: &str = "2024-03-01T00:00:00Z";

/// The `[dependencies]` table most fixtures declare.
const PCRE2_DEPENDENCIES: &str = indoc! {r#"
    [dependencies]
    pcre2 = ">=10.40,<11"
"#};

/// The conda platform of this machine, which a fixture must lock for pixi to install it.
fn host_platform() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "aarch64") => "linux-aarch64",
        ("linux", _) => "linux-64",
        ("macos", "aarch64") => "osx-arm64",
        ("macos", _) => "osx-64",
        _ => "win-64",
    }
}

/// A platform the suite locks beside the host's to cover a lock of several platforms.
fn other_platform() -> &'static str {
    if host_platform() == "win-64" {
        "linux-64"
    } else {
        "win-64"
    }
}

/// A `pixi.toml` that locks `platforms` from conda-forge alone, under `exclude-newer = cutoff` if
/// one is given, with `tables` after its workspace table.
fn manifest(platforms: &[&str], tables: &str, cutoff: Option<&str>) -> eyre::Result<String> {
    // A TOML array of strings reads like a JSON one.
    let platforms = serde_json::to_string(platforms)?;
    let cutoff = cutoff
        .map(|cutoff| format!("exclude-newer = \"{cutoff}\""))
        .unwrap_or_default();
    Ok(formatdoc! {r#"
        [workspace]
        name = "fixture"
        channels = ["conda-forge"]
        platforms = {platforms}
        {cutoff}

        {tables}
    "#})
}

/// Runs pixi with `args` at the fixture root and requires it to succeed.
fn pixi(fixture: &Fixture, args: &[&str]) -> eyre::Result<CapturedOutput> {
    fixture.run_tool(PIXI, args, &[]).require_success()
}

/// A pixi project that locks `platforms` and declares `tables`, whose lock pixi wrote as of
/// `seed`, and which pixi itself finds current.
fn locked(platforms: &[&str], tables: &str, seed: &str) -> eyre::Result<Fixture> {
    let fixture = Fixture::new();
    let version = pixi(&fixture, &["--version"])?;
    eyre::ensure!(
        version.stdout_str().trim() == format!("pixi {PIXI_VERSION}"),
        "`{PIXI}` runs {}, not the pinned pixi {PIXI_VERSION}",
        version.stdout_str().trim()
    );
    fixture.write("pixi.toml", &manifest(platforms, tables, Some(seed))?);
    pixi(&fixture, &["lock"])?;
    fixture.write("pixi.toml", &manifest(platforms, tables, None)?);
    pixi(&fixture, &["lock", "--check"])?;
    Ok(fixture)
}

/// The project most tests share: `pcre2` locked at 10.40, below the matured [`PCRE2_TARGET`] and
/// the newer releases its range admits.
fn pcre2_behind() -> eyre::Result<Fixture> {
    locked(&[host_platform()], PCRE2_DEPENDENCIES, SEED_BEHIND)
}

/// One package pixi reads from the lock for an environment on a platform, as `pixi list --json`
/// reports it.
#[derive(Debug, serde::Deserialize)]
struct Record {
    name: String,
    version: String,
    /// `conda` or `pypi`.
    kind: String,
    /// Whether the manifest declares the package.
    is_explicit: bool,
    /// When the conda build was uploaded, in milliseconds since the epoch; a PyPI package has none.
    timestamp: Option<i64>,
    /// The digest of the archive, which tells one build of a version from another.
    sha256: Option<String>,
}

/// Every package pixi reads from the lock for `environment` on `platform`, which neither installs
/// nor relocks anything.
fn records(fixture: &Fixture, environment: &str, platform: &str) -> eyre::Result<Vec<Record>> {
    let listed = pixi(
        fixture,
        &[
            "list",
            "--json",
            "--frozen",
            "--no-install",
            "--environment",
            environment,
            "--platform",
            platform,
        ],
    )?;
    Ok(serde_json::from_slice(&listed.stdout)?)
}

/// The archive digest of each of `records` by `kind/name`, as [`pins`] keys them.
fn builds(records: &[Record]) -> BTreeMap<String, Option<String>> {
    records
        .iter()
        .map(|record| {
            (
                format!("{}/{}", record.kind, record.name),
                record.sha256.clone(),
            )
        })
        .collect()
}

/// Every package of the default environment on the host.
fn default_records(fixture: &Fixture) -> eyre::Result<Vec<Record>> {
    records(fixture, "default", host_platform())
}

/// The version of each of `records` by `kind/name` (`conda/pcre2`, `pypi/idna`), since one name
/// can be both a conda and a PyPI package.
fn pins(records: &[Record]) -> BTreeMap<String, String> {
    records
        .iter()
        .map(|record| {
            (
                format!("{}/{}", record.kind, record.name),
                record.version.clone(),
            )
        })
        .collect()
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

/// Whether the first item of `report` for `name` is a dependency the manifest declares.
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
    files.remove("pixi.lock");
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

/// Asserts that a landing wrote nothing in the project but the lock: the manifest keeps its bytes,
/// no environment appeared in `.pixi`, and no other file differs from `beside`, taken before the
/// landing.
fn assert_only_the_lock_written(fixture: &Fixture, beside: &BTreeMap<String, Vec<u8>>) {
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    assert_eq!(
        text(&fixture.read_bytes("pixi.toml")),
        beside
            .get("pixi.toml")
            .map(|bytes| text(bytes))
            .unwrap_or_default(),
        "the landing must leave the manifest as it was"
    );
    // Checked before the walk below, since an installed environment holds thousands of files.
    assert!(
        !fixture.root().join(".pixi").exists(),
        "the landing installed an environment into the project"
    );
    assert_eq!(
        changed(beside, &beside_the_lock(fixture)),
        Vec::<String>::new(),
        "the landing must write nothing beside the lock"
    );
}

/// Asserts that every applied row of `report` landed in `after` exactly at its target, and that
/// every package `before` and `after` both hold at different versions moved through an applied
/// row.
///
/// A target may require other packages than the release it replaced, so a package that appears or
/// vanishes is allowed; [`assert_no_build_younger_than`] judges what such a package is worth.
fn assert_moved_exactly(report: &Envelope, before: &[Record], after: &[Record]) {
    let moves: BTreeMap<String, ChangeVersions> = report
        .items()
        .iter()
        .filter(|item| is_applied(item))
        .map(|item| {
            let field = |key: &str| item.get(key).and_then(Value::as_str).unwrap_or_default();
            (
                format!("{}/{}", field("registry"), field("name")),
                ChangeVersions::new(field("from"), field("to")),
            )
        })
        .collect();
    let (before, after) = (pins(before), pins(after));
    for (package, change) in &moves {
        assert_eq!(
            after.get(package),
            Some(&change.to),
            "{package} was applied {} -> {} but pixi reads {:?} from the lock\n{report:#?}",
            change.from,
            change.to,
            after.get(package)
        );
    }
    for (package, was) in &before {
        let Some(now) = after.get(package) else {
            continue;
        };
        if now != was {
            assert_eq!(
                moves.get(package),
                Some(&ChangeVersions::new(was, now)),
                "{package} moved from {was} to {now}, which no applied row reports\n{report:#?}"
            );
        }
    }
}

/// Asserts that no conda build of `records` was uploaded after `cutoff`.
fn assert_no_build_younger_than(records: &[Record], cutoff: &str) -> eyre::Result<()> {
    let cutoff_ms = cutoff.parse::<jiff::Timestamp>()?.as_millisecond();
    let mut younger = Vec::new();
    for record in records {
        if let Some(uploaded_ms) = record.timestamp
            && uploaded_ms > cutoff_ms
        {
            let uploaded = jiff::Timestamp::from_millisecond(uploaded_ms)?;
            younger.push(format!("{}@{} ({uploaded})", record.name, record.version));
        }
    }
    assert_eq!(
        younger,
        Vec::<String>::new(),
        "the lock holds builds uploaded after {cutoff}"
    );
    Ok(())
}

/// Asserts that pixi finds the lock current for the manifest as it stands: `pixi lock --check`
/// fails on a lock it would change.
fn assert_pixi_accepts(fixture: &Fixture) -> eyre::Result<()> {
    pixi(fixture, &["lock", "--check"])?;
    Ok(())
}

/// `check` cannot evaluate a pixi project yet: the adapter asks pixi for no verdict on the lock,
/// so it fails closed with `lock_unknown` alone, checks nothing, and leaves every file as it was,
/// although pixi itself just wrote the lock and finds it current.
///
/// This pins today's behavior; it flips once pixi's own `pixi lock --check` backs `check`.
#[test]
fn check_fails_closed_on_an_unverifiable_pixi_lock() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let fixture = pcre2_behind()?;
    let before = project_files(fixture.root());

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);

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

/// `outdated` reads exactly the packages pixi's own `pixi list` reads from the lock, and tells the
/// dependency the manifest declares from the transitive ones as pixi does.
/// It agrees with `upgrade` on the plan: nothing caps `pcre2` in this graph, so it is adoptable at
/// exactly the target `upgrade` plans for it, with newer releases still in cooldown past it.
/// Neither command writes a file.
#[test]
fn outdated_reads_pixis_lock_and_agrees_with_upgrade() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let fixture = pcre2_behind()?;
    let before = project_files(fixture.root());

    let outdated =
        fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive", "--all"]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);

    assert_eq!(
        changed(&before, &project_files(fixture.root())),
        Vec::<String>::new()
    );
    // What cooldown reads
    let listed = default_records(&fixture)?;
    let releases: BTreeSet<String> = listed
        .iter()
        .map(|record| format!("{}@{}", record.name, record.version))
        .collect();
    assert_eq!(item_releases(&outdated), releases, "{outdated:#?}");
    for record in &listed {
        assert_eq!(
            is_direct(&outdated, &record.name),
            Some(record.is_explicit),
            "{} must be direct exactly when the manifest declares it\n{outdated:#?}",
            record.name
        );
    }

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

    // The one plan
    assert!(
        adoptable.contains("pcre2"),
        "nothing caps pcre2, so it must be adoptable\n{outdated:#?}"
    );
    assert_eq!(
        outdated
            .item_field_str("pcre2", "adoptableTarget")
            .as_deref(),
        Some(PCRE2_TARGET),
        "{outdated:#?}"
    );
    assert_ne!(
        outdated.latest_version_for("pcre2").as_deref(),
        Some(PCRE2_TARGET),
        "a newer pcre2 must still be in cooldown at the freeze"
    );
    assert_eq!(
        upgrade
            .change_for("pcre2")
            .map(|planned| planned.to)
            .as_deref(),
        Some(PCRE2_TARGET),
        "upgrade must plan outdated's target\n{upgrade:#?}"
    );
    Ok(())
}

/// `upgrade` lands `pcre2` at exactly its matured target 10.42, below the releases its
/// `>=10.40,<11` admits past the freeze, and writes nothing but the lock: the manifest keeps its
/// range, and no environment is installed.
/// The lock moves exactly the reported row, pixi finds it current, and a second `upgrade` applies
/// nothing and leaves the lock byte for byte.
#[test]
fn upgrade_lands_the_exact_target_in_the_lock_alone_and_converges() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let fixture = pcre2_behind()?;
    let before = default_records(&fixture)?;
    let beside = beside_the_lock(&fixture);

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert_eq!(
        upgrade.applied_change_for("pcre2"),
        Some(ChangeVersions::new("10.40", PCRE2_TARGET)),
        "{upgrade:#?}"
    );
    assert_only_the_lock_written(&fixture, &beside);
    assert_moved_exactly(&upgrade, &before, &default_records(&fixture)?);
    assert_pixi_accepts(&fixture)?;

    let lock_after = fixture.read_bytes("pixi.lock");
    let again = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "the second upgrade must be a no-op\n{again:#?}"
    );
    assert_eq!(fixture.read_bytes("pixi.lock"), lock_after);
    Ok(())
}

/// A version conda-forge keeps rebuilding stays a version of builds: `pcre2` 10.44, the target a
/// freeze in mid-2024 matures 10.43 to, was first built on 2024-06-15 and rebuilt on 2024-08-12
/// and 2025-04-29.
/// `upgrade` lands it, but only through a build uploaded before the freeze, so the lock gains no
/// build younger than the cutoff, the target's own or a package it brings in.
#[test]
fn upgrade_adopts_no_build_younger_than_the_cutoff() -> eyre::Result<()> {
    // After `pcre2` 10.44 (2024-06-15), before 10.45 (2025-04-29).
    const BUILD_FREEZE: &str = "2024-07-01T00:00:00Z";
    skip_if_missing!(PIXI, Ok(()));
    let fixture = locked(&[host_platform()], PCRE2_DEPENDENCIES, SEED_LATER)?;

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", BUILD_FREEZE]);

    assert_eq!(
        upgrade.applied_change_for("pcre2"),
        Some(ChangeVersions::new("10.43", "10.44")),
        "{upgrade:#?}"
    );
    assert_no_build_younger_than(&default_records(&fixture)?, BUILD_FREEZE)
}

/// `fix` matures the too-fresh `pcre2` 10.43 pixi locked for `>=10.40,<11` down to its target
/// 10.42, reported as a downgrade, through the lock alone, which pixi finds current.
/// A second `fix` applies nothing and leaves the lock byte for byte.
#[test]
fn fix_matures_a_too_fresh_version_down_to_its_target_and_converges() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let fixture = locked(&[host_platform()], PCRE2_DEPENDENCIES, SEED_TOO_FRESH)?;
    let before = default_records(&fixture)?;
    assert_eq!(
        pins(&before).get("conda/pcre2").map(String::as_str),
        Some("10.43"),
        "pixi locks the range at its newest release as of the seed"
    );
    let beside = beside_the_lock(&fixture);

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);

    assert_eq!(
        fixed.applied_change_for("pcre2"),
        Some(ChangeVersions::new("10.43", PCRE2_TARGET)),
        "{fixed:#?}"
    );
    assert!(fixed.downgraded_names().contains("pcre2"), "{fixed:#?}");
    assert_only_the_lock_written(&fixture, &beside);
    assert_moved_exactly(&fixed, &before, &default_records(&fixture)?);
    assert_pixi_accepts(&fixture)?;

    let lock_after = fixture.read_bytes("pixi.lock");
    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "the second fix must be a no-op\n{again:#?}"
    );
    assert_eq!(fixture.read_bytes("pixi.lock"), lock_after);
    Ok(())
}

/// A `fix` target the manifest's own range excludes is pixi's to refuse: `pcre2 = ">=10.43,<11"`
/// admits the too-fresh 10.43 pixi locked but not its target 10.42, so pixi cannot lock the pin
/// cooldown adds, the move is held as a resolver conflict naming the range, and the lock stays as
/// it was.
#[test]
fn fix_leaves_a_target_outside_the_declared_range_to_pixi() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let tables = indoc! {r#"
        [dependencies]
        pcre2 = ">=10.43,<11"
    "#};
    let fixture = locked(&[host_platform()], tables, SEED_TOO_FRESH)?;
    let lock_before = fixture.read_bytes("pixi.lock");

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE, "--package", "pcre2"]);

    assert_eq!(
        fixed.skipped_reasons_for("pcre2"),
        BTreeSet::from(["resolver_conflict".to_owned()]),
        "{fixed:#?}"
    );
    assert!(
        fixed
            .skip_detail_for("pcre2")
            .is_some_and(|detail| detail.contains("pcre2 >=10.43,<11")),
        "{fixed:#?}"
    );
    assert_eq!(fixture.read_bytes("pixi.lock"), lock_before);
    Ok(())
}

/// `upgrade --dry-run` disposes of every row as the real run on an identical fixture does, and
/// leaves every project file byte-identical, while the real run lands `pcre2`.
#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let dry_fixture = pcre2_behind()?;
    let before = project_files(dry_fixture.root());
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    assert_eq!(
        changed(&before, &project_files(dry_fixture.root())),
        Vec::<String>::new()
    );
    assert_eq!(dry.lock_status(), None, "--dry-run never re-locks");

    let real_fixture = pcre2_behind()?;
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    // The real run lands, so the two cannot merely agree on the same failure.
    assert_eq!(
        real.applied_names(),
        BTreeSet::from(["pcre2".to_owned()]),
        "the real run must land pcre2\n{real:#?}"
    );
    assert_eq!(
        dispositions(&dry),
        dispositions(&real),
        "the dry run must dispose of every row as the real run does\ndry={dry:#?}\nreal={real:#?}"
    );
    Ok(())
}

/// A package only the lock names stays undeclared whatever cooldown does with it.
/// Under a freeze after `pcre2` 10.43 and `libzlib` 1.3 (2024-01-09) and before `libzlib` 1.3.1
/// (2024-02-29), which `pcre2` locks, `fix` plans to mature the transitive alone, and writes
/// nothing but the lock: pixi still reads `libzlib` as a package the manifest does not declare,
/// the lock moves exactly the applied rows, and pixi finds it current.
#[test]
fn a_too_fresh_transitive_matures_without_being_declared() -> eyre::Result<()> {
    const BEFORE_LIBZLIB: &str = "2024-02-20T00:00:00Z";
    skip_if_missing!(PIXI, Ok(()));
    let fixture = locked(&[host_platform()], PCRE2_DEPENDENCIES, SEED_LATER)?;
    let before = default_records(&fixture)?;
    let beside = beside_the_lock(&fixture);

    let fixed = fixture.cooldown_json(&["fix", "--freeze", BEFORE_LIBZLIB]);

    assert_eq!(
        fixed.applied_change_for("libzlib"),
        Some(ChangeVersions::new("1.3.1", "1.3")),
        "fix must mature the too-fresh transitive\n{fixed:#?}"
    );
    assert_only_the_lock_written(&fixture, &beside);
    let after = default_records(&fixture)?;
    assert!(
        after
            .iter()
            .any(|record| record.name == "libzlib" && !record.is_explicit),
        "pixi must still read libzlib as a transitive package: {after:#?}"
    );
    assert_moved_exactly(&fixed, &before, &after);
    assert_pixi_accepts(&fixture)
}

/// A lock of two platforms and two environments: only the `regex` feature declares `pcre2`, only
/// the `regex` environment includes it, and pixi locks that environment for both platforms.
/// `upgrade` lands the target in the `regex` environment on both platforms through the lock
/// alone, leaves the default environment as it was, and pixi finds the lock current.
#[test]
fn a_feature_dependency_lands_in_its_own_environment_on_every_platform() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let platforms = [host_platform(), other_platform()];
    let tables = indoc! {r#"
        [feature.regex.dependencies]
        pcre2 = ">=10.40,<11"

        [environments]
        regex = ["regex"]
    "#};
    let fixture = locked(&platforms, tables, SEED_BEHIND)?;
    let before = platforms
        .into_iter()
        .map(|platform| {
            Ok(PlatformLock {
                platform,
                regex: records(&fixture, "regex", platform)?,
                default: pins(&records(&fixture, "default", platform)?),
            })
        })
        .collect::<eyre::Result<Vec<_>>>()?;
    let beside = beside_the_lock(&fixture);

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert_eq!(
        upgrade.applied_change_for("pcre2"),
        Some(ChangeVersions::new("10.40", PCRE2_TARGET)),
        "{upgrade:#?}"
    );
    assert_only_the_lock_written(&fixture, &beside);
    for locked in &before {
        let platform = locked.platform;
        let regex = records(&fixture, "regex", platform)?;
        assert_moved_exactly(&upgrade, &locked.regex, &regex);
        assert_eq!(
            pins(&records(&fixture, "default", platform)?),
            locked.default,
            "the default environment on {platform} must keep every package"
        );
    }
    assert_pixi_accepts(&fixture)
}

/// What the lock held on one platform before a landing.
struct PlatformLock {
    platform: &'static str,
    /// Every package of the `regex` environment.
    regex: Vec<Record>,
    /// The version of each package of the default environment, by `kind/name`.
    default: BTreeMap<String, String>,
}

/// A PyPI dependency is a PyPI package to cooldown and pixi alike: under a freeze after `idna` 3.7
/// (2024-04-11) and before 3.8 (2024-08-23), `upgrade` lands the `idna` 3.6 pixi locked from PyPI
/// at 3.7, still as the PyPI package `[pypi-dependencies]` declares, through the lock alone, and
/// pixi finds the lock current.
/// `--package idna` keeps the conda Python it runs on out of the plan.
#[test]
fn a_pypi_dependency_lands_as_a_pypi_package() -> eyre::Result<()> {
    const PYPI_FREEZE: &str = "2024-06-01T00:00:00Z";
    skip_if_missing!(PIXI, Ok(()));
    let tables = indoc! {r#"
        [dependencies]
        python = "3.12.*"

        [pypi-dependencies]
        idna = ">=3,<4"
    "#};
    let fixture = locked(&[host_platform()], tables, "2024-01-01T00:00:00Z")?;
    let before = default_records(&fixture)?;
    assert_eq!(
        pins(&before).get("pypi/idna").map(String::as_str),
        Some("3.6"),
        "pixi locks the range at its newest release as of the seed"
    );
    let beside = beside_the_lock(&fixture);

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", PYPI_FREEZE, "--package", "idna"]);

    assert_eq!(
        upgrade.applied_change_for("idna"),
        Some(ChangeVersions::new("3.6", "3.7")),
        "{upgrade:#?}"
    );
    assert_only_the_lock_written(&fixture, &beside);
    let after = default_records(&fixture)?;
    assert!(
        !pins(&after).contains_key("conda/idna"),
        "the landing must not bring in the conda idna: {after:#?}"
    );
    assert_moved_exactly(&upgrade, &before, &after);
    assert_pixi_accepts(&fixture)
}

/// A PyPI dependency only a feature declares is held with its reason rather than landed, since
/// cooldown narrows only a top-level requirement, and the lock stays as it was.
#[test]
fn a_feature_pypi_dependency_is_held_with_its_reason() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let tables = indoc! {r#"
        [dependencies]
        python = "3.12.*"

        [feature.web.pypi-dependencies]
        idna = ">=3,<4"

        [environments]
        web = ["web"]
    "#};
    let fixture = locked(&[host_platform()], tables, "2024-01-01T00:00:00Z")?;
    let lock_before = fixture.read_bytes("pixi.lock");

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        "2024-06-01T00:00:00Z",
        "--package",
        "idna",
    ]);

    assert_eq!(
        upgrade.skipped_reasons_for("idna"),
        BTreeSet::from(["not_eligible".to_owned()]),
        "{upgrade:#?}"
    );
    assert!(
        upgrade
            .skip_detail_for("idna")
            .is_some_and(|detail| detail.contains("a top-level PyPI requirement")),
        "{upgrade:#?}"
    );
    assert_eq!(is_direct(&upgrade, "idna"), Some(true), "{upgrade:#?}");
    assert_eq!(fixture.read_bytes("pixi.lock"), lock_before);
    Ok(())
}

/// An environment no target touches may hold a build that is already younger than the cutoff:
/// the `zlib` environment requires `zlib >=1.3.1`, whose every build is younger than [`FREEZE`].
/// A `fix` of `pcre2` alone still lands, through the lock alone, and leaves that environment as
/// it was, instead of failing the lock that the cutoff would leave it without a solution in.
#[test]
fn a_young_binding_in_an_unrelated_environment_stays_while_a_fix_lands() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let tables = indoc! {r#"
        [dependencies]
        pcre2 = ">=10.40,<11"

        [feature.zlib.dependencies]
        zlib = ">=1.3.1"

        [environments]
        zlib = { features = ["zlib"], no-default-feature = true }
    "#};
    let fixture = locked(&[host_platform()], tables, SEED_LATER)?;
    let before = default_records(&fixture)?;
    let zlib_before = builds(&records(&fixture, "zlib", host_platform())?);
    let beside = beside_the_lock(&fixture);

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE, "--package", "pcre2"]);

    assert_eq!(
        fixed.applied_change_for("pcre2"),
        Some(ChangeVersions::new("10.43", PCRE2_TARGET)),
        "{fixed:#?}"
    );
    assert_only_the_lock_written(&fixture, &beside);
    assert_moved_exactly(&fixed, &before, &default_records(&fixture)?);
    assert_eq!(
        builds(&records(&fixture, "zlib", host_platform())?),
        zlib_before,
        "the zlib environment must keep every build, its young zlib among them"
    );
    assert_pixi_accepts(&fixture)
}

/// A conda package's age is its locked build's: the `libzlib` 1.3.1 pixi locked in mid-2024 is
/// the rebuild of 2024-05-28, so under a freeze after the version's first build (2024-02-29) and
/// before that rebuild, `fix` matures it down to 1.3 (2024-01-09) rather than keep it.
#[test]
fn a_young_rebuild_of_a_mature_version_is_too_fresh() -> eyre::Result<()> {
    const BEFORE_REBUILD: &str = "2024-04-01T00:00:00Z";
    skip_if_missing!(PIXI, Ok(()));
    let fixture = locked(
        &[host_platform()],
        PCRE2_DEPENDENCIES,
        "2024-06-01T00:00:00Z",
    )?;

    let fixed = fixture.cooldown_json(&[
        "fix",
        "--freeze",
        BEFORE_REBUILD,
        "--package",
        "libzlib",
        "--dry-run",
    ]);

    assert_eq!(
        fixed.change_for("libzlib"),
        Some(ChangeVersions::new("1.3.1", "1.3")),
        "{fixed:#?}"
    );
    Ok(())
}

/// A PyPI dependency keeps its extras: under a freeze after `urllib3` 2.2.0 (2024-01-30) and
/// before 2.2.1 (2024-02-18), `upgrade` lands the 2.1.0 pixi locked for `urllib3[socks]` at 2.2.0,
/// and the lock keeps the `pysocks` the `socks` extra brings in.
#[test]
fn a_pypi_dependency_keeps_its_extras() -> eyre::Result<()> {
    const EXTRAS_FREEZE: &str = "2024-02-10T00:00:00Z";
    skip_if_missing!(PIXI, Ok(()));
    let tables = indoc! {r#"
        [dependencies]
        python = "3.12.*"

        [pypi-dependencies]
        urllib3 = { version = ">=2,<3", extras = ["socks"] }
    "#};
    let fixture = locked(&[host_platform()], tables, "2024-01-01T00:00:00Z")?;
    let before = default_records(&fixture)?;
    let beside = beside_the_lock(&fixture);

    let upgrade =
        fixture.cooldown_json(&["upgrade", "--freeze", EXTRAS_FREEZE, "--package", "urllib3"]);

    assert_eq!(
        upgrade.applied_change_for("urllib3"),
        Some(ChangeVersions::new("2.1.0", "2.2.0")),
        "{upgrade:#?}"
    );
    assert_only_the_lock_written(&fixture, &beside);
    let after = default_records(&fixture)?;
    assert!(
        pins(&after).contains_key("pypi/pysocks"),
        "the socks extra must keep pysocks: {after:#?}"
    );
    assert_moved_exactly(&upgrade, &before, &after);
    assert_pixi_accepts(&fixture)
}

/// pixi's own cutoff is the project's native policy, and a landing it would refuse is held.
/// The manifest keeps `exclude-newer` at 2024-01-01, which cooldown reads as the window that
/// governs `pcre2` while no cooldown policy says otherwise.
/// Once a `cooldown.toml` freeze at 2024-03-01 outranks it, `upgrade` plans `pcre2` 10.43, which
/// would leave a lock pixi finds out of date for the manifest, so the move is held for that, and
/// the lock stays as it was.
#[test]
fn a_landing_the_projects_own_cutoff_rejects_is_held() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let fixture = Fixture::new();
    let tables = indoc! {r#"
        [dependencies]
        pcre2 = ">=10.40,<11"
    "#};
    fixture.write(
        "pixi.toml",
        &manifest(&[host_platform()], tables, Some("2024-01-01"))?,
    );
    pixi(&fixture, &["lock"])?;
    let lock_before = fixture.read_bytes("pixi.lock");

    let outdated = fixture.cooldown_json(&["outdated"]);
    let source = outdated.items().iter().find_map(|item| {
        let named = item.get("name").and_then(Value::as_str) == Some("pcre2");
        named.then(|| item.pointer("/window/source").and_then(Value::as_str))?
    });
    assert_eq!(source, Some("native"), "{outdated:#?}");

    fixture.write("cooldown.toml", "freeze = \"2024-03-01T00:00:00Z\"\n");
    let upgrade = fixture.cooldown_json(&["upgrade"]);

    assert_eq!(upgrade.summary_applied(), 0, "{upgrade:#?}");
    assert_eq!(
        upgrade
            .change_for("pcre2")
            .map(|planned| planned.to)
            .as_deref(),
        Some("10.43"),
        "{upgrade:#?}"
    );
    assert!(
        upgrade
            .skip_detail_for("pcre2")
            .is_some_and(|detail| detail.contains("out of date for the project's manifest")),
        "{upgrade:#?}"
    );
    assert_eq!(fixture.read_bytes("pixi.lock"), lock_before);
    assert_pixi_accepts(&fixture)
}

/// Whether pixi installed `name` at `version` into the project's default environment.
fn installed(fixture: &Fixture, name: &str, version: &str) -> eyre::Result<bool> {
    let metadata = fixture
        .root()
        .join(".pixi")
        .join("envs")
        .join("default")
        .join("conda-meta");
    let release = format!("{name}-{version}-");
    for entry in std::fs::read_dir(metadata)? {
        let file = entry?.file_name();
        let file = file.to_string_lossy();
        if file.starts_with(&release) && file.ends_with(".json") {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `upgrade --build` lands `pcre2` and then has pixi install the lock it wrote: the build
/// succeeds, installs `pcre2` at its target, and leaves the lock as the landing moved it.
#[test]
fn upgrade_build_installs_the_landed_lock() -> eyre::Result<()> {
    skip_if_missing!(PIXI, Ok(()));
    let fixture = pcre2_behind()?;
    let before = default_records(&fixture)?;

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--build"]);

    assert_eq!(upgrade.build_ok(), Some(true), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_change_for("pcre2"),
        Some(ChangeVersions::new("10.40", PCRE2_TARGET)),
        "{upgrade:#?}"
    );
    assert_moved_exactly(&upgrade, &before, &default_records(&fixture)?);
    assert!(
        installed(&fixture, "pcre2", PCRE2_TARGET)?,
        "the build must install the target"
    );
    Ok(())
}
