//! End-to-end convergence tests that drive the REAL `uv` resolver against fixtures generated on
//! the fly in temp dirs. These guard the uv adapter's whole-graph re-resolve: an earlier band-aid
//! shipped a regression that broke `fix` precisely because no integration test exercised the real
//! tool.
//!
//! # Determinism
//!
//! The cooldown window is relative ("14 days") and would drift over time, making exact-version
//! assertions flaky. Every test instead pins the resolution clock with `--freeze <FREEZE>` — an
//! absolute exclude-newer cutoff handed straight to uv — so uv replays PyPI's immutable release
//! history and reproduces the same resolve forever. The assertions check INVARIANTS (convergence,
//! no-silent-change, cross-command agreement), never hard-coded versions.
//!
//! # The conflict
//!
//! The fixture reproduces the proven resolver conflict: `huggingface-hub`'s 1.x line requires
//! `typer<0.26.0`, while typer's own newest matured release (0.26.7) is past it. At the freeze
//! cutoff the whole-graph upgrade resolve cannot land both newest-huggingface-hub and
//! newest-typer; cooldown adopts the newest typer and accepts a collateral huggingface-hub
//! downgrade, then holds huggingface-hub's newer line out (it would force typer back down).
//!
//! Sibling files cover the other adapters whose whole-graph re-resolve is now fixed, each reusing the
//! tool-agnostic `support/` harness: a fixture generator (manifest + seeding via the ecosystem's own
//! package manager) and a `PinParser`, then the same invariant assertions on the returned `Envelope`.
//! `convergence_cargo.rs` seeds via `cargo generate-lockfile` (`Cargo.lock` reuses
//! `support::toml_lock_pins`); `convergence_go.rs` seeds via `go mod tidy` (`support::go_mod_pins`);
//! `convergence_pnpm.rs` seeds via `pnpm install --lockfile-only --config.minimumReleaseAge=<m>`
//! (`support::pnpm_lock_pins`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use support::{Fixture, changed_packages, toml_lock_pins};

/// The absolute resolution cutoff. At this instant the huggingface-hub/typer conflict exists in
/// PyPI's immutable history, so the resolve is reproducible forever.
const FREEZE: &str = "2026-06-07T00:00:00Z";

/// A later cutoff used only to seed a genuinely too-fresh starting lock for the `fix` test: deps
/// resolved here are newer than `FREEZE`, so evaluating them under `--freeze FREEZE` flags them as
/// cooldown violations to mature down.
const FREEZE_LATER: &str = "2026-06-20T00:00:00Z";

/// The Python interpreter the fixture pins. uv's resolution is sensitive to the interpreter's
/// environment markers, so pinning a fixed minor keeps both the seed `uv lock` and cooldown's
/// internal re-resolve on the same interpreter — a prerequisite for a byte-reproducible lock. uv
/// downloads this interpreter on demand if the machine lacks it.
const PYTHON_VERSION: &str = "3.12";

/// The conflict fixture manifest. `certifi` is a direct dep with a loose floor so the manifest
/// admits older matured versions (this is what makes the `fix` downgrades land); huggingface-hub
/// and typer carry the mutually-exclusive requirement that drives the upgrade conflict.
const PYPROJECT: &str = r#"[project]
name = "cooldown-conflict-fixture"
version = "0.1.0"
requires-python = ">=3.10"
dependencies = [
    "huggingface-hub>=0.30.0",
    "typer>=0.24.0",
    "certifi>=2026.1.1",
]
"#;

/// Seed a uv.lock by resolving the fixture at `cutoff`, so the starting state is itself
/// reproducible from PyPI history. uv records the cutoff in the lock, so cooldown's freshness check
/// (`uv lock --check --exclude-newer <cutoff>`) must use the same instant to consider it current.
fn seed_lock(fixture: &Fixture, cutoff: &str) {
    fixture
        .run_tool("uv", &["lock"], &[("UV_EXCLUDE_NEWER", cutoff)])
        .expect_success();
}

fn conflict_fixture(seed_cutoff: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("pyproject.toml", PYPROJECT);
    fixture.write(".python-version", PYTHON_VERSION);
    seed_lock(&fixture, seed_cutoff);
    fixture
}

#[test]
fn upgrade_converges_to_a_fixed_point() {
    skip_if_missing!("uv");
    let fixture = conflict_fixture(FREEZE);

    // First upgrade: cooldown re-resolves the whole graph under the freeze cutoff. It cannot land
    // both newest-huggingface-hub and newest-typer, so it adopts newest typer and accepts a
    // collateral huggingface-hub downgrade — both reported.
    let first = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(first.ok(), "first upgrade should succeed");
    assert_eq!(first.lock_verified(), Some(true), "first upgrade re-locks");
    assert!(
        first.summary_applied() >= 2,
        "first upgrade should apply the conflict moves, got {}",
        first.summary_applied()
    );
    let lock_after_first = fixture.read_bytes("uv.lock");

    // Second upgrade: already at the fixed point, so nothing moves and the lock is byte-identical.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point)"
    );
    let lock_after_second = fixture.read_bytes("uv.lock");
    assert_eq!(
        lock_after_first, lock_after_second,
        "lock must be byte-identical across the two converged runs"
    );
}

#[test]
fn upgrade_reports_every_moved_version_no_silent_change() {
    skip_if_missing!("uv");
    let fixture = conflict_fixture(FREEZE);

    let lock_before = fixture.read_bytes("uv.lock");
    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    let lock_after = fixture.read_bytes("uv.lock");

    // Compute the set of packages whose pinned version changed in the lock, independent of the
    // report, then assert the report's applied set covers exactly those — the forced collateral
    // huggingface-hub downgrade included, never silent.
    let moved_in_lock = changed_packages(&lock_before, &lock_after, toml_lock_pins);
    assert!(
        !moved_in_lock.is_empty(),
        "the upgrade should have moved at least one package"
    );
    let reported = report.applied_names();
    assert_eq!(
        reported, moved_in_lock,
        "report set must equal the lock-diff set (no silent change)\nreported={reported:?}\nlock-diff={moved_in_lock:?}"
    );

    // The collateral move specifically: huggingface-hub is downgraded to make room for newest
    // typer, and that downgrade is in the report.
    let (from, to) = report
        .change_for("huggingface-hub")
        .expect("huggingface-hub move reported");
    // Compare component-wise as numbers, not lexicographically: a string compare happens to order
    // 1.18.0 > 1.16.1 but would misorder e.g. 1.9.0 vs 1.10.0.
    let parts = |v: &str| -> Vec<u64> { v.split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    assert!(
        parts(&from) > parts(&to),
        "huggingface-hub should be a reported downgrade, got {from} -> {to}"
    );
}

#[test]
fn outdated_agrees_with_upgrade() {
    skip_if_missing!("uv");
    let fixture = conflict_fixture(FREEZE);

    // Converge to the fixed point first, so `outdated` and `upgrade` describe the same stable state.
    fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive"]);
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let held = upgrade.held_conflict_names();

    assert!(
        !blocked.is_empty(),
        "the conflict should leave at least one blocked dep (huggingface-hub)"
    );
    assert_eq!(
        blocked, held,
        "the set `outdated` marks blocked must equal the set `upgrade` reports held\nblocked={blocked:?}\nheld={held:?}"
    );
    assert!(
        adoptable.is_disjoint(&held),
        "nothing `outdated` calls adoptable may be held by upgrade\nadoptable={adoptable:?}\nheld={held:?}"
    );
}

#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("uv");

    // Real upgrade on one fixture.
    let real_fixture = conflict_fixture(FREEZE);
    real_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let real_held = real.held_conflict_names();

    // Dry-run on a separate identical fixture (same converged state): the held set must match and
    // the lock must be untouched by the dry run.
    let dry_fixture = conflict_fixture(FREEZE);
    dry_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let lock_before = dry_fixture.read_bytes("uv.lock");
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let dry_held = dry.held_conflict_names();
    let lock_after = dry_fixture.read_bytes("uv.lock");

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
    skip_if_missing!("uv");

    // Seed a genuinely too-fresh lock: resolved at the LATER cutoff, so several deps are newer than
    // FREEZE and are cooldown violations under it.
    let fixture = conflict_fixture(FREEZE_LATER);

    // `fix` matures the too-fresh deps down to versions at or before the freeze cutoff and re-locks.
    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert!(fixed.ok(), "fix should succeed");
    assert_eq!(fixed.lock_verified(), Some(true), "fix re-locks cleanly");
    assert!(
        fixed.summary_applied() >= 1,
        "fix should downgrade at least one too-fresh dep, got {}",
        fixed.summary_applied()
    );
    assert_eq!(fixed.summary_errors(), 0, "fix should not error");

    // check now passes: nothing in the lock is younger than the freeze cutoff.
    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(check.ok(), "check must pass after fix");
    assert_eq!(check.summary_violations(), 0, "no violations remain");

    let lock_after_fix = fixture.read_bytes("uv.lock");

    // Re-running fix is idempotent: nothing left to mature, lock byte-identical.
    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "second fix must be a no-op (idempotent)"
    );
    assert_eq!(
        lock_after_fix,
        fixture.read_bytes("uv.lock"),
        "second fix must leave the lock byte-identical"
    );
}

/// A uv workspace whose dependency is declared in a MEMBER (`packages/app`), never the virtual root.
/// uv resolves the whole workspace into one shared `uv.lock` (a single flat environment — unlike
/// cargo/pnpm it cannot hold two versions of one package), so the upgrade must reach the member's
/// declaration. This is the monorepo-conformance analog of the pnpm member-dep regression.
const WORKSPACE_ROOT_PYPROJECT: &str = r#"[tool.uv.workspace]
members = ["packages/app"]
"#;

/// The member that actually declares `certifi`, with a loose floor so an older seed is a clear
/// forward move. Declared only here — the workspace root carries no dependencies of its own.
const WORKSPACE_MEMBER_PYPROJECT: &str = r#"[project]
name = "cooldown-uv-workspace-member"
version = "0.1.0"
requires-python = ">=3.10"
dependencies = [
    "certifi>=2024.1.1",
]
"#;

/// An early seed cutoff so the member's `certifi` resolves to an old version, strictly below the
/// newest matured under `FREEZE` — a clear forward move the upgrade must land.
const WORKSPACE_SEED: &str = "2024-06-01T00:00:00Z";

fn workspace_member_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("pyproject.toml", WORKSPACE_ROOT_PYPROJECT);
    fixture.write("packages/app/pyproject.toml", WORKSPACE_MEMBER_PYPROJECT);
    fixture.write(".python-version", PYTHON_VERSION);
    seed_lock(&fixture, WORKSPACE_SEED);
    fixture
}

#[test]
fn upgrade_moves_a_member_declared_dependency() {
    skip_if_missing!("uv");
    let fixture = workspace_member_fixture();

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );

    // certifi is declared only in `packages/app`, never the workspace root. The whole-workspace
    // re-resolve MUST reach the member and move it forward.
    assert!(
        upgrade.applied_names().contains("certifi"),
        "member-declared certifi must be upgraded\napplied={:?}\nheld={:?}",
        upgrade.applied_names(),
        upgrade.held_conflict_names()
    );
    let (from, to) = upgrade
        .change_for("certifi")
        .expect("certifi should be in the report");
    assert_ne!(from, to, "certifi must move to a newer matured version");

    // The landed version is within the cooldown window (no overshoot): `check` is clean.
    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert_eq!(
        check.summary_violations(),
        0,
        "the member upgrade must leave the graph cooldown-clean"
    );

    // Converged: a second upgrade is a byte-stable no-op.
    let lock_after_first = fixture.read_bytes("uv.lock");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a fixed point"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("uv.lock"),
        "lock must be byte-identical across the two converged runs"
    );
}

#[test]
fn outdated_does_not_falsely_block_a_member_declared_dependency() {
    skip_if_missing!("uv");
    let fixture = workspace_member_fixture();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let adoptable = outdated.outdated_with_status("adoptable");
    let blocked = outdated.outdated_with_status("blocked");

    assert!(
        adoptable.contains("certifi"),
        "member-declared certifi must be adoptable\nadoptable={adoptable:?}\nblocked={blocked:?}"
    );
    assert!(
        !blocked.contains("certifi"),
        "member-declared certifi must NOT be falsely blocked\nblocked={blocked:?}"
    );
}
