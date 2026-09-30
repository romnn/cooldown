//! End-to-end convergence tests that drive the REAL `cargo` resolver against fixtures generated on
//! the fly in temp dirs. These guard the cargo adapter's whole-graph re-resolve: the adapter
//! applies all of a project's planned `--precise` pins as one logical unit (one `cargo update` per
//! pin) and builds the report from the full before/after `Cargo.lock` diff, so a candidate can
//! never silently move another node and a converged graph re-applies to a byte-stable fixed point.
//!
//! # Determinism
//!
//! Unlike uv, cargo has **no** publish-date cutoff flag (no `--exclude-newer`), so the window cannot
//! be handed to cargo. cooldown realizes it out-of-band: the crates.io sparse index supplies each
//! version's immutable publish time, the core computes each crate's newest-within-window target, and
//! the adapter pins that as a concrete `cargo update --precise <version>`. Every test pins the
//! resolution clock with `--freeze <FREEZE>` (an absolute cutoff the core applies to the index
//! publish times), so the set of matured versions — and therefore the precise targets cooldown
//! computes — is reproducible from crates.io's immutable history. The starting lock is seeded with
//! the real `cargo` against live crates.io; most assertions check INVARIANTS (convergence,
//! no-silent-change, cross-command agreement). The focused `clap` regression below hard-pins
//! historical immutable crates.io versions to recreate a specific float-up/hold-back failure.
//!
//! # The conflict
//!
//! The fixture pins a shared transitive (`serde_derive`) to an exact `=` version via one direct dep
//! while another direct dep (`serde`) wants to move forward. Cargo coexists distinct majors but a
//! single-major `=`-pin caps the shared node: raising one side regresses the other. The whole-graph
//! batched re-resolve adopts the maximal consistent set under the freeze cutoff and reports every net
//! move — the held candidate names the crate whose `=`-pin blocks it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::{formatdoc, indoc};
use std::collections::BTreeSet;
use support::{ChangeVersions, Fixture, changed_packages, toml_lock_entries, toml_lock_pins};

/// The absolute resolution cutoff. crates.io's release history before this instant is immutable, so
/// the matured-version set — and the precise targets cooldown computes — reproduce forever.
const FREEZE: &str = "2024-06-01T00:00:00Z";

/// A later cutoff used only to seed a genuinely too-fresh starting lock for the `fix` test: deps
/// resolved against this newer instant are younger than `FREEZE`, so evaluating them under
/// `--freeze FREEZE` flags them as cooldown violations to mature down.
const FREEZE_LATER: &str = "2025-06-01T00:00:00Z";

/// The conflict fixture manifest. `serde` is a direct dep free to move forward within 1.x; the
/// dummy crate `cd-pin` re-exports an exact `=` pin on `serde_derive` (serde's proc-macro sibling),
/// so the shared `serde_derive` node is capped and raising `serde` would regress it — the
/// mutual-exclusion path. `log` is a loose-floor direct dep that gives `fix` an older matured target
/// to roll back to.
const ROOT_MANIFEST: &str = r#"[workspace]
members = ["crates/app", "crates/cd-pin"]
resolver = "2"
"#;

const APP_MANIFEST: &str = r#"[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
log = "0.4"
cd-pin = { path = "../cd-pin" }
"#;

/// A tiny in-workspace crate that imposes an exact `=` pin on `serde_derive`, the shared transitive
/// that both it and `serde`'s `derive` feature pull in. The pin caps the shared single-major node so
/// the resolver cannot freely raise it — reproducing the ping-pong the lock-diff report guards.
const PIN_MANIFEST: &str = r#"[package]
name = "cd-pin"
version = "0.1.0"
edition = "2021"

[dependencies]
serde_derive = "=1.0.180"
"#;

/// Seed a `Cargo.lock` by resolving the fixture with the real cargo against live crates.io. cargo has
/// no publish-date flag, so the seed is at "newest now"; cooldown then re-resolves to the
/// freeze-bounded targets it computes from the index. `cutoff` is unused by cargo itself — it only
/// affects how fresh the seed is relative to a later freeze (the `fix` test seeds against a window
/// where deps are too-fresh), which we approximate by seeding identically and letting the freeze
/// classify them.
fn seed_lock(fixture: &Fixture) {
    fixture
        .write("Cargo.toml", ROOT_MANIFEST)
        .write("crates/app/Cargo.toml", APP_MANIFEST)
        .write("crates/app/src/lib.rs", "")
        .write("crates/cd-pin/Cargo.toml", PIN_MANIFEST)
        .write("crates/cd-pin/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
}

fn conflict_fixture() -> Fixture {
    let fixture = Fixture::new();
    seed_lock(&fixture);
    fixture
}

const FLOATED_TRANSITIVE_FREEZE: &str = "2026-06-20T00:00:00Z";

const FLOATED_TRANSITIVE_MANIFEST: &str = r#"[package]
name = "cargo-floated-transitive"
version = "0.1.0"
edition = "2021"

[dependencies]
clap = { version = "4", features = ["derive"] }
"#;

fn floated_transitive_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", FLOATED_TRANSITIVE_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture
        .run_tool(
            "cargo",
            &["update", "-p", "clap", "--precise", "4.5.55"],
            &[],
        )
        .expect_success();
    fixture
        .run_tool(
            "cargo",
            &["update", "-p", "quote", "--precise", "1.0.44"],
            &[],
        )
        .expect_success();
    fixture
}

/// The cutoff admits the paired `jsonschema` / `referencing` 0.46.6 releases from 2026-06-23,
/// while excluding their paired 0.46.7 releases from 2026-06-30.
const INVALIDATED_PIN_FREEZE: &str = "2026-06-29T00:00:00Z";

const INVALIDATED_PIN_SEED_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-invalidated-pin"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    jsonschema = { version = "=0.46.5", default-features = false }
    referencing = "=0.46.5"
"#};

const INVALIDATED_PIN_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-invalidated-pin"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    jsonschema = { version = "0.46", default-features = false }
    referencing = "0.46"
"#};

fn invalidated_pin_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", INVALIDATED_PIN_SEED_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    // Widening the exact seed requirements keeps the lock current while making both packages
    // eligible direct upgrade candidates.
    fixture.write("Cargo.toml", INVALIDATED_PIN_MANIFEST);
    fixture
}

#[test]
fn upgrade_stabilizes_a_planned_pin_moved_by_an_earlier_pin() {
    skip_if_missing!("cargo");
    let fixture = invalidated_pin_fixture();
    let seeded = fixture.read_bytes("Cargo.lock");
    for crate_name in ["jsonschema", "referencing"] {
        assert_eq!(
            cargo_lock_versions_of(crate_name, &seeded),
            vec!["0.46.5".to_owned()],
            "the fixture must seed {crate_name} at the shared source version"
        );
    }

    // A single fetch at a time preserves the sorted jsonschema-before-referencing plan order. The
    // jsonschema pin makes Cargo float referencing before its own planned pin is attempted.
    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--strict",
        "--freeze",
        INVALIDATED_PIN_FREEZE,
        "--concurrency",
        "1",
        "--package",
        "jsonschema",
        "--package",
        "referencing",
    ]);

    assert_eq!(
        upgrade.changes_for("jsonschema"),
        vec![ChangeVersions::new("0.46.5", "0.46.6")],
        "jsonschema must have one baseline-to-final report row"
    );
    assert_eq!(
        upgrade.changes_for("referencing"),
        vec![ChangeVersions::new("0.46.5", "0.46.6")],
        "referencing must not retain a false skip or its transient 0.46.10 position"
    );
    assert_eq!(
        upgrade.applied_names(),
        ["jsonschema".to_owned(), "referencing".to_owned()]
            .into_iter()
            .collect(),
        "both planned targets must be applied"
    );
    assert!(
        upgrade.downgraded_names().is_empty(),
        "both baseline-to-final changes are upgrades"
    );
    assert!(
        upgrade.skipped_reasons_for("referencing").is_empty(),
        "a target present in the final graph is not a resolver conflict"
    );
    assert_eq!(upgrade.summary_applied(), 2);
    assert_eq!(upgrade.summary_skipped(), 0);
    assert_eq!(upgrade.summary_errors(), 0);
    assert!(
        upgrade.ok(),
        "--strict must succeed when every planned target reached the final graph"
    );

    let lock_after = fixture.read_bytes("Cargo.lock");
    for crate_name in ["jsonschema", "referencing"] {
        assert_eq!(
            cargo_lock_versions_of(crate_name, &lock_after),
            vec!["0.46.6".to_owned()],
            "{crate_name} must land exactly on the newest release admitted by the freeze"
        );
    }

    // The stabilized result is a fixed point, not a report-only correction.
    let second = fixture.cooldown_json(&[
        "upgrade",
        "--strict",
        "--freeze",
        INVALIDATED_PIN_FREEZE,
        "--package",
        "jsonschema",
        "--package",
        "referencing",
    ]);
    assert!(second.ok(), "the fixed-point strict run must succeed");
    assert_eq!(second.summary_applied(), 0);
    assert_eq!(second.summary_skipped(), 0);
    assert_eq!(lock_after, fixture.read_bytes("Cargo.lock"));
}

#[test]
fn upgrade_converges_to_a_fixed_point() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();

    // First upgrade: cooldown re-resolves the whole graph under the freeze cutoff, applying every
    // planned precise pin in one batched pass and reporting the full lock diff.
    let first = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        first.ok(),
        "first upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );
    assert_eq!(
        first.lock_status(),
        Some("current"),
        "first upgrade re-locks"
    );
    let lock_after_first = fixture.read_bytes("Cargo.lock");

    // Second upgrade: already at the fixed point, so nothing moves and the lock is byte-identical.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point)"
    );
    let lock_after_second = fixture.read_bytes("Cargo.lock");
    assert_eq!(
        lock_after_first, lock_after_second,
        "lock must be byte-identical across the two converged runs"
    );
}

#[test]
fn upgrade_reports_every_moved_version_no_silent_change() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();

    let lock_before = fixture.read_bytes("Cargo.lock");
    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    let lock_after = fixture.read_bytes("Cargo.lock");

    // The set of crates whose pinned version changed in the lock, computed independently of the
    // report, must equal the report's applied set — every collateral move surfaced, never silent.
    let moved_in_lock = changed_packages(&lock_before, &lock_after, toml_lock_pins);
    let reported = report.applied_names();
    assert_eq!(
        reported, moved_in_lock,
        "report set must equal the lock-diff set (no silent change)\nreported={reported:?}\nlock-diff={moved_in_lock:?}"
    );
}

/// The cutoff admits `rand_core` 0.6.4 (2022-09-15) as matured, while the exact-pinned parent
/// `rand 0.8.5` (2022-02-14) keeps the direct layer inert.
const TRANSITIVE_ADVANCE_FREEZE: &str = "2023-01-01T00:00:00Z";

const TRANSITIVE_ADVANCE_MANIFEST: &str = r#"[package]
name = "cargo-transitive-advance"
version = "0.1.0"
edition = "2021"

[dependencies]
rand = "=0.8.5"
"#;

fn transitive_advance_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", TRANSITIVE_ADVANCE_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    // Push the undeclared transitive behind its line's newest: rand_core 0.6.3 (2021-06-15), with
    // 0.6.4 matured under the freeze. The exact-pinned parent never plans, so nothing drags the
    // line — the standalone shape only graph-wide transitive advance can move.
    fixture
        .run_tool(
            "cargo",
            &["update", "-p", "rand_core", "--precise", "0.6.3"],
            &[],
        )
        .expect_success();
    fixture
}

#[test]
fn upgrade_advances_a_matured_transitive_no_direct_pin_drags() {
    skip_if_missing!("cargo");
    let fixture = transitive_advance_fixture();
    let lock_before = fixture.read_bytes("Cargo.lock");
    assert_eq!(
        cargo_lock_versions_of("rand_core", &lock_before),
        vec!["0.6.3".to_owned()],
        "the seed must hold the behind transitive"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", TRANSITIVE_ADVANCE_FREEZE]);
    assert!(upgrade.ok(), "upgrade should succeed");
    assert!(
        upgrade.applied_names().contains("rand_core"),
        "the transitive advance must be its own applied row, got {:?}",
        upgrade.applied_names()
    );
    assert!(
        !upgrade.applied_names().contains("rand"),
        "the exact-pinned parent must not move"
    );
    let lock_after = fixture.read_bytes("Cargo.lock");
    assert_eq!(
        cargo_lock_versions_of("rand_core", &lock_after),
        vec!["0.6.4".to_owned()],
        "rand_core advances to the newest release matured under the freeze"
    );
    assert_eq!(
        cargo_lock_versions_of("rand", &lock_after),
        vec!["0.8.5".to_owned()],
        "the exact pin stays"
    );

    // Converged: a second run under the same freeze plans nothing new for the line.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", TRANSITIVE_ADVANCE_FREEZE]);
    assert!(second.ok(), "converged re-run should succeed");
    assert!(
        !second.applied_names().contains("rand_core"),
        "a converged line re-applies to a fixed point, got {:?}",
        second.applied_names()
    );
}

#[test]
fn upgrade_holds_back_a_cargo_floated_transitive_instead_of_skipping() {
    skip_if_missing!("cargo");
    let fixture = floated_transitive_fixture();
    let lock_before = fixture.read_bytes("Cargo.lock");

    assert_eq!(
        cargo_lock_versions_of("clap", &lock_before),
        vec!["4.5.55".to_owned()],
        "fixture must start with a cooled clap that can move forward"
    );
    assert_eq!(
        cargo_lock_versions_of("quote", &lock_before),
        vec!["1.0.44".to_owned()],
        "fixture must start with the older cooled transitive from the regression"
    );

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--freeze",
        FLOATED_TRANSITIVE_FREEZE,
        "--package",
        "clap",
    ]);
    assert!(
        upgrade.ok(),
        "upgrade should adopt clap and reconcile quote instead of rolling back"
    );
    assert!(
        upgrade.applied_names().contains("clap"),
        "clap should be applied, got {:?}",
        upgrade.applied_names()
    );
    assert!(
        upgrade.applied_names().contains("quote"),
        "quote's held-back net move should be reported, got {:?}",
        upgrade.applied_names()
    );
    assert!(
        !upgrade.skipped_reasons().contains("transitive_in_cooldown"),
        "a reducible floated transitive must not skip the batch: {:?}",
        upgrade.skipped_reasons()
    );

    let lock_after = fixture.read_bytes("Cargo.lock");
    assert_eq!(
        cargo_lock_versions_of("clap", &lock_after),
        vec!["4.6.1".to_owned()],
        "freeze cutoff admits clap 4.6.1 as the newest mature compatible release"
    );
    assert_eq!(
        cargo_lock_versions_of("quote", &lock_after),
        vec!["1.0.45".to_owned()],
        "cargo floats quote to latest semver-compatible, but cooldown must hold it at the newest mature version"
    );
}

#[test]
fn outdated_agrees_with_upgrade() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();

    // Converge first so `outdated` and `upgrade` describe the same stable state.
    fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive"]);
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let held = upgrade.held_conflict_names();

    // Everything `upgrade` reports held, `outdated` must mark blocked. `outdated --transitive` can
    // additionally mark statically-blocked candidates (a declared bound, an exact pin) that
    // `upgrade` — even with graph-wide transitive advance — never plans, so `blocked` is a
    // superset, not a strict equal.
    assert!(
        held.is_subset(&blocked),
        "every held candidate must be blocked by outdated\nheld={held:?}\nblocked={blocked:?}"
    );
    assert!(
        adoptable.is_disjoint(&held),
        "nothing outdated calls adoptable may be held by upgrade\nadoptable={adoptable:?}\nheld={held:?}"
    );
}

#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("cargo");

    // Real upgrade converges one fixture.
    let real_fixture = conflict_fixture();
    real_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let real_held = real.held_conflict_names();

    // Dry-run on a separate converged fixture: the held set must match and the lock is untouched.
    let dry_fixture = conflict_fixture();
    dry_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let lock_before = dry_fixture.read_bytes("Cargo.lock");
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let dry_held = dry.held_conflict_names();
    let lock_after = dry_fixture.read_bytes("Cargo.lock");

    assert_eq!(
        real_held, dry_held,
        "dry-run held set must equal the real upgrade held set\nreal={real_held:?}\ndry={dry_held:?}"
    );
    assert_eq!(
        lock_before, lock_after,
        "--dry-run must leave the lock byte-identical"
    );
    assert_eq!(
        dry.lock_status(),
        None,
        "--dry-run never re-locks, so lockStatus is null"
    );
}

/// The mutating commands scope their rows against a staged copy of the project, which keeps the
/// project's location, so `-C <member> upgrade` still plans that member's rows instead of
/// dropping every attributed row and reporting nothing to do.
#[test]
fn selected_upgrade_dry_run_plans_the_selected_members_rows() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();
    let root = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let scoped = fixture.cooldown_json_in(
        Some("crates/app"),
        &["upgrade", "--freeze", FREEZE, "--dry-run"],
    );
    assert!(scoped.ok(), "{:?}", scoped.error_messages());
    let scoped_names = scoped.item_names();
    assert!(
        !scoped_names.is_empty(),
        "the selected member's rows are planned, not dropped"
    );
    assert!(
        scoped_names.is_subset(&root.item_names()),
        "{scoped_names:?} is not within the root plan {:?}",
        root.item_names()
    );
}

#[test]
fn upgrade_skip_only_batch_rolls_back_manifest_and_lock() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();

    let root_manifest_before = fixture.read_bytes("Cargo.toml");
    let app_manifest_before = fixture.read_bytes("crates/app/Cargo.toml");
    let lock_before = fixture.read_bytes("Cargo.lock");

    // `serde` wants to move forward under the freeze cutoff, but `cd-pin`'s exact serde_derive pin
    // blocks that target. This is a skip-only apply batch: no claimed applied change means cooldown
    // must roll back the temporary manifest widening before any post-apply graph verification.
    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--package", "serde"]);
    assert!(
        upgrade.ok(),
        "skip-only upgrade should be a successful no-op: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE, "--package", "serde"])
            .stderr_str()
    );
    assert_eq!(
        upgrade.summary_applied(),
        0,
        "blocked serde target must not be reported applied"
    );
    assert_eq!(
        upgrade.summary_errors(),
        0,
        "a resolver-held target is a skip, not an environment error"
    );
    assert!(
        upgrade.held_conflict_names().contains("serde"),
        "serde must be reported as held by the resolver\nheld={:?}",
        upgrade.held_conflict_names()
    );
    assert_eq!(
        root_manifest_before,
        fixture.read_bytes("Cargo.toml"),
        "root manifest must be restored after the skip-only trial"
    );
    assert_eq!(
        app_manifest_before,
        fixture.read_bytes("crates/app/Cargo.toml"),
        "member manifest must be restored after the skip-only trial"
    );
    assert_eq!(
        lock_before,
        fixture.read_bytes("Cargo.lock"),
        "lock must be restored after the skip-only trial"
    );
}

#[test]
fn check_fails_closed_on_stale_lock_unless_allowed() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();

    fixture.write(
        "crates/app/Cargo.toml",
        &format!("{APP_MANIFEST}regex = \"1\"\n"),
    );
    let stale_lock = fixture.read_bytes("Cargo.lock");

    let stale = fixture.cooldown_json(&["check", "--latest", "--package", "log"]);
    assert!(!stale.ok(), "stale lock must fail closed by default");
    assert_eq!(stale.summary_errors(), 1);
    assert!(
        stale.error_kinds().contains("stale_lock"),
        "expected a stale_lock diagnostic, got {:?}: {:?}",
        stale.error_kinds(),
        stale.error_messages()
    );
    // A repository can hold several Cargo locks, so the error must say which project's lock is
    // stale rather than only that one is.
    let root_name = fixture
        .root()
        .file_name()
        .and_then(|name| name.to_str())
        .expect("the fixture root has a name");
    assert!(
        stale
            .error_messages()
            .iter()
            .any(|message| message.contains("Cargo.lock is stale in ")
                && message.contains(root_name)),
        "the stale-lock error must name the project whose lock is stale, got {:?}",
        stale.error_messages()
    );

    let allowed = fixture.cooldown_json(&[
        "check",
        "--latest",
        "--allow-stale-lock",
        "--package",
        "log",
    ]);
    assert!(
        allowed.ok(),
        "--allow-stale-lock should downgrade the stale lock to a warning"
    );
    assert_eq!(allowed.summary_errors(), 0);
    assert!(
        allowed.warning_kinds().contains("stale_lock"),
        "expected a stale_lock warning, got {:?}",
        allowed.warning_kinds()
    );
    assert!(
        allowed
            .warning_paths()
            .iter()
            .any(|path| path.ends_with("Cargo.toml")),
        "stale-lock warning should name the manifest path, got {:?}",
        allowed.warning_paths()
    );
    assert_eq!(stale_lock, fixture.read_bytes("Cargo.lock"));

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--allow-stale-lock"]);
    assert!(
        outdated.ok(),
        "outdated remains informational on a stale lock"
    );
    assert!(outdated.warning_kinds().contains("stale_lock"));
    assert_eq!(stale_lock, fixture.read_bytes("Cargo.lock"));

    let explain = fixture.cooldown(&["explain", "log"]);
    assert!(
        !explain.status.success(),
        "explain must fail closed on a stale lock"
    );
    assert_eq!(stale_lock, fixture.read_bytes("Cargo.lock"));
}

/// The shape that surfaced the lockless gap: a workspace with a lock beside a cargo-fuzz-style
/// crate that declares its own `[workspace]` — so the root resolve can never cover it — and whose
/// `Cargo.lock` is gitignored, hence absent from a fresh checkout.
fn lockless_nested_workspace_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["member"]
                resolver = "2"
            "#},
        )
        .write(
            "member/Cargo.toml",
            indoc! {r#"
                [package]
                name = "member"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("member/src/lib.rs", "")
        .write(
            "fuzz/Cargo.toml",
            indoc! {r#"
                [package]
                name = "fuzzish"
                version = "0.1.0"
                edition = "2021"

                [workspace]

                [dependencies]
                ryu = "1"
            "#},
        )
        .write("fuzz/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    // Hold the root's own dependency at an immutable historical release, so the root project has a
    // real upgrade to plan while the lockless nested project is being reported or skipped.
    fixture
        .run_tool(
            "cargo",
            &["update", "-p", "itoa", "--precise", LOCKLESS_ROOT_PIN],
            &[],
        )
        .expect_success();
    fixture
}

/// An old, immutable `itoa` release: the starting pin the lockless-nested fixture upgrades from.
const LOCKLESS_ROOT_PIN: &str = "1.0.9";

/// Assert that `envelope` carries a `stale_lock` error attributed to `project`, saying the lock is
/// missing and giving the remedy that follows generating one.
///
/// Project identity is asserted through the diagnostic's structured `project` field, which is
/// run-relative and `/`-separated everywhere; the message quotes the project's native absolute
/// path, which spells its separators differently on Windows.
fn assert_missing_lock_error(envelope: &support::Envelope, project: &str) {
    assert!(
        envelope.error_kinds().contains("stale_lock"),
        "expected a stale_lock error, got {:?}: {:?}",
        envelope.error_kinds(),
        envelope.error_messages()
    );
    assert!(
        envelope.error_projects().contains(project),
        "the stale_lock error must be attributed to {project}, got {:?}",
        envelope.error_projects()
    );
    assert!(
        envelope.error_messages().iter().any(|message| {
            message.contains("Cargo.lock is missing in") && message.contains("cooldown fix")
        }),
        "the error must say the lock is missing and give the remedy that follows generating one, \
         got {:?}",
        envelope.error_messages()
    );
}

/// A nested `[workspace]` crate with no lock is a project of its own, and one cooldown cannot
/// evaluate: it fails the gate naming itself rather than passing silently while its dependencies
/// resolve to whatever is newest at build time.
#[test]
fn a_lockless_nested_workspace_fails_the_gate_instead_of_passing_silently() {
    skip_if_missing!("cargo");
    let fixture = lockless_nested_workspace_fixture();

    let gated = fixture.cooldown(&["check", "--cargo", "--latest"]);
    assert_eq!(
        gated.status.code(),
        Some(4),
        "a project cooldown could not evaluate must fail the gate: {}",
        gated.stderr_str()
    );
    let checked = fixture.cooldown_json(&["check", "--cargo", "--latest"]);
    assert!(!checked.ok());
    assert_missing_lock_error(&checked, "fuzz");

    // `outdated --all` names everything in scope, so it shows both that the root was evaluated and
    // that the fuzz project was not silently dropped from the run.
    let listed = fixture.cooldown_json(&["outdated", "--cargo", "--all", "--freeze", FREEZE]);
    assert_eq!(
        listed.item_names(),
        ["itoa".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "the root workspace's dependency is still reported"
    );
    assert_missing_lock_error(&listed, "fuzz");

    // The documented downgrade applies to an absent lock exactly as to a stale one, and says the
    // project went unevaluated rather than leaving the reader to infer it from a clean summary.
    let allowed = fixture.cooldown_json(&["check", "--cargo", "--latest", "--allow-stale-lock"]);
    assert!(
        allowed.ok(),
        "--allow-stale-lock downgrades the missing lock: {:?}",
        allowed.error_messages()
    );
    assert!(allowed.warning_kinds().contains("stale_lock"));
    assert!(
        allowed
            .warning_messages()
            .iter()
            .any(|message| message.contains("dependency evaluation was skipped")),
        "the warning must say the project went unevaluated, got {:?}",
        allowed.warning_messages()
    );

    // `--lock` is the way forward: it generates the missing lock and the gate then reads it.
    let locked = fixture.cooldown_json(&["check", "--cargo", "--latest", "--lock"]);
    assert!(
        locked.ok(),
        "check --lock must generate the missing lock and gate it: {:?} {:?}",
        locked.error_kinds(),
        locked.error_messages()
    );
    assert!(
        fixture.root().join("fuzz/Cargo.lock").is_file(),
        "--lock generates the nested project's lock"
    );
    assert_eq!(
        locked.summary_checked(),
        2,
        "both projects' dependencies are gated once the nested lock exists"
    );
}

/// The other half of cargo's ownership rule: a plain package the enclosing workspace `exclude`s is
/// no more covered by the root resolve than a nested `[workspace]` is.
#[test]
fn a_lockless_excluded_package_fails_the_gate() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["member"]
                exclude = ["tools/x"]
                resolver = "2"
            "#},
        )
        .write(
            "member/Cargo.toml",
            indoc! {r#"
                [package]
                name = "member"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("member/src/lib.rs", "")
        .write(
            "tools/x/Cargo.toml",
            indoc! {r#"
                [package]
                name = "toolx"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                ryu = "1"
            "#},
        )
        .write("tools/x/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();

    let checked = fixture.cooldown_json(&["check", "--cargo", "--latest"]);
    assert!(!checked.ok());
    assert_missing_lock_error(&checked, "tools/x");
}
/// A repository-root package beside a `bridge/` that points at a sibling `owner/` workspace, which
/// lists both `bridge` and `bridge/child` as members from outside its own directory.
fn sibling_owner_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("src/lib.rs", "")
        .write(
            "owner/Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["../bridge", "../bridge/child"]
                resolver = "2"
            "#},
        )
        .write(
            "bridge/Cargo.toml",
            indoc! {r#"
                [package]
                name = "bridge"
                version = "0.1.0"
                edition = "2021"
                workspace = "../owner"

                [dependencies]
                ryu = "1"
            "#},
        )
        .write("bridge/src/lib.rs", "")
        .write(
            "bridge/child/Cargo.toml",
            indoc! {r#"
                [package]
                name = "bridge-child"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                ryu = "1"
            "#},
        )
        .write("bridge/child/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture
        .run_tool(
            "cargo",
            &["generate-lockfile", "--manifest-path", "owner/Cargo.toml"],
            &[],
        )
        .expect_success();
    fixture
}

/// Cargo's root search stops at an ancestor carrying `package.workspace` and hands everything
/// below it to the workspace that ancestor points at — even a sibling tree the upward walk would
/// never otherwise reach. `bridge/child` is a member of `owner` through `bridge`'s pointer, so it is
/// covered by `owner`'s lock and must not be detected (and fail) as a lockless project of its own.
#[test]
fn a_pointer_carrying_ancestor_hands_its_subtree_to_the_named_workspace() {
    skip_if_missing!("cargo");
    let fixture = sibling_owner_fixture();

    let listed = fixture.cooldown_json(&["outdated", "--cargo", "--all", "--freeze", FREEZE]);

    assert!(
        listed.error_kinds().is_empty(),
        "nothing here is unevaluated: {:?}",
        listed.error_messages()
    );
    // One `ryu` row, attributed to the workspace that resolves both members. A second row (or a
    // `stale_lock`) would mean `bridge/child` was detected as a lockless project of its own.
    assert_eq!(listed.item_projects_for("ryu"), vec!["owner".to_string()]);
    assert_eq!(
        listed.item_names(),
        ["itoa".to_string(), "ryu".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>()
    );
}

/// Pointing the run at `bridge` runs the project that resolves it — `owner`, whose root is a
/// sibling — not the repository-root package that merely contains the directory, whose lock says
/// nothing about it. The report is scoped to what the selected directory declares, and the lock
/// the gate reads is `owner`'s even though it lives outside the selection.
#[test]
fn selecting_a_directory_another_project_resolves_runs_that_project() {
    skip_if_missing!("cargo");
    let fixture = sibling_owner_fixture();

    let scoped = fixture.cooldown_json_in(
        Some("bridge"),
        &["outdated", "--cargo", "--all", "--freeze", FREEZE],
    );
    assert!(
        scoped.error_kinds().is_empty(),
        "{:?}",
        scoped.error_messages()
    );
    assert_eq!(
        scoped.item_names(),
        ["ryu".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "only what the selected member declares, not the enclosing package's own dependency"
    );
    assert_eq!(
        scoped.item_projects_for("ryu"),
        vec!["owner".to_string()],
        "reported through the project that resolves the selection"
    );

    std::fs::remove_file(fixture.root().join("owner/Cargo.lock")).expect("remove owner lock");
    let gated = fixture.cooldown_json_in(Some("bridge"), &["check", "--cargo", "--latest"]);
    assert!(!gated.ok(), "the resolving project has no lock to read");
    assert!(
        gated.error_kinds().contains("stale_lock"),
        "{:?}",
        gated.error_kinds()
    );
    assert_eq!(
        gated.error_projects(),
        ["owner".to_string()].into_iter().collect::<BTreeSet<_>>(),
        "the missing lock is reported against the project that owns it"
    );
}

/// A workspace that excludes two siblings, the later-sorting of which (`b`) path-depends on the
/// earlier (`a`) and holds the lock that resolves it.
fn later_sibling_resolver_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["member"]
                exclude = ["a", "b"]
                resolver = "2"
            "#},
        )
        .write(
            "member/Cargo.toml",
            indoc! {r#"
                [package]
                name = "member"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("member/src/lib.rs", "")
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                cfg-if = "1"
            "#},
        )
        .write("a/src/lib.rs", "")
        .write(
            "b/Cargo.toml",
            indoc! {r#"
                [package]
                name = "b"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                a = { path = "../a" }
                ryu = "1"
            "#},
        )
        .write("b/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture
        .run_tool(
            "cargo",
            &["generate-lockfile", "--manifest-path", "b/Cargo.toml"],
            &[],
        )
        .expect_success();
    fixture
}

/// Which of two plain directories is the project must not depend on which one is visited first.
/// Here `b` resolves `a`, so `b` is the project and `a` is in its lock — the order that previously
/// misfired, since `a` sorts first and used to settle before `b` existed as a project.
#[test]
fn a_directory_resolved_by_a_later_sorting_sibling_is_not_a_project() {
    skip_if_missing!("cargo");
    let fixture = later_sibling_resolver_fixture();

    // Cargo's own lock is the oracle: `b` resolved `a`, and `a`'s dependency with it.
    let nested_lock = toml_lock_pins(&fixture.read_bytes("b/Cargo.lock"));
    assert!(
        nested_lock.contains_key("cfg-if"),
        "`a`'s dependency is in `b`'s lock: {nested_lock:?}"
    );

    let listed = fixture.cooldown_json(&["outdated", "--cargo", "--all", "--freeze", FREEZE]);
    assert!(
        listed.error_kinds().is_empty(),
        "`a` is in `b`'s lock, so nothing is unevaluated: {:?}",
        listed.error_messages()
    );
    assert_eq!(
        listed.item_names(),
        ["itoa".to_string(), "ryu".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "the two projects' direct dependencies, and no row for a third project"
    );

    // The gate reads the whole graph, so `a`'s own dependency is evaluated through `b`: three
    // crates, not the two a run that dropped `a` would see.
    let checked = fixture.cooldown_json(&["check", "--cargo", "--latest"]);
    assert!(checked.ok(), "{:?}", checked.error_messages());
    assert_eq!(
        checked.summary_checked(),
        3,
        "the dependee's dependency is gated through the project that resolves it"
    );

    // Take the path dependency away and `a` really is a project of its own, with no lock — the
    // contrast that shows the first result was coverage rather than a silent drop.
    fixture.write(
        "b/Cargo.toml",
        indoc! {r#"
            [package]
            name = "b"
            version = "0.1.0"
            edition = "2021"

            [dependencies]
            ryu = "1"
        "#},
    );
    fixture
        .run_tool(
            "cargo",
            &["generate-lockfile", "--manifest-path", "b/Cargo.toml"],
            &[],
        )
        .expect_success();
    let orphaned = fixture.cooldown_json(&["check", "--cargo", "--latest"]);
    assert!(!orphaned.ok());
    assert_missing_lock_error(&orphaned, "a");
}

/// Cargo activates a package it reached as an ordinary dependency with only the features its
/// requirer asked for, so an optional path dependency no feature turns on is never resolved — its
/// own dependencies are in nobody's lock, and it is a project of its own.
#[test]
fn an_optional_dependency_of_a_reached_package_is_its_own_project() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                a = { path = "a" }
                itoa = "1"
            "#},
        )
        .write("src/lib.rs", "")
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                opt = { path = "opt", optional = true }
            "#},
        )
        .write("a/src/lib.rs", "")
        .write(
            "a/opt/Cargo.toml",
            indoc! {r#"
                [package]
                name = "opt"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                ryu = "1"
            "#},
        )
        .write("a/opt/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();

    // Cargo's own lock is the oracle: it never resolved the optional crate, so `ryu` is absent.
    let locked = toml_lock_pins(&fixture.read_bytes("Cargo.lock"));
    assert!(
        !locked.contains_key("ryu"),
        "the optional path dependency is not in the root's lock: {locked:?}"
    );

    let checked = fixture.cooldown_json(&["check", "--cargo", "--latest"]);

    assert!(!checked.ok());
    assert_missing_lock_error(&checked, "a/opt");
}

/// Every member-discovery shape at once: a `./a` spelling, an inherited `workspace = true` path
/// dependency, and a nested workspace whose member lives outside it and points back at it.
/// The member-discovery root manifest. `EXCLUDED_DIR` stands in for the fixture's own absolute
/// path, which only exists once the temp directory does.
const MEMBER_DISCOVERY_ROOT: &str = indoc! {r#"
    [package]
    name = "root"
    version = "0.1.0"
    edition = "2021"

    [workspace]
    members = ["./a"]
    exclude = [EXCLUDED_DIR]

    [workspace.dependencies]
    inherited = { path = "inherited" }

    [dependencies]
    itoa = "1"
"#};

/// The nested workspace whose member `b` lives beside it and points back at it, with `b`'s own
/// dev-dependency subtree below: cargo admits a path dependency outside the root directory when
/// the package's root search lands here, and consults pointers on ancestors while searching.
fn write_outside_pointer_member(fixture: &Fixture) -> &Fixture {
    fixture
        // A nested workspace whose member `b` lives beside it and points back at it: cargo admits a
        // path dependency outside the root directory when the package's own root search lands here.
        .write(
            "ws/Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["a"]
                resolver = "2"
            "#},
        )
        .write(
            "ws/a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "ws-a"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                b = { path = "../../b" }
            "#},
        )
        .write("ws/a/src/lib.rs", "")
        .write(
            "b/Cargo.toml",
            indoc! {r#"
                [package]
                name = "b"
                version = "0.1.0"
                edition = "2021"
                workspace = "../ws"

                [dev-dependencies]
                fixture = { path = "fixture" }
            "#},
        )
        .write("b/src/lib.rs", "")
        .write(
            "b/fixture/Cargo.toml",
            indoc! {r#"
                [package]
                name = "fixture"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                cfg-if = "1"

                [dev-dependencies]
                helper = { path = "helper" }
            "#},
        )
        .write("b/fixture/src/lib.rs", "")
        // Two levels below the pointing directory: cargo's root search consults the pointer on the
        // ancestor `b`, so this is a member of `ws` too, and its dependency is in `ws`'s lock.
        .write(
            "b/fixture/helper/Cargo.toml",
            indoc! {r#"
                [package]
                name = "helper"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                log = "0.4"
            "#},
        )
        .write("b/fixture/helper/src/lib.rs", "");
    fixture
}

fn member_discovery_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("src/lib.rs", "")
        .write(
            "a/Cargo.toml",
            indoc! {r#"
                [package]
                name = "a"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                inherited = { workspace = true }
            "#},
        )
        .write("a/src/lib.rs", "")
        .write(
            "inherited/Cargo.toml",
            indoc! {r#"
                [package]
                name = "inherited"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                ryu = "1"
            "#},
        )
        .write("inherited/src/lib.rs", "")
        // An absolute `exclude` entry, which cargo honours: the crate is no member, has no lock of
        // its own, and must be reported rather than quietly folded into the workspace.
        .write(
            "excluded/Cargo.toml",
            indoc! {r#"
                [package]
                name = "excluded"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                bytes = "1"
            "#},
        )
        .write("excluded/src/lib.rs", "");
    write_outside_pointer_member(&fixture);
    // The root manifest names the excluded crate by absolute path — canonicalized, because that is
    // the spelling the run scans under and therefore the one cargo's prefix test has to match.
    // Serialized rather than interpolated: a Windows temp path's backslashes are escapes inside a
    // TOML string.
    let excluded = serde_json::to_string(
        std::fs::canonicalize(fixture.root())
            .expect("canonical fixture root")
            .join("excluded")
            .to_str()
            .expect("utf-8 fixture root"),
    )
    .expect("serialize the excluded path");
    fixture.write(
        "Cargo.toml",
        &MEMBER_DISCOVERY_ROOT.replace("EXCLUDED_DIR", &excluded),
    );
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture
        .run_tool(
            "cargo",
            &["generate-lockfile", "--manifest-path", "ws/Cargo.toml"],
            &[],
        )
        .expect_success();
    fixture
}

/// Three member-discovery shapes cargo accepts that a literal reading of the manifest would miss:
/// a `./a` member spelling, an inherited `workspace = true` path dependency, and a member outside
/// the workspace directory that points back at it — whose dev path dependency cargo resolves too,
/// since a member's dev units are part of the lock. Every crate here belongs to one of the two
/// locks, so none of them is a project of its own.
#[test]
fn member_discovery_follows_cargos_own_rules() {
    skip_if_missing!("cargo");
    let fixture = member_discovery_fixture();

    // Cargo's own locks are the oracle for what each project resolves.
    let root_lock = toml_lock_pins(&fixture.read_bytes("Cargo.lock"));
    assert!(
        root_lock.contains_key("ryu"),
        "the `./a` member's inherited path dependency is in the root's lock: {root_lock:?}"
    );
    let nested_lock = toml_lock_pins(&fixture.read_bytes("ws/Cargo.lock"));
    assert!(
        nested_lock.contains_key("cfg-if") && nested_lock.contains_key("log"),
        "the outside member's whole subtree is in the nested lock: {nested_lock:?}"
    );

    let listed = fixture.cooldown_json(&["outdated", "--cargo", "--all", "--freeze", FREEZE]);

    assert_eq!(
        listed.item_projects_for("ryu"),
        vec![".".to_string()],
        "reached through the `./a` member's inherited `workspace = true` entry"
    );
    assert_eq!(
        listed.item_projects_for("cfg-if"),
        vec!["ws".to_string()],
        "reached through the outside-directory member's dev path dependency"
    );
    assert_eq!(
        listed.item_projects_for("log"),
        vec!["ws".to_string()],
        "two levels below the pointing directory, and still a member through its ancestor"
    );
    // The absolutely-excluded crate is nobody's member: cargo never resolved it, and it has no
    // lock of its own, so it is reported rather than silently folded into the workspace.
    assert_eq!(
        listed.error_projects(),
        ["excluded".to_string()]
            .into_iter()
            .collect::<BTreeSet<_>>(),
        "{:?}",
        listed.error_messages()
    );
    assert!(listed.error_kinds().contains("stale_lock"));
    assert_eq!(
        listed.item_names(),
        [
            "cfg-if".to_string(),
            "itoa".to_string(),
            "log".to_string(),
            "ryu".to_string()
        ]
        .into_iter()
        .collect::<BTreeSet<_>>()
    );
}
/// Cargo's exclusion test compares the raw joined paths, so a `members` entry spelled differently
/// from the `exclude` entry does not cancel it: `members = ["tmp/../shadowed"]` leaves
/// `exclude = ["shadowed"]` in force, cargo drops the crate from the workspace, and it builds — and
/// must be gated — on its own. Normalizing both spellings to the same directory would let the
/// member entry override the exclusion and wave the crate through unevaluated.
#[test]
fn a_member_entry_spelled_around_an_exclusion_does_not_cancel_it() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["member", "tmp/../shadowed"]
                exclude = ["shadowed"]
                resolver = "2"
            "#},
        )
        .write(
            "member/Cargo.toml",
            indoc! {r#"
                [package]
                name = "member"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("member/src/lib.rs", "")
        .write(
            "shadowed/Cargo.toml",
            indoc! {r#"
                [package]
                name = "shadowed"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                once_cell = "1"
            "#},
        )
        .write("shadowed/src/lib.rs", "")
        // The directory the `tmp/..` spelling walks through has to exist for cargo to accept the
        // manifest at all.
        .write("tmp/.keep", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();

    // Cargo's own lock is the oracle: the excluded crate is no member, so its dependency is absent.
    let locked = toml_lock_pins(&fixture.read_bytes("Cargo.lock"));
    assert!(
        !locked.contains_key("once_cell"),
        "the excluded crate is not in the workspace's lock: {locked:?}"
    );

    let checked = fixture.cooldown_json(&["check", "--cargo", "--latest"]);

    assert!(!checked.ok());
    assert_missing_lock_error(&checked, "shadowed");
    assert_eq!(
        checked.summary_checked(),
        1,
        "only the real member's dependency is gated"
    );
}

/// A workspace with a member, and inside that member a cargo-fuzz-style workspace of its own with
/// no lock.
fn fuzz_inside_a_member_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["member"]
                resolver = "2"
            "#},
        )
        .write(
            "member/Cargo.toml",
            indoc! {r#"
                [package]
                name = "member"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("member/src/lib.rs", "")
        .write(
            "member/fuzz/Cargo.toml",
            indoc! {r#"
                [package]
                name = "fuzzish"
                version = "0.1.0"
                edition = "2021"

                [workspace]

                [dependencies]
                ryu = "1"
            "#},
        )
        .write("member/fuzz/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture
}

/// The workspace covers its member, so a selection inside the member is normally the workspace's —
/// but not past an independent project the workspace cannot resolve. Pointing the run at the
/// member's own lockless workspace, or anywhere below it, must evaluate that workspace alone;
/// evaluating the outer one instead would report a clean gate over a project with no lock.
#[test]
fn selecting_a_lockless_workspace_inside_a_covered_member_evaluates_it_alone() {
    skip_if_missing!("cargo");
    let fixture = fuzz_inside_a_member_fixture();

    for selected in ["member/fuzz", "member/fuzz/src"] {
        let gated = fixture.cooldown_json_in(Some(selected), &["check", "--cargo", "--latest"]);
        assert!(!gated.ok(), "-C {selected}: {:?}", gated.error_messages());
        assert_missing_lock_error(&gated, "member/fuzz");
        assert_eq!(
            gated.summary_checked(),
            0,
            "-C {selected} must evaluate nothing but the selected workspace, which has no lock"
        );
    }

    // From the repository root both projects are in scope, and the member's dependency is gated.
    let whole = fixture.cooldown_json(&["check", "--cargo", "--latest"]);
    assert_missing_lock_error(&whole, "member/fuzz");
    assert_eq!(whole.summary_checked(), 1);
}

/// Cargo's cycle check ignores dev edges, so three plain packages that dev-depend on each other in
/// a ring are a valid repository. The ownership fixpoint oscillates on that ring, and whatever
/// state it stops in must still be self-consistent: a claim on a directory that is not a project
/// would be rejected as an adapter bug and abort discovery for the whole run.
#[test]
fn a_dev_dependency_ring_between_plain_packages_does_not_abort_discovery() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "root"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("src/lib.rs", "");
    for (name, registry, next) in [
        ("a", "ryu = \"1\"", "b"),
        ("b", "cfg-if = \"1\"", "c"),
        ("c", "log = \"0.4\"", "a"),
    ] {
        fixture
            .write(
                &format!("{name}/Cargo.toml"),
                &formatdoc! {r#"
                    [package]
                    name = "{name}"
                    version = "0.1.0"
                    edition = "2021"

                    [dependencies]
                    {registry}

                    [dev-dependencies]
                    {next} = {{ path = "../{next}" }}
                "#},
            )
            .write(&format!("{name}/src/lib.rs"), "");
    }
    for manifest in ["Cargo.toml", "a/Cargo.toml", "b/Cargo.toml", "c/Cargo.toml"] {
        fixture
            .run_tool(
                "cargo",
                &["generate-lockfile", "--manifest-path", manifest],
                &[],
            )
            .expect_success();
    }

    let checked = fixture.cooldown_json(&["check", "--cargo", "--latest"]);

    assert!(
        checked.ok(),
        "a valid repository must not fail discovery: {:?}",
        checked.error_messages()
    );
    assert!(
        checked.error_kinds().is_empty(),
        "{:?}",
        checked.error_kinds()
    );
    // Every crate in the ring is gated, through whichever project the fixpoint settled on as its
    // resolver: the root's own dependency plus the four the ring declares.
    assert_eq!(checked.summary_checked(), 5);

    let listed = fixture.cooldown_json(&["outdated", "--cargo", "--all", "--freeze", FREEZE]);
    assert!(
        listed.error_kinds().is_empty(),
        "{:?}",
        listed.error_kinds()
    );
    // One row per project's own direct dependency; a crate the ring resolves is reported through
    // the project whose lock holds it rather than as a project of its own.
    for name in listed.item_names() {
        assert_eq!(
            listed.item_projects_for(&name).len(),
            1,
            "{name} is reported by more than one project: {:?}",
            listed.item_projects_for(&name)
        );
    }
}

/// A lone crate whose lock was never generated is a project cooldown cannot evaluate, not a
/// directory with no supported tool: exit 4 naming the lock, rather than exit 3 naming nothing.
#[test]
fn a_lone_crate_without_a_lock_is_a_stale_lock_not_a_missing_tool() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write(
            "Cargo.toml",
            indoc! {r#"
                [package]
                name = "solo"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("src/lib.rs", "");

    let gated = fixture.cooldown(&["check", "--cargo", "--latest"]);
    assert_eq!(
        gated.status.code(),
        Some(4),
        "a crate with no lock is a project that fails, not an absent tool: {}",
        gated.stderr_str()
    );
    let checked = fixture.cooldown_json(&["check", "--cargo", "--latest"]);
    // The project *is* the scan root, which every report spells `.`.
    assert_missing_lock_error(&checked, ".");
    assert!(
        !checked.error_kinds().iter().any(|kind| kind == "not_found"),
        "the crate is right there; only its lock is missing: {:?}",
        checked.error_kinds()
    );
}

/// `upgrade` must not skip the project `check` now fails on: the same missing lock stops the
/// mutation, with the same diagnostic kind and the same remedy.
#[test]
fn upgrade_reports_a_missing_lock_instead_of_skipping_the_project() {
    skip_if_missing!("cargo");
    let fixture = lockless_nested_workspace_fixture();

    let dry = fixture.cooldown_json(&["upgrade", "--cargo", "--latest", "--dry-run"]);
    assert!(
        !dry.ok(),
        "upgrade must not report success for a project it could not resolve"
    );
    assert_missing_lock_error(&dry, "fuzz");
    assert!(
        !fixture.root().join("fuzz/Cargo.lock").exists(),
        "upgrade reports the missing lock; it does not generate one"
    );
}

/// `--allow-stale-lock` means the same thing on a mutating command as on `check`: the project
/// whose lock cooldown cannot read is skipped with a warning, and the projects it *can* read are
/// planned as usual.
#[test]
fn allow_stale_lock_skips_the_lockless_project_and_plans_the_rest() {
    skip_if_missing!("cargo");
    let fixture = lockless_nested_workspace_fixture();

    let dry = fixture.cooldown_json(&[
        "upgrade",
        "--cargo",
        "--latest",
        "--dry-run",
        "--allow-stale-lock",
    ]);

    assert!(
        dry.ok(),
        "--allow-stale-lock must downgrade the missing lock on upgrade too: {:?}",
        dry.error_messages()
    );
    assert!(dry.error_kinds().is_empty(), "{:?}", dry.error_messages());
    assert!(
        dry.warning_kinds().contains("stale_lock"),
        "expected a stale_lock warning, got {:?}",
        dry.warning_kinds()
    );
    assert!(
        dry.warning_projects().contains("fuzz"),
        "the warning must be attributed to the skipped project, got {:?}",
        dry.warning_projects()
    );
    assert!(
        dry.warning_messages()
            .iter()
            .any(|message| message.contains("dependency evaluation was skipped")),
        "the warning must say the project went unevaluated, got {:?}",
        dry.warning_messages()
    );
    // The root workspace is still planned: skipping one project must not skip the run.
    assert_eq!(
        dry.change_for("itoa").map(|change| change.from),
        Some(LOCKLESS_ROOT_PIN.to_string()),
        "the readable project's upgrade is still planned: {:?}",
        dry.item_names()
    );
}

/// `file` in the fixture directory `dir`, which is the fixture root when empty.
fn under(dir: &str, file: &str) -> String {
    if dir.is_empty() {
        file.to_owned()
    } else {
        format!("{dir}/{file}")
    }
}

/// A cargo workspace at `workspace` (the fixture root when empty) whose member `member` inherits
/// `itoa` from `[workspace.dependencies]`, and a standalone project at `standalone` — its own empty
/// `[workspace]`, as cargo-fuzz writes one — that reaches the member through a `path` dependency,
/// so its resolve reads the workspace's manifest.
/// Both locks are seeded current, with `itoa` on the old `0.4` line, so `upgrade --major` rewrites
/// the workspace's requirement and stales the standalone lock mid-run.
fn run_staled_fixture(workspace: &str, standalone: &str, member_from_standalone: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write(
            &under(workspace, "Cargo.toml"),
            indoc! {r#"
                [workspace]
                members = ["member"]
                resolver = "2"

                [workspace.dependencies]
                itoa = "0.4"
            "#},
        )
        .write(
            &under(workspace, "member/Cargo.toml"),
            indoc! {r#"
                [package]
                name = "member"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa.workspace = true
            "#},
        )
        .write(&under(workspace, "member/src/lib.rs"), "")
        .write(
            &under(standalone, "Cargo.toml"),
            &formatdoc! {r#"
                [package]
                name = "fuzzish"
                version = "0.1.0"
                edition = "2021"

                [workspace]

                [dependencies.member]
                path = "{member_from_standalone}"
            "#},
        )
        .write(&under(standalone, "src/lib.rs"), "");
    for dir in [workspace, standalone] {
        fixture
            .run_tool(
                "cargo",
                &[
                    "generate-lockfile",
                    "--manifest-path",
                    &under(dir, "Cargo.toml"),
                ],
                &[],
            )
            .expect_success();
    }
    fixture
}

/// Runs `upgrade --major` over a [`run_staled_fixture`] and asserts the run absorbed the
/// staleness it caused in the standalone project's lock: the run succeeds, the lock is current and
/// on the workspace's matured `itoa`, the refresh is reported, and the result passes the gate.
fn assert_run_absorbs_the_lock_it_staled(fixture: &Fixture, workspace: &str, standalone: &str) {
    let upgraded = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);

    // The staleness is the run's own doing, so it neither fails the run nor goes unreported.
    assert!(
        upgraded.ok(),
        "a lock the run itself staled must not fail it: {:?}",
        upgraded.error_messages()
    );
    assert!(
        upgraded.warning_kinds().contains("stale_lock")
            && upgraded.warning_projects().contains(standalone),
        "the refresh must be reported against {standalone}, got {:?} {:?}",
        upgraded.warning_projects(),
        upgraded.warning_messages()
    );
    assert!(
        upgraded
            .warning_messages()
            .iter()
            .any(|message| message.contains("another project in this run changed a manifest")),
        "the warning must name the cause, got {:?}",
        upgraded.warning_messages()
    );

    // The run leaves no lock stale behind it: cargo itself accepts the standalone lock as is.
    let manifest = under(standalone, "Cargo.toml");
    fixture
        .run_tool(
            "cargo",
            &[
                "metadata",
                "--locked",
                "--format-version",
                "1",
                "--manifest-path",
                &manifest,
            ],
            &[],
        )
        .expect_success();

    // The refresh resolves `itoa` to the newest `1.x`, which is too fresh under the freeze; the
    // gate pass rolls it back to the matured release the workspace adopted.
    let workspace_itoa = toml_lock_pins(&fixture.read_bytes(&under(workspace, "Cargo.lock")))
        .get("itoa")
        .cloned();
    let standalone_itoa = toml_lock_pins(&fixture.read_bytes(&under(standalone, "Cargo.lock")))
        .get("itoa")
        .cloned();
    assert!(
        workspace_itoa
            .as_deref()
            .is_some_and(|version| version.starts_with("1.")),
        "the workspace must adopt the matured 1.x line, got {workspace_itoa:?}"
    );
    assert_eq!(standalone_itoa, workspace_itoa);
    let checked = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(
        checked.ok(),
        "the refreshed lock must pass the gate: {:?}",
        checked.error_messages()
    );
}

/// The root workspace runs first and rewrites `[workspace.dependencies]`, which stales the
/// standalone project's lock before its turn: the turn refreshes and gates the lock instead of
/// failing on it.
#[test]
fn upgrade_refreshes_a_lock_an_earlier_project_staled() {
    skip_if_missing!("cargo");
    let fixture = run_staled_fixture("", "fuzz", "../member");

    assert_run_absorbs_the_lock_it_staled(&fixture, "", "fuzz");
}

/// The standalone project sorts first, so the workspace stales its lock only after its turn: the
/// run probes again once every project ran and re-runs the staled one.
#[test]
fn upgrade_reruns_a_project_a_later_project_staled() {
    skip_if_missing!("cargo");
    let fixture = run_staled_fixture("b", "a", "../b/member");

    assert_run_absorbs_the_lock_it_staled(&fixture, "b", "a");
}

/// A monorepo whose root config excludes both a nested incubator workspace and one of its own
/// members, each workspace seeded with its own lock by the real cargo.
fn excluded_subtrees_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write(
            "cooldown.toml",
            indoc! {r#"
                [global]
                exclude-folders = ["incubator", "crates/app"]
            "#},
        )
        .write(
            "Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["crates/app", "crates/core"]
                exclude = ["incubator"]
                resolver = "2"
            "#},
        )
        .write(
            "crates/app/Cargo.toml",
            indoc! {r#"
                [package]
                name = "app"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                log = "0.4"
            "#},
        )
        .write("crates/app/src/lib.rs", "")
        .write(
            "crates/core/Cargo.toml",
            indoc! {r#"
                [package]
                name = "core-lib"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                cfg-if = "1"
            "#},
        )
        .write("crates/core/src/lib.rs", "")
        // A nested workspace root the enclosing workspace excludes: a project of its own.
        .write(
            "incubator/Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["lab"]
                resolver = "2"
            "#},
        )
        .write(
            "incubator/lab/Cargo.toml",
            indoc! {r#"
                [package]
                name = "lab"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                bytes = "1"
            "#},
        )
        .write("incubator/lab/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture
        .run_tool(
            "cargo",
            &[
                "generate-lockfile",
                "--manifest-path",
                "incubator/Cargo.toml",
            ],
            &[],
        )
        .expect_success();
    fixture
}

/// The default scan honors both excludes; pointing the run at either excluded directory (`-C`)
/// scans it anyway, rather than pruning the directory during detection, scoping the root project
/// to zero members, and exiting clean having checked nothing.
#[test]
fn selecting_an_excluded_directory_scans_it_instead_of_reporting_nothing() {
    skip_if_missing!("cargo");
    let fixture = excluded_subtrees_fixture();

    // `outdated --all` names every dependency in scope, so it shows exactly what a run covered.
    // The harness pins `--dir` itself, so the subdirectory runs go through `cooldown_json_in`.
    let listed = |dir: Option<&str>| {
        let out = fixture.cooldown_json_in(dir, &["outdated", "--all", "--freeze", FREEZE]);
        assert!(out.ok(), "{dir:?}: {:?}", out.error_messages());
        out.item_names()
    };
    let names = |items: &[&str]| {
        items
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>()
    };

    // The default scan honors both excludes: the member and the nested workspace are absent.
    assert_eq!(listed(None), names(&["cfg-if"]));
    // Selecting the excluded nested workspace scans it: its dependencies are the whole report.
    assert_eq!(
        listed(Some("incubator")),
        names(&["bytes"]),
        "-C into an excluded nested workspace reports that workspace, not an empty result"
    );
    // Selecting the excluded member of the root workspace reports that member's dependencies.
    assert_eq!(
        listed(Some("crates/app")),
        names(&["log"]),
        "-C into an excluded member reports that member's dependencies"
    );
    // Selecting a non-excluded member changes nothing about the excludes elsewhere.
    assert_eq!(listed(Some("crates/core")), names(&["cfg-if"]));

    // The gate evaluates the selection too, instead of passing on zero dependencies.
    let checked = fixture.cooldown_json_in(Some("incubator"), &["check", "--latest"]);
    assert!(checked.ok(), "{:?}", checked.error_messages());
    assert_eq!(
        checked.summary_checked(),
        1,
        "check -C incubator must evaluate the incubator's dependency"
    );
}

/// `--lock` brings a stale `Cargo.lock` current before the read-only commands evaluate it.
/// The refresh is the minimal one (`cargo update --workspace`): the new requirement is resolved
/// into the lock, and every version the stale lock already held and the manifests still admit
/// stays put — a refresh that floated the graph would itself introduce the too-fresh versions the
/// gate then flags.
#[test]
fn lock_flag_refreshes_a_stale_cargo_lock_before_reading_it() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();

    fixture.write(
        "crates/app/Cargo.toml",
        &format!("{APP_MANIFEST}regex = \"1\"\n"),
    );
    let stale_lock = fixture.read_bytes("Cargo.lock");
    let before = toml_lock_entries(&stale_lock);
    assert!(
        !before.iter().any(|(name, _)| name == "regex"),
        "the seed lock predates the new requirement"
    );

    let checked = fixture.cooldown_json(&["check", "--latest", "--lock"]);
    assert!(
        checked.ok(),
        "check --lock must gate the refreshed lock cleanly: {:?} {:?}",
        checked.error_kinds(),
        checked.error_messages()
    );
    assert_eq!(checked.summary_errors(), 0);
    assert!(
        !checked.warning_kinds().contains("stale_lock"),
        "a refreshed lock is current, not stale: {:?}",
        checked.warning_kinds()
    );
    let refreshed_lock = fixture.read_bytes("Cargo.lock");
    assert_ne!(
        stale_lock, refreshed_lock,
        "--lock must rewrite the stale lock"
    );
    // Compare whole `(name, version)` nodes: a crate moved to another version, or locked at a second
    // one, shows up as a node the stale lock had (or lacked), which a name-keyed map would hide.
    let after = toml_lock_entries(&refreshed_lock);
    let moved = before.difference(&after).collect::<Vec<_>>();
    assert!(
        moved.is_empty(),
        "a refresh must leave every existing pin where it was, but these changed: {moved:?}"
    );
    let added = after.difference(&before).collect::<BTreeSet<_>>();
    assert!(
        added.iter().any(|(name, _)| name == "regex"),
        "the new requirement is resolved into the lock: {added:?}"
    );
    // The gate read the refreshed graph, new nodes included, not the stale one it started from.
    assert!(
        usize::try_from(checked.summary_checked()).expect("checked count") >= added.len(),
        "check --lock evaluated {} dependencies, fewer than the {} nodes the refresh added",
        checked.summary_checked(),
        added.len()
    );

    // A current lock refreshes to itself, and `outdated --lock` takes the same path.
    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--lock"]);
    assert!(outdated.ok(), "outdated --lock stays informational");
    assert!(!outdated.warning_kinds().contains("stale_lock"));
    assert_eq!(refreshed_lock, fixture.read_bytes("Cargo.lock"));

    // `--dry-run` never mutates, so `--lock` is inert and the stale lock fails closed as usual.
    fixture.write(
        "crates/app/Cargo.toml",
        &format!("{APP_MANIFEST}regex = \"1\"\nbytes = \"1\"\n"),
    );
    let stale_again = fixture.read_bytes("Cargo.lock");
    let dry = fixture.cooldown_json(&["check", "--latest", "--lock", "--dry-run"]);
    assert!(
        !dry.ok(),
        "a dry run must not refresh, so the stale lock still fails closed"
    );
    assert!(
        dry.error_kinds().contains("stale_lock"),
        "{:?}",
        dry.error_kinds()
    );
    assert_eq!(stale_again, fixture.read_bytes("Cargo.lock"));
}

/// `--lock` refreshes only the locks the run evaluates.
/// Selecting the excluded nested workspace leaves the enclosing workspace's `Cargo.lock` untouched
/// even though that workspace also encloses the selection, and a root run leaves the excluded
/// nested lock alone in turn.
#[test]
fn lock_refresh_touches_only_the_locks_in_scope() {
    skip_if_missing!("cargo");
    let fixture = excluded_subtrees_fixture();
    // Make both locks stale, so a refresh reaching either one would rewrite it.
    fixture.write(
        "crates/core/Cargo.toml",
        indoc! {r#"
            [package]
            name = "core-lib"
            version = "0.1.0"
            edition = "2021"

            [dependencies]
            cfg-if = "1"
            log = "0.4"
        "#},
    );
    fixture.write(
        "incubator/lab/Cargo.toml",
        indoc! {r#"
            [package]
            name = "lab"
            version = "0.1.0"
            edition = "2021"

            [dependencies]
            bytes = "1"
            cfg-if = "1"
        "#},
    );
    let root_lock = fixture.read_bytes("Cargo.lock");
    let nested_lock = fixture.read_bytes("incubator/Cargo.lock");

    let nested = fixture.cooldown_json_in(Some("incubator"), &["check", "--latest", "--lock"]);
    assert!(
        nested.ok(),
        "{:?} {:?}",
        nested.error_kinds(),
        nested.error_messages()
    );
    assert!(!nested.warning_kinds().contains("stale_lock"));
    // Both of the nested workspace's dependencies are gated from the refreshed lock.
    assert_eq!(nested.summary_checked(), 2);
    assert_ne!(
        nested_lock,
        fixture.read_bytes("incubator/Cargo.lock"),
        "the selected workspace's lock is refreshed"
    );
    assert_eq!(
        root_lock,
        fixture.read_bytes("Cargo.lock"),
        "the enclosing workspace's lock is not refreshed by a run inside the nested one"
    );

    // From the root, the excluded nested workspace is out of scope, and so is its lock.
    let refreshed_nested = fixture.read_bytes("incubator/Cargo.lock");
    let root = fixture.cooldown_json(&["check", "--latest", "--lock"]);
    assert!(
        root.ok(),
        "{:?} {:?}",
        root.error_kinds(),
        root.error_messages()
    );
    assert_ne!(
        root_lock,
        fixture.read_bytes("Cargo.lock"),
        "the root run refreshes the root lock"
    );
    assert_eq!(refreshed_nested, fixture.read_bytes("incubator/Cargo.lock"));
}

/// A refresh needs the resolver's network access, so `--lock --offline` is rejected up front as
/// a usage error instead of failing, or silently skipping, per project.
#[test]
fn lock_with_offline_is_a_usage_error() {
    skip_if_missing!("cargo");
    let fixture = conflict_fixture();
    fixture.write(
        "crates/app/Cargo.toml",
        &format!("{APP_MANIFEST}regex = \"1\"\n"),
    );
    let stale_lock = fixture.read_bytes("Cargo.lock");

    let rejected = fixture.cooldown(&["check", "--latest", "--lock", "--offline"]);
    assert_eq!(
        rejected.status.code(),
        Some(2),
        "a usage error, not a refresh failure: {}",
        rejected.stderr_str()
    );
    assert!(
        rejected.stderr_str().contains("--offline"),
        "{}",
        rejected.stderr_str()
    );
    assert_eq!(stale_lock, fixture.read_bytes("Cargo.lock"), "nothing ran");
}

/// Selecting a directory that holds members rather than sitting inside one (`-C crates`) scopes
/// the run to the members below it, and the excludes still apply to those.
/// A nested workspace of the same tool below the selection is in scope as well, and does not
/// push the enclosing workspace's members out of it.
#[test]
fn selecting_a_directory_above_members_scopes_to_the_members_below_it() {
    skip_if_missing!("cargo");
    let fixture = excluded_subtrees_fixture();
    let names = |items: &[&str]| {
        items
            .iter()
            .map(ToString::to_string)
            .collect::<BTreeSet<_>>()
    };
    let listed = |dir: Option<&str>| {
        let out = fixture.cooldown_json_in(dir, &["outdated", "--all", "--freeze", FREEZE]);
        assert!(out.ok(), "{dir:?}: {:?}", out.error_messages());
        out.item_names()
    };
    // `crates/core` lies below the selection and is reported; `crates/app` lies below it too but
    // stays excluded, because only the selected path itself outranks the excludes.
    assert_eq!(listed(Some("crates")), names(&["cfg-if"]));

    // An independent nested workspace below the selection: its own project, evaluated alongside
    // the enclosing workspace's members rather than instead of them.
    fixture
        .write(
            "crates/nested/Cargo.toml",
            indoc! {r#"
                [workspace]
                members = ["tool"]
                resolver = "2"
            "#},
        )
        .write(
            "crates/nested/tool/Cargo.toml",
            indoc! {r#"
                [package]
                name = "nested-tool"
                version = "0.1.0"
                edition = "2021"

                [dependencies]
                itoa = "1"
            "#},
        )
        .write("crates/nested/tool/src/lib.rs", "");
    fixture
        .run_tool(
            "cargo",
            &[
                "generate-lockfile",
                "--manifest-path",
                "crates/nested/Cargo.toml",
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        listed(Some("crates")),
        names(&["cfg-if", "itoa"]),
        "a nested workspace below the selection joins the enclosing members, not replaces them"
    );
    assert_eq!(listed(Some("crates/nested")), names(&["itoa"]));
    assert_eq!(listed(None), names(&["cfg-if", "itoa"]));
}

/// A selection the run cannot honor is an error, and one that evaluates nothing is reported: the
/// two ways a `-C` run could otherwise pass on zero dependencies.
#[test]
fn contradictory_or_empty_selections_are_reported() {
    skip_if_missing!("cargo");
    let fixture = excluded_subtrees_fixture();

    // The run's own `--exclude-folders` names the selected directory.
    let conflict = fixture.cooldown_in(
        Some("incubator"),
        &["check", "--latest", "--exclude-folders", "incubator"],
    );
    assert_eq!(conflict.status.code(), Some(2), "{}", conflict.stderr_str());
    assert!(
        conflict.stderr_str().contains("--exclude-folders excludes"),
        "{}",
        conflict.stderr_str()
    );

    // No project or member covers the selected directory.
    std::fs::create_dir_all(fixture.root().join("docs")).expect("docs dir");
    let empty = fixture.cooldown_json_in(Some("docs"), &["check", "--latest"]);
    assert!(empty.ok(), "{:?}", empty.error_messages());
    assert_eq!(empty.summary_checked(), 0);
    assert!(
        empty.warning_kinds().contains("config"),
        "an empty selection is called out, not passed silently: {:?}",
        empty.warning_kinds()
    );

    // A gitignored selection is never reached by the scan; the error names the flag that lifts the
    // rule, and with it the selection is evaluated.
    fixture.write(".gitignore", "incubator/\n");
    let ignored = fixture.cooldown_in(Some("incubator"), &["check", "--latest"]);
    assert_eq!(ignored.status.code(), Some(2), "{}", ignored.stderr_str());
    assert!(
        ignored.stderr_str().contains("--no-gitignore"),
        "{}",
        ignored.stderr_str()
    );
    let lifted =
        fixture.cooldown_json_in(Some("incubator"), &["check", "--latest", "--no-gitignore"]);
    assert!(lifted.ok(), "{:?}", lifted.error_messages());
    assert_eq!(lifted.summary_checked(), 1);
}

#[test]
fn fix_matures_too_fresh_deps_and_is_idempotent() {
    skip_if_missing!("cargo");

    // Seed at "newest now" (cargo has no cutoff), then evaluate under the past FREEZE: every dep
    // published after FREEZE is too-fresh and a cooldown violation `fix` must mature down. The later
    // constant documents the seeding intent even though cargo ignores it.
    let _ = FREEZE_LATER;
    let fixture = conflict_fixture();

    // `fix` matures the reducible too-fresh deps down to versions at or before the freeze cutoff and
    // re-locks. It applies at least one downgrade (a direct dep with a loose floor and a matured older
    // release) and never errors.
    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert!(
        fixed.ok(),
        "fix should succeed: {}",
        fixture.cooldown(&["fix", "--freeze", FREEZE]).stderr_str()
    );
    assert_eq!(fixed.lock_status(), Some("current"), "fix re-locks cleanly");
    assert_eq!(fixed.summary_errors(), 0, "fix should not error");
    assert!(
        fixed.summary_applied() >= 1,
        "fix should downgrade at least one reducible too-fresh dep, got {}",
        fixed.summary_applied()
    );

    // Any violations `check` still reports after `fix` must be graph-held: a `=`-pinned proc-macro
    // stack (serde_derive's own deps) pins fresh transitives the resolver cannot roll back without
    // breaking the lock — exactly the deps `fix` warns it must leave in place. No *direct* dep may
    // remain too-fresh, since a direct violation is always reducible by `fix`.
    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert_eq!(
        check.summary_direct_violations(),
        0,
        "no direct dep may remain too-fresh after fix (graph-held transitives may)"
    );

    let lock_after_fix = fixture.read_bytes("Cargo.lock");

    // Re-running fix is idempotent: only graph-held transitives remain (which fix leaves), so nothing
    // new is applied and the lock is byte-identical — the fixed point.
    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "second fix must be a no-op (idempotent)"
    );
    assert_eq!(
        lock_after_fix,
        fixture.read_bytes("Cargo.lock"),
        "second fix must leave the lock byte-identical"
    );
}

/// Two workspace members that depend on the SAME crate at DIFFERENT, semver-incompatible majors.
/// cargo keeps both versions in one `Cargo.lock` (like pnpm, unlike uv's single environment), so the
/// whole-graph resolve must preserve both lines. cooldown targets each instance with `update -p
/// <crate>@<from> --precise <to>`, so it moves one line without collapsing the other — this test locks
/// that property in alongside the pnpm multi-version test. (The existing fixtures already exercise
/// member-declared deps: `serde`/`log` live in `crates/app`, never the bare `[workspace]` root.)
const MULTI_VERSION_ROOT_MANIFEST: &str = r#"[workspace]
members = ["crates/old", "crates/new"]
resolver = "2"
"#;

const MULTI_VERSION_OLD_MANIFEST: &str = r#"[package]
name = "old"
version = "0.1.0"
edition = "2021"

[dependencies]
bitflags = "1"
"#;

const MULTI_VERSION_NEW_MANIFEST: &str = r#"[package]
name = "new"
version = "0.1.0"
edition = "2021"

[dependencies]
bitflags = "2"
"#;

fn multi_version_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", MULTI_VERSION_ROOT_MANIFEST)
        .write("crates/old/Cargo.toml", MULTI_VERSION_OLD_MANIFEST)
        .write("crates/old/src/lib.rs", "")
        .write("crates/new/Cargo.toml", MULTI_VERSION_NEW_MANIFEST)
        .write("crates/new/src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture
}

/// Every `version` recorded for `crate_name` in a `Cargo.lock` — a crate held at several majors has
/// one `[[package]]` block per version, so this returns them all (unlike `toml_lock_pins`, which keeps
/// only the newest).
fn cargo_lock_versions_of(crate_name: &str, lock: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(lock);
    let mut versions = Vec::new();
    let mut in_target = false;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("name = ") {
            in_target = rest.trim_matches('"') == crate_name;
        } else if in_target && let Some(rest) = line.strip_prefix("version = ") {
            versions.push(rest.trim_matches('"').to_string());
            in_target = false;
        }
    }
    versions
}

#[test]
fn upgrade_preserves_distinct_versions_across_members() {
    skip_if_missing!("cargo");
    let fixture = multi_version_fixture();

    // Sanity: the seed holds both major lines.
    let seed = fixture.read_bytes("Cargo.lock");
    let before = cargo_lock_versions_of("bitflags", &seed);
    assert!(
        before.iter().any(|v| v.starts_with("1.")),
        "seed must hold a bitflags v1 line, got {before:?}"
    );
    assert!(
        before.iter().any(|v| v.starts_with("2.")),
        "seed must hold a bitflags v2 line, got {before:?}"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );

    // BOTH lines must survive: `crates/old` keeps a bitflags v1, `crates/new` a bitflags v2. A naive
    // single-target pin would collapse one onto the other.
    let after = cargo_lock_versions_of("bitflags", &fixture.read_bytes("Cargo.lock"));
    assert!(
        after.iter().any(|v| v.starts_with("1.")),
        "bitflags v1 line must survive the upgrade, got {after:?}"
    );
    assert!(
        after.iter().any(|v| v.starts_with("2.")),
        "bitflags v2 line must survive the upgrade, got {after:?}"
    );
}

/// Two independent direct deps whose planned targets share one version string. `futures-core` and
/// `futures-io` release in lockstep (identical version numbers and publish dates) but neither
/// depends on the other, so nothing cascades one pin into the other: each crate only lands if its
/// own `cargo update --precise` pin is issued. Together with the wgpu-family fixture below this
/// guards the one-command-per-pin apply: cargo applies `--precise` to only one spec when several
/// dependency-connected `-p` specs share a single invocation.
const SHARED_TARGET_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-shared-target"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    futures-core = "0.3"
    futures-io = "0.3"
"#};

fn shared_target_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", SHARED_TARGET_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    // Seed both at 0.3.29 (2023-10-26) so the FREEZE cutoff (2024-06-01) plans the same matured
    // target for both: 0.3.30 (2023-12-24) — one shared target-version string.
    for spec in ["futures-core", "futures-io"] {
        fixture
            .run_tool("cargo", &["update", "-p", spec, "--precise", "0.3.29"], &[])
            .expect_success();
    }
    fixture
}

#[test]
fn upgrade_moves_every_crate_sharing_a_target_version() {
    skip_if_missing!("cargo");
    let fixture = shared_target_fixture();

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );
    let applied = upgrade.applied_names();
    assert!(
        applied.contains("futures-core") && applied.contains("futures-io"),
        "both crates sharing the target version must be applied, got {applied:?}"
    );

    let lock_after = fixture.read_bytes("Cargo.lock");
    for crate_name in ["futures-core", "futures-io"] {
        assert_eq!(
            cargo_lock_versions_of(crate_name, &lock_after),
            vec!["0.3.30".to_owned()],
            "{crate_name} must land on the shared matured target, not stay at the seed"
        );
    }
}

/// The freeze the two float-and-reconcile regressions below pin: it falls between the wgpu family's
/// matured 29.0.3 (2026-05-02) / zbus family's matured 5.16.0+4.3.2 (2026-05-29 / 2026-04-26) and
/// the too-fresh 29.0.4 / 5.17.0+4.3.3 wave (2026-07-02 / 2026-07-07), so a re-lock floats the
/// families to versions the gate rejects while a matured target exists for every floated node.
const FAMILY_FREEZE: &str = "2026-06-20T00:00:00Z";

/// The real-world shape behind the "0 applied · 58 skipped" regression: a cross-major bump of one
/// direct dep (`wgpu 26 → 29`) drags a whole family of same-versioned, dependency-connected
/// transitives (`wgpu-core`, `wgpu-hal`, `wgpu-types`, `naga`, …) into the lock at the newest
/// in-range version — too fresh under the freeze — and the reconcile pass must mature every one of
/// them down to the shared 29.0.3. Each downgrade must be its own `cargo update` invocation: with
/// the family batched into one multi-`-p --precise` call, cargo silently pinned only the first
/// crate, the residual gate saw the rest still fresh, and the entire upgrade rolled back.
const WGPU_FAMILY_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-wgpu-family"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    wgpu = "26"
"#};

#[test]
fn upgrade_reconciles_a_cross_major_family_float_instead_of_rolling_back() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", WGPU_FAMILY_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();

    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FAMILY_FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--major", "--freeze", FAMILY_FREEZE])
            .stderr_str()
    );
    assert!(
        upgrade.applied_names().contains("wgpu"),
        "the cross-major wgpu bump must land, got {:?}",
        upgrade.applied_names()
    );
    assert!(
        !upgrade.skipped_reasons().contains("transitive_in_cooldown"),
        "a fully reconcilable family float must not roll the batch back: {:?}",
        upgrade.skipped_reasons()
    );

    let lock_after = fixture.read_bytes("Cargo.lock");
    for crate_name in ["wgpu", "wgpu-core", "wgpu-hal", "wgpu-types"] {
        assert_eq!(
            cargo_lock_versions_of(crate_name, &lock_after),
            vec!["29.0.3".to_owned()],
            "{crate_name} must mature to the newest release under the freeze"
        );
    }
}

/// A graph-held violation that only becomes fixable after an earlier reconcile round: upgrading
/// `zbus` 5.12.0 → 5.16.0 floats `zbus_macros` to the too-fresh 5.17.0 (newest satisfying
/// `^5.16.0`), which in turn demands `zbus_names ^4.3.3` — also too fresh, and graph-held at its
/// floor, so round one can only mature `zbus_macros` down. Only once macros sits at 5.16.0 (whose
/// requirement is `^4.3.2`) does `zbus_names` gain a matured target. The reconcile loop must keep
/// re-planning while rounds make progress — terminating on "no new violations" strands
/// `zbus_names` at 4.3.3 and the final gate rolls the whole upgrade back.
const ZBUS_CHAIN_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-zbus-chain"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    zbus = "5"
"#};

fn zbus_chain_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", ZBUS_CHAIN_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    // Seed a fully matured (pre-freeze) zbus line: zbus 5.12.0 admits macros ^5.12 / names ^4.2, so
    // the seed holds no violation. The upgrade to 5.16.0 (`zbus_macros ^5.16.0`) then floats macros
    // to 5.17.0 and, through it, names to 4.3.3 — both past the freeze.
    for (spec, precise) in [
        ("zbus", "5.12.0"),
        ("zbus_macros", "5.15.0"),
        ("zbus_names", "4.3.2"),
        ("zvariant", "5.12.0"),
    ] {
        fixture
            .run_tool("cargo", &["update", "-p", spec, "--precise", precise], &[])
            .expect_success();
    }
    fixture
}

#[test]
fn upgrade_reconciles_a_violation_unblocked_by_an_earlier_round() {
    skip_if_missing!("cargo");
    let fixture = zbus_chain_fixture();

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FAMILY_FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FAMILY_FREEZE])
            .stderr_str()
    );
    assert!(
        upgrade.applied_names().contains("zbus"),
        "the zbus upgrade must land, got {:?}",
        upgrade.applied_names()
    );
    assert!(
        !upgrade.skipped_reasons().contains("transitive_in_cooldown"),
        "a chain reconcilable across rounds must not roll the batch back: {:?}",
        upgrade.skipped_reasons()
    );

    let lock_after = fixture.read_bytes("Cargo.lock");
    for (crate_name, expected) in [
        ("zbus", "5.16.0"),
        ("zbus_macros", "5.16.0"),
        ("zbus_names", "4.3.2"),
    ] {
        assert_eq!(
            cargo_lock_versions_of(crate_name, &lock_after),
            vec![expected.to_owned()],
            "{crate_name} must settle on the newest matured version under the freeze"
        );
    }
}

/// One crate declared twice in a member — `[dependencies] itoa = "1"` beside `[dev-dependencies]
/// itoa = "0.4"` — so the lock holds two version lines and the member has a direct edge into the
/// target major *before* the planned `0.4.8 → 1.0.11` move is attempted (the 1.x line is seeded at
/// exactly the freeze target to recreate the masking shape). The apply must widen *both* manifest
/// entries (stopping at the first leaves `"0.4"` capping the old line forever) and the reach check
/// must demand the old line actually vacates (the pre-existing 1.x edge must not verify the move
/// as applied while the 0.4.8 line sits untouched — a phantom "upgraded" the lock never took,
/// re-reported on every subsequent run).
const DUAL_ENTRY_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-dual-entry"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    itoa = "1"

    [dev-dependencies]
    itoa = "0.4"
"#};

fn dual_entry_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", DUAL_ENTRY_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    // Pin the 1.x line to the exact version the freeze will target (itoa 1.0.11, 2024-03-26), so
    // the sibling edge sits precisely at the planned target — the masking shape. The seed version
    // is whatever generate-lockfile picked, so read it back rather than hard-coding it.
    let seeded = cargo_lock_versions_of("itoa", &fixture.read_bytes("Cargo.lock"))
        .into_iter()
        .find(|version| version.starts_with("1."))
        .expect("the [dependencies] itoa entry seeds a 1.x line");
    fixture
        .run_tool(
            "cargo",
            &[
                "update",
                "-p",
                &format!("itoa@{seeded}"),
                "--precise",
                "1.0.11",
            ],
            &[],
        )
        .expect_success();
    fixture
}

#[test]
fn upgrade_moves_a_dual_entry_crate_for_real_and_converges() {
    skip_if_missing!("cargo");
    let fixture = dual_entry_fixture();

    let first = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(
        first.ok(),
        "first upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
            .stderr_str()
    );
    assert!(
        first.applied_names().contains("itoa"),
        "the dual-entry crate must land, got {:?}",
        first.applied_names()
    );
    assert_eq!(
        cargo_lock_versions_of("itoa", &fixture.read_bytes("Cargo.lock")),
        vec!["1.0.11".to_owned()],
        "the 0.4 line must vacate: both entries settle on the single matured 1.x line"
    );

    // The move must persist: a second run has nothing left to do and the lock stays byte-stable.
    // (The masked variant reported the same phantom "upgraded" row on every run while the lock
    // never changed.)
    let lock_after_first = fixture.read_bytes("Cargo.lock");
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point)"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("Cargo.lock"),
        "lock must be byte-identical across the two converged runs"
    );
}

const VERSION_BOUND_MANIFEST: &str = indoc! {r#"
    [package]
    name = "cargo-version-bound"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    clap = { version = ">=3, <4", default-features = false }
"#};

#[test]
fn explicit_upper_bound_holds_until_rewrite() {
    skip_if_missing!("cargo");
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", VERSION_BOUND_MANIFEST)
        .write("src/lib.rs", "");
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    let manifest_before = fixture.read_bytes("Cargo.toml");
    let lock_before = fixture.read_bytes("Cargo.lock");
    assert!(
        cargo_lock_versions_of("clap", &lock_before)
            .iter()
            .all(|version| version.starts_with("3.")),
        "the explicit bound must seed clap 3.x"
    );

    let held = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(
        held.skipped_reasons_for("clap"),
        ["declared_bound_held".to_owned()].into_iter().collect()
    );
    assert_eq!(manifest_before, fixture.read_bytes("Cargo.toml"));
    assert_eq!(lock_before, fixture.read_bytes("Cargo.lock"));

    let rewritten = fixture.cooldown_json(&["upgrade", "--major", "--rewrite", "--freeze", FREEZE]);
    assert!(rewritten.applied_names().contains("clap"));
    assert!(
        cargo_lock_versions_of("clap", &fixture.read_bytes("Cargo.lock"))
            .iter()
            .any(|version| version.starts_with("4.")),
        "--rewrite must cross the explicit bound"
    );
    assert!(
        !String::from_utf8_lossy(&fixture.read_bytes("Cargo.toml")).contains(">=3, <4"),
        "the crossed bound must be rewritten"
    );
}
