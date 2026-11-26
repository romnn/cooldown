//! End-to-end convergence tests that drive the REAL `mix` against `mix.lock` fixtures generated on
//! the fly in temp dirs.
//!
//! They hold the Hex adapter to the contract the other suites hold their adapters to: an `upgrade`
//! or `fix` lands every planned change exactly on cooldown's target, moves nothing it did not
//! report, converges, and its `--dry-run` reports the same rows without touching a file.
//! cooldown relocks in a scratch directory and publishes only `mix.lock`, so the project's fetched
//! dependencies (`deps/`) and build (`_build/`) stay byte-identical, and a successful test then
//! runs `mix deps.get --check-locked` itself to show that Mix accepts the published lock.
//! Every fixture gives Mix and Hex private homes (see the harness's `tool_env`) and installs Hex
//! into its own archive dir, so nothing reaches the user's `~/.mix` or `~/.hex` or the Elixir
//! install mise shares between projects.
//!
//! # Determinism
//!
//! Every test pins the cooldown window with `--freeze`, and the Hex history before that instant is
//! immutable, so cooldown's targets reproduce forever.
//! Mix itself has no publish-date cutoff (Hex's own `cooldown` setting is an age, not an instant),
//! so the seed lock is pinned instead: the fixture first locks a `mix.exs` that pins every package
//! exactly, then relaxes it to the real requirement and fetches again, which keeps the seeded
//! versions because they still satisfy it.
//! The assertions check invariants (landing on the planned target, no silent change, convergence,
//! cross-command agreement), never hard-coded versions, except where a test is about one release.
//!
//! # The fixtures
//!
//! `makeup` is a pure-Elixir package that depends on `nimble_parsec`, and both have published
//! within their major line on both sides of `FREEZE`.
//! Seeded at `makeup` 1.1.0 and `nimble_parsec` 1.2.3, `makeup` has a matured within-major upgrade
//! (1.1.2) and newer releases still in cooldown (the 1.2 line).
//! Seeded at `makeup` 1.2.1 and `nimble_parsec` 1.4.1, both are younger than `FREEZE`, so `fix` has
//! to mature them back down (to 1.1.2 and 1.4.0).
//! Nothing in this graph caps either package, so a hold or a blocked verdict is never legitimate
//! here.
//!
//! `makeup` 1.2.1 requires `nimble_parsec ~> 1.4`, while at `COMPANION_FREEZE` the newest matured
//! `nimble_parsec` is 1.3.1 and the newest matured `makeup` is 1.1.0, which accepts it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use indoc::formatdoc;
use support::{Envelope, Fixture, changed_packages, project_files};

/// The Hex release every fixture installs.
const HEX: &str = "2.7.0";

/// The absolute cooldown cutoff most tests evaluate under.
const FREEZE: &str = "2024-06-01T00:00:00Z";

/// A cutoff before `nimble_parsec` 1.4.0, so maturing `nimble_parsec` past `makeup` 1.2.1's
/// requirement takes `makeup` down as well.
const COMPANION_FREEZE: &str = "2023-10-01T00:00:00Z";

/// The real requirement of the fixtures: a within-major range on `makeup`, so cooldown may float it
/// up to the newest matured 1.x release.
const MAKEUP: &str = r#"{:makeup, "~> 1.0"}"#;

/// The versions a fixture's starting lock resolves, as `(package, version)` pairs.
type Seed = &'static [(&'static str, &'static str)];

/// A starting lock old enough that `makeup` has a matured upgrade at `FREEZE`.
const SEED_OLD: Seed = &[("makeup", "1.1.0"), ("nimble_parsec", "1.2.3")];

/// A starting lock whose packages were both published after `FREEZE`, so both violate the
/// cooldown.
const SEED_TOO_FRESH: Seed = &[("makeup", "1.2.1"), ("nimble_parsec", "1.4.1")];

/// A `mix.exs` declaring `deps`, each an Elixir dependency tuple.
fn mix_exs(deps: &[String]) -> String {
    mix_exs_with(deps, "")
}

/// A `mix.exs` declaring `deps`, with `config` (`, key: value…`) added to its project keywords.
fn mix_exs_with(deps: &[String], config: &str) -> String {
    formatdoc! {r#"
        defmodule Demo.MixProject do
          use Mix.Project

          def project do
            [app: :demo, version: "0.1.0", deps: deps(){config}]
          end

          defp deps do
            [
              {deps}
            ]
          end
        end
    "#, deps = deps.join(",\n      ")}
}

/// The `mix.exs` of a local dependency, the project `module` for the app `app` declaring `deps`.
fn dependency_mix_exs(module: &str, app: &str, deps: &str) -> String {
    formatdoc! {r#"
        defmodule {module}.MixProject do
          use Mix.Project

          def project do
            [app: :{app}, version: "0.1.0", deps: [{deps}]]
          end
        end
    "#}
}

/// A project declaring [`MAKEUP`] whose `mix.lock` the real `mix deps.get` resolved to `seed`.
fn fixture(seed: Seed) -> Fixture {
    fixture_with(&[MAKEUP.to_owned()], seed)
}

/// A fixture with Hex installed into its private archive dir, which Mix needs to resolve anything,
/// at the release the suite was verified with rather than whatever is newest.
fn fixture_with_hex() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .run_tool("mix", &["local.hex", HEX, "--force"], &[])
        .expect_success();
    fixture
}

/// A project declaring `deps` whose `mix.lock` the real `mix deps.get` resolved to `seed`, with its
/// dependencies fetched into `deps/` as a working checkout has them.
///
/// `pinned` spells the exact-pin declaration of one seeded package.
fn fixture_with_pins(
    deps: &[String],
    seed: Seed,
    pinned: impl Fn(&str, &str) -> String,
) -> Fixture {
    let fixture = fixture_with_hex();

    // Lock every package at its seed version first, since Mix cannot resolve as of a date.
    let exact: Vec<String> = seed
        .iter()
        .map(|(package, version)| pinned(package, version))
        .collect();
    fixture.write("mix.exs", &mix_exs(&exact));
    fixture.run_tool("mix", &["deps.get"], &[]).expect_success();

    // Relax to the real requirements; Hex keeps the locked versions since they still satisfy them.
    fixture.write("mix.exs", &mix_exs(deps));
    fixture.run_tool("mix", &["deps.get"], &[]).expect_success();

    assert_seeded(&fixture, seed);
    fixture
}

/// A project declaring `deps`, seeded at `seed` under each package's own name.
fn fixture_with(deps: &[String], seed: Seed) -> Fixture {
    fixture_with_pins(deps, seed, |package, version| {
        format!(r#"{{:{package}, "== {version}"}}"#)
    })
}

/// Asserts that `fixture`'s lock holds every package of `seed` at its seeded version.
fn assert_seeded(fixture: &Fixture, seed: Seed) {
    let pins = mix_lock_pins(&fixture.read_bytes("mix.lock"));
    for (package, version) in seed {
        assert_eq!(
            pins.get(*package).map(String::as_str),
            Some(*version),
            "the relock must keep the seeded versions\n{pins:?}"
        );
    }
}

/// The `name -> version` map of every Hex entry a `mix.lock` resolves.
///
/// Mix writes one entry per line, `"name": {:hex, :package, "version", …}`, keyed by the
/// dependency's app name.
fn mix_lock_pins(lock: &[u8]) -> BTreeMap<String, String> {
    let text = String::from_utf8_lossy(lock);
    let mut pins = BTreeMap::new();
    for line in text.lines() {
        let Some((name, entry)) = line.trim().split_once("\": {:hex, ") else {
            continue;
        };
        let Some(version) = entry.split('"').nth(1) else {
            continue;
        };
        pins.insert(name.trim_start_matches('"').to_owned(), version.to_owned());
    }
    pins
}

/// One reported change row, `(name, from, to)`.
type Row = (String, String, String);

/// Every `(name, from, to)` change row of an `upgrade`/`fix` report, held proposals included.
fn rows(report: &Envelope) -> BTreeSet<Row> {
    rows_where(report, |_| true)
}

/// The `(name, from, to)` change rows of an `upgrade`/`fix` report that it applied.
fn applied_rows(report: &Envelope) -> BTreeSet<Row> {
    rows_where(report, |item| {
        item.get("applied")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    })
}

fn rows_where(report: &Envelope, keep: impl Fn(&serde_json::Value) -> bool) -> BTreeSet<Row> {
    report
        .items()
        .iter()
        .filter(|item| keep(item))
        .filter_map(|item| {
            let field = |key: &str| item.get(key)?.as_str().map(str::to_owned);
            Some((field("name")?, field("from")?, field("to")?))
        })
        .collect()
}

/// Asserts that every applied row of `report` landed in `lock` exactly at its target.
///
/// A package the lock holds at any other version either stayed behind or overshot the target to a
/// release younger than the cooldown window.
fn assert_rows_landed_on_target(report: &Envelope, lock: &[u8]) {
    let pins = mix_lock_pins(lock);
    for (name, from, to) in applied_rows(report) {
        assert_eq!(
            pins.get(&name),
            Some(&to),
            "{name} was applied {from} -> {to} but the lock holds {:?}\n{report:#?}",
            pins.get(&name),
        );
    }
}

/// The project's fetched dependencies and build, by path, with their bytes.
fn checkout(fixture: &Fixture) -> BTreeMap<String, Vec<u8>> {
    project_files(fixture.root())
        .into_iter()
        .filter(|(path, _)| path.starts_with("deps/") || path.starts_with("_build/"))
        .collect()
}

/// Asserts that the dependencies fetched into `deps/` are the ones `mix.lock` records, as Mix
/// itself judges them: `deps/` and a lock that disagree leave Mix reporting a lock mismatch that
/// fails the next `mix compile`.
fn assert_fetched_deps_match_the_lock(fixture: &Fixture) {
    let status = fixture.run_tool("mix", &["deps"], &[]).expect_success();
    let status = status.stdout_str();
    assert!(
        !status.contains("lock mismatch") && !status.contains("out of date"),
        "deps/ no longer matches mix.lock\n{status}\n{}",
        String::from_utf8_lossy(&fixture.read_bytes("mix.lock"))
    );
}

/// Asserts that Mix fetches the published lock as it stands: `--check-locked` fails on any change
/// Mix would make to it, and the fetched dependencies then match it.
fn assert_mix_accepts_the_lock(fixture: &Fixture) {
    let lock = fixture.read_bytes("mix.lock");
    fixture
        .run_tool("mix", &["deps.get", "--check-locked"], &[])
        .expect_success();
    assert_eq!(fixture.read_bytes("mix.lock"), lock);
    assert_fetched_deps_match_the_lock(fixture);
}

/// `check` cannot evaluate a Hex project yet: the adapter has no lock-currency probe (#211), so it
/// fails closed with `lock_unknown` alone, checks nothing, and leaves every file as it was.
///
/// This pins today's behavior; it flips once Mix's own verdict backs `check`.
#[test]
fn check_fails_closed_on_an_unverifiable_mix_lock() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);

    assert!(!check.ok(), "{check:#?}");
    assert_eq!(
        check.error_kinds(),
        BTreeSet::from(["lock_unknown".to_owned()]),
        "{check:#?}"
    );
    assert_eq!(check.summary_checked(), 0, "{check:#?}");
    assert_eq!(project_files(fixture.root()), before);
}

/// `outdated` and `upgrade` describe the same plan: what `upgrade` holds `outdated` blocks, and the
/// unconstrained `makeup` is adoptable at exactly the target `upgrade` plans for it.
#[test]
fn outdated_agrees_with_upgrade_on_the_matured_target() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive"]);
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);

    // Both are read-only: `deps/`, `_build/` and the lock stay as they were.
    assert_eq!(project_files(fixture.root()), before);

    // The agreement contract every suite asserts.
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

    // Nothing in this graph caps `makeup`, so its matured upgrade is adoptable, not blocked, and
    // newer releases remain in cooldown past it.
    assert!(
        adoptable.contains("makeup") && !blocked.contains("makeup"),
        "makeup must be adoptable\nadoptable={adoptable:?}\nblocked={blocked:?}\n{outdated:#?}"
    );
    let target = outdated
        .item_field_str("makeup", "adoptableTarget")
        .expect("makeup has an adoptable target");
    assert_ne!(
        outdated.latest_version_for("makeup").as_deref(),
        Some(target.as_str()),
        "a newer makeup must still be in cooldown at the freeze"
    );
    let planned = upgrade.change_for("makeup").expect("upgrade plans makeup");
    assert_eq!(planned.to, target, "upgrade must plan outdated's target");
}

/// A real `upgrade` lands every planned change exactly on its matured target, never past it into
/// the cooldown window, leaves no candidate held in a graph that caps nothing, and reports every
/// version it moved.
/// It rewrites only the lock, leaves the fetched dependencies and the build as they were, and Mix
/// accepts the lock it publishes.
/// A second `upgrade` then finds nothing to do and rewrites nothing.
#[test]
fn upgrade_lands_each_planned_change_on_its_target_and_converges() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    // A compiled checkout, whose `_build/` records which fetched dependencies it built from, as
    // a fetch into the project would change.
    fixture.run_tool("mix", &["compile"], &[]).expect_success();
    let manifest = fixture.read_bytes("mix.exs");
    let lock_before = fixture.read_bytes("mix.lock");
    let checkout_before = checkout(&fixture);

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.lock_status(),
        Some("unknown"),
        "Mix re-locks, but cooldown cannot prove mix.lock currency yet (#211)"
    );
    assert!(
        upgrade.held_conflict_names().is_empty(),
        "nothing in this graph caps a package, so nothing may be held\n{upgrade:#?}"
    );
    assert!(
        upgrade.applied_names().contains("makeup"),
        "the matured makeup upgrade must apply\n{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("mix.lock");
    assert_rows_landed_on_target(&upgrade, &lock_after);
    assert_eq!(
        upgrade.applied_names(),
        changed_packages(&lock_before, &lock_after, mix_lock_pins),
        "the applied set must equal the lock diff (no silent change)"
    );
    assert_eq!(fixture.read_bytes("mix.exs"), manifest);
    assert_eq!(checkout(&fixture), checkout_before);
    assert_mix_accepts_the_lock(&fixture);

    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "the second upgrade must be a no-op\n{second:#?}"
    );
    assert!(
        rows(&second).is_empty(),
        "a converged lock leaves nothing to plan\n{second:#?}"
    );
    assert_eq!(
        fixture.read_bytes("mix.lock"),
        lock_after,
        "the lock must be byte-identical across the two converged runs"
    );
}

/// `upgrade --dry-run` reports the same rows and holds as the real run on an identical fixture,
/// and leaves every project file byte-identical, while the real run lands them all.
#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("mix");

    let dry_fixture = fixture(SEED_OLD);
    let before = project_files(dry_fixture.root());
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    assert_eq!(project_files(dry_fixture.root()), before);
    assert_eq!(dry.lock_status(), None, "--dry-run never re-locks");

    let real_fixture = fixture(SEED_OLD);
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    // The real run succeeds, so the two cannot merely agree on the same failure.
    assert!(real.ok(), "{real:#?}");
    assert!(
        real.applied_names().contains("makeup"),
        "the real run must land makeup\n{real:#?}"
    );
    assert_rows_landed_on_target(&real, &real_fixture.read_bytes("mix.lock"));

    assert!(
        !rows(&dry).is_empty(),
        "the dry run must plan makeup\n{dry:#?}"
    );
    assert_eq!(
        rows(&dry),
        rows(&real),
        "the dry run must report the real run's rows\ndry={dry:#?}\nreal={real:#?}"
    );
    assert_eq!(
        dry.held_conflict_names(),
        real.held_conflict_names(),
        "the dry run must hold what the real run holds"
    );
}

/// `fix` matures every too-fresh package down to its target, moving a parent and its child down
/// together, reports each move as a downgrade, and a second `fix` finds nothing left to do.
#[test]
fn fix_matures_too_fresh_packages_to_their_targets_and_is_idempotent() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_TOO_FRESH);
    let checkout_before = checkout(&fixture);

    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);

    assert!(fixed.ok(), "{fixed:#?}");
    assert_eq!(fixed.summary_errors(), 0, "{fixed:#?}");
    let applied = fixed.applied_names();
    assert!(
        applied.contains("makeup") && applied.contains("nimble_parsec"),
        "fix must mature both too-fresh packages\n{fixed:#?}"
    );
    assert_eq!(
        fixed.downgraded_names(),
        applied,
        "every fix move is a downgrade"
    );
    assert_rows_landed_on_target(&fixed, &fixture.read_bytes("mix.lock"));
    assert_eq!(checkout(&fixture), checkout_before);
    let lock_after_fix = fixture.read_bytes("mix.lock");

    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "the second fix must be a no-op\n{again:#?}"
    );
    assert_eq!(
        lock_after_fix,
        fixture.read_bytes("mix.lock"),
        "the second fix must leave the lock byte-identical"
    );
    assert_mix_accepts_the_lock(&fixture);
}

/// `upgrade` of the parent alone keeps the child it still accepts: `makeup` lands on 1.1.2 and
/// `nimble_parsec` stays on the 1.2.3 the lock holds, rather than following Mix to its newest
/// release.
#[test]
fn upgrade_retains_a_child_the_target_still_accepts() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let lock_before = fixture.read_bytes("mix.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "makeup"]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["makeup".to_owned()]),
        "{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("mix.lock");
    assert_rows_landed_on_target(&upgrade, &lock_after);
    assert_eq!(
        changed_packages(&lock_before, &lock_after, mix_lock_pins),
        upgrade.applied_names(),
        "the child must not move"
    );
    assert_mix_accepts_the_lock(&fixture);
}

/// `upgrade` advances a package no `mix.exs` declares: with `makeup` already at its newest matured
/// release, only the transitive `nimble_parsec` moves, and it lands exactly on its target.
#[test]
fn upgrade_advances_a_transitive_package_on_its_own() {
    skip_if_missing!("mix");
    let fixture = fixture(&[("makeup", "1.1.2"), ("nimble_parsec", "1.2.3")]);
    let lock_before = fixture.read_bytes("mix.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["nimble_parsec".to_owned()]),
        "only the transitive has a matured upgrade\n{upgrade:#?}"
    );
    let lock_after = fixture.read_bytes("mix.lock");
    assert_rows_landed_on_target(&upgrade, &lock_after);
    assert_eq!(
        changed_packages(&lock_before, &lock_after, mix_lock_pins),
        upgrade.applied_names(),
        "the applied set must equal the lock diff (no silent change)"
    );
    assert_mix_accepts_the_lock(&fixture);
}

/// A rejected change leaves the checkout as it was: maturing `nimble_parsec` alone to 1.3.1, which
/// the locked `makeup` 1.2.1 does not accept, is held, and the lock, the fetched dependencies and
/// the build stay byte-identical and still agree.
#[test]
fn a_rejected_change_leaves_the_checkout_matching_the_original_lock() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_TOO_FRESH);
    let lock_before = fixture.read_bytes("mix.lock");
    let checkout_before = checkout(&fixture);

    let fixed = fixture.cooldown_json(&[
        "fix",
        "--freeze",
        COMPANION_FREEZE,
        "--package",
        "nimble_parsec",
    ]);

    assert!(fixed.applied_names().is_empty(), "{fixed:#?}");
    assert!(
        !fixed.skipped_reasons_for("nimble_parsec").is_empty(),
        "nimble_parsec must be reported held\n{fixed:#?}"
    );
    assert_eq!(fixture.read_bytes("mix.lock"), lock_before);
    assert_eq!(checkout(&fixture), checkout_before);
    assert_fetched_deps_match_the_lock(&fixture);
}

/// A target that needs another package moved lands when the same batch moves that one too:
/// maturing `makeup` to 1.1.0 together with `nimble_parsec` to 1.3.1 lands both, although
/// `nimble_parsec` 1.3.1 alone is held.
#[test]
fn a_companion_move_in_the_same_batch_lands_with_its_target() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_TOO_FRESH);

    let fixed = fixture.cooldown_json(&["fix", "--freeze", COMPANION_FREEZE]);

    assert!(fixed.ok(), "{fixed:#?}");
    assert_eq!(
        fixed.applied_names(),
        BTreeSet::from(["makeup".to_owned(), "nimble_parsec".to_owned()]),
        "{fixed:#?}"
    );
    assert_rows_landed_on_target(&fixed, &fixture.read_bytes("mix.lock"));
    assert_mix_accepts_the_lock(&fixture);
}

/// A target the project's own requirement excludes is held on its own, while the target beside it
/// still lands: `mix.exs` keeps `makeup` on 1.1, below the 1.2.1 cooldown plans for it, since
/// cooldown does not read `mix.exs` bounds yet (#210).
#[test]
fn a_direct_requirement_conflict_is_held_while_the_others_land() {
    skip_if_missing!("mix");
    let fixture = fixture_with(&[r#"{:makeup, "~> 1.1.0"}"#.to_owned()], SEED_OLD);

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", "2025-06-01T00:00:00Z"]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["nimble_parsec".to_owned()]),
        "{upgrade:#?}"
    );
    assert!(
        !upgrade.skipped_reasons_for("makeup").is_empty(),
        "makeup must be reported held\n{upgrade:#?}"
    );
    let lock = fixture.read_bytes("mix.lock");
    assert_rows_landed_on_target(&upgrade, &lock);
    assert_eq!(
        mix_lock_pins(&lock).get("makeup").map(String::as_str),
        Some("1.1.0")
    );
    assert_mix_accepts_the_lock(&fixture);
}

/// A target no dependency reaches is held rather than reported landed: the lock keeps a `jason`
/// entry nothing requires any more, which Mix leaves in place, so the entry cooldown seeds for it
/// would otherwise read as applied.
#[test]
fn a_target_no_dependency_reaches_is_held() {
    skip_if_missing!("mix");
    let fixture = fixture(&[
        ("makeup", "1.1.0"),
        ("nimble_parsec", "1.2.3"),
        ("jason", "1.4.0"),
    ]);
    let lock_before = fixture.read_bytes("mix.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "jason"]);

    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert!(
        upgrade
            .skip_detail_for("jason")
            .is_some_and(|detail| detail.contains("no dependency reaches jason")),
        "{upgrade:#?}"
    );
    assert_eq!(fixture.read_bytes("mix.lock"), lock_before);
}

/// A dependency declared under another app name (`{:parsec, "~> 1.2", hex: :nimble_parsec}`) is
/// the Hex package the lock entry names, not the lock key: `outdated` reads its history exactly as
/// it reads `nimble_parsec`'s own.
#[test]
fn an_aliased_dependency_is_read_under_its_hex_package_name() {
    skip_if_missing!("mix");
    let seed: Seed = &[("nimble_parsec", "1.2.3")];
    let aliased = aliased_fixture();
    let plain = fixture_with(&[r#"{:nimble_parsec, "~> 1.2"}"#.to_owned()], seed);

    let aliased = aliased.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let plain = plain.cooldown_json(&["outdated", "--freeze", FREEZE]);

    assert_eq!(aliased.summary_errors(), 0, "{aliased:#?}");
    let target = plain
        .item_field_str("nimble_parsec", "adoptableTarget")
        .expect("nimble_parsec has an adoptable target");
    assert_eq!(
        aliased.item_field_str("parsec", "adoptableTarget"),
        Some(target),
        "{aliased:#?}"
    );
    assert_eq!(
        aliased.latest_version_for("parsec"),
        plain.latest_version_for("nimble_parsec"),
        "{aliased:#?}"
    );
}

/// A project declaring `nimble_parsec` under the app name `parsec`, seeded at 1.2.3.
fn aliased_fixture() -> Fixture {
    fixture_with_pins(
        &[r#"{:parsec, "~> 1.2", hex: :nimble_parsec}"#.to_owned()],
        &[("parsec", "1.2.3")],
        |package, version| format!(r#"{{:{package}, "== {version}", hex: :nimble_parsec}}"#),
    )
}

/// An aliased dependency lands under its app name, still locking its own Hex package.
#[test]
fn an_aliased_dependency_lands_under_its_app_name() {
    skip_if_missing!("mix");
    let fixture = aliased_fixture();

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["parsec".to_owned()]),
        "{upgrade:#?}"
    );
    let lock = fixture.read_bytes("mix.lock");
    assert_rows_landed_on_target(&upgrade, &lock);
    let text = String::from_utf8_lossy(&lock);
    assert!(
        text.contains(r#""parsec": {:hex, :nimble_parsec, "#),
        "{text}"
    );
    assert_mix_accepts_the_lock(&fixture);
}

/// A mirror, or a repository configured in hex.pm's place, can serve something else under a
/// public package's name and version, so `upgrade` refuses to relock while one is configured,
/// writes nothing, and never repeats its address, which can carry credentials.
#[test]
fn a_hexpm_mirror_refuses_the_relock() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());
    let mirror = "https://user:secret@hex.example.invalid";
    let assert_refused = |upgrade: &Envelope, label: &str| {
        assert!(upgrade.summary_errors() > 0, "{label}: {upgrade:#?}");
        assert!(upgrade.applied_names().is_empty(), "{label}: {upgrade:#?}");
        assert!(
            !format!("{upgrade:?}").contains("secret"),
            "{label}: {upgrade:#?}"
        );
        assert_eq!(project_files(fixture.root()), before, "{label}");
    };

    for key in [
        "HEX_MIRROR",
        "HEX_MIRROR_URL",
        "HEX_TRUSTED_MIRROR",
        "HEX_TRUSTED_MIRROR_URL",
    ] {
        let upgrade = fixture.cooldown_json_env(&["upgrade", "--freeze", FREEZE], &[(key, mirror)]);
        assert_refused(&upgrade, key);
    }

    // A repository URL persisted in the fixture's own Hex config.
    fixture
        .run_tool("mix", &["hex.repo", "set", "hexpm", "--url", mirror], &[])
        .expect_success();
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_refused(&upgrade, "hex.repo set hexpm --url");
}

/// Hex's own cooldown and policy, and Mix's shared dependency and build paths, set in the
/// environment, leave the relock alone: the upgrade reports the rows a dry run without them plans,
/// lands them, and never fetches or builds where the environment points.
#[test]
fn ambient_hex_and_mix_settings_leave_the_relock_alone() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let elsewhere = tempfile::tempdir().expect("create a shared directory");
    let deps = elsewhere.path().join("deps");
    let build = elsewhere.path().join("_build");
    let planned = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);

    let upgrade = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE],
        &[
            // A cooldown that would hide every release cooldown plans, if it applied.
            ("HEX_COOLDOWN", "3650d"),
            // A policy from a repository nobody configured, which fails the resolve if it applies.
            ("HEX_POLICY", "acme/strict"),
            ("MIX_DEPS_PATH", deps.to_str().expect("a UTF-8 temp dir")),
            ("MIX_BUILD_PATH", build.to_str().expect("a UTF-8 temp dir")),
        ],
    );

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(!rows(&planned).is_empty(), "{planned:#?}");
    assert_eq!(rows(&upgrade), rows(&planned), "{upgrade:#?}");
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("mix.lock"));
    assert!(!deps.exists() && !build.exists());
}

/// A project naming another lock than `mix.lock` is refused rather than relocked from it.
#[test]
fn a_project_naming_another_lock_is_refused() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let lock = String::from_utf8(fixture.read_bytes("mix.lock")).expect("a UTF-8 lock");
    fixture.write("other.lock", &lock);
    fixture.write(
        "mix.exs",
        &mix_exs_with(&[MAKEUP.to_owned()], r#", lockfile: "other.lock""#),
    );
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.summary_errors() > 0, "{upgrade:#?}");
    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert_eq!(project_files(fixture.root()), before);
}

/// A Hex older than the relock relies on is refused before anything is resolved, as is a Mix
/// without Hex and a Hex that lacks a function the relock calls, and each refusal names what it
/// found.
#[test]
fn an_unsupported_hex_is_refused() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let before = project_files(fixture.root());
    let older = tempfile::tempdir().expect("create an archive dir");
    let older = older.path().to_str().expect("a UTF-8 temp dir");
    fixture
        .run_tool(
            "mix",
            &["local.hex", "2.5.1", "--force"],
            &[("MIX_ARCHIVES", older)],
        )
        .expect_success();
    let empty = tempfile::tempdir().expect("create an archive dir");
    let empty = empty.path().to_str().expect("a UTF-8 temp dir");
    // A Hex archive that reports a version past every floor but has no `Hex.State`.
    let stand_in = tempfile::tempdir().expect("create an archive dir");
    let ebin = stand_in
        .path()
        .join("hex-9.9.9")
        .join("hex-9.9.9")
        .join("ebin");
    let source = stand_in.path().join("hex.ex");
    std::fs::write(&source, r#"defmodule Hex do def version, do: "9.9.9" end"#)
        .expect("write the stand-in Hex");
    fixture
        .run_tool(
            "elixirc",
            &[
                source.to_str().expect("a UTF-8 temp dir"),
                "-o",
                ebin.to_str().expect("a UTF-8 temp dir"),
            ],
            &[],
        )
        .expect_success();
    let stand_in = stand_in.path().to_str().expect("a UTF-8 temp dir");

    for (archives, found) in [
        (
            older,
            "needs Hex >= 2.5.2 to land exact versions, but loaded Hex 2.5.1",
        ),
        (
            empty,
            "needs Hex >= 2.5.2 to land exact versions, but Hex is not installed",
        ),
        (stand_in, "with Hex 9.9.9, which has no Hex.State.fetch!/1"),
    ] {
        let upgrade = fixture.cooldown_json_env(
            &["upgrade", "--freeze", FREEZE],
            &[("MIX_ARCHIVES", archives)],
        );

        assert!(upgrade.summary_errors() > 0, "{found}: {upgrade:#?}");
        assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
        assert!(
            format!("{upgrade:?}").contains(found),
            "{found}: {upgrade:#?}"
        );
        assert_eq!(project_files(fixture.root()), before, "{found}");
    }
}

/// `upgrade --build` relocks and then fetches the lock the run accepted into the checkout with
/// Mix's own `deps.get` and `deps.get --check-locked`, whatever task the project aliases as
/// `deps.get` and whatever project `MIX_EXS` names.
#[test]
fn upgrade_build_fetches_the_accepted_lock() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    fixture.write(
        "mix.exs",
        &mix_exs_with(
            &[MAKEUP.to_owned()],
            r#", aliases: ["deps.get": [fn _ -> raise "the deps.get alias ran" end]]"#,
        ),
    );
    // Another project, whose dependency a fetch for it would have to add to the lock.
    fixture.write(
        "other/mix.exs",
        &mix_exs(&[r#"{:jason, "~> 1.4"}"#.to_owned()]),
    );
    let other = fixture.root().join("other").join("mix.exs");

    let upgrade = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE, "--build"],
        &[("MIX_EXS", other.to_str().expect("a UTF-8 temp dir"))],
    );

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(upgrade.build_ok(), Some(true), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("makeup"), "{upgrade:#?}");
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("mix.lock"));
    assert_fetched_deps_match_the_lock(&fixture);
}

/// An umbrella project whose child app declares `makeup` upgrades it in the umbrella's lock.
#[test]
fn an_umbrella_child_dependency_lands() {
    skip_if_missing!("mix");
    let fixture = fixture_with_hex();
    fixture.write(
        "mix.exs",
        indoc::indoc! {r#"
            defmodule Umbrella.MixProject do
              use Mix.Project

              def project do
                [apps_path: "apps", version: "0.1.0", deps: []]
              end
            end
        "#},
    );
    let child = |deps: &str| {
        formatdoc! {r#"
            defmodule Child.MixProject do
              use Mix.Project

              def project do
                [
                  app: :child,
                  version: "0.1.0",
                  build_path: "../../_build",
                  config_path: "../../config/config.exs",
                  deps_path: "../../deps",
                  lockfile: "../../mix.lock",
                  deps: [{deps}]
                ]
              end
            end
        "#}
    };
    // Lock the seed versions first, since Mix cannot resolve as of a date.
    fixture.write(
        "apps/child/mix.exs",
        &child(r#"{:makeup, "== 1.1.0"}, {:nimble_parsec, "== 1.2.3"}"#),
    );
    fixture.run_tool("mix", &["deps.get"], &[]).expect_success();
    fixture.write("apps/child/mix.exs", &child(MAKEUP));
    fixture.run_tool("mix", &["deps.get"], &[]).expect_success();
    assert_seeded(&fixture, SEED_OLD);
    let checkout_before = checkout(&fixture);

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("makeup"), "{upgrade:#?}");
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("mix.lock"));
    assert_eq!(checkout(&fixture), checkout_before);
    assert_mix_accepts_the_lock(&fixture);
}

/// A path dependency under a planned app's name never lands a Hex release: once `makeup` comes
/// from a local checkout, the lock still holds its Hex entry, which no Hex dependency uses, so the
/// upgrade cooldown plans from that entry is held rather than reported applied.
#[test]
fn a_path_dependency_never_lands_a_hex_target() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    fixture.write(
        "vendor/makeup/mix.exs",
        &dependency_mix_exs("Makeup", "makeup", ""),
    );
    fixture.write(
        "mix.exs",
        &mix_exs(&[r#"{:makeup, path: "vendor/makeup"}"#.to_owned()]),
    );
    let lock_before = fixture.read_bytes("mix.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "makeup"]);

    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert!(
        upgrade
            .skip_detail_for("makeup")
            .is_some_and(|detail| detail.contains("no dependency reaches makeup")),
        "{upgrade:#?}"
    );
    assert_eq!(fixture.read_bytes("mix.lock"), lock_before);
}

/// A dependency cannot move the relock out of its scratch directory: a local dependency whose
/// `system_env` points `MIX_DEPS_PATH` elsewhere would have Mix fetch its own dependency there, so
/// the upgrade is refused before anything is fetched, and nothing lands there.
#[test]
fn a_dependency_cannot_move_the_relock_out_of_its_scratch_directory() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let elsewhere = tempfile::tempdir().expect("create a shared directory");
    let deps = elsewhere.path().join("deps");
    fixture.write(
        "local/escape/mix.exs",
        &dependency_mix_exs("Escape", "escape", r#"{:jason, "~> 1.4"}"#),
    );
    // The `Debug` form of a path is an Elixir string literal too, with any backslash escaped.
    fixture.write(
        "mix.exs",
        &mix_exs(&[
            MAKEUP.to_owned(),
            format!(
                r#"{{:escape, path: "local/escape", system_env: [{{"MIX_DEPS_PATH", {deps:?}}}]}}"#
            ),
        ]),
    );
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.summary_errors() > 0, "{upgrade:#?}");
    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert!(
        format!("{upgrade:?}").contains("would fetch or build jason outside it"),
        "{upgrade:#?}"
    );
    assert!(!deps.exists(), "Mix fetched into {}", deps.display());
    assert_eq!(project_files(fixture.root()), before);
}

/// The relock installs whatever Mix installs, such as Rebar, into a Mix home of its own: a local
/// dependency whose `mix.exs` records the Mix home as Mix loads it sees the project's home when the
/// project fetches its dependencies, and another one during the relock.
#[test]
fn the_relock_uses_a_mix_home_of_its_own() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    fixture.write(
        "local/probe/mix.exs",
        indoc::indoc! {r#"
            defmodule Probe.MixProject do
              use Mix.Project

              def project do
                File.write!(Path.join(__DIR__, "mix_home"), Mix.Utils.mix_home())
                [app: :probe, version: "0.1.0", deps: []]
              end
            end
        "#},
    );
    fixture.write(
        "mix.exs",
        &mix_exs(&[
            MAKEUP.to_owned(),
            r#"{:probe, path: "local/probe"}"#.to_owned(),
        ]),
    );
    fixture.run_tool("mix", &["deps.get"], &[]).expect_success();
    let project_home = fixture.read_bytes("local/probe/mix_home");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("makeup"), "{upgrade:#?}");
    assert_ne!(fixture.read_bytes("local/probe/mix_home"), project_home);
}

/// A relock never adds a dependency cooldown cannot judge: with a Git dependency declared but not
/// yet locked, upgrading `makeup` would add its pin to the lock, so the upgrade is held and the
/// lock stays as it was.
#[test]
fn a_git_dependency_the_relock_would_add_holds_the_upgrade() {
    skip_if_missing!("mix");
    skip_if_missing!("git");
    let fixture = fixture(SEED_OLD);
    fixture.write(
        "mix.exs",
        &mix_exs(&[
            MAKEUP.to_owned(),
            r#"{:nimble_options, git: "https://github.com/dashbitco/nimble_options.git", tag: "v1.1.1"}"#
                .to_owned(),
        ]),
    );
    let lock_before = fixture.read_bytes("mix.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "makeup"]);

    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert!(
        upgrade
            .skip_detail_for("makeup")
            .is_some_and(|detail| detail.contains("Mix would change nimble_options")),
        "{upgrade:#?}"
    );
    assert_eq!(fixture.read_bytes("mix.lock"), lock_before);
}

/// A preview never resolves a smaller graph than the project's: `--dry-run` relocks a copy of the
/// project, which leaves a sibling path dependency behind, so the preview is refused, while the
/// real upgrade, which finds the sibling, lands.
#[test]
fn a_preview_without_a_sibling_path_dependency_is_refused() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    // The project sits one level down, beside its sibling, with the lock the fixture seeded.
    fixture.write(
        "cooldown_sibling/mix.exs",
        &dependency_mix_exs("CooldownSibling", "cooldown_sibling", ""),
    );
    fixture.write(
        "app/mix.exs",
        &mix_exs(&[
            MAKEUP.to_owned(),
            r#"{:cooldown_sibling, path: "../cooldown_sibling"}"#.to_owned(),
        ]),
    );
    let lock = String::from_utf8(fixture.read_bytes("mix.lock")).expect("a UTF-8 lock");
    fixture.write("app/mix.lock", &lock);

    let dry = fixture.cooldown_json_in(Some("app"), &["upgrade", "--freeze", FREEZE, "--dry-run"]);

    assert!(dry.summary_errors() > 0, "{dry:#?}");
    assert!(
        format!("{dry:?}").contains("the path dependency cooldown_sibling, which is missing"),
        "{dry:#?}"
    );

    let real = fixture.cooldown_json_in(Some("app"), &["upgrade", "--freeze", FREEZE]);

    assert!(real.ok(), "{real:#?}");
    assert!(real.applied_names().contains("makeup"), "{real:#?}");
    assert_rows_landed_on_target(&real, &fixture.read_bytes("app/mix.lock"));
}

/// The relock reads hex.pm's registry into a cache of its own, never the user's, where an earlier
/// run may have left records it did not verify: offline, with the records and the tarball the
/// upgrade needs in the fixture's Hex cache, the relock has nothing to resolve from and lands
/// nothing.
#[test]
fn the_relock_never_reads_the_users_registry_cache() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let manifest = String::from_utf8(fixture.read_bytes("mix.exs")).expect("a UTF-8 manifest");
    let lock_before = fixture.read_bytes("mix.lock");
    // Fetching the target once leaves its registry records and tarball in the fixture's Hex cache.
    fixture.write(
        "mix.exs",
        &mix_exs(&[r#"{:makeup, "== 1.1.2"}"#.to_owned()]),
    );
    fixture.run_tool("mix", &["deps.get"], &[]).expect_success();
    fixture.write("mix.exs", &manifest);
    fixture.write(
        "mix.lock",
        std::str::from_utf8(&lock_before).expect("a UTF-8 lock"),
    );

    let upgrade = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE, "--package", "makeup"],
        &[("HEX_OFFLINE", "1")],
    );

    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert_eq!(fixture.read_bytes("mix.lock"), lock_before);
}

/// A lock Mix cannot read in full is refused rather than relocked: Mix reads a `mix.lock` cut off
/// before its closing brace as empty, which would drop every entry cooldown judged it by.
#[test]
fn a_lock_mix_reads_differently_is_refused() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let lock = String::from_utf8(fixture.read_bytes("mix.lock")).expect("a UTF-8 lock");
    let truncated = lock
        .trim_end()
        .strip_suffix('}')
        .expect("a lock ends in a brace");
    fixture.write("mix.lock", truncated);
    let before = project_files(fixture.root());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);

    assert!(upgrade.summary_errors() > 0, "{upgrade:#?}");
    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert!(
        format!("{upgrade:?}").contains("reads other entries from mix.lock than Mix does"),
        "{upgrade:#?}"
    );
    assert_eq!(project_files(fixture.root()), before);
}

/// A relock never changes a locked dependency cooldown cannot judge: with a Git dependency's tag
/// moved in `mix.exs` but not yet in the lock, upgrading `makeup` would move its pin, so the
/// upgrade is held and the lock stays as it was.
#[test]
fn a_git_dependency_the_relock_would_change_holds_the_upgrade() {
    skip_if_missing!("mix");
    skip_if_missing!("git");
    let fixture = fixture(SEED_OLD);
    let git = |tag: &str| {
        format!(
            r#"{{:nimble_options, git: "https://github.com/dashbitco/nimble_options.git", tag: "{tag}"}}"#
        )
    };
    fixture.write("mix.exs", &mix_exs(&[MAKEUP.to_owned(), git("v1.1.0")]));
    fixture.run_tool("mix", &["deps.get"], &[]).expect_success();
    // The checkout carries the dependency's own `mix.lock`, which cooldown would take for a project
    // of its own.
    std::fs::remove_dir_all(fixture.root().join("deps").join("nimble_options"))
        .expect("remove the Git checkout");
    fixture.write("mix.exs", &mix_exs(&[MAKEUP.to_owned(), git("v1.1.1")]));
    let lock_before = fixture.read_bytes("mix.lock");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "makeup"]);

    assert!(upgrade.applied_names().is_empty(), "{upgrade:#?}");
    assert!(
        upgrade
            .skip_detail_for("makeup")
            .is_some_and(|detail| detail.contains("Mix would change nimble_options")),
        "{upgrade:#?}"
    );
    assert_eq!(fixture.read_bytes("mix.lock"), lock_before);
}

/// A preview reads an in-tree Rebar path dependency's requirements as the real relock does:
/// `local/pinned` pins `makeup` to 1.1.0 in its `rebar.config`, so the dry run holds the planned
/// `makeup` upgrade exactly as the real run does.
#[test]
fn a_preview_reads_an_in_tree_rebar_dependency_like_the_relock() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    fixture.write(
        "local/pinned/rebar.config",
        "{deps, [{makeup, \"1.1.0\"}]}.\n",
    );
    fixture.write(
        "mix.exs",
        &mix_exs(&[
            MAKEUP.to_owned(),
            r#"{:pinned, path: "local/pinned"}"#.to_owned(),
        ]),
    );
    let lock_before = fixture.read_bytes("mix.lock");

    let dry = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FREEZE,
        "--package",
        "makeup",
        "--dry-run",
    ]);
    let real = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "makeup"]);

    assert!(real.applied_names().is_empty(), "{real:#?}");
    assert!(
        real.held_conflict_names().contains("makeup"),
        "the real run must hold makeup\n{real:#?}"
    );
    assert_eq!(fixture.read_bytes("mix.lock"), lock_before);
    assert_eq!(
        dry.held_conflict_names(),
        real.held_conflict_names(),
        "the dry run must hold what the real run holds\ndry={dry:#?}\nreal={real:#?}"
    );
    assert_eq!(rows(&dry), rows(&real), "dry={dry:#?}\nreal={real:#?}");
}

/// The relock judges its scratch directory by where a path leads, not how it is spelled: with
/// the temporary directory named through `..`, the scratch paths cooldown hands Mix are spelled
/// that way too, and the upgrade still lands.
#[cfg(unix)]
#[test]
fn a_scratch_directory_spelled_through_dot_dot_is_still_the_relocks_own() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let temp = tempfile::tempdir().expect("create a temporary directory");
    std::fs::create_dir(temp.path().join("nested")).expect("create a nested directory");
    let spelled = temp.path().join("nested").join("..");

    let upgrade = fixture.cooldown_json_env(
        &["upgrade", "--freeze", FREEZE],
        &[("TMPDIR", spelled.to_str().expect("a UTF-8 temp dir"))],
    );

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(upgrade.applied_names().contains("makeup"), "{upgrade:#?}");
    assert_rows_landed_on_target(&upgrade, &fixture.read_bytes("mix.lock"));
}

/// An older public pin, which names its repository but ends before the outer checksum, is a hex.pm
/// release cooldown judges: the upgrade of `makeup` lands although Hex rewrites the unrelated
/// `nimble_parsec` pin with its outer checksum as it relocks.
#[test]
fn an_older_public_pin_does_not_hold_an_unrelated_upgrade() {
    skip_if_missing!("mix");
    let fixture = fixture(SEED_OLD);
    let lock = String::from_utf8(fixture.read_bytes("mix.lock")).expect("a UTF-8 lock");
    let older: Vec<String> = lock
        .lines()
        .map(|line| match line.strip_prefix(r#"  "nimble_parsec": "#) {
            // Drop the tuple's last element, the outer checksum.
            Some(_) => {
                let (pin, _) = line.rsplit_once(r#", ""#).expect("a quoted last element");
                format!("{pin}}},")
            }
            None => line.to_owned(),
        })
        .collect();
    let older = older.join("\n") + "\n";
    assert!(older.contains(r#""hexpm"},"#), "{older}");
    fixture.write("mix.lock", &older);
    let before = mix_lock_pins(older.as_bytes());

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "makeup"]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        BTreeSet::from(["makeup".to_owned()]),
        "{upgrade:#?}"
    );
    let after = fixture.read_bytes("mix.lock");
    assert_rows_landed_on_target(&upgrade, &after);
    assert_eq!(
        mix_lock_pins(&after).get("nimble_parsec"),
        before.get("nimble_parsec"),
        "the older pin keeps its version"
    );
}
