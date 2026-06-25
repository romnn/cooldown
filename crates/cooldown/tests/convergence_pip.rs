//! End-to-end tests for the pip `requirements.txt` adapter. The pip adapter is intentionally simple:
//! a pinned requirements file is both manifest and lock, so a mutation must rewrite that file rather
//! than installing into the ambient Python environment.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use support::Fixture;

/// PyPI history before this instant is immutable. `requests` has a within-major target after 2.28.0
/// but before this cutoff, so the tests assert invariants without hard-coding the selected version.
const FREEZE: &str = "2024-01-01T00:00:00Z";

fn requirements_fixture(contents: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture.write("requirements.txt", contents);
    fixture
}

#[test]
fn check_fails_closed_on_unknown_lock_currency_even_when_stale_allowed() {
    let fixture = requirements_fixture("requests==2.28.0\n");

    let check =
        fixture.cooldown_json(&["check", "--tool", "pip", "--latest", "--allow-stale-lock"]);
    assert!(
        !check.ok(),
        "`--allow-stale-lock` must not downgrade unknown lock currency"
    );
    assert_eq!(check.summary_errors(), 1);
    assert!(
        check.error_kinds().contains("lock_unknown"),
        "expected lock_unknown diagnostic, got {:?}",
        check.error_kinds()
    );
    assert!(
        check
            .error_paths()
            .iter()
            .any(|path| path.ends_with("requirements.txt")),
        "unknown-lock diagnostic should name requirements.txt, got {:?}",
        check.error_paths()
    );
}

#[test]
fn upgrade_rewrites_plain_requirements_file_and_converges() {
    let fixture = requirements_fixture("requests == 2.28.0\n");

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--tool",
        "pip",
        "--freeze",
        FREEZE,
        "--package",
        "requests",
    ]);
    assert!(upgrade.ok(), "pip requirements rewrite should succeed");
    assert_eq!(upgrade.summary_applied(), 1);
    assert_eq!(
        upgrade.lock_status(),
        Some("unknown"),
        "pip mutates requirements.txt, but lock currency is still not independently provable"
    );
    assert!(
        upgrade.warning_kinds().contains("lock_unknown"),
        "unknown lock currency after mutation should be a warning, got {:?}",
        upgrade.warning_kinds()
    );

    let (_from, to) = upgrade
        .change_for("requests")
        .expect("requests should be reported applied");
    let rewritten = String::from_utf8(fixture.read_bytes("requirements.txt")).unwrap();
    assert_eq!(
        rewritten,
        format!("requests == {to}\n"),
        "the project file must contain the exact reported target"
    );

    let after_first = fixture.read_bytes("requirements.txt");
    let second = fixture.cooldown_json(&[
        "upgrade",
        "--tool",
        "pip",
        "--freeze",
        FREEZE,
        "--package",
        "requests",
    ]);
    assert_eq!(
        second.summary_applied(),
        0,
        "second upgrade must be a no-op fixed point"
    );
    assert_eq!(
        after_first,
        fixture.read_bytes("requirements.txt"),
        "requirements.txt must be byte-identical after the converged re-run"
    );
}

#[test]
fn upgrade_refuses_hash_checked_requirements_without_mutating_them() {
    let fixture =
        requirements_fixture("--require-hashes\nrequests==2.28.0 \\\n    --hash=sha256:old\n");
    let before = fixture.read_bytes("requirements.txt");

    let upgrade = fixture.cooldown_json(&[
        "upgrade",
        "--tool",
        "pip",
        "--freeze",
        FREEZE,
        "--package",
        "requests",
    ]);
    assert!(
        upgrade.ok(),
        "hash-checked requirements should be skipped cleanly, not treated as an environment error"
    );
    assert_eq!(upgrade.summary_applied(), 0);
    assert_eq!(upgrade.summary_skipped(), 1);
    assert!(
        upgrade
            .skipped_reasons_for("requests")
            .contains("not_eligible"),
        "hash-checked requests pin should be reported not_eligible, got {:?}",
        upgrade.skipped_reasons_for("requests")
    );
    assert_eq!(
        before,
        fixture.read_bytes("requirements.txt"),
        "hash-checked requirements must remain byte-identical"
    );
}
