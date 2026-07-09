//! End-to-end convergence tests that drive the REAL `pnpm` resolver against fixtures generated on
//! the fly in temp dirs. These guard the pnpm adapter's whole-graph re-resolve: the adapter pins each
//! eligible candidate to cooldown's exact target in one joint recursive update, then builds the report
//! from the full before/after `pnpm-lock.yaml` diff. So a candidate can never silently move another
//! package, mutually-exclusive peers settle at a single fixed point, and a converged graph re-applies
//! to a byte-stable lock.
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
/// The caret ranges (`^8.40.0`, `^7.0.0`) seed an old v7/eslint-8 line that is a clear forward move and,
/// being open ranges rather than exact pins, let cooldown actually *plan* the move (an exact pin is
/// `held`, so plan-respecting apply would never touch it). The eslint split is cross-major, so the
/// conflict tests pass `--major` to admit it.
const PACKAGE_JSON: &str = r#"{
  "name": "cooldown-pnpm-conflict-fixture",
  "version": "0.1.0",
  "private": true,
  "dependencies": {
    "eslint": "^8.40.0",
    "@typescript-eslint/eslint-plugin": "^7.0.0"
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

fn assert_pnpm_lock_current(report: &support::Envelope) {
    assert_eq!(
        report.lock_status(),
        Some("current"),
        "pnpm should prove pnpm-lock.yaml current for this run"
    );
    assert!(
        !report.warning_kinds().contains("lock_unknown"),
        "successful pnpm mutations must not emit the pre-existing-lock warning"
    );
}

fn conflict_fixture(seed_cutoff: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("package.json", PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, seed_cutoff);
    fixture
}

fn add_root_dependency(fixture: &Fixture, name: &str, spec: &str) {
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package.json")).expect("package.json parses");
    let deps = manifest
        .get_mut("dependencies")
        .and_then(serde_json::Value::as_object_mut)
        .expect("fixture has dependencies");
    deps.insert(
        name.to_string(),
        serde_json::Value::String(spec.to_string()),
    );
    let body = serde_json::to_string_pretty(&manifest).expect("manifest serializes");
    fixture.write("package.json", &format!("{body}\n"));
}

#[test]
fn check_accepts_a_current_pnpm_lock() {
    skip_if_missing!("pnpm");
    let fixture = conflict_fixture(FREEZE);

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(check.ok(), "current pnpm lock should pass check");
    assert_eq!(check.summary_errors(), 0);
    assert!(
        !check.error_kinds().contains("lock_unknown"),
        "pnpm frozen verification must not fall back to unknown lock currency"
    );
    assert!(
        !check.error_kinds().contains("stale_lock"),
        "current pnpm lock must not be reported stale"
    );
}

#[test]
fn check_reports_a_stale_pnpm_lock_from_frozen_verification() {
    skip_if_missing!("pnpm");
    let fixture = conflict_fixture(FREEZE);
    add_root_dependency(&fixture, "is-number", "^7.0.0");

    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(!check.ok(), "stale pnpm lock should fail the check gate");
    assert!(
        check.error_kinds().contains("stale_lock"),
        "stale manifest/lock mismatch must be a stale_lock error, got {:?}",
        check.error_kinds()
    );
    assert!(
        !check.error_kinds().contains("lock_unknown"),
        "pnpm should prove staleness, not report unknown lock currency"
    );
}

#[test]
fn check_lock_refreshes_a_stale_pnpm_lock_before_evaluation() {
    skip_if_missing!("pnpm");
    let fixture = conflict_fixture(FREEZE);
    let lock_before = fixture.read_bytes("pnpm-lock.yaml");
    add_root_dependency(&fixture, "is-number", "^7.0.0");

    let check = fixture.cooldown_json(&["check", "--lock", "--freeze", FREEZE]);
    assert!(check.ok(), "check --lock should refresh and then evaluate");
    assert_eq!(check.summary_errors(), 0);
    assert!(
        !check.error_kinds().contains("stale_lock")
            && !check.error_kinds().contains("lock_unknown"),
        "refreshed pnpm lock should not emit lock-currency errors: {:?}",
        check.error_kinds()
    );

    let lock_after = fixture.read_bytes("pnpm-lock.yaml");
    assert_ne!(
        lock_before, lock_after,
        "check --lock should rewrite the stale lock"
    );
    let lock_text = String::from_utf8(lock_after).expect("lock is utf8");
    assert!(
        lock_text.contains("is-number"),
        "refreshed lock should include the newly declared dependency"
    );
}

#[test]
fn upgrade_converges_to_a_fixed_point() {
    skip_if_missing!("pnpm");
    let fixture = conflict_fixture(FREEZE);

    // First upgrade: cooldown re-resolves the whole graph under the window in one joint pass, pinning
    // each planned candidate to its target and settling the cross-major eslint peer split (`--major`
    // admits the v8→v9 / v7→v8 moves).
    let first = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(
        first.ok(),
        "first upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
            .stderr_str()
    );
    assert_pnpm_lock_current(&first);
    assert!(
        first.summary_applied() >= 2,
        "first upgrade should apply the matured eslint/typescript-eslint line, got {}",
        first.summary_applied()
    );
    let lock_after_first = fixture.read_bytes("pnpm-lock.yaml");

    // Second upgrade: already at the fixed point, so nothing moves and the lock is byte-identical.
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
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
    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
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
        .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
        .expect_success();

    let outdated =
        fixture.cooldown_json(&["outdated", "--major", "--freeze", FREEZE, "--transitive"]);
    let blocked = outdated.outdated_with_status("blocked");
    let adoptable = outdated.outdated_with_status("adoptable");

    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE, "--dry-run"]);
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
        .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
        .expect_success();
    let real = real_fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE, "--dry-run"]);
    let real_held = real.held_conflict_names();

    // Dry-run on a separate converged fixture: the held set must match and the lock is untouched.
    let dry_fixture = conflict_fixture(FREEZE);
    dry_fixture
        .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
        .expect_success();
    let lock_before = dry_fixture.read_bytes("pnpm-lock.yaml");
    let dry = dry_fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE, "--dry-run"]);
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
        dry.lock_status(),
        None,
        "--dry-run never re-locks, so lockStatus is null"
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
    assert_pnpm_lock_current(&fixed);
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

/// The per-package-window fixture's manifest: a single direct `eslint` on the v9 line with a caret
/// range, so the upgrade is free to float it within v9 (an exact pin would be `held`). eslint's dense
/// 2024 release cadence (a minor every ~2 weeks) makes the project-default window and a stricter
/// per-package window admit *different* newest versions, which is the whole point of the test.
const PERPKG_PACKAGE_JSON: &str = r#"{
  "name": "cooldown-pnpm-perpkg-fixture",
  "version": "0.1.0",
  "private": true,
  "dependencies": {
    "eslint": "^9.0.0"
  }
}
"#;

/// The seed cutoff for the per-package fixture: old enough that the seeded `eslint` (9.0.0, published
/// 2024-04-05) sits *below* both the project-default and the stricter per-package target, so the
/// upgrade is a clear forward move under either window.
const PERPKG_SEED: &str = "2024-04-10T00:00:00Z";

/// The project-default resolution cutoff for the per-package fixture. eslint 9.5.0 (2024-06-14) is the
/// newest matured under this window, so a uniform run would land 9.5.0 — the version the stricter
/// per-package window must hold the package *below*.
const PERPKG_PROJECT_FREEZE: &str = "2024-07-01T00:00:00Z";

/// The stricter per-package cutoff for `eslint`. It is earlier than the project default, so eslint's
/// own window admits only up to 9.4.0 (2024-05-31) — strictly older than the 9.5.0 the project-default
/// window admits. Expressed in the config as a `min-age` (the per-package window knob), computed at
/// run time as the day-count reproducing this absolute instant, so the matured target is deterministic
/// regardless of when the test runs.
const PERPKG_STRICT_FREEZE: &str = "2024-06-05T00:00:00Z";

/// The eslint version the stricter per-package window admits (newest matured on or before
/// `PERPKG_STRICT_FREEZE`). The project-default window admits a *newer* one, so landing here proves the
/// per-package target — not the global-window-newest — is what the resolve pinned.
const PERPKG_STRICT_TARGET: &str = "9.4.0";

/// The eslint version the *project-default* window admits — strictly newer than the stricter
/// per-package target. The fix is correct only if the resolve does NOT overshoot onto this version.
const PERPKG_PROJECT_NEWEST: &str = "9.5.0";

/// Whole days (rounded down) from `cutoff` to now — the `min-age` value that reproduces an absolute
/// cutoff as a rolling window. eslint's releases are ~2 weeks apart and `cutoff` sits mid-gap, so the
/// day-granularity rounding never drifts the matured set across a release boundary.
fn min_age_days(cutoff: &str) -> i64 {
    let cutoff: jiff::Timestamp = cutoff.parse().expect("cutoff parses");
    let days = jiff::Timestamp::now().duration_since(cutoff).as_secs() / (24 * 60 * 60);
    assert!(days > 0, "cutoff {cutoff} must be in the past");
    days
}

/// A pnpm project with a `cooldown.toml` that sets the project-default window (a `freeze`) and gives
/// `eslint` a *stricter* per-package `min-age`. Both rules live in the same config layer, so the
/// eslint-specific selector beats the bare default by specificity — eslint resolves under its stricter
/// window while everything else uses the project default. (The project default is a config `freeze`,
/// not a CLI `--freeze`: a CLI flag is the highest-authority layer and would override the per-package
/// rule, which is exactly the overshoot this test guards against.)
fn perpkg_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("package.json", PERPKG_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    let config = format!(
        "freeze = \"{PERPKG_PROJECT_FREEZE}\"\n\n[package.\"eslint\"]\nmin-age = \"{}d\"\n",
        min_age_days(PERPKG_STRICT_FREEZE),
    );
    fixture.write("cooldown.toml", &config);
    seed_lock(&fixture, PERPKG_SEED);
    fixture
}

#[test]
fn upgrade_honors_a_stricter_per_package_window() {
    skip_if_missing!("pnpm");
    let fixture = perpkg_fixture();

    // The upgrade re-resolves the whole graph, pinning each candidate to its own per-package target.
    // eslint's stricter window admits only 9.4.0, so it must land there — NOT the 9.5.0 the
    // project-default window would admit. A bare `--latest --config.minimumReleaseAge=<global>` resolve
    // (the old behavior) would overshoot eslint onto 9.5.0, leaving it in violation of its own window.
    let upgrade = fixture.cooldown_json(&["upgrade"]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture.cooldown(&["upgrade"]).stderr_str()
    );
    assert_pnpm_lock_current(&upgrade);

    let (from, to) = upgrade
        .change_for("eslint")
        .expect("eslint should be in the report");
    assert_eq!(from, "9.0.0", "eslint started at the seeded 9.0.0");
    assert_eq!(
        to, PERPKG_STRICT_TARGET,
        "eslint must land at its stricter per-package target {PERPKG_STRICT_TARGET}, not the \
         project-default-window newest {PERPKG_PROJECT_NEWEST}"
    );

    // The committed lock pins exactly the per-package target — the resolve never overshot.
    let lock_pins = pnpm_lock_pins(&fixture.read_bytes("pnpm-lock.yaml"));
    assert_eq!(
        lock_pins.get("eslint").map(String::as_str),
        Some(PERPKG_STRICT_TARGET),
        "the lock must hold eslint at {PERPKG_STRICT_TARGET}"
    );

    // With eslint at its own target and every transitive within the project-default window, the graph
    // is cooldown-clean: `check` reports zero violations. (The old overshoot left eslint one minor
    // too fresh, which `check` would have flagged.)
    let check = fixture.cooldown_json(&["check"]);
    assert_eq!(
        check.summary_violations(),
        0,
        "check must report zero violations after the per-package-correct upgrade"
    );

    // A second upgrade is a fixed point: eslint is already at its target, nothing moves, and the lock
    // is byte-identical — no ping-pong between the per-package target and the global-window-newest.
    let lock_after_first = fixture.read_bytes("pnpm-lock.yaml");
    let second = fixture.cooldown_json(&["upgrade"]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point)"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("pnpm-lock.yaml"),
        "the lock must be byte-identical across the two converged runs"
    );
}

/// A workspace whose dependency is declared in a MEMBER (`pkgs/app`), never the root `package.json`.
/// This is the monorepo-conformance fixture: the earlier adapter ran `pnpm update <pkg>@<target>`
/// without `--recursive`, which at the workspace root only re-pins root-declared dependencies — so a
/// member-declared candidate silently stayed put and `outdated` reported it falsely `blocked`.
const WORKSPACE_ROOT_PACKAGE_JSON: &str = r#"{
  "name": "cooldown-pnpm-workspace-root",
  "version": "0.1.0",
  "private": true
}
"#;

const WORKSPACE_YAML: &str = "packages:\n  - \"pkgs/*\"\n";

/// The member that actually declares `eslint`. Seeded on the old 9.0.0 line (a clear forward move to
/// the project-default-window newest), declared only here — not in the workspace root.
const WORKSPACE_MEMBER_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app",
  "version": "0.1.0",
  "dependencies": {
    "eslint": "^9.0.0"
  }
}
"#;

fn workspace_member_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("package.json", WORKSPACE_ROOT_PACKAGE_JSON);
    fixture.write("pnpm-workspace.yaml", WORKSPACE_YAML);
    fixture.write("pkgs/app/package.json", WORKSPACE_MEMBER_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    // Reuse the per-package fixture's eslint timeline: seed at 9.0.0, resolve at the project-default
    // freeze whose newest matured eslint is 9.5.0 — a forward move the upgrade must land.
    seed_lock(&fixture, PERPKG_SEED);
    fixture
}

#[test]
fn upgrade_moves_a_member_declared_dependency() {
    skip_if_missing!("pnpm");
    let fixture = workspace_member_fixture();

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", PERPKG_PROJECT_FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", PERPKG_PROJECT_FREEZE])
            .stderr_str()
    );

    // eslint is declared only in `pkgs/app`, never the root. The whole-graph `--recursive` resolve
    // MUST reach the member and move it; the pre-fix adapter left it untouched (and never reported it
    // applied), which is exactly the member-dep regression this guards.
    assert!(
        upgrade.applied_names().contains("eslint"),
        "member-declared eslint must be upgraded\napplied={:?}\nheld={:?}",
        upgrade.applied_names(),
        upgrade.held_conflict_names()
    );
    let (from, to) = upgrade
        .change_for("eslint")
        .expect("eslint should be in the report");
    assert_eq!(from, "9.0.0", "eslint started at the seeded 9.0.0");
    assert!(
        to.starts_with("9.") && to != "9.0.0",
        "member eslint must move forward within its major, got {to}"
    );

    // The committed lock holds exactly the reported target — the resolve reached the member's pin.
    let pins = pnpm_lock_pins(&fixture.read_bytes("pnpm-lock.yaml"));
    assert_eq!(
        pins.get("eslint").map(String::as_str),
        Some(to.as_str()),
        "the lock must hold the member's eslint at the reported target {to}"
    );

    // The landed version is within the cooldown window (not an overshoot): `check` is clean.
    let check = fixture.cooldown_json(&["check", "--freeze", PERPKG_PROJECT_FREEZE]);
    assert_eq!(
        check.summary_violations(),
        0,
        "the member upgrade must leave the graph cooldown-clean"
    );

    // Converged: a second upgrade is a byte-stable no-op.
    let lock_after_first = fixture.read_bytes("pnpm-lock.yaml");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", PERPKG_PROJECT_FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a fixed point"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("pnpm-lock.yaml"),
        "lock must be byte-identical across the two converged runs"
    );
}

#[test]
fn outdated_does_not_falsely_block_a_member_declared_dependency() {
    skip_if_missing!("pnpm");
    let fixture = workspace_member_fixture();

    let outdated = fixture.cooldown_json(&["outdated", "--freeze", PERPKG_PROJECT_FREEZE]);
    let adoptable = outdated.outdated_with_status("adoptable");
    let blocked = outdated.outdated_with_status("blocked");

    // The whole-graph verify resolve lands the member-declared eslint, so `outdated` must call it
    // adoptable — never `blocked`. Before the `--recursive` fix the verify resolve could not move the
    // member dep, so every member candidate fell into `blocked`.
    assert!(
        adoptable.contains("eslint"),
        "member-declared eslint must be adoptable\nadoptable={adoptable:?}\nblocked={blocked:?}"
    );
    assert!(
        !blocked.contains("eslint"),
        "member-declared eslint must NOT be falsely blocked\nblocked={blocked:?}"
    );
}

/// Two members that declare the SAME dependency at DIFFERENT majors. pnpm keeps both lines (like
/// cargo, unlike uv's single flat environment), so the whole-graph resolve must preserve them:
/// exact-pinning one target across the workspace would collapse every other copy onto it.
const MULTI_VERSION_A_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app-v4",
  "version": "0.1.0",
  "dependencies": {
    "chalk": "^4.1.0"
  }
}
"#;

const MULTI_VERSION_B_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app-v5",
  "version": "0.1.0",
  "dependencies": {
    "chalk": "^5.0.0"
  }
}
"#;

/// An early seed so the chalk v5 line has a clear within-window forward move (the v4 line is already at
/// its final 4.1.2). Both majors are present in the seed lock.
const MULTI_VERSION_SEED: &str = "2022-06-01T00:00:00Z";

fn multi_version_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("package.json", WORKSPACE_ROOT_PACKAGE_JSON);
    fixture.write("pnpm-workspace.yaml", WORKSPACE_YAML);
    fixture.write("pkgs/a/package.json", MULTI_VERSION_A_PACKAGE_JSON);
    fixture.write("pkgs/b/package.json", MULTI_VERSION_B_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, MULTI_VERSION_SEED);
    fixture
}

/// Whether the lock holds at least one `chalk` package key on the given major line (`"4."`/`"5."`).
fn lock_has_chalk_major(lock: &[u8], major_prefix: &str) -> bool {
    String::from_utf8_lossy(lock).lines().any(|line| {
        line.trim_start()
            .starts_with(&format!("chalk@{major_prefix}"))
    })
}

#[test]
fn upgrade_preserves_distinct_versions_across_members() {
    skip_if_missing!("pnpm");
    let fixture = multi_version_fixture();

    // Sanity: the seed holds both major lines.
    let seed_lock = fixture.read_bytes("pnpm-lock.yaml");
    assert!(
        lock_has_chalk_major(&seed_lock, "4."),
        "seed must hold a chalk v4 line"
    );
    assert!(
        lock_has_chalk_major(&seed_lock, "5."),
        "seed must hold a chalk v5 line"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );

    // BOTH lines must survive: the v4 importer keeps a chalk v4, the v5 importer a chalk v5. The
    // pre-fix exact-pin (`pnpm update chalk@<a> chalk@<b> --no-save`) collapsed every copy onto a
    // single target, erasing one line.
    let after = fixture.read_bytes("pnpm-lock.yaml");
    assert!(
        lock_has_chalk_major(&after, "4."),
        "chalk v4 line must survive the upgrade"
    );
    assert!(
        lock_has_chalk_major(&after, "5."),
        "chalk v5 line must survive the upgrade"
    );
}

/// `sync` bakes the cooldown.toml policy into pnpm's native config: the default `min-age` becomes
/// `minimumReleaseAge`, AND every `[package."…"] latest` selector becomes an entry in
/// `minimumReleaseAgeExclude` — so a package cooldown's own policy exempts is also exempt from pnpm's
/// rolling gate (otherwise the native window would keep quarantining a `latest`-pinned package, the
/// `@typescript/native-preview` nightly problem). `sync` writes the native YAML directly (no resolver
/// run), but the fixture still needs the pnpm project marker for discovery.
#[test]
fn sync_writes_minimum_release_age_exclude_for_latest_packages() {
    skip_if_missing!("pnpm");
    let fixture = Fixture::new();
    fixture.write(
        "package.json",
        "{\n  \"name\": \"cooldown-sync-fixture\",\n  \"version\": \"0.1.0\",\n  \"private\": true\n}\n",
    );
    fixture.write("pnpm-workspace.yaml", "packages: []\n");
    fixture.write("pnpm-lock.yaml", "lockfileVersion: '9.0'\n");
    fixture.write(
        "cooldown.toml",
        "min-age = \"14d\"\n\n[package.\"@typescript/native-preview\"]\nlatest = true\n",
    );

    let report = fixture.cooldown_json(&["sync", "--tool", "pnpm"]);
    assert!(
        report.ok(),
        "sync should succeed: {}",
        fixture.cooldown(&["sync", "--tool", "pnpm"]).stderr_str()
    );

    let yaml = String::from_utf8(fixture.read_bytes("pnpm-workspace.yaml")).expect("utf8");
    assert!(
        yaml.contains("minimumReleaseAge: 20160"),
        "the default 14d window is synced as minutes: {yaml}"
    );
    assert!(
        yaml.contains("minimumReleaseAgeExclude:") && yaml.contains("@typescript/native-preview"),
        "the latest-exempt package is written to the native exemption list: {yaml}"
    );
}
