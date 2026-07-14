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

fn residual_isolation_fixture() -> Fixture {
    let fixture = Fixture::new();
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
    let fixture = Fixture::new();
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
        vec![("0.25.4".to_owned(), "0.26.10".to_owned())],
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
        vec![("9.16.0".to_owned(), "10.6.0".to_owned())]
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
