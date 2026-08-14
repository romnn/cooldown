//! End-to-end advisory-feed tests: the REAL `cooldown` binary, a real cargo fixture seeded by
//! the real `cargo`, and the live OSV API — the whole pipeline from `[advisories]` config
//! through the batched feed fetch, per-dependency classification, and the JSON report.
//!
//! # Determinism
//!
//! The fixture pins `time = "=0.1.45"`, affected by GHSA-wcg3-cvx6-7396 (RUSTSEC-2020-0071 /
//! CVE-2020-26235, published 2020, moderate severity, fixed in `0.2.23`) — immutable advisory
//! history by now, the advisory analogue of the hard-pinned `clap` regression in
//! `convergence_cargo`.
//! Beyond that one id the assertions are invariants (a security block is present, an adoptable
//! target exists that the ordinary window could never admit), never registry-state snapshots;
//! the check test runs under `min-age = "0d"` so a brand-new release of a transitive dependency
//! cannot turn it red.
//! The exact `=` pin also keeps the row's *graph* verdict (`held`) independent of live registry
//! state — the advisory annotations under test ride on top of it either way.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::indoc;
use support::Fixture;

/// The advisory every test revolves around: `time < 0.2.23` (`localtime_r` UB), moderate
/// severity, fixed in `0.2.23`.
const GHSA: &str = "GHSA-wcg3-cvx6-7396";

const AFFECTED_MANIFEST: &str = indoc! {r#"
    [package]
    name = "advisory-fixture"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    time = "=0.1.45"
"#};

const FIXED_MANIFEST: &str = indoc! {r#"
    [package]
    name = "advisory-fixture"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    time = "=0.2.23"
"#};

/// Seed a lock with the real cargo, then `cargo fetch` so cooldown's offline `cargo metadata`
/// probes work regardless of what the local cargo cache happens to hold.
fn fixture(manifest: &str, cooldown_toml: &str) -> Fixture {
    let fixture = Fixture::new();
    fixture
        .write("Cargo.toml", manifest)
        .write("src/lib.rs", "")
        .write("cooldown.toml", cooldown_toml);
    fixture
        .run_tool("cargo", &["generate-lockfile"], &[])
        .expect_success();
    fixture.run_tool("cargo", &["fetch"], &[]).expect_success();
    fixture
}

/// Flag mode: the row whose candidates escape the advisory carries a `security` block naming
/// the GHSA — annotated, verdict machinery untouched (`applied` stays false, no `shortenedBy`).
#[test]
fn flag_mode_annotates_the_fixing_candidate() {
    crate::skip_if_missing!("cargo");
    let fixture = fixture(
        AFFECTED_MANIFEST,
        indoc! {r#"
            min-age = "7d"

            [advisories]
            enabled = true
        "#},
    );

    let out = fixture.cooldown_json(&["outdated"]);
    let security = out
        .security_for("time")
        .expect("the time row must carry a security block");
    let fixes: Vec<&str> = security["fixes"]
        .as_array()
        .expect("fixes array")
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(
        fixes.contains(&GHSA),
        "fixes must name the advisory, got {fixes:?}"
    );
    assert_eq!(security["source"], "osv");
    assert_eq!(security["applied"], false, "flag mode never applies");
    assert_eq!(
        out.shortened_by_for("time"),
        None,
        "flag mode never shortens"
    );
}

/// Shorten mode: under an ordinary window nothing in `time`'s release history could ever clear
/// (9999 days), yet a candidate fixing the advisory resolves against the 1-day security window
/// — an adoptable target appears that only the shortened window can explain, and vanishes again
/// the moment the feed is disabled.
#[test]
fn shorten_mode_makes_the_fix_adoptable_through_the_security_window() {
    crate::skip_if_missing!("cargo");
    let fixture = fixture(
        AFFECTED_MANIFEST,
        indoc! {r#"
            # Nothing in time's release history can clear this ordinarily.
            min-age = "9999d"

            [advisories]
            enabled = true
            mode = "shorten"
            min-age = "1d"
            severity = "low"  # the advisory is moderate; the default high threshold would decline
        "#},
    );

    let out = fixture.cooldown_json(&["outdated"]);
    let security = out
        .security_for("time")
        .expect("the fixing candidate keeps its security block");
    assert_eq!(security["applied"], true, "the security window applied");
    assert!(
        out.item_field_str("time", "adoptableTarget").is_some(),
        "a 9999d window admits nothing; only the security window can produce a target"
    );

    // The control: disabling the feed on the same fixture removes both the target and the
    // block.
    let control = fixture.cooldown_json(&["outdated", "--no-advisories"]);
    assert_eq!(control.item_field_str("time", "adoptableTarget"), None);
    assert!(control.security_for("time").is_none());
}

/// The check side: a locked version that IS the advisory's fix is tallied `security-relevant`
/// while the gate stays green — the pin a security bump just produced is a signal, never a
/// violation.
#[test]
fn check_tallies_a_locked_fix_version() {
    crate::skip_if_missing!("cargo");
    let fixture = fixture(
        FIXED_MANIFEST,
        indoc! {r#"
            # Age is not under test here, and a brand-new release of a transitive dependency must
            # not be able to turn the fixture red.
            min-age = "0d"

            [advisories]
            enabled = true
        "#},
    );

    let out = fixture.cooldown_json(&["check"]);
    assert!(out.ok(), "a mature fix pin passes the gate");
    assert_eq!(out.summary_violations(), 0);
    assert!(
        out.summary_security_relevant() >= 1,
        "the locked 0.2.23 is {GHSA}'s fix version and must be tallied"
    );
}
