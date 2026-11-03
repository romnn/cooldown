//! End-to-end regression tests that drive the real npm resolver against a generated package-lock
//! fixture.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::{formatdoc, indoc};
use support::{ChangeVersions, Fixture};

const SEED_BEFORE: &str = "2025-05-20T00:00:00Z";
const FREEZE: &str = "2026-06-30T00:00:00Z";
const ESLINT_SEED_BEFORE: &str = "2024-12-01T00:00:00Z";

const PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-residual-isolation",
      "version": "0.1.0",
      "private": true,
      "devDependencies": {
        "@es-joy/jsdoccomment": "^0.49.0",
        "tree-sitter-cli": "^0.25.4"
      }
    }
"#};

const NPMRC: &str = indoc! {"
    min-release-age=0
    audit=false
    fund=false
"};

const RESIDUAL_POLICY: &str = indoc! {r#"
    freeze = "2026-06-30T00:00:00Z"

    [package."@typescript-eslint/types"]
    freeze = "2025-05-20T00:00:00Z"
"#};

const ESLINT_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-eslint-cutoff",
      "version": "0.1.0",
      "private": true,
      "devDependencies": {
        "eslint": "^9.16.0",
        "eslint-config-treesitter": "^1.0.2"
      }
    }
"#};

fn package_lock_version(fixture: &Fixture, name: &str) -> Option<String> {
    let lock: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package-lock.json")).expect("lock parses");
    lock.get("packages")?
        .get(format!("node_modules/{name}"))?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

fn package_lock_dependency_range(fixture: &Fixture, importer: &str, name: &str) -> Option<String> {
    let lock: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package-lock.json")).expect("lock parses");
    lock.get("packages")?
        .get(importer)?
        .get("dependencies")?
        .get(name)?
        .as_str()
        .map(str::to_owned)
}

fn assert_plain_npm_relock_is_noop(fixture: &Fixture) {
    let lock_before = fixture.read_bytes("package-lock.json");
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        lock_before,
        fixture.read_bytes("package-lock.json"),
        "an ordinary npm relock must have no manifest metadata left to repair"
    );
}

fn residual_isolation_fixture() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    fixture.write("cooldown.toml", RESIDUAL_POLICY);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    fixture
}

fn eslint_cutoff_fixture() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", ESLINT_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={ESLINT_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    fixture
}

#[test]
fn upgrade_keeps_a_safe_sibling_when_another_candidate_has_no_mature_transitive() {
    skip_if_missing!("npm");
    let fixture = residual_isolation_fixture();
    assert_eq!(
        package_lock_version(&fixture, "@es-joy/jsdoccomment").as_deref(),
        Some("0.49.0"),
        "the fixture must seed the candidate that later forces a fresh transitive"
    );
    assert_eq!(
        package_lock_version(&fixture, "tree-sitter-cli").as_deref(),
        Some("0.25.4"),
        "the fixture must seed the independent safe candidate"
    );

    // Both direct targets mature under the project cutoff, but jsdoccomment requires a types release
    // newer than that transitive's stricter package cutoff. The independent tree-sitter target must
    // still commit when residual policy isolation holds jsdoccomment back.
    let upgrade = fixture.cooldown_json(&["upgrade", "--major"]);

    assert_eq!(
        upgrade.changes_for("tree-sitter-cli"),
        vec![ChangeVersions::new("0.25.4", "0.26.10")],
        "the safe sibling must retain its baseline-to-target applied row: {upgrade:?}"
    );
    assert!(
        upgrade.skipped_reasons_for("tree-sitter-cli").is_empty(),
        "the blocked sibling must not make tree-sitter-cli look policy-blocked"
    );
    assert_eq!(
        upgrade.skipped_reasons_for("@es-joy/jsdoccomment"),
        ["transitive_in_cooldown".to_owned()].into_iter().collect(),
        "only the candidate with no satisfying mature transitive is held"
    );
    assert_eq!(upgrade.summary_applied(), 1);
    assert_eq!(upgrade.summary_skipped(), 1);
    assert_eq!(upgrade.summary_errors(), 0);
    assert_eq!(
        package_lock_version(&fixture, "tree-sitter-cli").as_deref(),
        Some("0.26.10"),
        "the committed lock must contain the safe target"
    );
    assert_eq!(
        package_lock_version(&fixture, "@es-joy/jsdoccomment").as_deref(),
        Some("0.49.0"),
        "the policy-blocked candidate must remain at its baseline"
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--major"]);
    assert_eq!(second.summary_applied(), 0);
    assert_eq!(second.summary_skipped(), 1);
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

#[test]
fn outdated_and_upgrade_resolve_eslint_with_mature_transitives() {
    skip_if_missing!("npm");
    let fixture = eslint_cutoff_fixture();
    assert_eq!(
        package_lock_version(&fixture, "eslint").as_deref(),
        Some("9.16.0")
    );

    let outdated = fixture.cooldown_json(&["outdated", "--major", "--freeze", FREEZE]);
    let adoptable = outdated.outdated_with_status("adoptable");
    let blocked = outdated.outdated_with_status("blocked");
    assert!(
        adoptable.contains("eslint"),
        "outdated must call eslint adoptable: adoptable={adoptable:?}, blocked={blocked:?}, envelope={outdated:?}"
    );
    assert!(
        !blocked.contains("eslint"),
        "outdated must not call eslint blocked: adoptable={adoptable:?}, blocked={blocked:?}"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(
        upgrade.changes_for("eslint"),
        vec![ChangeVersions::new("9.16.0", "10.6.0")]
    );
    assert!(upgrade.skipped_reasons_for("eslint").is_empty());
    assert!(upgrade.summary_applied() >= 1);
    assert_eq!(upgrade.summary_skipped(), 0);
    assert_eq!(upgrade.summary_errors(), 0);
    assert_eq!(
        package_lock_version(&fixture, "eslint").as_deref(),
        Some("10.6.0")
    );
    assert_eq!(
        package_lock_version(&fixture, "@typescript-eslint/types").as_deref(),
        Some("8.62.1")
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(second.summary_applied(), 0);
    assert_eq!(second.summary_skipped(), 0);
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

const VERSION_BOUND_FREEZE: &str = "2025-01-01T00:00:00Z";

const VERSION_BOUND_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-version-bound",
      "version": "0.1.0",
      "private": true,
      "devDependencies": {
        "typescript": ">=4 <5"
      }
    }
"#};

const MAX_MAJOR_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-max-major",
      "version": "0.1.0",
      "private": true,
      "devDependencies": {
        "typescript": "^4.9.5"
      }
    }
"#};

const MAX_MAJOR_POLICY: &str = indoc! {"
    [tool.npm.package.typescript]
    max-major = 4
"};

fn typescript_fixture(package_json: &str) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", package_json);
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={VERSION_BOUND_FREEZE}"),
            ],
            &[],
        )
        .expect_success();
    fixture
}

#[test]
fn explicit_upper_bound_holds_until_rewrite() {
    skip_if_missing!("npm");
    let fixture = typescript_fixture(VERSION_BOUND_PACKAGE_JSON);
    let manifest_before = fixture.read_bytes("package.json");
    let lock_before = fixture.read_bytes("package-lock.json");
    assert!(
        package_lock_version(&fixture, "typescript")
            .is_some_and(|version| version.starts_with("4.")),
        "the explicit bound must seed TypeScript 4.x"
    );

    let held = fixture.cooldown_json(&["upgrade", "--major", "--freeze", VERSION_BOUND_FREEZE]);
    assert_eq!(
        held.skipped_reasons_for("typescript"),
        ["declared_bound_held".to_owned()].into_iter().collect()
    );
    assert_eq!(manifest_before, fixture.read_bytes("package.json"));
    assert_eq!(lock_before, fixture.read_bytes("package-lock.json"));

    let rewritten = fixture.cooldown_json(&[
        "upgrade",
        "--major",
        "--rewrite",
        "--freeze",
        VERSION_BOUND_FREEZE,
    ]);
    assert!(rewritten.applied_names().contains("typescript"));
    assert!(
        package_lock_version(&fixture, "typescript")
            .is_some_and(|version| version.starts_with("5.")),
        "--rewrite must cross the explicit bound"
    );
    assert!(
        !String::from_utf8_lossy(&fixture.read_bytes("package.json")).contains(">=4 <5"),
        "the crossed bound must be rewritten"
    );
}

#[test]
fn tool_scoped_max_major_holds_an_otherwise_adoptable_major() {
    skip_if_missing!("npm");
    let fixture = typescript_fixture(MAX_MAJOR_PACKAGE_JSON);
    fixture.write("cooldown.toml", MAX_MAJOR_POLICY);
    let manifest_before = fixture.read_bytes("package.json");
    let lock_before = fixture.read_bytes("package-lock.json");

    let held = fixture.cooldown_json(&["upgrade", "--major", "--freeze", VERSION_BOUND_FREEZE]);
    assert_eq!(
        held.skipped_reasons_for("typescript"),
        ["max_major_held".to_owned()].into_iter().collect()
    );
    assert_eq!(manifest_before, fixture.read_bytes("package.json"));
    assert_eq!(lock_before, fixture.read_bytes("package-lock.json"));
}

const LOCKSTEP_PEER_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-lockstep-peer",
      "version": "0.1.0",
      "private": true,
      "dependencies": {
        "react": "^18.3.1",
        "react-dom": "^18.3.1"
      }
    }
"#};

/// `legacy-peer-deps` is the relaxed enforcement under which plain `npm install` *commits* a peer
/// break instead of failing `ERESOLVE` — the regime the post-apply verification exists for.
const LOCKSTEP_PEER_NPMRC: &str = indoc! {"
    min-release-age=0
    audit=false
    fund=false
    legacy-peer-deps=true
"};

const LOCKSTEP_SEED_BEFORE: &str = "2024-11-01T00:00:00Z";

/// A lockstep peer pair (react/react-dom: each major's `react-dom` peer-requires exactly its own
/// `react` major) under relaxed peer enforcement. The gate holds react (react-dom@18's recorded
/// `^18.3.1` provably excludes 19), and react-dom's solo move — which npm happily commits under
/// `legacy-peer-deps`, persisting `react-dom@19` beside `react@18` — must be caught by the
/// post-apply verification and rolled back: no run may end (or pause) with the exact broken peer
/// graph the feature exists to prevent. Both sides report `peer_held`, each blaming the other.
#[test]
fn upgrade_rolls_back_a_dependent_whose_new_peer_range_breaks_the_held_target() {
    skip_if_missing!("npm");
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", LOCKSTEP_PEER_PACKAGE_JSON);
    fixture.write(".npmrc", LOCKSTEP_PEER_NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={LOCKSTEP_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "react").as_deref(),
        Some("18.3.1"),
        "the seed must start on the react 18 line"
    );
    let manifest_before = fixture.read_bytes("package.json");
    let lock_before = fixture.read_bytes("package-lock.json");

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report.skipped_reasons_for("react").contains("peer_held"),
        "react is held up front by react-dom@18's recorded peer range, got {:?}",
        report.skipped_reasons_for("react")
    );
    assert_eq!(
        report.skipped_offending_for("react").as_deref(),
        Some("react-dom"),
        "react's hold blames the dependent"
    );
    assert!(
        report
            .skipped_reasons_for("react-dom")
            .contains("peer_held"),
        "react-dom's solo landing must be rolled back by the post-apply verification, got {:?}",
        report.skipped_reasons_for("react-dom")
    );
    assert_eq!(
        report.skipped_offending_for("react-dom").as_deref(),
        Some("react"),
        "react-dom's hold blames the still-held peer target"
    );
    assert_eq!(report.summary_applied(), 0, "nothing may land");
    assert_eq!(
        package_lock_version(&fixture, "react").as_deref(),
        Some("18.3.1"),
        "the lock must stay on the react 18 line"
    );
    assert_eq!(
        package_lock_version(&fixture, "react-dom").as_deref(),
        Some("18.3.1"),
        "the broken intermediate (react-dom@19 beside react@18) must not persist"
    );
    assert_eq!(
        manifest_before,
        fixture.read_bytes("package.json"),
        "the rolled-back candidate must not leak its widened manifest"
    );
    assert_eq!(
        lock_before,
        fixture.read_bytes("package-lock.json"),
        "the lock must be byte-identical after the fully-held run"
    );

    // Convergence: the deadlocked pair reports identically on a second run and moves nothing.
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(second.summary_applied(), 0);
    assert!(second.skipped_reasons_for("react").contains("peer_held"));
    assert!(
        second
            .skipped_reasons_for("react-dom")
            .contains("peer_held")
    );
    assert_eq!(lock_before, fixture.read_bytes("package-lock.json"));
}

const PEER_CONTRACT_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-peer-contract",
      "version": "1.2.0",
      "private": true,
      "devDependencies": { "chalk": "^5.6.0" },
      "peerDependencies": { "chalk": ">=5.6.0 <5.6.2" }
    }
"#};

const PEER_CONTRACT_SEED_BEFORE: &str = "2025-09-01T00:00:00Z";

/// A package's own `peerDependencies` is a contract it publishes to *its* consumers, so cooldown
/// never rewrites it — not even under `--rewrite`, which rewrites every other declaration. The
/// author's narrow bound `>=5.6.0 <5.6.2` excludes a mere 5.6.0 → 5.6.2 patch bump, so the move is
/// reported `peer_held` with the manifest byte-identical. The alternative would silently shift the
/// contract to `^5.6.2` and drop the consumers on 5.6.0/5.6.1 that the author still supports.
#[test]
fn upgrade_never_rewrites_a_published_peer_contract_to_land_a_move() {
    skip_if_missing!("npm");
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", PEER_CONTRACT_PACKAGE_JSON);
    fixture.write(".npmrc", LOCKSTEP_PEER_NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={PEER_CONTRACT_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.0"),
        "the seed must start below the peer contract's upper bound"
    );
    let manifest_before = fixture.read_bytes("package.json");
    let lock_before = fixture.read_bytes("package-lock.json");

    let report = fixture.cooldown_json(&["upgrade", "--rewrite", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report.skipped_reasons_for("chalk").contains("peer_held"),
        "the published contract must hold the move, got {:?}",
        report.skipped_reasons_for("chalk")
    );
    assert_eq!(
        report.skipped_offending_for("chalk").as_deref(),
        Some("cooldown-npm-peer-contract"),
        "the hold names the package whose contract must be edited"
    );
    assert_eq!(report.summary_applied(), 0, "nothing may land");
    assert_eq!(
        manifest_before,
        fixture.read_bytes("package.json"),
        "`--rewrite` must not touch the published peer contract"
    );
    assert_eq!(
        lock_before,
        fixture.read_bytes("package-lock.json"),
        "the lock must be byte-identical after the held run"
    );

    // Editing the contract is what authorizes the move — and then it lands.
    fixture.write(
        "package.json",
        &PEER_CONTRACT_PACKAGE_JSON.replace(">=5.6.0 <5.6.2", ">=5.6.0 <5.7.0"),
    );
    let authorized = fixture.cooldown_json(&["upgrade", "--rewrite", "--freeze", FREEZE]);
    assert!(
        authorized.skipped_reasons_for("chalk").is_empty(),
        "the widened contract releases the hold, got {:?}",
        authorized.skipped_reasons_for("chalk")
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.2"),
        "the move lands once the author authorizes it"
    );
    assert!(
        String::from_utf8_lossy(&fixture.read_bytes("package.json")).contains(">=5.6.0 <5.7.0"),
        "the author's edited contract is preserved verbatim"
    );
}

const MIXED_PEER_ROOT_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-mixed-peer",
      "version": "1.0.0",
      "private": true,
      "workspaces": ["apps/*"],
      "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" }
    }
"#};

const MIXED_PEER_MEMBER_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "mixed-peer-app",
      "version": "1.0.0",
      "devDependencies": { "chalk": "^5.6.0" }
    }
"#};

const PEER_ONLY_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-peer-only",
      "version": "1.0.0",
      "private": true,
      "peerDependencies": { "chalk": ">=5.6.0 <5.7.0" }
    }
"#};

/// The root publishes a peer contract that *admits* the target while a member declares the install.
/// The move must land without touching the root contract. The exact install is scoped to the member
/// that owns the install declaration, then the authorized manifests are restored and the lock is
/// synchronized without saving.
#[test]
fn upgrade_moves_a_member_install_without_saving_into_the_roots_peer_contract() {
    skip_if_missing!("npm");
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", MIXED_PEER_ROOT_PACKAGE_JSON);
    fixture.write("apps/app/package.json", MIXED_PEER_MEMBER_PACKAGE_JSON);
    fixture.write(".npmrc", LOCKSTEP_PEER_NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={PEER_CONTRACT_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.0"),
        "the seed must start below the target"
    );
    let root_before = fixture.read_bytes("package.json");

    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert_eq!(
        report.changes_for("chalk"),
        vec![ChangeVersions::new("5.6.0", "5.6.2")],
        "the admitted move must land: {report:?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.2")
    );
    assert_eq!(
        root_before,
        fixture.read_bytes("package.json"),
        "the root's published peer contract must be byte-identical"
    );
    assert!(
        String::from_utf8_lossy(&fixture.read_bytes("apps/app/package.json"))
            .contains(r#""chalk": "^5.6.2""#),
        "the member's install declaration is the entry that widens"
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

/// A dependency declared *only* in `peerDependencies` is still declared: npm installs the root's
/// peers, so the lock carries it and an in-range move is real work. Nothing may be rewritten, so
/// the move uses a bracketed exact pin that restores the published contract. An empty widen write
/// set must not be mistaken for an absent declaration and reported `not_eligible`.
#[test]
fn upgrade_advances_a_peer_only_declaration_without_rewriting_it() {
    skip_if_missing!("npm");
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", PEER_ONLY_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={PEER_CONTRACT_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.0"),
        "npm installs the root's own peer dependency"
    );
    let manifest_before = fixture.read_bytes("package.json");

    let report = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        !report.skipped_reasons_for("chalk").contains("not_eligible"),
        "a peer-only declaration is declared, not absent: {report:?}"
    );
    assert_eq!(
        report.changes_for("chalk"),
        vec![ChangeVersions::new("5.6.0", "5.6.2")],
        "the in-range move lands through the manifest-preserving updater: {report:?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.2")
    );
    assert_eq!(
        manifest_before,
        fixture.read_bytes("package.json"),
        "the published contract must be byte-identical"
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

const BROAD_PEER_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-broad-peer",
      "version": "1.0.0",
      "private": true,
      "peerDependencies": { "chalk": ">=5 <7" }
    }
"#};

/// A cutoff *after* chalk 6.0.0's publish (2026-07-26), so the broad `>=5 <7` range's maximum is
/// reachable and differs from the planned target. A range whose maximum happens to equal the target
/// would mask both failures this test exists for.
const BROAD_PEER_FREEZE: &str = "2026-07-29T00:00:00Z";

const TILDE_WIDEN_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-tilde-widen",
      "version": "1.0.0",
      "private": true,
      "dependencies": { "chalk": "~5.6.0" }
    }
"#};

const MIXED_WIDEN_ROOT_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-mixed-widen",
      "version": "1.0.0",
      "private": true,
      "workspaces": ["apps/*"],
      "dependencies": { "chalk": ">=5 <7" }
    }
"#};

const MIXED_WIDEN_MEMBER_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "mixed-widen-app",
      "version": "1.0.0",
      "dependencies": { "chalk": "^5.6.0" }
    }
"#};

const WORKSPACE_METADATA_ROOT_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-workspace-metadata",
      "version": "1.0.0",
      "private": true,
      "workspaces": ["apps/*"],
      "dependencies": { "chalk": "^5.6.0" }
    }
"#};

const WORKSPACE_METADATA_MEMBER_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "workspace-metadata-app",
      "version": "1.0.0",
      "dependencies": { "chalk": ">=5 <7" }
    }
"#};

fn chalk_six_fixture(root_manifest: &str, member_manifest: Option<&str>) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", root_manifest);
    if let Some(member_manifest) = member_manifest {
        fixture.write("apps/app/package.json", member_manifest);
    }
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={PEER_CONTRACT_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.0"),
        "the fixture must seed the last release before chalk 6"
    );
    fixture
}

/// npm's exact pin may save its default caret range, but cooldown's authorized tilde widening
/// remains in both the manifest and copied lock metadata and converges with the exact target locked.
#[test]
fn upgrade_preserves_a_tilde_range_when_landing_an_npm_major() {
    skip_if_missing!("npm");
    let fixture = chalk_six_fixture(TILDE_WIDEN_PACKAGE_JSON, None);

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);

    assert_eq!(
        report.changes_for("chalk"),
        vec![ChangeVersions::new("5.6.0", "6.0.0")],
        "the exact planned major must land: {report:?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("6.0.0"),
        "the lock must hold the exact planned target"
    );
    let manifest_after = fixture.read_bytes("package.json");
    assert!(
        String::from_utf8_lossy(&manifest_after).contains(r#""chalk": "~6.0.0""#),
        "npm must not broaden cooldown's authorized tilde range"
    );
    assert_eq!(
        package_lock_dependency_range(&fixture, "", "chalk").as_deref(),
        Some("~6.0.0"),
        "the lock's copied root metadata must match the authorized manifest range"
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(manifest_after, fixture.read_bytes("package.json"));
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

/// A plain npm relock must recopy cooldown's restored root range instead of retaining the exact
/// pin's saved caret range in `package-lock.json`.
#[test]
fn upgrade_resynchronizes_root_lock_metadata_after_restoring_the_manifest() {
    skip_if_missing!("npm");
    let fixture = chalk_six_fixture(TILDE_WIDEN_PACKAGE_JSON, None);

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);

    assert_eq!(
        report.changes_for("chalk"),
        vec![ChangeVersions::new("5.6.0", "6.0.0")],
        "the exact planned major must land: {report:?}"
    );
    assert!(
        String::from_utf8_lossy(&fixture.read_bytes("package.json"))
            .contains(r#""chalk": "~6.0.0""#),
        "cooldown's authorized tilde range must be restored after npm's exact pin"
    );
    assert_eq!(
        package_lock_dependency_range(&fixture, "", "chalk").as_deref(),
        Some("~6.0.0"),
        "the root lock metadata must be recopied from the restored manifest"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("6.0.0"),
        "metadata synchronization must retain the exact pinned target"
    );

    let manifest_after = fixture.read_bytes("package.json");
    let lock_after = fixture.read_bytes("package-lock.json");
    assert_plain_npm_relock_is_noop(&fixture);
    assert_eq!(manifest_after, fixture.read_bytes("package.json"));
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(manifest_after, fixture.read_bytes("package.json"));
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

/// Automatic rewriting changes only the workspace member whose range excludes the target. Both the
/// manifests and copied lock metadata retain the compatible root range and authorized member edit.
#[test]
fn upgrade_keeps_a_compatible_root_range_while_widening_a_member() {
    skip_if_missing!("npm");
    let fixture = chalk_six_fixture(
        MIXED_WIDEN_ROOT_PACKAGE_JSON,
        Some(MIXED_WIDEN_MEMBER_PACKAGE_JSON),
    );

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);

    assert_eq!(
        report.changes_for("chalk"),
        vec![ChangeVersions::new("5.6.0", "6.0.0")],
        "the exact planned major must land: {report:?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("6.0.0"),
        "the lock must hold the exact planned target"
    );
    let root_after = fixture.read_bytes("package.json");
    let member_after = fixture.read_bytes("apps/app/package.json");
    assert_eq!(
        root_after,
        MIXED_WIDEN_ROOT_PACKAGE_JSON.as_bytes(),
        "the already-compatible root range remains byte-identical"
    );
    assert!(
        String::from_utf8_lossy(&member_after).contains(r#""chalk": "^6.0.0""#),
        "the incompatible member receives cooldown's authorized widening"
    );
    assert_eq!(
        package_lock_dependency_range(&fixture, "", "chalk").as_deref(),
        Some(">=5 <7"),
        "the lock's copied root metadata must preserve the compatible root range"
    );
    assert_eq!(
        package_lock_dependency_range(&fixture, "apps/app", "chalk").as_deref(),
        Some("^6.0.0"),
        "the lock's copied member metadata must match the authorized member widening"
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(root_after, fixture.read_bytes("package.json"));
    assert_eq!(member_after, fixture.read_bytes("apps/app/package.json"));
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

/// A plain npm relock must recopy every workspace range after restoration, including a compatible
/// member range that the exact root pin copied incorrectly into `package-lock.json`.
#[test]
fn upgrade_resynchronizes_workspace_lock_metadata_after_restoring_manifests() {
    skip_if_missing!("npm");
    let fixture = chalk_six_fixture(
        WORKSPACE_METADATA_ROOT_PACKAGE_JSON,
        Some(WORKSPACE_METADATA_MEMBER_PACKAGE_JSON),
    );

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);

    assert_eq!(
        report.changes_for("chalk"),
        vec![ChangeVersions::new("5.6.0", "6.0.0")],
        "the exact planned major must land: {report:?}"
    );
    assert!(
        String::from_utf8_lossy(&fixture.read_bytes("package.json"))
            .contains(r#""chalk": "^6.0.0""#),
        "the incompatible root range receives cooldown's authorized widening"
    );
    assert_eq!(
        fixture.read_bytes("apps/app/package.json"),
        WORKSPACE_METADATA_MEMBER_PACKAGE_JSON.as_bytes(),
        "the compatible member manifest must remain byte-identical"
    );
    assert_eq!(
        package_lock_dependency_range(&fixture, "", "chalk").as_deref(),
        Some("^6.0.0"),
        "the root lock metadata must match the authorized widening"
    );
    assert_eq!(
        package_lock_dependency_range(&fixture, "apps/app", "chalk").as_deref(),
        Some(">=5 <7"),
        "the member lock metadata must be recopied from the compatible manifest range"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("6.0.0"),
        "metadata synchronization must retain the exact pinned target"
    );

    let root_after = fixture.read_bytes("package.json");
    let member_after = fixture.read_bytes("apps/app/package.json");
    let lock_after = fixture.read_bytes("package-lock.json");
    assert_plain_npm_relock_is_noop(&fixture);
    assert_eq!(root_after, fixture.read_bytes("package.json"));
    assert_eq!(member_after, fixture.read_bytes("apps/app/package.json"));
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", BROAD_PEER_FREEZE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(root_after, fixture.read_bytes("package.json"));
    assert_eq!(member_after, fixture.read_bytes("apps/app/package.json"));
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

/// Landing a peer-only declaration must be **target-directed**, not "newest the range admits".
/// Under `>=5 <7` with `--major` off, cooldown plans the 5.x patch while the range's maximum is
/// 6.0.0: a range-maximum update overshoots, fails the exact-target check, and the safe patch never
/// lands. The manifest must still come out byte-identical.
#[test]
fn upgrade_lands_the_exact_target_under_a_broad_peer_range() {
    skip_if_missing!("npm");
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", BROAD_PEER_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={PEER_CONTRACT_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.0"),
        "the seed must start below both the target and the range maximum"
    );
    let manifest_before = fixture.read_bytes("package.json");

    // No `--major`: the plan stays on the 5.x line even though the range admits 6.0.0, which is
    // reported separately as `needs_major`.
    let report = fixture.cooldown_json(&["upgrade", "--freeze", BROAD_PEER_FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report.applied_names().contains("chalk"),
        "the planned patch must land, not be rejected as an overshoot: {report:?}"
    );
    assert!(
        report
            .changes_for("chalk")
            .contains(&ChangeVersions::new("5.6.0", "5.6.2")),
        "the exact planned target is the one reported: {report:?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.2"),
        "the lock holds the exact planned target"
    );
    assert_eq!(
        manifest_before,
        fixture.read_bytes("package.json"),
        "the published peer contract must be byte-identical"
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", BROAD_PEER_FREEZE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

/// `fix` rolls a too-fresh pin *backwards*, which a within-range update cannot do at all — it would
/// exit successfully without moving and leave the violation installed. The exact pin must downgrade,
/// again without touching the published contract.
#[test]
fn fix_downgrades_a_peer_only_declaration_without_rewriting_it() {
    skip_if_missing!("npm");
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", PEER_ONLY_PACKAGE_JSON);
    fixture.write(".npmrc", NPMRC);
    // Seed the newest 5.x, then judge against a cutoff that predates it: the locked pin is a
    // violation and the remedy is a downgrade to 5.6.0.
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.2"),
        "the seed must start on a pin the cutoff rejects"
    );
    let manifest_before = fixture.read_bytes("package.json");

    let report = fixture.cooldown_json(&["fix", "--freeze", PEER_CONTRACT_SEED_BEFORE]);
    assert!(report.ok(), "fix should succeed");
    assert_eq!(
        report.changes_for("chalk"),
        vec![ChangeVersions::new("5.6.2", "5.6.0")],
        "the downgrade must land: a within-range update could not move backwards: {report:?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("5.6.0"),
        "the violation must be remediated in the lock"
    );
    assert_eq!(
        manifest_before,
        fixture.read_bytes("package.json"),
        "the published peer contract must be byte-identical"
    );

    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["fix", "--freeze", PEER_CONTRACT_SEED_BEFORE]);
    assert_eq!(second.summary_applied(), 0, "the second run is a no-op");
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}

const HOISTED_ROOT_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-npm-hoisted-peer",
      "version": "0.1.0",
      "private": true,
      "workspaces": ["apps/*"],
      "dependencies": {
        "react": "^18.3.1"
      }
    }
"#};

const HOISTED_MEMBER_PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "app-a",
      "version": "0.1.0",
      "dependencies": {
        "react-dom": "^18.3.1"
      }
    }
"#};

/// The lockstep pair split across *disjoint* workspace members: the root declares `react`, only
/// `apps/a` declares `react-dom` — yet npm hoists both to the root `node_modules`, so the peer
/// contract binds across the declaration boundary. Declaration disjointness must not let either
/// side of the break through: the gate judges the physical layout, holds `react` up front, and
/// rolls back `react-dom`'s solo landing (which `legacy-peer-deps` would otherwise commit beside
/// the held `react@18`).
#[test]
fn upgrade_holds_a_hoisted_peer_contract_across_disjoint_workspace_members() {
    skip_if_missing!("npm");
    let fixture = Fixture::new().tag_independent();
    fixture.write("package.json", HOISTED_ROOT_PACKAGE_JSON);
    fixture.write("apps/a/package.json", HOISTED_MEMBER_PACKAGE_JSON);
    fixture.write(".npmrc", LOCKSTEP_PEER_NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={LOCKSTEP_SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    assert_eq!(
        package_lock_version(&fixture, "react").as_deref(),
        Some("18.3.1"),
        "the seed must start on the react 18 line"
    );
    assert_eq!(
        package_lock_version(&fixture, "react-dom").as_deref(),
        Some("18.3.1"),
        "the member's react-dom must hoist beside it"
    );
    let lock_before = fixture.read_bytes("package-lock.json");
    let member_manifest_before = fixture.read_bytes("apps/a/package.json");

    let report = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert!(report.ok(), "upgrade should succeed");
    assert!(
        report.skipped_reasons_for("react").contains("peer_held"),
        "react must be held by the hoisted react-dom@18 contract despite the disjoint members, got {:?}",
        report.skipped_reasons_for("react")
    );
    assert_eq!(
        report.skipped_offending_for("react").as_deref(),
        Some("react-dom"),
        "react's hold blames the dependent"
    );
    assert!(
        report
            .skipped_reasons_for("react-dom")
            .contains("peer_held"),
        "react-dom's solo landing must be rolled back, got {:?}",
        report.skipped_reasons_for("react-dom")
    );
    assert_eq!(
        package_lock_version(&fixture, "react").as_deref(),
        Some("18.3.1"),
        "the lock must stay on the react 18 line"
    );
    assert_eq!(
        package_lock_version(&fixture, "react-dom").as_deref(),
        Some("18.3.1"),
        "the broken intermediate (react-dom@19 beside react@18) must not persist"
    );
    assert_eq!(
        lock_before,
        fixture.read_bytes("package-lock.json"),
        "the lock must be byte-identical after the fully-held run"
    );
    assert_eq!(
        member_manifest_before,
        fixture.read_bytes("apps/a/package.json"),
        "the rolled-back candidate must not leak its widened member manifest"
    );

    // Convergence: a second run reports the same holds and moves nothing.
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(second.summary_applied(), 0);
    assert!(second.skipped_reasons_for("react").contains("peer_held"));
    assert_eq!(lock_before, fixture.read_bytes("package-lock.json"));
}

/// A cutoff after chalk 4.1.1 and before 4.1.2, so a 4.1.2 seed is too fresh and 4.1.1 is the
/// matured downgrade.
const HIDDEN_PIN_FIX_FREEZE: &str = "2021-06-01T00:00:00Z";

/// A package declaring `chalk` at a caret in `dependencies` and at the exact `pin` in
/// `devDependencies`.
/// npm resolves the pin and records both declarations in the lock's root entry, so the dependency
/// is not pinned as a whole and the planner moves it.
fn two_field_fixture(pin: &str) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "package.json",
        &indoc::formatdoc! {r#"
            {{
              "name": "cooldown-npm-two-fields",
              "version": "0.1.0",
              "private": true,
              "dependencies": {{ "chalk": "^4.1.0" }},
              "devDependencies": {{ "chalk": "{pin}" }}
            }}
        "#},
    );
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
            ],
            &[],
        )
        .expect_success();
    // The lock records the canonical version, never the pin's `=` or `v` spelling.
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some(pinned_version(pin))
    );
    fixture
}

/// The version an exact `pin` names, its `=`, spacing, and `v` prefix stripped (`= v4.1.0` →
/// `4.1.0`).
fn pinned_version(pin: &str) -> &str {
    let unequal = pin.trim().trim_start_matches('=').trim_start();
    unequal.strip_prefix('v').unwrap_or(unequal)
}

fn declared_chalk(fixture: &Fixture, field: &str) -> String {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package.json")).expect("manifest parses");
    manifest
        .get(field)
        .and_then(|section| section.get("chalk"))
        .and_then(serde_json::Value::as_str)
        .expect("chalk is declared")
        .to_string()
}

/// The exact pin in `devDependencies` holds an in-line move the caret beside it admits: the row
/// names the pin and nothing changes, until `--rewrite`, planned through the real planner, rewrites
/// the pin and lands the move.
#[test]
fn upgrade_holds_a_hidden_exact_pin_until_rewrite() {
    skip_if_missing!("npm");
    assert_hidden_pin_holds_until_rewrite("4.1.0", "");
}

/// A `v`-prefixed pin is held up front like a bare one, rather than widened to a caret and refused
/// after the resolve, and `--rewrite` rewrites it to the bare target.
#[test]
fn upgrade_holds_a_v_prefixed_hidden_exact_pin_until_rewrite() {
    skip_if_missing!("npm");
    assert_hidden_pin_holds_until_rewrite("v4.1.0", "");
}

/// Runs the hidden-pin upgrade against a `devDependencies` pin spelled `pin`, expecting the
/// authorized rewrite to write `rewritten` before the target, and a second run to change nothing.
fn assert_hidden_pin_holds_until_rewrite(pin: &str, rewritten: &str) {
    let fixture = two_field_fixture(pin);
    let manifest_before = fixture.read_bytes("package.json");
    let lock_before = fixture.read_bytes("package-lock.json");

    let held = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    // The held-back chalk 5 major is reported beside it.
    assert!(
        held.skipped_reasons_for("chalk").contains("pin_held"),
        "{held:#?}"
    );
    assert_eq!(held.summary_applied(), 0, "{held:#?}");
    let detail = held.skip_detail_for("chalk").unwrap_or_default();
    assert!(
        detail.contains(&format!(
            "the exact pin {pin} in devDependencies of package.json excludes"
        )),
        "{detail}"
    );
    assert!(detail.contains("--rewrite"), "{detail}");
    assert_eq!(manifest_before, fixture.read_bytes("package.json"));
    assert_eq!(lock_before, fixture.read_bytes("package-lock.json"));

    let rewritten_run = fixture.cooldown_json(&["upgrade", "--rewrite", "--freeze", FREEZE]);
    assert!(
        rewritten_run.applied_names().contains("chalk"),
        "{rewritten_run:#?}"
    );
    let target = package_lock_version(&fixture, "chalk").expect("chalk is locked");
    assert_ne!(target, pinned_version(pin));
    assert_eq!(
        declared_chalk(&fixture, "devDependencies"),
        format!("{rewritten}{target}")
    );

    // Convergence: neither a plain run nor the authorized rewrite moves the rewritten pin again.
    let manifest_after = fixture.read_bytes("package.json");
    let lock_after = fixture.read_bytes("package-lock.json");
    for args in [
        &["upgrade", "--freeze", FREEZE][..],
        &["upgrade", "--rewrite", "--freeze", FREEZE][..],
    ] {
        let second = fixture.cooldown_json(args);
        assert!(second.ok(), "{args:?}: {second:#?}");
        assert_eq!(second.summary_applied(), 0, "{args:?}: {second:#?}");
        assert_eq!(
            manifest_after,
            fixture.read_bytes("package.json"),
            "{args:?}"
        );
        assert_eq!(
            lock_after,
            fixture.read_bytes("package-lock.json"),
            "{args:?}"
        );
    }
}

/// A plain `fix` holds a downgrade off a too-fresh exact pin in `devDependencies`, naming
/// `--downgrade-pinned`, and fails under `--strict`; `--downgrade-pinned` then rewrites the pin,
/// lands the downgrade, and passes.
#[test]
fn fix_downgrades_a_hidden_exact_pin_only_with_downgrade_pinned() {
    skip_if_missing!("npm");
    assert_hidden_pin_downgrades_only_with_downgrade_pinned("4.1.2", "");
}

/// A spaced `= v` pin is held and downgraded like a bare one, and `--downgrade-pinned` keeps it an
/// `=` equality.
#[test]
fn fix_downgrades_a_spaced_hidden_exact_pin_only_with_downgrade_pinned() {
    skip_if_missing!("npm");
    assert_hidden_pin_downgrades_only_with_downgrade_pinned("= v4.1.2", "=");
}

/// Runs the hidden-pin `fix` against a too-fresh `devDependencies` pin spelled `pin`, expecting
/// `--downgrade-pinned` to write `rewritten` before the matured version, and a second strict run
/// to pass without changes.
fn assert_hidden_pin_downgrades_only_with_downgrade_pinned(pin: &str, rewritten: &str) {
    let fixture = two_field_fixture(pin);
    let manifest_before = fixture.read_bytes("package.json");
    let lock_before = fixture.read_bytes("package-lock.json");

    // The held downgrade leaves the violation in place, so a strict `fix` fails.
    let held = fixture.cooldown_json(&["fix", "--strict", "--freeze", HIDDEN_PIN_FIX_FREEZE]);
    assert!(!held.ok(), "{held:#?}");
    assert_eq!(held.summary_applied(), 0, "{held:#?}");
    assert!(
        held.skipped_reasons_for("chalk").contains("pin_held"),
        "{held:#?}"
    );
    let detail = held.skip_detail_for("chalk").unwrap_or_default();
    assert!(
        detail.contains(&format!(
            "the exact pin {pin} in devDependencies of package.json excludes 4.1.1"
        )),
        "{held:#?}"
    );
    assert!(detail.contains("--downgrade-pinned"), "{detail}");
    assert!(!detail.contains("--rewrite"), "{detail}");
    assert_eq!(manifest_before, fixture.read_bytes("package.json"));
    assert_eq!(lock_before, fixture.read_bytes("package-lock.json"));

    let fixed = fixture.cooldown_json(&[
        "fix",
        "--strict",
        "--downgrade-pinned",
        "--freeze",
        HIDDEN_PIN_FIX_FREEZE,
    ]);
    assert!(fixed.ok(), "{fixed:#?}");
    assert_eq!(
        package_lock_version(&fixture, "chalk").as_deref(),
        Some("4.1.1"),
        "{fixed:#?}"
    );
    assert_eq!(
        declared_chalk(&fixture, "devDependencies"),
        format!("{rewritten}4.1.1")
    );
    assert_eq!(declared_chalk(&fixture, "dependencies"), "^4.1.0");

    // Convergence: the downgraded pin passes a second strict run untouched, with or without the
    // authority that rewrote it.
    let manifest_after = fixture.read_bytes("package.json");
    let lock_after = fixture.read_bytes("package-lock.json");
    for args in [
        &["fix", "--strict", "--freeze", HIDDEN_PIN_FIX_FREEZE][..],
        &[
            "fix",
            "--strict",
            "--downgrade-pinned",
            "--freeze",
            HIDDEN_PIN_FIX_FREEZE,
        ][..],
    ] {
        let second = fixture.cooldown_json(args);
        assert!(second.ok(), "{args:?}: {second:#?}");
        assert_eq!(second.summary_applied(), 0, "{args:?}: {second:#?}");
        assert_eq!(
            manifest_after,
            fixture.read_bytes("package.json"),
            "{args:?}"
        );
        assert_eq!(
            lock_after,
            fixture.read_bytes("package-lock.json"),
            "{args:?}"
        );
    }
}

/// A workspace member pinning `chalk` exactly in `devDependencies` beside the caret npm also
/// records, while the root declares `ansi-regex` at a caret, seeded under `before`.
fn member_pin_beside_sibling_fixture(before: &str) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "package.json",
        indoc! {r#"
            {
              "name": "cooldown-npm-member-pin",
              "version": "0.1.0",
              "private": true,
              "workspaces": ["pkgs/a"],
              "dependencies": { "ansi-regex": "^5.0.0" }
            }
        "#},
    );
    fixture.write(
        "pkgs/a/package.json",
        indoc! {r#"
            {
              "name": "@cooldown/pinned",
              "version": "0.1.0",
              "dependencies": { "chalk": "^4.1.0" },
              "devDependencies": { "chalk": "4.1.0" }
            }
        "#},
    );
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                &format!("--before={before}"),
            ],
            &[],
        )
        .expect_success();
    fixture
}

/// The version the install tree resolves `name` to from the member directory `member`, walking up
/// to the root like npm's lookup.
fn member_resolved_version(fixture: &Fixture, member: &str, name: &str) -> Option<String> {
    let lock: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package-lock.json")).expect("lock parses");
    let packages = lock.get("packages")?;
    [
        format!("{member}/node_modules/{name}"),
        format!("node_modules/{name}"),
    ]
    .iter()
    .find_map(|key| packages.get(key)?.get("version")?.as_str())
    .map(str::to_owned)
}

/// A sibling landing beside a member's exact pin, which no planned change names, is judged at the
/// member's own resolution in npm's real install tree: the pin stays satisfied, so the sibling
/// lands with no false refusal.
#[test]
fn upgrade_lands_a_sibling_beside_a_protected_member_pin() {
    skip_if_missing!("npm");
    let fixture = member_pin_beside_sibling_fixture("2021-01-01T00:00:00Z");
    let pinned_manifest = fixture.read_bytes("pkgs/a/package.json");
    assert_eq!(
        member_resolved_version(&fixture, "pkgs/a", "chalk").as_deref(),
        Some("4.1.0")
    );
    assert_eq!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("5.0.0")
    );

    let upgrade =
        fixture.cooldown_json(&["upgrade", "--package", "ansi-regex", "--freeze", FREEZE]);

    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(
        upgrade.applied_names().contains("ansi-regex"),
        "{upgrade:#?}"
    );
    // The held-back ansi-regex 6 major is reported beside it; nothing is refused.
    assert!(
        !upgrade.skipped_reasons().contains("resolver_conflict"),
        "{upgrade:#?}"
    );
    assert_ne!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("5.0.0")
    );
    assert_eq!(
        member_resolved_version(&fixture, "pkgs/a", "chalk").as_deref(),
        Some("4.1.0")
    );
    assert_eq!(fixture.read_bytes("pkgs/a/package.json"), pinned_manifest);
}

/// A root declaring `ansi-regex` at `^4.1.0` beside an alias `ansi-five` of `ansi-regex@^5.0.0`,
/// seeded before ansi-regex 4.1.1 and 5.0.1, so both lines have a matured in-range patch.
fn alias_collision_fixture() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "package.json",
        indoc! {r#"
            {
              "name": "cooldown-npm-alias-collision",
              "version": "0.1.0",
              "private": true,
              "dependencies": { "ansi-regex": "^4.1.0", "ansi-five": "npm:ansi-regex@^5.0.0" }
            }
        "#},
    );
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--before=2021-01-01T00:00:00Z",
            ],
            &[],
        )
        .expect_success();
    fixture
}

/// The `dependencies` range the root manifest declares for `name`.
fn root_range(fixture: &Fixture, name: &str) -> Option<String> {
    let manifest: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package.json")).expect("manifest parses");
    manifest
        .pointer(&format!("/dependencies/{name}"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// An alias installing `ansi-regex@5` beside the root's own `ansi-regex@^4.1.0` gives the alias's
/// copy no claim on that ordinary declaration: `upgrade` plans no move of the 5.0.0 copy, which
/// would rewrite `^4.1.0` across a major, while the ordinary copy lands its 4.1.1 patch, the alias
/// and its copy stay as they were, and the result is a plain-relock and second-run fixed point.
#[test]
fn upgrade_never_moves_an_alias_target_through_an_ordinary_declaration() {
    skip_if_missing!("npm");
    let fixture = alias_collision_fixture();
    let alias = (Some("ansi-regex".to_string()), Some("5.0.0".to_string()));
    assert_eq!(
        lock_entry_identity(&fixture, "node_modules/ansi-five"),
        alias
    );
    assert_eq!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("4.1.0")
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(upgrade.ok(), "{upgrade:#?}");
    assert!(
        upgrade
            .changes_for("ansi-regex")
            .iter()
            .all(|change| change.from != "5.0.0"),
        "the alias's copy is planned through no declaration: {upgrade:#?}"
    );
    assert_eq!(
        upgrade.applied_names(),
        std::collections::BTreeSet::from(["ansi-regex".to_string()]),
        "{upgrade:#?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("4.1.1")
    );
    assert_eq!(
        lock_entry_identity(&fixture, "node_modules/ansi-five"),
        alias
    );
    assert_eq!(
        root_range(&fixture, "ansi-regex").as_deref(),
        Some("^4.1.1")
    );
    assert_eq!(
        root_range(&fixture, "ansi-five").as_deref(),
        Some("npm:ansi-regex@^5.0.0")
    );
    assert_plain_npm_relock_is_noop(&fixture);

    // Convergence: a second run moves nothing.
    let manifest_after = fixture.read_bytes("package.json");
    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(second.ok(), "{second:#?}");
    assert_eq!(second.summary_applied(), 0, "{second:#?}");
    assert_eq!(fixture.read_bytes("package.json"), manifest_after);
    assert_eq!(fixture.read_bytes("package-lock.json"), lock_after);
}

/// A root declaring `ansi-regex` at `^4.1.0` beside `strip-ansi@6.0.0`, whose own `^5.0.0` npm
/// nests as `ansi-regex@5.0.0` under it, seeded before ansi-regex 4.1.1 and 5.0.1, so both lines
/// have a matured in-range patch.
/// With `alias`, the root also aliases `ansi-five` to `ansi-regex@^5.0.0`, so the alias's release
/// has a genuine copy too, which keeps the alias safeguard from deciding anything.
fn nested_duplicate_fixture(alias: bool) -> Fixture {
    let fixture = Fixture::new().tag_independent();
    let aliased = if alias {
        r#", "ansi-five": "npm:ansi-regex@^5.0.0""#
    } else {
        ""
    };
    fixture.write(
        "package.json",
        &formatdoc! {r#"
            {{
              "name": "cooldown-npm-nested-duplicate",
              "version": "0.1.0",
              "private": true,
              "dependencies": {{ "ansi-regex": "^4.1.0", "strip-ansi": "6.0.0"{aliased} }}
            }}
        "#},
    );
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--before=2021-01-01T00:00:00Z",
            ],
            &[],
        )
        .expect_success();
    fixture
}

/// The nested `ansi-regex@5.0.0` belongs to `strip-ansi`, not to the root's `^4.1.0`, which binds
/// the hoisted `4.1.0`: `upgrade` without `--major` plans the root only its own `4.1.0 -> 4.1.1`,
/// never a `5.x` move that would rewrite `^4.1.0` across a major, and the result is a plain-relock
/// and second-run fixed point.
#[test]
fn upgrade_never_moves_a_nested_duplicate_through_the_roots_declaration() {
    skip_if_missing!("npm");
    assert_nested_duplicate_stays_transitive(false);
}

/// A genuine nested copy of an alias's release does not hand that release to the root's own
/// declaration of the target's name either.
#[test]
fn upgrade_never_moves_a_nested_alias_release_through_the_roots_declaration() {
    skip_if_missing!("npm");
    assert_nested_duplicate_stays_transitive(true);
}

fn assert_nested_duplicate_stays_transitive(alias: bool) {
    let fixture = nested_duplicate_fixture(alias);
    let nested = "node_modules/strip-ansi/node_modules/ansi-regex";
    let aliased = (Some("ansi-regex".to_string()), Some("5.0.0".to_string()));
    // The topology must hold the duplicate, or nothing here can reproduce the attribution bug.
    assert_eq!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("4.1.0")
    );
    assert_eq!(
        lock_entry_identity(&fixture, nested),
        (None, Some("5.0.0".to_string()))
    );
    if alias {
        assert_eq!(
            lock_entry_identity(&fixture, "node_modules/ansi-five"),
            aliased
        );
    }

    let planned = fixture.cooldown_json(&["upgrade", "--dry-run", "--freeze", FREEZE]);
    assert!(planned.ok(), "{planned:#?}");
    let rows = planned.changes_for("ansi-regex");
    assert_eq!(
        planned.direct_changes_for("ansi-regex"),
        rows,
        "every ansi-regex row is the root's own: {planned:#?}"
    );
    assert!(
        rows.iter()
            .all(|row| row.from == "4.1.0" && !row.to.starts_with("5.")),
        "no 5.x copy is planned through the root's declaration: {planned:#?}"
    );
    assert_eq!(
        planned.applied_change_for("ansi-regex"),
        Some(ChangeVersions::new("4.1.0", "4.1.1")),
        "{planned:#?}"
    );
    assert_eq!(
        planned.applied_names(),
        std::collections::BTreeSet::from(["ansi-regex".to_string()]),
        "{planned:#?}"
    );

    let upgrade = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_change_for("ansi-regex"),
        Some(ChangeVersions::new("4.1.0", "4.1.1")),
        "{upgrade:#?}"
    );
    // A move of the nested copy may only ever surface as collateral, never as the root's change.
    assert!(
        upgrade
            .direct_changes_for("ansi-regex")
            .iter()
            .all(|row| row.from == "4.1.0" && !row.to.starts_with("5.")),
        "{upgrade:#?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("4.1.1")
    );
    assert_eq!(
        root_range(&fixture, "ansi-regex").as_deref(),
        Some("^4.1.1")
    );
    assert_eq!(
        lock_entry_identity(&fixture, nested).1.as_deref(),
        Some("5.0.0")
    );
    if alias {
        assert_eq!(
            lock_entry_identity(&fixture, "node_modules/ansi-five"),
            aliased
        );
        assert_eq!(
            root_range(&fixture, "ansi-five").as_deref(),
            Some("npm:ansi-regex@^5.0.0")
        );
    }
    assert_plain_npm_relock_is_noop(&fixture);

    // Convergence: a second run moves nothing.
    let manifest_after = fixture.read_bytes("package.json");
    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--freeze", FREEZE]);
    assert!(second.ok(), "{second:#?}");
    assert_eq!(second.summary_applied(), 0, "{second:#?}");
    assert_eq!(fixture.read_bytes("package.json"), manifest_after);
    assert_eq!(fixture.read_bytes("package-lock.json"), lock_after);
}

/// A workspace member pinning `left-pad` exactly, which a root override aliases to `ms`, beside a
/// root `ansi-regex` caret, seeded before ansi-regex 5.0.1.
/// The override is keyed by a member's dependency, never the root's own, since npm rejects an
/// override that conflicts with a root dependency.
fn aliased_member_pin_fixture() -> Fixture {
    let fixture = Fixture::new().tag_independent();
    fixture.write(
        "package.json",
        indoc! {r#"
            {
              "name": "cooldown-npm-aliased-pin",
              "version": "0.1.0",
              "private": true,
              "workspaces": ["pkgs/a"],
              "dependencies": { "ansi-regex": "^5.0.0" },
              "overrides": { "left-pad": "npm:ms@2.1.2" }
            }
        "#},
    );
    fixture.write(
        "pkgs/a/package.json",
        indoc! {r#"
            {
              "name": "@cooldown/aliased",
              "version": "0.1.0",
              "dependencies": { "left-pad": "1.3.0" }
            }
        "#},
    );
    fixture.write(".npmrc", NPMRC);
    fixture
        .run_tool(
            "npm",
            &[
                "install",
                "--package-lock-only",
                "--ignore-scripts",
                "--no-audit",
                "--no-fund",
                "--before=2021-01-01T00:00:00Z",
            ],
            &[],
        )
        .expect_success();
    fixture
}

/// The `name` and `version` the lock records at the install slot `key`.
fn lock_entry_identity(fixture: &Fixture, key: &str) -> (Option<String>, Option<String>) {
    let lock: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package-lock.json")).expect("lock parses");
    let entry = lock.get("packages").and_then(|packages| packages.get(key));
    let field = |field: &str| {
        entry
            .and_then(|entry| entry.get(field))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
    };
    (field("name"), field("version"))
}

/// An override aliasing a member's exact pin installs `ms` at `node_modules/left-pad`, which npm
/// records with `name: "ms"`: cooldown reports that copy as `ms`, never as a `left-pad` release,
/// while a sibling lands beside it with the declarations and the alias untouched, and the result
/// is a plain-relock and second-run fixed point.
#[test]
fn upgrade_lands_a_sibling_beside_an_aliased_member_pin() {
    skip_if_missing!("npm");
    let fixture = aliased_member_pin_fixture();
    let member_manifest = fixture.read_bytes("pkgs/a/package.json");
    let alias = (Some("ms".to_string()), Some("2.1.2".to_string()));
    assert_eq!(
        lock_entry_identity(&fixture, "node_modules/left-pad"),
        alias
    );
    assert_eq!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("5.0.0")
    );

    let outdated = fixture.cooldown_json(&["outdated", "--transitive", "--freeze", FREEZE]);
    assert!(outdated.ok(), "{outdated:#?}");
    let names = outdated.item_names();
    assert!(!names.contains("left-pad"), "{outdated:#?}");
    assert_eq!(
        outdated.item_field_str("ms", "current").as_deref(),
        Some("2.1.2"),
        "{outdated:#?}"
    );

    let upgrade =
        fixture.cooldown_json(&["upgrade", "--package", "ansi-regex", "--freeze", FREEZE]);
    assert!(upgrade.ok(), "{upgrade:#?}");
    assert_eq!(
        upgrade.applied_names(),
        std::collections::BTreeSet::from(["ansi-regex".to_string()]),
        "{upgrade:#?}"
    );
    assert!(
        !upgrade.skipped_reasons().contains("resolver_conflict"),
        "{upgrade:#?}"
    );
    assert_eq!(
        package_lock_version(&fixture, "ansi-regex").as_deref(),
        Some("5.0.1")
    );
    assert_eq!(
        lock_entry_identity(&fixture, "node_modules/left-pad"),
        alias
    );
    assert_eq!(fixture.read_bytes("pkgs/a/package.json"), member_manifest);
    let root: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package.json")).expect("manifest parses");
    assert_eq!(
        root.pointer("/overrides/left-pad")
            .and_then(serde_json::Value::as_str),
        Some("npm:ms@2.1.2")
    );
    assert_plain_npm_relock_is_noop(&fixture);

    // Convergence: a second run moves nothing.
    let manifest_after = fixture.read_bytes("package.json");
    let lock_after = fixture.read_bytes("package-lock.json");
    let second = fixture.cooldown_json(&["upgrade", "--package", "ansi-regex", "--freeze", FREEZE]);
    assert!(second.ok(), "{second:#?}");
    assert_eq!(second.summary_applied(), 0, "{second:#?}");
    assert_eq!(fixture.read_bytes("package.json"), manifest_after);
    assert_eq!(fixture.read_bytes("package-lock.json"), lock_after);
}
