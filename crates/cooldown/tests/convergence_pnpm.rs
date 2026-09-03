//! End-to-end convergence tests that drive the REAL `pnpm` resolver against fixtures generated on
//! the fly in temp dirs. These guard the pnpm adapter's whole-graph re-resolve: the adapter pins each
//! eligible importer-declared candidate to cooldown's exact target in one joint importer-filtered
//! update — a candidate no importer declares takes the temporary qualified-override leg instead —
//! then builds the report from the full before/after `pnpm-lock.yaml` diff. So a candidate can
//! never silently move another package, mutually-exclusive peers settle at a single fixed point,
//! and a converged graph re-applies to a byte-stable lock.
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
//! `minimumReleaseAge = now - FREEZE`, which replays the npm registry's publish history as of the
//! freeze instant (append-mostly: only an unpublish of a fixture dep could change it). The minute
//! count drifts by only seconds between the seed and the cooldown run (far below the day-scale
//! window), so the matured set is stable. The registry's other live input — the mutable `latest`
//! dist-tag, which a maintainer can move at any time — is decoupled by running every fixture
//! [`tag_independent`](support::Fixture::tag_independent) (`--no-respect-dist-tags`); only the
//! dedicated `convergence_pnpm_dist_tags` probe couples to it. Assertions check INVARIANTS
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

use color_eyre::eyre;
use support::{ChangeVersions, Fixture, changed_packages, pnpm_lock_pins};

/// The absolute resolution cutoff. The npm registry's publish history before this instant is
/// append-mostly (only an unpublish of a fixture dep could change it), so the matured-version set
/// is stable across runs. At this instant the eslint v9 / typescript-eslint v8 line is matured and
/// the seed v7/eslint-8 line is upgradable.
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
    let fixture = Fixture::new().tag_independent();
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
    // The error names the lockfile and its project ahead of pnpm's own failure text, so a
    // repository with several locks knows which one pnpm rejected.
    assert!(
        check
            .error_messages()
            .iter()
            .any(|message| message.contains("pnpm-lock.yaml is stale in ")),
        "the stale-lock error must name the project whose lock is stale, got {:?}",
        check.error_messages()
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
fn upgrade_advances_a_matured_transitive_no_importer_declares() {
    skip_if_missing!("pnpm");
    // The dompurify/quinn-proto class: `agent-base` is exact-pinned so the direct layer is inert,
    // and no importer declares its transitive `debug` (declared range `^4.3.4`). Seeding under
    // FREEZE locks debug at 4.3.6 (2024-07-27, the newest release then);
    // 4.3.7 (2024-09-06) matures under FREEZE_LATER while 4.4.0 (2024-12-06) stays outside it.
    // `pnpm update debug@…` cannot reach an undeclared package (named selectors match direct
    // dependencies only, `--depth` notwithstanding), so only the temporary qualified-override leg
    // can advance it.
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "package.json",
        r#"{ "name": "transitive-advance", "private": true, "dependencies": { "agent-base": "7.1.1" } }"#,
    );
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, FREEZE);
    let seeded = String::from_utf8(fixture.read_bytes("pnpm-lock.yaml")).expect("lock is utf-8");
    assert!(
        seeded.contains("debug@4.3.6"),
        "seed sanity: registry history as of FREEZE locks debug at 4.3.6"
    );

    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE_LATER]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report.applied_names().contains("debug"),
        "the transitive advance must be its own applied row, applied={:?}",
        report.applied_names()
    );
    let lock = String::from_utf8(fixture.read_bytes("pnpm-lock.yaml")).expect("lock is utf-8");
    assert!(
        lock.contains("debug@4.3.7"),
        "debug advances to the newest release matured under FREEZE_LATER"
    );
    assert!(
        !lock.contains("overrides:"),
        "the temporary override must not persist in the settled lock"
    );

    // Converged: a second run under the same freeze plans nothing new.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE_LATER]);
    assert!(second.ok(), "converged re-run should succeed");
    assert!(
        second.applied_names().is_empty(),
        "a converged graph re-applies to a fixed point, applied={:?}",
        second.applied_names()
    );
}

/// The settlement's self-validation: a transitive advance whose target a dependent's declared
/// range excludes must NOT survive the temporary override — the override-free settlement reverts
/// the pin instead of committing a broken constraint, and the row reports the hold truthfully.
#[test]
fn upgrade_reverts_a_transitive_advance_a_dependents_exact_pin_excludes() {
    skip_if_missing!("pnpm");
    // A first-party `file:` dependency exact-pins `debug 4.3.6`, standing in for the
    // monaco-editor/dompurify shape: no importer declares debug, so the advance rides the
    // qualified-override leg; 4.3.7 matures under FREEZE_LATER, the override forces it, and the
    // settlement must take it back — 4.3.6 is the only version the pinner admits.
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "package.json",
        r#"{ "name": "settlement-revert", "private": true, "dependencies": { "pinner": "file:./pinner" } }"#,
    );
    fixture.write(
        "pinner/package.json",
        r#"{ "name": "pinner", "version": "1.0.0", "dependencies": { "debug": "4.3.6" } }"#,
    );
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, FREEZE);
    let seeded = String::from_utf8(fixture.read_bytes("pnpm-lock.yaml")).expect("lock is utf-8");
    assert!(
        seeded.contains("debug@4.3.6"),
        "seed sanity: the exact pin locks debug at 4.3.6"
    );

    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE_LATER]);
    assert!(report.ok(), "a held advance is a skip, not an error");
    assert!(
        report.held_conflict_names().contains("debug"),
        "the reverted advance is a held row, held={:?}",
        report.held_conflict_names()
    );
    let detail = report.skip_detail_for("debug").unwrap_or_default();
    assert!(
        detail.contains("holds it at 4.3.6"),
        "the hold names where the settlement actually left the copy: {detail}"
    );
    let lock = String::from_utf8(fixture.read_bytes("pnpm-lock.yaml")).expect("lock is utf-8");
    assert!(
        lock.contains("debug@4.3.6") && !lock.contains("debug@4.3.7"),
        "the settlement reverts the out-of-range advance instead of committing it"
    );
    assert!(
        !lock.contains("overrides:"),
        "the temporary override must not persist in the settled lock"
    );

    // Converged: the held advance stays held, nothing oscillates.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE_LATER]);
    assert!(second.ok(), "converged re-run should succeed");
    assert!(
        second.applied_names().is_empty(),
        "a held advance must not oscillate into an applied row, applied={:?}",
        second.applied_names()
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
    assert!(fixed.ok(), "fix should succeed: {fixed:#?}");
    assert_pnpm_lock_current(&fixed);
    assert_eq!(fixed.summary_errors(), 0, "fix should not error");
    let check = fixture.cooldown_json(&["check", "--freeze", FREEZE]);
    assert!(check.ok(), "fix must leave a policy-clean graph");
    assert_eq!(check.summary_violations(), 0);

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

fn native_minimum_age_migration_fixture() -> eyre::Result<(Fixture, Vec<u8>)> {
    // Seed without a persistent native policy so the lock can contain versions newer than FREEZE.
    // `nanoid` remains intentionally exempt after sync; the repair must preserve that native
    // exemption while adding exact allowances for the versions it is repairing. TypeScript is
    // pinned to an older, still-in-range version so `outdated` has one independently adoptable
    // candidate to verify against the rejected starting lock.
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    add_root_dependency(&fixture, "nanoid", "^3.3.0");
    add_root_dependency(&fixture, "typescript", "^5.0.0");
    let seed_minutes = minimum_release_age_minutes(FREEZE_LATER).to_string();
    fixture
        .run_tool_traced(
            "seed pre-policy lock",
            "pnpm",
            &[
                "install",
                "--lockfile-only",
                &format!("--config.minimumReleaseAge={seed_minutes}"),
            ],
            &[],
        )?
        .require_success()?;
    fixture
        .run_tool_traced(
            "pin independently adoptable TypeScript",
            "pnpm",
            &[
                "update",
                "typescript@5.4.5",
                "--lockfile-only",
                "--no-save",
                &format!("--config.minimumReleaseAge={seed_minutes}"),
            ],
            &[],
        )?
        .require_success()?;
    fixture.write("cooldown.toml", "[package.nanoid]\nlatest = true\n");
    let minimum_age_minutes = minimum_release_age_minutes(FREEZE);
    let min_age = format!("{minimum_age_minutes}m");

    // Activating the native gate after resolution makes pnpm reject the existing lock. This is the
    // migration state `fix` must cross rather than misclassifying every downgrade as a conflict.
    fixture
        .cooldown_traced(
            "activate native minimum age",
            &["sync", "--tool", "pnpm", "--min-age", &min_age],
        )?
        .require_success()?;
    let native_before_fix = fixture.read_bytes("pnpm-workspace.yaml");
    let native = String::from_utf8(native_before_fix.clone())?;
    assert!(
        native.contains(&format!("minimumReleaseAge: {minimum_age_minutes}")),
        "sync must persist the requested native release-age gate: {native}"
    );
    assert!(
        native.contains("minimumReleaseAgeExclude:") && native.contains("nanoid"),
        "sync must persist the package exemption the repair needs to preserve: {native}"
    );
    Ok((fixture, native_before_fix))
}

fn assert_native_gate_rejects_starting_lock(fixture: &Fixture) -> eyre::Result<()> {
    let lock_before_rejection = fixture.read_bytes("pnpm-lock.yaml");
    let rejected = fixture.run_tool_traced(
        "verify the starting lock is rejected",
        "pnpm",
        &["install", "--lockfile-only"],
        &[],
    )?;
    let rejection = format!("{}\n{}", rejected.stdout_str(), rejected.stderr_str());
    assert!(
        !rejected.status.success(),
        "the fixture must reproduce pnpm's rejected-lock migration state"
    );
    assert!(
        rejection.contains("[ERR_PNPM_MINIMUM_RELEASE_AGE_VIOLATION]"),
        "pnpm must reject the starting lock for the reason under test: {rejection}"
    );
    assert_eq!(
        lock_before_rejection,
        fixture.read_bytes("pnpm-lock.yaml"),
        "the raw preflight probe must leave the rejected lock unchanged"
    );
    Ok(())
}

/// `outdated` and `fix` recover after `sync` activates pnpm's native release-age gate.
#[test]
fn outdated_and_fix_recover_after_sync_activates_native_minimum_age() -> eyre::Result<()> {
    skip_if_missing!("pnpm", Ok(()));

    let (fixture, native_before_fix) = native_minimum_age_migration_fixture()?;
    assert_native_gate_rejects_starting_lock(&fixture)?;

    let outdated = fixture.cooldown_json_traced(
        "evaluate outdated from rejected lock",
        &["outdated", "--freeze", FREEZE],
    )?;
    assert!(outdated.ok(), "outdated should complete: {outdated:#?}");
    assert!(
        outdated
            .outdated_with_status("adoptable")
            .contains("typescript"),
        "the rejected baseline must not make the valid TypeScript update look blocked: {outdated:#?}"
    );
    assert!(
        !outdated
            .outdated_with_status("blocked")
            .contains("typescript"),
        "the valid TypeScript update must not be blocked by the starting-lock preflight"
    );

    let fixed =
        fixture.cooldown_json_traced("repair the rejected lock", &["fix", "--freeze", FREEZE])?;
    assert!(
        fixed.ok(),
        "fix should repair the pre-policy lock: {fixed:#?}"
    );
    assert_pnpm_lock_current(&fixed);
    assert!(
        fixed.summary_applied() > 0,
        "the too-new seed must require at least one downgrade"
    );
    assert_eq!(
        fixed.summary_skipped(),
        0,
        "every planned repair should land"
    );
    assert_eq!(fixed.summary_errors(), 0, "fix should not error");
    assert_eq!(
        native_before_fix,
        fixture.read_bytes("pnpm-workspace.yaml"),
        "temporary repair overrides must not leak into native config"
    );

    let check =
        fixture.cooldown_json_traced("verify the repaired lock", &["check", "--freeze", FREEZE])?;
    assert!(check.ok(), "the repaired lock should satisfy the policy");
    assert_eq!(check.summary_violations(), 0);
    Ok(())
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
    let fixture = Fixture::new().tag_independent();
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

    let ChangeVersions { from, to } = upgrade
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

/// A workspace whose dependency is declared in a member with YAML quoting, spaces, and filter
/// metacharacters in its path (`'app [literal]`), never the root `package.json`. The resolver must
/// select that importer explicitly; a root-only update leaves the candidate in place and makes
/// `outdated` report it falsely `blocked`.
const WORKSPACE_ROOT_PACKAGE_JSON: &str = r#"{
  "name": "cooldown-pnpm-workspace-root",
  "version": "0.1.0",
  "private": true
}
"#;

/// A workspace root that alone declares `eslint`, so cooldown must select the root importer without
/// running the update in unrelated members.
const WORKSPACE_ROOT_DEP_PACKAGE_JSON: &str = r#"{
  "name": "cooldown-pnpm-workspace-root-dep",
  "version": "0.1.0",
  "private": true,
  "dependencies": {
    "eslint": "^9.0.0"
  }
}
"#;

const WORKSPACE_YAML: &str = "packages:\n  - \"pkgs/*\"\n";
const COMPLEX_MEMBER_WORKSPACE_YAML: &str = "packages:\n  - \"*\"\n";

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

/// An unrelated member held on the `vite` v4 line while v5 is available at the target cutoff. A
/// broad recursive update can force it across the declared range and make the lock inconsistent.
const WORKSPACE_UNRELATED_MEMBER_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/unrelated",
  "version": "0.1.0",
  "private": true,
  "dependencies": {
    "vite": "^4.0.0"
  }
}
"#;

fn workspace_member_fixture() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", WORKSPACE_ROOT_PACKAGE_JSON);
    fixture.write("pnpm-workspace.yaml", COMPLEX_MEMBER_WORKSPACE_YAML);
    fixture.write("'app [literal]/package.json", WORKSPACE_MEMBER_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    // Reuse the per-package fixture's eslint timeline: seed at 9.0.0, resolve at the project-default
    // freeze whose newest matured eslint is 9.5.0 — a forward move the upgrade must land.
    seed_lock(&fixture, PERPKG_SEED);
    fixture
}

fn workspace_root_dependency_fixture() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", WORKSPACE_ROOT_DEP_PACKAGE_JSON);
    fixture.write("pnpm-workspace.yaml", WORKSPACE_YAML);
    fixture.write(
        "pkgs/unrelated/package.json",
        WORKSPACE_UNRELATED_MEMBER_PACKAGE_JSON,
    );
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, PERPKG_SEED);
    fixture
}

#[test]
fn upgrade_moves_a_root_declared_dependency_in_a_workspace() {
    skip_if_missing!("pnpm");
    let fixture = workspace_root_dependency_fixture();

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", PERPKG_PROJECT_FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", PERPKG_PROJECT_FREEZE])
            .stderr_str()
    );
    assert_pnpm_lock_current(&upgrade);

    // The adapter must select the root importer explicitly so a successful resolver invocation
    // cannot leave this root-only dependency unchanged.
    assert!(
        upgrade.applied_names().contains("eslint"),
        "root-declared eslint must be upgraded\napplied={:?}\nheld={:?}",
        upgrade.applied_names(),
        upgrade.held_conflict_names()
    );
    let ChangeVersions { from, to } = upgrade
        .change_for("eslint")
        .expect("eslint should be in the report");
    assert_eq!(from, "9.0.0", "eslint started at the seeded 9.0.0");
    assert!(
        to.starts_with("9.") && to != "9.0.0",
        "root eslint must move forward within its major, got {to}"
    );

    // Reading the lock independently proves the reported application reached the root importer.
    let pins = pnpm_lock_pins(&fixture.read_bytes("pnpm-lock.yaml"));
    assert_eq!(
        pins.get("eslint").map(String::as_str),
        Some(to.as_str()),
        "the lock must hold the root's eslint at the reported target {to}"
    );
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

    // eslint is declared only in the complex-path member, never the root. The location-filtered
    // resolve must reach the member and report the lock movement.
    assert!(
        upgrade.applied_names().contains("eslint"),
        "member-declared eslint must be upgraded\napplied={:?}\nheld={:?}",
        upgrade.applied_names(),
        upgrade.held_conflict_names()
    );
    let ChangeVersions { from, to } = upgrade
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

    // Policy verification runs in a copied workspace. Its canonical root plus the portable location
    // selector must identify the same member on every platform, so eslint remains adoptable.
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
    let fixture = Fixture::new().tag_independent();
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
    let fixture = Fixture::new().tag_independent();
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

/// The peer-hold fixture: `eslint` on an open v8 range while `@typescript-eslint/eslint-plugin`
/// is exact-pinned to `6.21.0`, whose peer range `eslint: ^7.0.0 || ^8.0.0` excludes eslint 9.
/// At `FREEZE` the eslint 9.x line has long matured, so a `--major` upgrade plans the cross-major
/// move — exactly the trap: pnpm itself only *warns* on the peer mismatch
/// (`strict-peer-dependencies=false`, the realistic default) and would land the break silently.
/// The exact pin keeps the plugin in place (`held`), so its lock-recorded peer range stays the
/// authoritative constraint the gate judges.
const PEER_HELD_PACKAGE_JSON: &str = indoc::indoc! {r#"
    {
      "name": "cooldown-pnpm-peer-held-fixture",
      "version": "0.1.0",
      "private": true,
      "dependencies": {
        "eslint": "^8.40.0",
        "@typescript-eslint/eslint-plugin": "6.21.0"
      }
    }
"#};

fn peer_held_fixture(seed_cutoff: &str) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", PEER_HELD_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, seed_cutoff);
    fixture
}

/// The lock's resolved eslint pin, for asserting the major line never moved.
fn locked_eslint(fixture: &Fixture) -> String {
    pnpm_lock_pins(&fixture.read_bytes("pnpm-lock.yaml"))
        .get("eslint")
        .cloned()
        .expect("eslint is pinned in the lock")
}

#[test]
fn upgrade_holds_a_cross_major_move_a_dependents_peer_range_excludes() {
    skip_if_missing!("pnpm");
    let fixture = peer_held_fixture(FREEZE);
    assert!(
        locked_eslint(&fixture).starts_with("8."),
        "the seed must start on the eslint v8 line"
    );

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(
        report.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
            .stderr_str()
    );

    // The cross-major eslint move is held up front with the dependent named — never attempted, so
    // the lock keeps eslint on v8 (without the gate pnpm lands eslint 9 with only a warning).
    let reasons = report.skipped_reasons_for("eslint");
    assert!(
        reasons.contains("peer_held"),
        "eslint must be peer-held, got {reasons:?}"
    );
    assert_eq!(
        report.skipped_offending_for("eslint").as_deref(),
        Some("@typescript-eslint/eslint-plugin"),
        "the hold names the peer-declaring dependent"
    );
    assert!(
        locked_eslint(&fixture).starts_with("8."),
        "eslint must stay on the v8 line, got {}",
        locked_eslint(&fixture)
    );

    // Convergence: a second run reports the same hold and moves nothing further.
    let lock_after_first = fixture.read_bytes("pnpm-lock.yaml");
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (fixed point)"
    );
    assert!(
        second.skipped_reasons_for("eslint").contains("peer_held"),
        "the hold is reported consistently across converged runs"
    );
    assert_eq!(
        lock_after_first,
        fixture.read_bytes("pnpm-lock.yaml"),
        "lock must be byte-identical across the converged runs"
    );
}

#[test]
fn outdated_reports_the_peer_held_target_as_blocked_by_the_dependent() {
    skip_if_missing!("pnpm");
    let fixture = peer_held_fixture(FREEZE);

    // `outdated`'s whole-graph verification must agree with `upgrade`: the matured cross-major
    // eslint is not advertised `adoptable` — it is `blocked`, naming the peer-declaring dependent.
    let outdated = fixture.cooldown_json(&["outdated", "--major", "--freeze", FREEZE]);
    assert!(outdated.ok(), "outdated should succeed");
    let blocked = outdated.outdated_with_status("blocked");
    assert!(
        blocked.contains("eslint"),
        "eslint must be blocked, got blocked={blocked:?}, adoptable={:?}",
        outdated.outdated_with_status("adoptable")
    );
    assert_eq!(
        outdated.item_field_str("eslint", "blockedBy").as_deref(),
        Some("@typescript-eslint/eslint-plugin"),
        "the blocked row names the dependent whose peer range excludes the target"
    );
}

/// A *workspace-local* package's peer contract lives only in its own `package.json`: with
/// `auto-install-peers=false` pnpm records the linked shim in the lock without any peer metadata
/// (under `auto-install-peers=true` the peer materializes as a shim importer dependency and the
/// specifier-split machinery already holds the move as `multi_version_held`), so the gate must
/// read the member manifest. The linked shim peer-requires `eslint@^8.0.0`; the matured
/// cross-major eslint 9 must be held with the shim blamed, and the lock must stay on the v8
/// line.
#[test]
fn upgrade_holds_a_cross_major_move_a_workspace_dependents_peer_excludes() {
    skip_if_missing!("pnpm");
    let fixture = Fixture::new().tag_independent();
    // `autoInstallPeers: false` (a pnpm-workspace.yaml setting on pnpm ≥ 10; the `.npmrc` key is
    // ignored there) is the regime that needs the manifest source: the shim's peer is NOT
    // materialized as an importer dependency, so the lock carries no trace of the contract — the
    // shim importer is literally `packages/shim: {}`.
    fixture.write(
        "pnpm-workspace.yaml",
        indoc::indoc! {"
            packages:
              - packages/*
            autoInstallPeers: false
        "},
    );
    fixture.write(
        "package.json",
        indoc::indoc! {r#"
            {
              "name": "cooldown-pnpm-workspace-peer-fixture",
              "version": "0.1.0",
              "private": true,
              "dependencies": {
                "eslint": "^8.40.0",
                "local-eslint-shim": "workspace:*"
              }
            }
        "#},
    );
    fixture.write(
        "packages/shim/package.json",
        indoc::indoc! {r#"
            {
              "name": "local-eslint-shim",
              "version": "0.1.0",
              "peerDependencies": {
                "eslint": "^8.0.0"
              }
            }
        "#},
    );
    fixture.write(".npmrc", "strict-peer-dependencies=false\n");
    seed_lock(&fixture, FREEZE);
    assert!(
        locked_eslint(&fixture).starts_with("8."),
        "the seed must start on the eslint v8 line"
    );

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report.skipped_reasons_for("eslint").contains("peer_held"),
        "the cross-major eslint move is held by the workspace shim's manifest peer range, got {:?}",
        report.skipped_reasons_for("eslint")
    );
    assert_eq!(
        report.skipped_offending_for("eslint").as_deref(),
        Some("local-eslint-shim"),
        "the hold blames the workspace-local dependent"
    );
    assert!(
        locked_eslint(&fixture).starts_with("8."),
        "the lock must stay on the v8 line"
    );
}

/// The *injected* encoding of the same contract: with `dependenciesMeta.*.injected` pnpm records
/// the shim as a root-relative `file:` version instead of a `link:` — and its peer stays out of
/// the importer records exactly as in the linked case. The member deliberately lives at
/// `packages/shim(foo@bar)`, so pnpm emits `file:packages/shim(foo@bar)(eslint@x)` — a scalar in
/// which the directory's own `(foo@bar)` group is indistinguishable from the appended peer
/// context; the gate must recover the path against the importer set to find the manifest. The
/// peer is marked *optional* to preserve the original bypass reproduction (an optional peer on a
/// present package still binds). Same regime: `autoInstallPeers: false`, so nothing materializes
/// the contract into the importers.
#[test]
fn upgrade_holds_a_cross_major_move_an_injected_workspace_dependents_peer_excludes() {
    skip_if_missing!("pnpm");
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "pnpm-workspace.yaml",
        indoc::indoc! {"
            packages:
              - packages/*
            autoInstallPeers: false
        "},
    );
    fixture.write(
        "package.json",
        indoc::indoc! {r#"
            {
              "name": "cooldown-pnpm-injected-peer-fixture",
              "version": "0.1.0",
              "private": true,
              "dependencies": {
                "eslint": "^8.40.0",
                "local-eslint-shim": "workspace:*"
              },
              "dependenciesMeta": {
                "local-eslint-shim": { "injected": true }
              }
            }
        "#},
    );
    fixture.write(
        "packages/shim(foo@bar)/package.json",
        indoc::indoc! {r#"
            {
              "name": "local-eslint-shim",
              "version": "0.1.0",
              "peerDependencies": {
                "eslint": "^8.0.0"
              },
              "peerDependenciesMeta": {
                "eslint": { "optional": true }
              }
            }
        "#},
    );
    fixture.write(".npmrc", "strict-peer-dependencies=false\n");
    seed_lock(&fixture, FREEZE);
    assert!(
        locked_eslint(&fixture).starts_with("8."),
        "the seed must start on the eslint v8 line"
    );
    let lock = String::from_utf8(fixture.read_bytes("pnpm-lock.yaml")).expect("utf8 lock");
    assert!(
        lock.contains("file:packages/shim(foo@bar)"),
        "the seed must record the shim via the injected `file:` encoding at the ambiguous path"
    );

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report.skipped_reasons_for("eslint").contains("peer_held"),
        "the cross-major eslint move is held by the injected shim's manifest peer range, got {:?}",
        report.skipped_reasons_for("eslint")
    );
    assert_eq!(
        report.skipped_offending_for("eslint").as_deref(),
        Some("local-eslint-shim"),
        "the hold blames the injected workspace-local dependent"
    );
    assert!(
        locked_eslint(&fixture).starts_with("8."),
        "the lock must stay on the v8 line"
    );
}

/// Seed cutoff for the ceiling-held lockstep pair: react 18.3.1 is the newest react/react-dom
/// before this date, and react 19 (2024-12) is beyond it.
const LOCKSTEP_SEED: &str = "2024-11-01T00:00:00Z";

/// Freeze under which react-dom 19.x is long matured while `max-major` still holds react at 18.
const LOCKSTEP_FREEZE: &str = "2026-06-30T00:00:00Z";

/// One side of a lockstep peer pair held by a *ceiling* rather than the peer gate: react is capped
/// by config `max-major = 18`, so only react-dom enters the joint resolve — and pnpm, which only
/// warns on a peer mismatch, would commit `react-dom@19.x(react@18.3.1)` whose recorded peer range
/// `^19.x` provably excludes the still-held react. The post-resolve verification must reject that
/// candidate with structured blame and leave the graph untouched; no run may end with the exact
/// break the gate exists to prevent.
#[test]
fn upgrade_rejects_a_joint_move_that_breaks_a_ceiling_held_targets_peer_contract() {
    skip_if_missing!("pnpm");
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "package.json",
        indoc::indoc! {r#"
            {
              "name": "cooldown-pnpm-lockstep-ceiling-fixture",
              "version": "0.1.0",
              "private": true,
              "dependencies": {
                "react": "^18.3.1",
                "react-dom": "^18.3.1"
              }
            }
        "#},
    );
    fixture.write(".npmrc", "strict-peer-dependencies=false\n");
    fixture.write(
        "cooldown.toml",
        indoc::indoc! {"
            [tool.pnpm.package.react]
            max-major = 18
        "},
    );
    seed_lock(&fixture, LOCKSTEP_SEED);
    let pins = support::pnpm_lock_pins(&fixture.read_bytes("pnpm-lock.yaml"));
    assert_eq!(
        pins.get("react").map(String::as_str),
        Some("18.3.1"),
        "the seed must start on the react 18 line"
    );
    assert_eq!(pins.get("react-dom").map(String::as_str), Some("18.3.1"));
    let manifest_before = fixture.read_bytes("package.json");
    let lock_before = fixture.read_bytes("pnpm-lock.yaml");

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", LOCKSTEP_FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report
            .skipped_reasons_for("react")
            .contains("max_major_held"),
        "react is held by its configured ceiling, got {:?}",
        report.skipped_reasons_for("react")
    );
    assert!(
        report
            .skipped_reasons_for("react-dom")
            .contains("peer_held"),
        "react-dom's joint landing must be rejected by the post-resolve verification, got {:?}",
        report.skipped_reasons_for("react-dom")
    );
    assert_eq!(
        report.skipped_offending_for("react-dom").as_deref(),
        Some("react"),
        "react-dom's hold blames the ceiling-held peer target"
    );
    assert_eq!(report.summary_applied(), 0, "nothing may land");
    let pins = support::pnpm_lock_pins(&fixture.read_bytes("pnpm-lock.yaml"));
    assert_eq!(
        pins.get("react-dom").map(String::as_str),
        Some("18.3.1"),
        "the broken pair (react-dom@19 beside react@18) must not persist"
    );
    assert_eq!(
        manifest_before,
        fixture.read_bytes("package.json"),
        "the rejected candidate must not leak its widened manifest"
    );
    assert_eq!(
        lock_before,
        fixture.read_bytes("pnpm-lock.yaml"),
        "the lock must be byte-identical after the fully-held run"
    );

    // Convergence: the held pair reports identically on a second run and moves nothing.
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", LOCKSTEP_FREEZE]);
    assert_eq!(second.summary_applied(), 0);
    assert!(
        second
            .skipped_reasons_for("react-dom")
            .contains("peer_held")
    );
    assert_eq!(lock_before, fixture.read_bytes("pnpm-lock.yaml"));
}
