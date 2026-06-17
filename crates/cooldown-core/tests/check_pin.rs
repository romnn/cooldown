//! The `check` truth table — a core security contract — pinned by example: the cross product of
//! {fresh? · `allow`-matched? · graph-held? · pseudo? · unknown-age? · frozen?} → the pin verdict.

mod common;
use common::*;
use cooldown_core::*;

fn locked(v: &str, pub_at: Option<&str>, quality: ReleaseQuality) -> Release {
    rel(v, &[1, 0, 0], "v1", None, pub_at, quality)
}

#[test]
fn fresh_pin_is_a_violation() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let lk = locked(
        "v1.0.0",
        Some("2026-06-15T00:00:00Z"),
        ReleaseQuality::Stable,
    ); // 2 days
    let layers = layers_from(vec![]);
    let h = ctx();
    let pv = check_pin(&d, &lk, &layers, &h.get(), now());
    assert_eq!(pv.status, Status::CurrentInCooldown);
    assert!(!pv.graph_held);
}

#[test]
fn matured_pin_passes() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let lk = locked(
        "v1.0.0",
        Some("2026-05-01T00:00:00Z"),
        ReleaseQuality::Stable,
    ); // ~47 days
    let layers = layers_from(vec![]);
    let h = ctx();
    let pv = check_pin(&d, &lk, &layers, &h.get(), now());
    assert_eq!(pv.status, Status::UpToDate);
}

#[test]
fn unknown_age_pin_is_unknown_not_pass() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let lk = locked("v1.0.0", None, ReleaseQuality::Stable);
    let layers = layers_from(vec![]);
    let h = ctx();
    let pv = check_pin(&d, &lk, &layers, &h.get(), now());
    assert_eq!(pv.status, Status::UnknownAge);
}

#[test]
fn pseudo_pin_is_exempt() {
    let d = dep("ex", "v0.0.0-20260615000000-abc", ReleaseQuality::Pseudo);
    let lk = locked(
        "v0.0.0-20260615000000-abc",
        Some("2026-06-15T00:00:00Z"),
        ReleaseQuality::Pseudo,
    );
    let layers = layers_from(vec![]);
    let h = ctx();
    let pv = check_pin(&d, &lk, &layers, &h.get(), now());
    assert_eq!(pv.status, Status::Exempt);
}

#[test]
fn allow_matched_pin_is_exempt() {
    let d = dep("github.com/acme/widget", "v1.0.0", ReleaseQuality::Stable);
    let lk = locked(
        "v1.0.0",
        Some("2026-06-16T00:00:00Z"),
        ReleaseQuality::Stable,
    );
    let cfg = layer(
        "allow = [\"github.com/acme/*\"]",
        Origin::Repo(camino::Utf8PathBuf::from("cooldown.toml")),
    );
    let layers = layers_from(vec![cfg]);
    let h = ctx();
    let pv = check_pin(&d, &lk, &layers, &h.get(), now());
    assert_eq!(pv.status, Status::Exempt);
}

/// A graph-pinned fresh pin is STILL a violation, annotated `graph_held` — never a silent pass.
#[test]
fn graph_held_fresh_pin_still_violates_with_annotation() {
    let mut d = dep("k8s.io/api", "v0.36.2", ReleaseQuality::Stable);
    d.graph_floor = Some(Version::new("v0.36.2")); // MVS holds it at exactly this version
    let lk = locked(
        "v0.36.2",
        Some("2026-06-15T00:00:00Z"),
        ReleaseQuality::Stable,
    );
    let layers = layers_from(vec![]);
    let h = ctx();
    let pv = check_pin(&d, &lk, &layers, &h.get(), now());
    assert_eq!(pv.status, Status::CurrentInCooldown);
    assert!(pv.graph_held);
    assert_eq!(pv.graph_floor, Some(Version::new("v0.36.2")));
}

/// `check` always uses the bare `min-age`, never a per-kind window (a pin has no from→to kind).
#[test]
fn check_uses_bare_min_age_ignoring_per_kind() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    // 10 days old: violates the bare 14d but would pass a 3d patch window. The pin must use 14d.
    let lk = locked(
        "v1.0.0",
        Some("2026-06-07T00:00:00Z"),
        ReleaseQuality::Stable,
    );
    let cfg = layer(
        "min-age = { default = \"14d\", patch = \"3d\" }",
        Origin::Repo(camino::Utf8PathBuf::from("cooldown.toml")),
    );
    let layers = layers_from(vec![cfg]);
    let h = ctx();
    let pv = check_pin(&d, &lk, &layers, &h.get(), now());
    assert_eq!(pv.status, Status::CurrentInCooldown);
}

/// A freeze cutoff gates the pin reproducibly.
#[test]
fn freeze_cutoff_gates_pin() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let cfg = layer(
        "freeze = \"2026-06-10\"",
        Origin::Repo(camino::Utf8PathBuf::from("cooldown.toml")),
    );
    let layers = layers_from(vec![cfg]);
    let h = ctx();

    // Published after the freeze date → violation.
    let after = locked(
        "v1.0.0",
        Some("2026-06-12T00:00:00Z"),
        ReleaseQuality::Stable,
    );
    assert_eq!(
        check_pin(&d, &after, &layers, &h.get(), now()).status,
        Status::CurrentInCooldown
    );

    // Published before the freeze date → pass.
    let before = locked(
        "v1.0.0",
        Some("2026-06-08T00:00:00Z"),
        ReleaseQuality::Stable,
    );
    assert_eq!(
        check_pin(&d, &before, &layers, &h.get(), now()).status,
        Status::UpToDate
    );
}
