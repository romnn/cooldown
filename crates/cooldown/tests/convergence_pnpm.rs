//! End-to-end convergence tests that drive the REAL `pnpm` resolver against fixtures generated on
//! the fly in temp dirs. These guard the pnpm adapter's whole-graph re-resolve: the adapter now
//! floats the entire importer graph — direct and transitive — to the newest-within-window fixed point
//! in one joint `pnpm update --latest --lockfile-only --config.minimumReleaseAge=<m>` pass, then
//! builds the report from the full before/after `pnpm-lock.yaml` diff. So a candidate can never
//! silently move another package, mutually-exclusive peers settle at a single fixed point, and a
//! converged graph re-applies to a byte-stable lock.
//!
//! # The old bug
//!
//! The earlier adapter applied each candidate with its own `pnpm update <name>@<version> --no-save`.
//! For a transitive-only candidate that command moves nothing (pnpm only re-pins *direct*
//! dependencies by name), so a real upgrade silently did nothing; worse, a per-package update that
//! *did* re-resolve could move other packages between candidates without recording it. The whole-graph
//! pass closes both gaps.
//!
//! # Determinism
//!
//! pnpm has no absolute publish-date cutoff — only a *rolling* `minimumReleaseAge` minute count. But
//! the two coincide: excluding releases younger than `now - FREEZE` is exactly excluding releases
//! published after `FREEZE`. So the fixture seeds (and cooldown resolves) with
//! `minimumReleaseAge = now - FREEZE`, which replays the npm registry's immutable history as of the
//! freeze instant. The minute count drifts by only seconds between the seed and the cooldown run
//! (far below the day-scale window), so the matured set is stable. Assertions check INVARIANTS
//! (convergence, no-silent-change, cross-command agreement), never hard-coded versions.
//!
//! # The conflict
//!
//! Peer-dependency mutual exclusion is the canonical pnpm ping-pong source. The fixture is the
//! `eslint` v8/v9 split: the importer declares `eslint` and `@typescript-eslint/eslint-plugin`, seeded on the
//! v7/eslint-8 line. The newest-within-window `@typescript-eslint/eslint-plugin` (v8) peers on
//! `eslint: ^8.57.0 || ^9.0.0` and pulls `eslint` to v9, while the older toolchain peers on eslint 8 —
//! mutually exclusive peers the whole-graph resolve settles in one pass.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use support::{Fixture, changed_packages, pnpm_lock_pins};

/// The absolute resolution cutoff. The npm registry's release history before this instant is
/// immutable, so the matured-version set reproduces forever. At this instant the eslint v9 / typescript-eslint
/// v8 line is matured and the seed v7/eslint-8 line is upgradable.
const FREEZE: &str = "2024-08-01T00:00:00Z";

/// A later cutoff used only to seed a genuinely too-fresh starting lock for the `fix` test: deps
/// resolved here are newer than `FREEZE`, so evaluating them under it flags them as cooldown
/// violations to mature down.
const FREEZE_LATER: &str = "2024-10-01T00:00:00Z";

/// The conflict fixture manifest. `eslint` spans the v8/v9 boundary and `@typescript-eslint/eslint-plugin`
/// the v7/v8 boundary, so the within-window upgrade pulls the peer-mutually-exclusive newest of each.
/// The exact seed pins (eslint 8.40.0, plugin 7.0.0) are old enough that the matured line is a clear
/// forward move.
const PACKAGE_JSON: &str = r#"{
  "name": "cooldown-pnpm-conflict-fixture",
  "version": "0.1.0",
  "private": true,
  "dependencies": {
    "eslint": "8.40.0",
    "@typescript-eslint/eslint-plugin": "7.0.0"
  }
}
"#;

/// pnpm warns (not errors) on peer mismatches by default and auto-installs missing peers, so the joint
/// resolve can settle the eslint split rather than hard-failing on it — the realistic developer
/// configuration this test exercises.
const NPMRC: &str = "strict-peer-dependencies=false\nauto-install-peers=true\n";

/// The rolling `minimumReleaseAge` (whole minutes) that reproduces an absolute cutoff: everything
/// younger than `now - cutoff` is excluded, i.e. everything published after `cutoff`. The seed and the
/// cooldown run share the same wall clock (seconds apart), so the matured set is stable.
fn minimum_release_age_minutes(cutoff: &str) -> i64 {
    let cutoff: jiff::Timestamp = cutoff.parse().expect("cutoff parses");
    let minutes = jiff::Timestamp::now().duration_since(cutoff).as_secs() / 60;
    assert!(minutes > 0, "cutoff {cutoff} must be in the past");
    minutes
}

/// Seed a `pnpm-lock.yaml` by resolving the fixture under the freeze cutoff's `minimumReleaseAge`, so
/// the starting state itself reproduces from the registry history as of `cutoff` and every seeded
/// entry is already within the window (a plain seed would resolve to *latest* and then trip the
/// window on the first cooldown run).
fn seed_lock(fixture: &Fixture, cutoff: &str) {
    let minutes = minimum_release_age_minutes(cutoff).to_string();
    fixture
        .run_tool(
            "pnpm",
            &[
                "install",
                "--lockfile-only",
                &format!("--config.minimumReleaseAge={minutes}"),
            ],
            &[],
        )
        .expect_success();
}

fn conflict_fixture(seed_cutoff: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("package.json", PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, seed_cutoff);
    fixture
}

#[test]
fn upgrade_converges_to_a_fixed_point() {
    skip_if_missing!("pnpm");
    let fixture = conflict_fixture(FREEZE);

    // First upgrade: cooldown re-resolves the whole graph under the window in one joint pass, floating
    // direct and transitive deps to the newest-within-window set and settling the eslint peer split.
    let first = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        first.ok(),
        "first upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );
    assert_eq!(first.lock_verified(), Some(true), "first upgrade re-locks");
    assert!(
        first.summary_applied() >= 2,
        "first upgrade should apply the matured eslint/typescript-eslint line, got {}",
        first.summary_applied()
    );
    let lock_after_first = fixture.read_bytes("pnpm-lock.yaml");

    // Second upgrade: already at the fixed point, so nothing moves and the lock is byte-identical.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point), no ping-pong"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("pnpm-lock.yaml"),
        "lock must be byte-identical across the two converged runs"
    );
}

#[test]
fn upgrade_reports_every_moved_version_no_silent_change() {
    skip_if_missing!("pnpm");
    let fixture = conflict_fixture(FREEZE);

    let lock_before = fixture.read_bytes("pnpm-lock.yaml");
    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    let lock_after = fixture.read_bytes("pnpm-lock.yaml");

    // The set of packages whose pinned version changed in the lock, computed independently of the
    // report, must equal the report's applied set — including any collateral the joint resolve forced
    // on a transitive the plan never named, never silent. This is exactly the gap the old per-package
    // apply left: a transitive `pnpm update` moved (or failed to move) packages with no report row.
    let moved_in_lock = changed_packages(&lock_before, &lock_after, pnpm_lock_pins);
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
    skip_if_missing!("pnpm");
    let fixture = conflict_fixture(FREEZE);

    // Converge first so `outdated` and `upgrade` describe the same stable state.
    fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", FREEZE, "--transitive"]);
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let held = upgrade.held_conflict_names();

    // Everything `upgrade` reports held, `outdated` must mark blocked. (A duplicate graph copy held by
    // an unrelated requirer can leave `blocked` a superset, so this is a subset check, not equality.)
    assert!(
        held.is_subset(&blocked),
        "every held candidate must be blocked by outdated\nheld={held:?}\nblocked={blocked:?}"
    );
    // Nothing `outdated` calls adoptable may be one `upgrade` holds.
    assert!(
        adoptable.is_disjoint(&held),
        "nothing outdated calls adoptable may be held by upgrade\nadoptable={adoptable:?}\nheld={held:?}"
    );
}

#[test]
fn upgrade_dry_run_agrees_with_real_upgrade() {
    skip_if_missing!("pnpm");

    // Real upgrade converges one fixture; the held set on the converged state is the real held set.
    let real_fixture = conflict_fixture(FREEZE);
    real_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let real = real_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let real_held = real.held_conflict_names();

    // Dry-run on a separate converged fixture: the held set must match and the lock is untouched.
    let dry_fixture = conflict_fixture(FREEZE);
    dry_fixture
        .cooldown(&["upgrade", "--freeze", FREEZE])
        .expect_success();
    let lock_before = dry_fixture.read_bytes("pnpm-lock.yaml");
    let dry = dry_fixture.cooldown_json(&["upgrade", "--freeze", FREEZE, "--dry-run"]);
    let dry_held = dry.held_conflict_names();
    let lock_after = dry_fixture.read_bytes("pnpm-lock.yaml");

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
    skip_if_missing!("pnpm");

    // Seed a genuinely too-fresh lock: resolved at the LATER cutoff, so several deps are newer than
    // FREEZE and are cooldown violations under it.
    let fixture = conflict_fixture(FREEZE_LATER);

    // `fix` matures the too-fresh deps down to versions at or before the freeze cutoff and re-locks.
    let fixed = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert!(
        fixed.ok(),
        "fix should succeed: {}",
        fixture.cooldown(&["fix", "--freeze", FREEZE]).stderr_str()
    );
    assert_eq!(fixed.lock_verified(), Some(true), "fix re-locks cleanly");
    assert_eq!(fixed.summary_errors(), 0, "fix should not error");

    let lock_after_fix = fixture.read_bytes("pnpm-lock.yaml");

    // Re-running fix is idempotent: nothing left to mature, lock byte-identical.
    let again = fixture.cooldown_json(&["fix", "--freeze", FREEZE]);
    assert_eq!(
        again.summary_applied(),
        0,
        "second fix must be a no-op (idempotent)"
    );
    assert_eq!(
        lock_after_fix,
        fixture.read_bytes("pnpm-lock.yaml"),
        "second fix must leave the lock byte-identical"
    );
}
