//! Truth-table tests for `evaluate()` — the candidate-set decision, pinned by example so the
//! semantics cannot silently drift.

mod common;
use common::*;
use cooldown_core::*;

/// A fresh stable upgrade (2 days old, 7d window) is `InCooldown`, not adoptable.
#[test]
fn fresh_stable_is_in_cooldown() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.1.0",
            &[1, 1, 0],
            "v1",
            Some(UpdateKind::Minor),
            Some("2026-06-15T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::InCooldown);
    assert_eq!(verdict.adoptable_target, None);
    assert_eq!(verdict.latest, Some(Version::new("v1.1.0")));
}

/// A matured stable upgrade (16 days old) is `Adoptable`.
#[test]
fn matured_stable_is_adoptable() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.1.0",
            &[1, 1, 0],
            "v1",
            Some(UpdateKind::Minor),
            Some("2026-06-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Adoptable);
    assert_eq!(verdict.adoptable_target, Some(Version::new("v1.1.0")));
}

/// An unknown publish time is never treated as mature → `UnknownAge`.
#[test]
fn unknown_age_is_never_mature() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.1.0",
            &[1, 1, 0],
            "v1",
            Some(UpdateKind::Minor),
            None,
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::UnknownAge);
    assert_eq!(verdict.adoptable_target, None);
}

/// A yanked newer version is never an adoptable target and is excluded from `latest`.
#[test]
fn yanked_never_adoptable() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        yanked(rel(
            "v1.1.0",
            &[1, 1, 0],
            "v1",
            Some(UpdateKind::Minor),
            Some("2026-01-10T00:00:00Z"),
            ReleaseQuality::Stable,
        )),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::UpToDate);
    assert_eq!(verdict.adoptable_target, None);
    assert_eq!(verdict.latest, Some(Version::new("v1.0.0")));
}

/// Prereleases are excluded unless the current pin is itself a prerelease.
#[test]
fn prereleases_excluded_unless_current_is_prerelease() {
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.1.0-rc1",
            &[1, 1, 0, 0],
            "v1",
            Some(UpdateKind::Minor),
            Some("2026-01-10T00:00:00Z"),
            ReleaseQuality::Prerelease,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();

    // Stable current → prerelease excluded → up to date.
    let stable = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let v = evaluate(&stable, &releases, &layers, &h.get(), now());
    assert_eq!(v.status, Status::UpToDate);

    // Prerelease current → prerelease candidate eligible.
    let pre = dep("ex", "v1.0.0", ReleaseQuality::Prerelease);
    // give the current pin its own entry so order resolves
    let mut releases2 = releases.clone();
    releases2.insert(
        0,
        rel(
            "v1.0.0-pre",
            &[0, 9],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Prerelease,
        ),
    );
    let pre = Dependency {
        current: Version::new("v1.0.0-pre"),
        ..pre
    };
    let v = evaluate(&pre, &releases2, &layers, &h.get(), now());
    assert_eq!(v.status, Status::Adoptable);
    assert_eq!(v.adoptable_target, Some(Version::new("v1.1.0-rc1")));
}

/// A commit-pinned (pseudo) current pin is `Held` — no tagged comparison.
#[test]
fn pseudo_current_is_held() {
    let d = dep(
        "ex",
        "v0.0.0-20260101000000-abcdef123456",
        ReleaseQuality::Pseudo,
    );
    let releases = vec![rel(
        "v1.0.0",
        &[1, 0, 0],
        "v1",
        Some(UpdateKind::Major),
        Some("2026-06-01T00:00:00Z"),
        ReleaseQuality::Stable,
    )];
    let layers = layers_from(vec![]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Held);
    assert_eq!(verdict.adoptable_target, None);
    assert_eq!(verdict.latest, Some(Version::new("v1.0.0")));
}

/// A version older than the current pin is not a candidate (downgrades are not gated).
#[test]
fn downgrade_is_not_a_candidate() {
    let d = dep("ex", "v1.2.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.1.0",
            &[1, 1, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.2.0",
            &[1, 2, 0],
            "v1",
            None,
            Some("2026-01-05T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::UpToDate);
}

/// A major jump is filtered out unless `--major`; same-major upgrades stay eligible.
#[test]
fn major_filtered_unless_allowed() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v2.0.0",
            &[2, 0, 0],
            "v2",
            Some(UpdateKind::Major),
            Some("2026-06-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);

    let no_major = ctx();
    let v = evaluate(&d, &releases, &layers, &no_major.get(), now());
    assert_eq!(v.status, Status::UpToDate, "major jump excluded by default");

    let with_major = ctx().major();
    let v = evaluate(&d, &releases, &layers, &with_major.get(), now());
    assert_eq!(v.status, Status::Adoptable);
    assert_eq!(v.adoptable_target, Some(Version::new("v2.0.0")));
}

/// Per candidate: a patch is adoptable at the patch window while a major still cools at 30d.
#[test]
fn per_kind_windows_decide_per_candidate() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.0.1",
            &[1, 0, 1],
            "v1",
            Some(UpdateKind::Patch),
            Some("2026-06-05T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v2.0.0",
            &[2, 0, 0],
            "v2",
            Some(UpdateKind::Major),
            Some("2026-06-05T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
    ];
    let cfg = layer(
        "min-age = { default = \"7d\", patch = \"3d\", major = \"30d\" }",
        Origin::Repo(camino::Utf8PathBuf::from("cooldown.toml")),
    );
    let layers = layers_from(vec![cfg]);
    let h = ctx().major();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());

    // Headline is the newest candidate (v2.0.0), still cooling at 30d.
    assert_eq!(verdict.status, Status::InCooldown);
    // But the patch matured at 3d → adoptable now.
    assert_eq!(verdict.adoptable_target, Some(Version::new("v1.0.1")));

    let patch = verdict
        .candidates
        .iter()
        .find(|c| c.version == Version::new("v1.0.1"))
        .unwrap();
    assert_eq!(patch.status, Status::Adoptable);
    let major = verdict
        .candidates
        .iter()
        .find(|c| c.version == Version::new("v2.0.0"))
        .unwrap();
    assert_eq!(major.status, Status::InCooldown);
}

/// An `allow` exemption makes a fresh candidate `Exempt` (adoptable regardless of age).
#[test]
fn allow_exempts_candidate() {
    let d = dep("github.com/acme/widget", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.1.0",
            &[1, 1, 0],
            "v1",
            Some(UpdateKind::Minor),
            Some("2026-06-16T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
    ];
    let cfg = layer(
        "allow = [\"github.com/acme/*\"]",
        Origin::Repo(camino::Utf8PathBuf::from("cooldown.toml")),
    );
    let layers = layers_from(vec![cfg]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Exempt);
    assert_eq!(verdict.adoptable_target, Some(Version::new("v1.1.0")));
}

/// `+incompatible` is a stable, adoptable release (not a prerelease).
#[test]
fn incompatible_is_adoptable() {
    let d = dep("ex", "v1.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "v1.0.0",
            &[1, 0, 0],
            "v1",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        rel(
            "v3.0.0+incompatible",
            &[3, 0, 0],
            "v3",
            Some(UpdateKind::Major),
            Some("2026-06-01T00:00:00Z"),
            ReleaseQuality::Incompatible,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx().major();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Adoptable);
    assert_eq!(
        verdict.adoptable_target,
        Some(Version::new("v3.0.0+incompatible"))
    );
}
