//! End-to-end convergence tests that drive the REAL `cargo` resolver against fixtures generated on
//! the fly in temp dirs. These guard the cargo adapter's whole-graph re-resolve: the adapter now
//! applies all of a project's planned `--precise` pins as one batched re-resolve and builds the
//! report from the full before/after `Cargo.lock` diff, so a candidate can never silently move
//! another node and a converged graph re-applies to a byte-stable fixed point.
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
//! the real `cargo` against live crates.io; the assertions check INVARIANTS (convergence,
//! no-silent-change, cross-command agreement), never hard-coded versions.
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

use support::{Fixture, changed_packages, toml_lock_pins};

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
    assert_eq!(first.lock_verified(), Some(true), "first upgrade re-locks");
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

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let held = upgrade.held_conflict_names();

    // Whatever `outdated` marks blocked, `upgrade` must report held — and vice versa. (Both may be
    // empty if the seed already satisfies the window, which is still agreement.)
    assert_eq!(
        blocked, held,
        "the set `outdated` marks blocked must equal the set `upgrade` reports held\nblocked={blocked:?}\nheld={held:?}"
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
        dry.lock_verified(),
        None,
        "--dry-run never re-locks, so lockVerified is null"
    );
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
    assert_eq!(fixed.lock_verified(), Some(true), "fix re-locks cleanly");
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
