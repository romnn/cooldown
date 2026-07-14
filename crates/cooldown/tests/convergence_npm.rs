//! End-to-end regression tests that drive the real npm resolver against a generated package-lock
//! fixture.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::indoc;
use support::Fixture;

const SEED_BEFORE: &str = "2025-05-20T00:00:00Z";
const FREEZE: &str = "2026-06-30T00:00:00Z";

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

fn package_lock_version(fixture: &Fixture, name: &str) -> Option<String> {
    let lock: serde_json::Value =
        serde_json::from_slice(&fixture.read_bytes("package-lock.json")).expect("lock parses");
    lock.get("packages")?
        .get(format!("node_modules/{name}"))?
        .get("version")?
        .as_str()
        .map(str::to_owned)
}

fn residual_isolation_fixture() -> Fixture {
    let fixture = Fixture::new();
    fixture.write("package.json", PACKAGE_JSON);
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
                &format!("--before={SEED_BEFORE}"),
            ],
            &[],
        )
        .expect_success();
    fixture
}

#[test]
fn upgrade_keeps_a_safe_sibling_when_another_candidate_forces_a_fresh_transitive() {
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

    // The two direct targets mature before the cutoff, but jsdoccomment's new range resolves a
    // post-cutoff @typescript-eslint/types. The safe tree-sitter-cli target has no such edge.
    let upgrade = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);

    assert_eq!(
        upgrade.changes_for("tree-sitter-cli"),
        vec![("0.25.4".to_owned(), "0.26.10".to_owned())],
        "the safe sibling must retain its baseline-to-target applied row"
    );
    assert!(
        upgrade.skipped_reasons_for("tree-sitter-cli").is_empty(),
        "the unsafe sibling must not make tree-sitter-cli look policy-blocked"
    );
    assert_eq!(
        upgrade.skipped_reasons_for("@es-joy/jsdoccomment"),
        ["transitive_in_cooldown".to_owned()].into_iter().collect(),
        "only the candidate that forces the fresh transitive is held"
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
    let second = fixture.cooldown_json(&["upgrade", "--major", "--freeze", FREEZE]);
    assert_eq!(second.summary_applied(), 0);
    assert_eq!(second.summary_skipped(), 1);
    assert_eq!(lock_after, fixture.read_bytes("package-lock.json"));
}
