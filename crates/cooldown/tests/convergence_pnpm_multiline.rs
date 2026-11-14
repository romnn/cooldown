//! End-to-end tests for the pnpm whole-graph resolve on a dependency declared by SEVERAL workspace
//! members at DIFFERENT version lines — the case that surfaced two real defects against the luup
//! monorepo:
//!
//! 1. **Conservative holds.** A dependency split across workspace members must be reported as held
//!    rather than passed to pnpm as a bare package name. Bare `pnpm update <name> --recursive
//!    --no-save` can write an out-of-range lock entry while leaving the declaring `package.json`
//!    untouched, which makes the final frozen-lock verification fail.
//!
//! 2. **Caret preservation.** The resolve must never move an importer's lock OUT of the range that
//!    importer declares. The earlier adapter's exact-pin (`pnpm update <name>@<target> --recursive
//!    --no-save`) re-pins EVERY importer's lock to `<target>` regardless of its declared range, so a
//!    cross-major target chosen for one member (`vite@7.3.5` for the `^7` importer) was forced onto a
//!    sibling that declares `^6`, leaving its lock at `7.3.5` against an untouched `^6` manifest — a
//!    lock/manifest inconsistency the next plain `install` then snapped back, breaking the fixed
//!    point.
//!
//! Both are exercised with the REAL pnpm resolver against the npm registry's publish history,
//! frozen at an absolute cutoff and decoupled from the mutable `latest` dist-tag via
//! `tag_independent` fixtures (see `convergence_pnpm.rs` for the determinism argument).
//! Assertions check INVARIANTS (held classification, in-range, byte-stable re-run), never the
//! absolute registry-newest.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use support::Fixture;

/// pnpm warns (not errors) on peer mismatches and auto-installs missing peers, matching the realistic
/// developer configuration the other pnpm tests use.
const NPMRC: &str = "strict-peer-dependencies=false\nauto-install-peers=true\n";

const WORKSPACE_ROOT_PACKAGE_JSON: &str = r#"{
  "name": "cooldown-pnpm-multiline-root",
  "version": "0.1.0",
  "private": true
}
"#;

const WORKSPACE_YAML: &str = "packages:\n  - \"pkgs/*\"\n";

/// The member on the `semver` v6 line. `semver`'s v6 line has exactly one release after `6.3.0`
/// (`6.3.1`, 2023-07-10), so seeding below the freeze gives this importer a single, unambiguous
/// in-range forward move for cooldown to hold conservatively.
const MEMBER_V6_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app-v6",
  "version": "0.1.0",
  "dependencies": {
    "semver": "^6.0.0"
  }
}
"#;

/// The member on the `semver` v7 line. Has its own forward move (`7.3.8` → `7.5.4` within the freeze
/// window), so the split fixture has eligible updates on both lines.
const MEMBER_V7_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app-v7",
  "version": "0.1.0",
  "dependencies": {
    "semver": "^7.0.0"
  }
}
"#;

/// Seed cutoff: `^6` resolves to `6.3.0` (6.3.1 is 2023-07, still ahead) and `^7` to `7.3.8`
/// (2022-10-04; 7.4.0 is 2023-04, still ahead). Both lines sit strictly below their freeze target.
const SEED: &str = "2023-01-01T00:00:00Z";

/// The resolution freeze: `^6` admits `6.3.1` (2023-07-10) and `^7` admits up to `7.5.4` (2023-07-07);
/// `7.6.0` (2024-02-05) stays excluded.
const FREEZE: &str = "2023-12-01T00:00:00Z";

fn minimum_release_age_minutes(cutoff: &str) -> i64 {
    let cutoff: jiff::Timestamp = cutoff.parse().expect("cutoff parses");
    let minutes = jiff::Timestamp::now().duration_since(cutoff).as_secs() / 60;
    assert!(minutes > 0, "cutoff {cutoff} must be in the past");
    minutes
}

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

fn multiline_fixture() -> Fixture {
    let fixture = Fixture::pnpm().tag_independent();
    fixture.write("package.json", WORKSPACE_ROOT_PACKAGE_JSON);
    fixture.write("pnpm-workspace.yaml", WORKSPACE_YAML);
    fixture.write("pkgs/v6/package.json", MEMBER_V6_PACKAGE_JSON);
    fixture.write("pkgs/v7/package.json", MEMBER_V7_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, SEED);
    fixture
}

/// The version `member` (an `importers:` path like `pkgs/v6`) resolves `dep` to in `pnpm-lock.yaml`,
/// read from the importer's own `version:` line (its `(peer)` suffix stripped). This is the
/// PER-IMPORTER pin — `support::pnpm_lock_pins` keeps only the newest copy per name, which would mask
/// the lower line, so the multi-line invariants need this importer-scoped view instead.
fn importer_resolved(lock: &[u8], member: &str, dep: &str) -> Option<String> {
    let text = String::from_utf8_lossy(lock);
    let mut in_importers = false;
    let mut in_member = false;
    let mut in_group = false;
    let mut in_dep = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim();
        match indent {
            0 => in_importers = trimmed == "importers:",
            2 if in_importers => {
                in_member = trimmed
                    .trim_end_matches(':')
                    .trim_matches('\'')
                    .trim_matches('"')
                    == member;
                in_group = false;
                in_dep = false;
            }
            4 if in_importers && in_member => {
                in_group = matches!(
                    trimmed.trim_end_matches(':'),
                    "dependencies" | "devDependencies" | "optionalDependencies"
                );
                in_dep = false;
            }
            6 if in_importers && in_member && in_group => {
                in_dep = trimmed
                    .trim_end_matches(':')
                    .trim_matches('\'')
                    .trim_matches('"')
                    == dep;
            }
            8 if in_importers && in_member && in_group && in_dep => {
                if let Some(raw) = trimmed.strip_prefix("version:") {
                    let value = raw.trim().trim_matches('\'').trim_matches('"');
                    let version = value.split('(').next().unwrap_or(value);
                    return Some(version.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[test]
fn upgrade_holds_multi_version_dependency_without_mutating_lock() {
    skip_if_missing!("pnpm");
    let fixture = multiline_fixture();

    // Sanity: the seed sits below both lines' newest-within-window targets, so cooldown has a real
    // candidate it must hold conservatively.
    let seed = fixture.read_bytes("pnpm-lock.yaml");
    assert_eq!(
        importer_resolved(&seed, "pkgs/v6", "semver").as_deref(),
        Some("6.3.0"),
        "seed must hold the v6 importer at 6.3.0"
    );
    assert_eq!(
        importer_resolved(&seed, "pkgs/v7", "semver").as_deref(),
        Some("7.3.8"),
        "seed must hold the v7 importer at 7.3.8"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
            .stderr_str()
    );
    assert_pnpm_lock_current(&upgrade);
    assert_eq!(
        upgrade.summary_applied(),
        0,
        "multi-version dependency must not be applied via bare pnpm update"
    );
    assert!(
        upgrade
            .skipped_reasons_for("semver")
            .contains("multi_version_held"),
        "semver must be reported as a multi-version hold: {:?}",
        upgrade.skipped_reasons_for("semver")
    );

    let after = fixture.read_bytes("pnpm-lock.yaml");
    assert_eq!(
        seed, after,
        "held multi-version candidates must leave the lock byte-identical"
    );

    // Fixed point: a second upgrade also moves nothing and the lock is byte-identical.
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op"
    );
    assert_eq!(
        after,
        fixture.read_bytes("pnpm-lock.yaml"),
        "the lock must be byte-identical across the two converged runs"
    );
}

/// A member that pins `semver` to the `7.3.x` line with a tilde range — a DELIBERATE narrow
/// constraint (the luup analogue is `airtype`'s `tailwindcss: ">=3.4.19 <4"` cap). It shares the
/// resolved `7.3.8` with the `^7.0.0` member below, so the before-lock holds `semver` at a SINGLE
/// resolved version even though the two members declare DIFFERENT ranges.
const MEMBER_TILDE_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app-tilde",
  "version": "0.1.0",
  "dependencies": {
    "semver": "~7.3.0"
  }
}
"#;

/// A member on the open `^7.0.0` range that has a forward move to `7.5.4` under the freeze. Its wider
/// range must NOT drag the tilde member's lock past `7.3.x`.
const MEMBER_CARET7_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app-caret7",
  "version": "0.1.0",
  "dependencies": {
    "semver": "^7.0.0"
  }
}
"#;

/// The newest `7.3.x` release — the most the `~7.3.0` member may ever adopt. Both members seed here.
const TILDE_MAX: &str = "7.3.8";

fn tilde_fixture() -> Fixture {
    let fixture = Fixture::pnpm().tag_independent();
    fixture.write("package.json", WORKSPACE_ROOT_PACKAGE_JSON);
    fixture.write("pnpm-workspace.yaml", WORKSPACE_YAML);
    fixture.write("pkgs/tilde/package.json", MEMBER_TILDE_PACKAGE_JSON);
    fixture.write("pkgs/caret/package.json", MEMBER_CARET7_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, SEED);
    fixture
}

/// The defining case: two members declare the same dependency at DIFFERENT ranges (`~7.3.0` and
/// `^7.0.0`) that the seed resolves to the SAME version (`7.3.8`). Because the before-lock holds a
/// single resolved version, a resolved-version-only "multi-version" test misclassifies the dep as
/// uniform and exact-pins it (`pnpm update semver@7.5.4 --recursive`), collapsing the `~7.3.0` member
/// onto `7.5.4` and widening its manifest off `~7.3.0` — overriding the author's deliberate narrow
/// cap. The whole-graph resolve must instead hold the dep so each member stays within its OWN declared
/// range and the lock remains current.
#[test]
fn upgrade_holds_a_narrow_range_when_a_sibling_declares_a_wider_one() {
    skip_if_missing!("pnpm");
    let fixture = tilde_fixture();

    // Sanity: the seed holds BOTH members at the same resolved 7.3.8 (the single-resolved-version
    // precondition that the resolved-version-only detection trips over).
    let seed = fixture.read_bytes("pnpm-lock.yaml");
    assert_eq!(
        importer_resolved(&seed, "pkgs/tilde", "semver").as_deref(),
        Some(TILDE_MAX),
        "seed must hold the tilde member at {TILDE_MAX}"
    );
    assert_eq!(
        importer_resolved(&seed, "pkgs/caret", "semver").as_deref(),
        Some(TILDE_MAX),
        "seed must hold the caret member at the same {TILDE_MAX}"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
            .stderr_str()
    );
    assert_pnpm_lock_current(&upgrade);
    assert!(
        upgrade
            .skipped_reasons_for("semver")
            .contains("multi_version_held"),
        "semver must be reported as a multi-version hold: {:?}",
        upgrade.skipped_reasons_for("semver")
    );

    let after = fixture.read_bytes("pnpm-lock.yaml");
    // The tilde member must stay on the 7.3.x line its `~7.3.0` admits — never dragged onto the
    // caret member's 7.5.4.
    let tilde = importer_resolved(&after, "pkgs/tilde", "semver").expect("tilde resolves semver");
    assert_eq!(
        tilde, TILDE_MAX,
        "the ~7.3.0 member must stay at {TILDE_MAX}, not be collapsed onto the sibling's 7.5.4 (got {tilde})"
    );
    // And its manifest must keep the deliberate narrow cap — never silently widened to chase the
    // sibling's target.
    let manifest = String::from_utf8(fixture.read_bytes("pkgs/tilde/package.json")).unwrap();
    assert!(
        manifest.contains("\"~7.3.0\""),
        "the tilde member's manifest must stay ~7.3.0 (its deliberate cap), not be widened: {manifest}"
    );
    // The caret member is held too; the important invariant is that cooldown never asks pnpm to write
    // an out-of-range lock entry while the manifest stays unchanged.
    let caret = importer_resolved(&after, "pkgs/caret", "semver").expect("caret resolves semver");
    assert_eq!(
        caret, TILDE_MAX,
        "the ^7.0.0 member must remain at {TILDE_MAX} when the split is held (got {caret})"
    );
}

#[test]
fn upgrade_never_moves_an_importer_out_of_its_declared_range() {
    skip_if_missing!("pnpm");
    let fixture = multiline_fixture();

    let before = fixture.read_bytes("pnpm-lock.yaml");
    fixture
        .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
        .expect_success();

    // Caret preservation: the `^6` importer keeps a v6 lock and the `^7` importer a v7 lock. A
    // cross-line exact-pin (`pnpm update semver@7.5.4 --recursive`) would force the `^6` importer's
    // lock to 7.5.4 — out of its declared `^6.0.0` — leaving the lock inconsistent with the manifest.
    let after = fixture.read_bytes("pnpm-lock.yaml");
    let v6 = importer_resolved(&after, "pkgs/v6", "semver").expect("v6 importer resolves semver");
    let v7 = importer_resolved(&after, "pkgs/v7", "semver").expect("v7 importer resolves semver");
    assert!(
        v6.starts_with("6."),
        "the ^6.0.0 importer must keep a v6 lock, got {v6} — exact-pin crossed the caret"
    );
    assert!(
        v7.starts_with("7."),
        "the ^7.0.0 importer must keep a v7 lock, got {v7}"
    );

    // The v6 importer's manifest is never widened off ^6, so its lock staying in v6 means lock and
    // manifest agree — the consistency the caret-crossing bug broke.
    let manifest = String::from_utf8(fixture.read_bytes("pkgs/v6/package.json")).unwrap();
    assert!(
        manifest.contains("\"^6.0.0\""),
        "the v6 member's manifest must stay ^6.0.0 (never widened across the major): {manifest}"
    );
    assert_eq!(
        before, after,
        "held multi-version candidates must not mutate the lock"
    );
}

#[test]
fn outdated_marks_a_cross_line_multi_version_bump_blocked_not_adoptable() {
    skip_if_missing!("pnpm");
    let fixture = multiline_fixture();

    // `outdated --major` must AGREE with what `upgrade --major` does: the split-line bump is reported
    // `blocked`, never `adoptable`.
    let outdated = fixture.cooldown_json(&["outdated", "--major", "--freeze", FREEZE]);
    let adoptable = outdated.outdated_with_status("adoptable");
    let blocked = outdated.outdated_with_status("blocked");
    assert!(
        !adoptable.contains("semver"),
        "the cross-line multi-version bump must NOT be adoptable: adoptable={adoptable:?}"
    );
    assert!(
        blocked.contains("semver"),
        "the cross-line multi-version bump must be reported blocked: blocked={blocked:?}"
    );

    // `upgrade --major` agrees: it never applies the held multi-version bump.
    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE, "--dry-run"]);
    assert!(
        !upgrade.applied_names().contains("semver"),
        "upgrade must not apply the cross-line multi-version bump\napplied={:?}",
        upgrade.applied_names()
    );
}

/// A single project declaring `semver` on the v7 line, plus `make-dir` 3.1.0, whose own
/// `semver ^6.0.0` requirement keeps an undeclared v6 copy in the graph.
/// Seeded below the freeze, the declared copy has its own in-range move (`7.3.8` → `7.5.4`) and
/// the undeclared one another (`6.3.0` → `6.3.1`).
const DECLARED_AND_UNDECLARED_PACKAGE_JSON: &str = indoc::indoc! {r#"
    {
      "name": "cooldown-pnpm-declared-and-undeclared",
      "version": "0.1.0",
      "private": true,
      "dependencies": {
        "make-dir": "3.1.0",
        "semver": "^7.3.8"
      }
    }
"#};

/// The version every `semver@<version>:` package key in `lock` names.
fn semver_copies(lock: &[u8]) -> std::collections::BTreeSet<String> {
    String::from_utf8_lossy(lock)
        .lines()
        .filter_map(|line| line.trim().strip_prefix("semver@")?.strip_suffix(':'))
        .map(str::to_owned)
        .collect()
}

/// The undeclared copy's own move must not hold the declared copy's in-range upgrade: the two
/// lines are decided separately, so the declared `semver` lands.
/// pnpm's update of the declared name re-resolves the undeclared v6 copy within its own range too,
/// which lands that copy's own target, so both moves are reported applied.
#[test]
fn an_undeclared_copy_on_another_line_does_not_hold_the_declared_upgrade() {
    let fixture = Fixture::pnpm().tag_independent();
    fixture.write("package.json", DECLARED_AND_UNDECLARED_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, SEED);
    let seeded = fixture.read_bytes("pnpm-lock.yaml");
    assert_eq!(
        importer_resolved(&seeded, ".", "semver").as_deref(),
        Some("7.3.8"),
        "seed sanity: the declared copy"
    );
    assert!(
        semver_copies(&seeded).contains("6.3.0"),
        "seed sanity: make-dir keeps an undeclared v6 copy, got {:?}",
        semver_copies(&seeded)
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", FREEZE])
            .stderr_str()
    );
    assert_pnpm_lock_current(&upgrade);

    // The declared copy lands its in-range target.
    let after = fixture.read_bytes("pnpm-lock.yaml");
    assert_eq!(
        importer_resolved(&after, ".", "semver").as_deref(),
        Some("7.5.4"),
        "the declared copy must land, report: {:?}",
        upgrade.items()
    );
    // The undeclared copy advanced within its own line, to its own planned target.
    assert_eq!(
        semver_copies(&after),
        ["6.3.1".to_string(), "7.5.4".to_string()].into(),
        "the undeclared copy stays on the v6 line"
    );
    let mut applied: Vec<String> = upgrade
        .items()
        .iter()
        .filter(|item| {
            item.get("name").and_then(serde_json::Value::as_str) == Some("semver")
                && item.get("applied").and_then(serde_json::Value::as_bool) == Some(true)
        })
        .filter_map(|item| {
            let from = item.get("from")?.as_str()?;
            let to = item.get("to")?.as_str()?;
            Some(format!("{from} -> {to}"))
        })
        .collect();
    applied.sort();
    assert_eq!(
        applied,
        ["6.3.0 -> 6.3.1", "7.3.8 -> 7.5.4"],
        "both copies' moves are credited, each once, report: {:?}",
        upgrade.items()
    );

    // Converged: a second run moves nothing.
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(second.summary_applied(), 0, "second run must be a no-op");
    assert_eq!(after, fixture.read_bytes("pnpm-lock.yaml"));
}

/// `solid-js` 1.9.17 requires `seroval ~1.6.8` and `@tanstack/router-core` 1.171.36 requires
/// `seroval ^1.6.7`, both pinned exactly so neither moves.
/// Seeded on 2026-10-15 both bind one undeclared `seroval` 1.6.9 copy (1.6.10 is 2026-10-18), whose
/// advance under the freeze targets 1.7.2 (2026-10-24), a release only the caret range admits.
const SHARED_COPY_PACKAGE_JSON: &str = indoc::indoc! {r#"
    {
      "name": "cooldown-pnpm-shared-copy",
      "version": "0.1.0",
      "private": true,
      "dependencies": {
        "@tanstack/router-core": "1.171.36",
        "solid-js": "1.9.17"
      }
    }
"#};

const SHARED_COPY_SEED: &str = "2026-10-15T00:00:00Z";

const SHARED_COPY_FREEZE: &str = "2026-10-25T00:00:00Z";

/// Every version a `name@<version>:` package key in `lock` names.
fn copies_of(lock: &[u8], name: &str) -> std::collections::BTreeSet<String> {
    let prefix = format!("{name}@");
    String::from_utf8_lossy(lock)
        .lines()
        .filter(|line| line.starts_with("  ") && !line.starts_with("   "))
        .filter_map(|line| line.trim().strip_prefix(&prefix)?.strip_suffix(':'))
        .map(str::to_owned)
        .collect()
}

/// A transitive advance the settlement keeps for one dependent and reverts for another advances
/// the shared copy only partly: `@tanstack/router-core` keeps `seroval` 1.7.2, while `solid-js`'s
/// tilde range settles on 1.6.12.
/// The advance is held, naming who moved and who stayed, rather than credited from the copy at
/// the target, and with nothing landed the run leaves the lock as it was.
#[test]
fn a_partly_advanced_shared_copy_is_held_not_credited() {
    skip_if_missing!("pnpm");
    let fixture = Fixture::pnpm().tag_independent();
    fixture.write("package.json", SHARED_COPY_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    seed_lock(&fixture, SHARED_COPY_SEED);
    let seeded = fixture.read_bytes("pnpm-lock.yaml");
    assert_eq!(
        copies_of(&seeded, "seroval"),
        ["1.6.9".to_string()].into(),
        "seed sanity: both dependents share one copy"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", SHARED_COPY_FREEZE]);
    assert!(
        upgrade.ok(),
        "upgrade should succeed: {}",
        fixture
            .cooldown(&["upgrade", "--freeze", SHARED_COPY_FREEZE])
            .stderr_str()
    );

    // The advance is held with the copy's real outcome, never credited.
    assert!(
        !upgrade.applied_names().contains("seroval"),
        "the partly advanced copy must not be credited, report: {:?}",
        upgrade.items()
    );
    assert_eq!(
        upgrade.skipped_reasons_for("seroval"),
        ["graph_ceiling_held".to_string()].into(),
        "report: {:?}",
        upgrade.items()
    );
    let detail = upgrade.skip_detail_for("seroval").unwrap_or_default();
    assert!(
        detail.contains("moved to 1.7.2 only for @tanstack/router-core@1.171.36")
            && detail.contains("solid-js@1.9.17 still resolves seroval 1.6.12"),
        "{detail}"
    );
    // Nothing landed, so the batch's lock was restored.
    assert_eq!(fixture.read_bytes("pnpm-lock.yaml"), seeded);
}
