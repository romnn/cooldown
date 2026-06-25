//! End-to-end tests for the pnpm whole-graph resolve on a dependency declared by SEVERAL workspace
//! members at DIFFERENT version lines — the case that surfaced two real defects against the luup
//! monorepo:
//!
//! 1. **Convergence.** After a single `upgrade`, every importer of a multi-line dependency must reach
//!    the newest-within-window version its own declared range admits. The earlier adapter left a
//!    lower line stuck below its in-range latest (`vite ^6` pinned at `6.4.1` while `6.4.3` was
//!    adoptable; `zustand` at `5.0.10` while `5.0.14` was), so a converged `upgrade` still reported
//!    adoptable leftovers — non-convergence.
//!
//! 2. **Caret preservation.** The resolve must never move an importer's lock OUT of the range that
//!    importer declares. The earlier adapter's exact-pin (`pnpm update <name>@<target> --recursive
//!    --no-save`) re-pins EVERY importer's lock to `<target>` regardless of its declared range, so a
//!    cross-major target chosen for one member (`vite@7.3.5` for the `^7` importer) was forced onto a
//!    sibling that declares `^6`, leaving its lock at `7.3.5` against an untouched `^6` manifest — a
//!    lock/manifest inconsistency the next plain `install` then snapped back, breaking the fixed point.
//!
//! Both are exercised with the REAL pnpm resolver against the npm registry's immutable history, frozen
//! at an absolute cutoff (see `convergence_pnpm.rs` for the determinism argument). Assertions check
//! INVARIANTS (convergence, in-range, byte-stable re-run), never the absolute registry-newest.

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
/// in-range forward move — the lower line a non-converging resolve leaves behind.
const MEMBER_V6_PACKAGE_JSON: &str = r#"{
  "name": "@cooldown/app-v6",
  "version": "0.1.0",
  "dependencies": {
    "semver": "^6.0.0"
  }
}
"#;

/// The member on the `semver` v7 line. Has its own forward move (`7.3.8` → `7.5.4` within the freeze
/// window), so the resolve must advance BOTH lines, not just one.
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
/// `7.6.0` (2024-02-05) stays excluded. So the converged lock is `6.3.1` + `7.5.4`.
const FREEZE: &str = "2023-12-01T00:00:00Z";

/// The version each line must converge to under `FREEZE`.
const V6_TARGET: &str = "6.3.1";
const V7_TARGET: &str = "7.5.4";

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

fn multiline_fixture() -> Fixture {
    let fixture = Fixture::new();
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
fn upgrade_converges_every_line_of_a_multi_version_dependency() {
    skip_if_missing!("pnpm");
    let fixture = multiline_fixture();

    // Sanity: the seed sits below both targets, so each line has a genuine in-range forward move.
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
    assert_eq!(
        upgrade.lock_status(),
        Some("unknown"),
        "pnpm applies and re-locks, but cooldown cannot prove pnpm-lock.yaml currency yet"
    );

    // Convergence: BOTH importers reach the newest-within-window version their OWN range admits. The
    // non-converging adapter left the lower line (`pkgs/v6`) below its in-range latest.
    let after = fixture.read_bytes("pnpm-lock.yaml");
    assert_eq!(
        importer_resolved(&after, "pkgs/v6", "semver").as_deref(),
        Some(V6_TARGET),
        "the v6 importer must converge to {V6_TARGET}, not stay below its in-range latest"
    );
    assert_eq!(
        importer_resolved(&after, "pkgs/v7", "semver").as_deref(),
        Some(V7_TARGET),
        "the v7 importer must converge to {V7_TARGET}"
    );

    // Fixed point: a second upgrade moves nothing and the lock is byte-identical (no leftover
    // adoptable, no ping-pong).
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op (converged fixed point), no adoptable leftover"
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
    let fixture = Fixture::new();
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
/// cap. The whole-graph resolve must instead range-float the dep so each member stays within its OWN
/// declared range: the tilde member at `7.3.x`, the caret member free to advance.
#[test]
fn upgrade_respects_a_narrow_range_when_a_sibling_declares_a_wider_one() {
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

    fixture
        .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
        .expect_success();

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
    // The caret member, on its own wider range, still advances.
    let caret = importer_resolved(&after, "pkgs/caret", "semver").expect("caret resolves semver");
    assert_eq!(
        caret, V7_TARGET,
        "the ^7.0.0 member must still advance to {V7_TARGET} (got {caret})"
    );
}

#[test]
fn upgrade_never_moves_an_importer_out_of_its_declared_range() {
    skip_if_missing!("pnpm");
    let fixture = multiline_fixture();

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
}

#[test]
fn outdated_marks_a_cross_line_multi_version_bump_blocked_not_adoptable() {
    skip_if_missing!("pnpm");
    let fixture = multiline_fixture();

    // Converge both lines within their own major: v6 → 6.3.1, v7 → 7.5.4. The only move left for the
    // `^6` line is a cross-major bump onto v7 — which `upgrade` floats in-range and never takes.
    fixture
        .cooldown(&["upgrade", "--major", "--freeze", FREEZE])
        .expect_success();

    // `outdated --major` must AGREE with what `upgrade --major` does: the cross-line bump the resolve
    // floats in-range is reported `blocked`, never `adoptable`. Before the per-member `reached` fix the
    // resolve judged it landed (the name's newest copy, v7, already sat at the target), so `outdated`
    // advertised an adoptable update `upgrade` would silently hold — the classification bug.
    let outdated = fixture.cooldown_json(&["outdated", "--major", "--freeze", FREEZE]);
    let adoptable = outdated.outdated_with_status("adoptable");
    let blocked = outdated.outdated_with_status("blocked");
    assert!(
        !adoptable.contains("semver"),
        "the cross-line ^6→v7 bump must NOT be adoptable (upgrade floats v6 within ^6): adoptable={adoptable:?}"
    );
    assert!(
        blocked.contains("semver"),
        "the cross-line multi-version bump must be reported blocked: blocked={blocked:?}"
    );

    // `upgrade --major` agrees: it never applies the held cross-line bump (the v6 line stays in ^6).
    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE, "--dry-run"]);
    assert!(
        !upgrade.applied_names().contains("semver"),
        "upgrade must not apply the cross-line multi-version bump\napplied={:?}",
        upgrade.applied_names()
    );
}
