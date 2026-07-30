//! End-to-end test for the npm dist-tag adoption ceiling against the LIVE registry, driven through
//! the real `pnpm` resolver.
//!
//! # Why adaptive assertions
//!
//! Unlike publish times (append-mostly and replayable with `--freeze`; unpublishing can still
//! remove a version), the `latest` dist-tag is *mutable* — the maintainer can retag at any time —
//! so this test asserts the ceiling's **invariants** against the registry's live tag rather than
//! hard-coding versions:
//!
//! - with the default `respect-dist-tags`, a pin at or below the `latest` tag is never offered a
//!   target above it — cooldown never proposes what a bare `npm install <pkg>` wouldn't resolve to
//!   (a pin already above the tag deactivates the ceiling, and the test skips that regime);
//! - with `--no-respect-dist-tags`, the ceiling lifts: the run's target equals its own **Latest**
//!   column — the newest stable release that same response saw. Comparing fields of one response
//!   keeps the assertion immune to a publication landing between separate registry reads.
//!
//! The fixture package is `fumadocs-core`, the case that motivated the ceiling: its stable `17.0.0`
//! was published months *before* the `16.x` line continued, so `latest` deliberately points below
//! the semver-max. When the maintainer eventually retags (tag == semver-max), the two invariants
//! coincide and the test still passes — it never depends on the anomaly being live.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "integration-test code; a failing assertion or missing fixture SHOULD panic (clippy.toml allows unwrap/expect/panic in tests)"
)]

mod support;

use indoc::indoc;
use support::Fixture;

/// The fixture pins the motivating package on its caret range, exactly as the affected repo does.
const PACKAGE_JSON: &str = indoc! {r#"
    {
      "name": "cooldown-pnpm-dist-tag-fixture",
      "version": "0.1.0",
      "private": true,
      "dependencies": {
        "fumadocs-core": "^16.0.0"
      }
    }
"#};

/// A plain `x.y.z` triple for ordering assertions. The fixture package versions all parse; a
/// non-conforming version (prerelease build metadata) would simply be excluded from the stable set
/// before this is called.
fn triple(version: &str) -> Option<(u64, u64, u64)> {
    let mut parts = version.split('.').map(str::parse::<u64>);
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch)), None) => Some((major, minor, patch)),
        _ => None,
    }
}

/// The one external fact the ceiling inequality needs: the live `latest` dist-tag, read through
/// the same `pnpm` the resolver uses.
///
/// Returns [`None`] when the live tag is not a plain stable `x.y.z` (a prerelease or build-tagged
/// `latest` is a legitimate registry regime this probe's ordering assertions cannot express) — the
/// caller then skips rather than failing on external state; the deterministic conformance and core
/// tests remain the regression proof.
fn live_latest_tag(fixture: &Fixture) -> Option<String> {
    let tag = fixture
        .run_tool("pnpm", &["view", "fumadocs-core", "dist-tags.latest"], &[])
        .expect_success()
        .stdout_str()
        .trim()
        .to_string();
    if triple(&tag).is_none() {
        eprintln!("note: live latest tag {tag} is not a stable triple; skipping the live probe");
        return None;
    }
    Some(tag)
}

#[test]
fn outdated_caps_the_adoptable_target_at_the_live_latest_dist_tag() {
    skip_if_missing!("pnpm");
    let fixture = Fixture::new();
    fixture.write("package.json", PACKAGE_JSON);
    fixture
        .run_tool("pnpm", &["install", "--lockfile-only"], &[])
        .expect_success();

    let Some(tag) = live_latest_tag(&fixture) else {
        return;
    };

    // `--latest` (window 0) removes cooldown maturity from the picture, isolating the dist-tag
    // ceiling; `--all` keeps the row visible even when it is already up to date.
    let outdated = fixture.cooldown_json(&["outdated", "--major", "--latest", "--all"]);
    assert!(outdated.ok(), "outdated should succeed");
    let current = outdated
        .item_field_str("fumadocs-core", "current")
        .expect("the fumadocs-core row is present");

    if triple(&current) > triple(&tag) {
        // The seeded pin already sits above the live tag (the registry state moved under us): a
        // pin beyond the tag deactivates the ceiling, so there is nothing to assert.
        eprintln!("note: current {current} is above the live latest tag {tag}; ceiling inactive");
        return;
    }

    // The capped run never proposes above the tag, while the newest existing version stays visible
    // as context (an internal fact of the same response, so no separate registry read to race).
    let capped_target = outdated.item_field_str("fumadocs-core", "adoptableTarget");
    if let Some(target) = &capped_target
        && triple(target) > triple(&tag)
    {
        // The tag may have moved between the `pnpm view` read and cooldown's own fetch (it is
        // mutable and the two reads are not atomic, possibly against different CDN nodes).
        // Re-read once: a moved tag means the external state changed mid-probe, so skip with a
        // diagnostic; only a breach confirmed by two consistent reads is a real ceiling failure.
        let Some(fresh_tag) = live_latest_tag(&fixture) else {
            return;
        };
        if fresh_tag != tag {
            eprintln!(
                "note: the latest tag moved {tag} -> {fresh_tag} mid-probe; skipping the capped assertion"
            );
            return;
        }
        panic!("adoptable target {target} must not exceed the live latest tag {tag}");
    }
    let capped_latest = outdated
        .latest_version_for("fumadocs-core")
        .expect("the Latest column keeps surfacing the newest existing version");
    if let Some(target) = &capped_target {
        assert!(
            triple(&capped_latest) >= triple(target),
            "Latest ({capped_latest}) is context at or above the capped target ({target})"
        );
    }

    // The escape hatch lifts the ceiling: the target is that response's own newest stable — its
    // Latest column. Same-response comparison, immune to a publication between reads.
    let uncapped = fixture.cooldown_json(&[
        "outdated",
        "--major",
        "--latest",
        "--all",
        "--no-respect-dist-tags",
    ]);
    assert!(
        uncapped.ok(),
        "outdated --no-respect-dist-tags should succeed"
    );
    let target = uncapped
        .item_field_str("fumadocs-core", "adoptableTarget")
        .unwrap_or(current);
    let uncapped_latest = uncapped
        .latest_version_for("fumadocs-core")
        .expect("the uncapped Latest column is present");
    assert_eq!(
        target, uncapped_latest,
        "without the ceiling the run's own newest stable release is the target"
    );
    if triple(&uncapped_latest) > triple(&tag) {
        eprintln!(
            "note: live anomaly present (latest tag {tag} < semver-max {uncapped_latest}); the ceiling is doing real work"
        );
    }
}
