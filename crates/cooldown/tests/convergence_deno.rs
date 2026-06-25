//! End-to-end convergence tests that drive the REAL `deno` resolver against fixtures generated on
//! the fly in temp dirs. These guard the Deno adapter's whole-graph re-resolve: the adapter now
//! pins every planned candidate to its EXACT per-package target in `deno.json`, settles the entire
//! graph — direct and transitive — in one `deno install --lockfile-only --minimum-dependency-age
//! <cutoff>` pass, then builds the report from the full before/after `deno.lock` diff. So a candidate
//! can never silently move another package and a converged graph re-applies to a byte-stable lock.
//!
//! # Determinism
//!
//! Unlike cargo/go/pnpm, deno has a NATIVE absolute publish-date cutoff: `--minimum-dependency-age`
//! accepts an RFC3339 instant verbatim and excludes everything published after it — the uv model. So
//! cooldown hands deno the freeze instant directly and deno windows the whole graph itself, replaying
//! the npm registry's immutable history as of that instant. Every test pins the resolution clock with
//! `--freeze <FREEZE>` (which the adapter forwards as `--minimum-dependency-age FREEZE`), so the
//! matured-version set reproduces forever. The starting lock is seeded with the real `deno` against
//! the live npm registry; the assertions check INVARIANTS (convergence, no-silent-change,
//! cross-command agreement), never hard-coded versions.
//!
//! # The fixture
//!
//! `npm:debug` (with its single transitive `ms`) is a clean, dependency-light package whose 4.3.x
//! release cadence straddles the chosen cutoffs: at `SEED_EARLY` it resolves old (4.3.2), at `FREEZE`
//! a within-major forward move is matured (4.3.6), and at `FREEZE_LATER` it is genuinely too-fresh
//! (4.4.x) so `fix` matures it back down. Deno resolves like npm — a transitive floats up unless a
//! manifest pins it, so there is no transitive `==` ceiling and therefore no structural "blocked"
//! case (the `graph_ceiling` audit is intentionally absent for the npm family, exactly as for pnpm).
//! The agreement invariant is asserted as the subset/disjoint contract pnpm uses; on this
//! conflict-free fixture both the blocked and held sets are empty, which is still agreement and guards
//! that the generic conflict path never fabricates a spurious hold.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use support::{Fixture, changed_packages, deno_lock_pins};

/// The absolute resolution cutoff. The npm registry's release history before this instant is
/// immutable, so the matured-version set reproduces forever. `debug` 4.3.6 is the newest matured here.
const FREEZE: &str = "2024-09-01T00:00:00Z";

/// An earlier cutoff used to seed a genuinely old starting lock for the upgrade tests: `debug`
/// resolves to 4.3.2 here, strictly below the 4.3.6 the `FREEZE` window admits, so the upgrade is a
/// clear within-major forward move (the `^4.0.0` range admits it, so no `--major` is needed).
const SEED_EARLY: &str = "2021-06-01T00:00:00Z";

/// A later cutoff used only to seed a too-fresh starting lock for the `fix` test: `debug` resolves to
/// 4.4.x here, which is newer than `FREEZE`, so evaluating it under `--freeze FREEZE` flags it as a
/// cooldown violation to mature down.
const FREEZE_LATER: &str = "2025-09-01T00:00:00Z";

/// The fixture manifest: a single direct `debug` on the 4.x line with a caret range, so cooldown is
/// free to float it within the major (an exact pin would be `held`, which a plan-respecting apply
/// would never touch). `debug` pulls exactly one transitive (`ms`), keeping the resolve fast and the
/// collateral diff small but non-empty.
const DENO_JSON: &str = r#"{
  "imports": {
    "debug": "npm:debug@^4.0.0"
  }
}
"#;

/// Seed a `deno.lock` by resolving the fixture under `cutoff`'s native publish-date floor, so the
/// starting state itself reproduces from the npm registry history as of `cutoff` and every seeded
/// entry is already within that window.
fn seed_lock(fixture: &Fixture, cutoff: &str) {
    fixture
        .run_tool(
            "deno",
            &[
                "install",
                "--lockfile-only",
                "--minimum-dependency-age",
                cutoff,
            ],
            &[],
        )
        .expect_success();
}

fn fixture(seed_cutoff: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("deno.json", DENO_JSON);
    seed_lock(&fixture, seed_cutoff);
    fixture
}

fn jsonc_fixture(seed_cutoff: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("deno.jsonc", DENO_JSON);
    seed_lock(&fixture, seed_cutoff);
    fixture
}

#[test]
fn check_on_jsonc_only_project_reports_the_jsonc_manifest_path() {
    skip_if_missing!("deno");
    let fixture = jsonc_fixture(SEED_EARLY);

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(
        !check.ok(),
        "Deno lock currency is unknown, so check must fail closed"
    );
    assert!(
        check.error_kinds().contains("lock_unknown"),
        "expected lock_unknown diagnostic, got {:?}",
        check.error_kinds()
    );
    assert!(
        check
            .error_paths()
            .iter()
            .any(|path| path.ends_with("deno.jsonc")),
        "jsonc-only project diagnostics must point at deno.jsonc, got {:?}",
        check.error_paths()
    );
}

#[test]
fn upgrade_converges_to_a_fixed_point() {
    skip_if_missing!("deno");
    let fixture = fixture(SEED_EARLY);

    // First upgrade: cooldown pins `debug` to its matured target and re-resolves the whole graph under
    // the freeze cutoff in one pass.
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
        Some("unknown"),
        "deno applies and re-locks, but cooldown cannot prove deno.lock currency yet"
    );
    assert!(
        first.summary_applied() >= 1,
        "first upgrade should apply the matured debug move, got {}",
        first.summary_applied()
    );
    let lock_after_first = fixture.read_bytes("deno.lock");

    // Second upgrade: already at the fixed point, so nothing moves and the lock is byte-identical.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point), no ping-pong"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("deno.lock"),
        "lock must be byte-identical across the two converged runs"
    );
}

#[test]
fn upgrade_reports_every_moved_version_no_silent_change() {
    skip_if_missing!("deno");
    let fixture = fixture(SEED_EARLY);

    let lock_before = fixture.read_bytes("deno.lock");
    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    let lock_after = fixture.read_bytes("deno.lock");

    // The set of packages whose pinned version changed in the lock, computed independently of the
    // report, must equal the report's applied set — including any collateral the joint resolve forced
    // on a transitive the plan never named (e.g. `ms`), never silent.
    let moved_in_lock = changed_packages(&lock_before, &lock_after, deno_lock_pins);
    assert!(
        !moved_in_lock.is_empty(),
        "the upgrade should have moved at least one package"
    );
    let reported = report.applied_names();
    assert_eq!(
        reported, moved_in_lock,
        "report set must equal the lock-diff set (no silent change)\nreported={reported:?}\nlock-diff={moved_in_lock:?}"
    );
}

#[test]
fn outdated_agrees_with_upgrade() {
    skip_if_missing!("deno");
    let fixture = fixture(SEED_EARLY);

    // Converge first so `outdated` and `upgrade` describe the same stable state.
    fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive"]);
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let held = upgrade.held_conflict_names();

    // Everything `upgrade` reports held, `outdated` must mark blocked (a superset is allowed). On this
    // conflict-free fixture both are empty — still agreement, and a guard that the generic conflict
    // path never fabricates a spurious hold.
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
    skip_if_missing!("deno");

    // Real upgrade converges one fixture; the held set on the converged state is the real held set.
    let real_fixture = fixture(SEED_EARLY);
    real_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let real_held = real.held_conflict_names();

    // Dry-run on a separate converged fixture: the held set must match and the lock is untouched.
    let dry_fixture = fixture(SEED_EARLY);
    dry_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let lock_before = dry_fixture.read_bytes("deno.lock");
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let dry_held = dry.held_conflict_names();
    let lock_after = dry_fixture.read_bytes("deno.lock");

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
    skip_if_missing!("deno");

    // Seed a genuinely too-fresh lock: resolved at the LATER cutoff, so `debug` is newer than FREEZE
    // and is a cooldown violation under it.
    let fixture = fixture(FREEZE_LATER);

    // `fix` matures the too-fresh dep down to a version at or before the freeze cutoff and re-locks.
    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert!(
        fixed.ok(),
        "fix should succeed: {}",
        fixture.cooldown(&["fix", "--freeze", FREEZE]).stderr_str()
    );
    assert_eq!(
        fixed.lock_status(),
        Some("unknown"),
        "deno fix re-locks, but cooldown cannot prove deno.lock currency yet"
    );
    assert_eq!(fixed.summary_errors(), 0, "fix should not error");
    assert!(
        fixed.summary_applied() >= 1,
        "fix should mature the too-fresh debug down, got {}",
        fixed.summary_applied()
    );

    let lock_after_fix = fixture.read_bytes("deno.lock");

    // Re-running fix is idempotent: nothing left to mature, lock byte-identical.
    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "second fix must be a no-op (idempotent)"
    );
    assert_eq!(
        lock_after_fix,
        fixture.read_bytes("deno.lock"),
        "second fix must leave the lock byte-identical"
    );
}

/// A deno workspace whose dependency is declared in a MEMBER (`member/deno.json`), never the root.
/// deno resolves the whole workspace into one shared `deno.lock`, so the upgrade must reach the
/// member's import. The monorepo-conformance analog of the pnpm member-dep regression.
const WORKSPACE_ROOT_DENO_JSON: &str = r#"{
  "workspace": ["./member"]
}
"#;

const WORKSPACE_MEMBER_DENO_JSON: &str = r#"{
  "name": "@cooldown/member",
  "version": "0.1.0",
  "exports": "./mod.ts",
  "imports": {
    "debug": "npm:debug@^4.0.0"
  }
}
"#;

fn workspace_member_fixture(seed_cutoff: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("deno.json", WORKSPACE_ROOT_DENO_JSON);
    fixture.write("member/deno.json", WORKSPACE_MEMBER_DENO_JSON);
    fixture.write("member/mod.ts", "");
    seed_lock(&fixture, seed_cutoff);
    fixture
}

#[test]
fn upgrade_moves_a_member_declared_dependency() {
    skip_if_missing!("deno");
    let fixture = workspace_member_fixture(SEED_EARLY);

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );

    // debug is imported only by the member, never the workspace root. The whole-workspace re-resolve
    // MUST reach the member and move it forward within its major.
    assert!(
        upgrade.applied_names().contains("debug"),
        "member-declared debug must be upgraded\napplied={:?}\nheld={:?}",
        upgrade.applied_names(),
        upgrade.held_conflict_names()
    );
    let (from, to) = upgrade
        .change_for("debug")
        .expect("debug should be in the report");
    assert_ne!(from, to, "debug must move to a newer matured version");

    // Converged: a second upgrade is a byte-stable no-op.
    let lock_after_first = fixture.read_bytes("deno.lock");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a fixed point"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("deno.lock"),
        "lock must be byte-identical across the two converged runs"
    );
}

#[test]
fn outdated_does_not_falsely_block_a_member_declared_dependency() {
    skip_if_missing!("deno");
    let fixture = workspace_member_fixture(SEED_EARLY);

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE]);
    let adoptable = outdated.outdated_with_status("adoptable");
    let blocked = outdated.outdated_with_status("blocked");

    assert!(
        adoptable.contains("debug"),
        "member-declared debug must be adoptable\nadoptable={adoptable:?}\nblocked={blocked:?}"
    );
    assert!(
        !blocked.contains("debug"),
        "member-declared debug must NOT be falsely blocked\nblocked={blocked:?}"
    );
}
