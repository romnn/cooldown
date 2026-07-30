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

/// An exact-pinned dependency is `Held` even when a matured upgrade exists — it won't move without a
/// manifest edit — but `adoptable_target` still reports the newest matured version, so the report
/// shows which version could be manually pinned to without failing the cooldown gate.
#[test]
fn exact_pin_is_held() {
    let d = Dependency {
        pinned: true,
        ..dep("ex", "v1.0.0", ReleaseQuality::Stable)
    };
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
            Some("2026-06-01T00:00:00Z"), // matured (16 days)
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();

    let held = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(held.status, Status::Held);
    // The matured upgrade is surfaced as the manual-pin target, even though the dep stays Held.
    assert_eq!(held.adoptable_target, Some(Version::new("v1.1.0")));
    assert_eq!(held.latest, Some(Version::new("v1.1.0")));
}

/// An exact pin with no newer candidate is still up to date; `Held` is reserved for pins where a
/// newer candidate exists but cannot be applied automatically.
#[test]
fn exact_pin_without_newer_candidate_is_up_to_date() {
    let d = Dependency {
        pinned: true,
        ..dep("ex", "v1.0.0", ReleaseQuality::Stable)
    };
    let releases = vec![rel(
        "v1.0.0",
        &[1, 0, 0],
        "v1",
        None,
        Some("2026-01-01T00:00:00Z"),
        ReleaseQuality::Stable,
    )];
    let layers = layers_from(vec![]);
    let h = ctx();

    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::UpToDate);
    assert_eq!(verdict.adoptable_target, None);
    assert_eq!(verdict.latest, Some(Version::new("v1.0.0")));
}

/// A pinned dep whose only newer version is still in cooldown stays `Held` with no manual-pin target
/// yet — there is nothing matured to safely pin to.
#[test]
fn pinned_with_only_fresh_upgrade_has_no_target() {
    let d = Dependency {
        pinned: true,
        ..dep("ex", "v1.0.0", ReleaseQuality::Stable)
    };
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
            Some("2026-06-15T00:00:00Z"), // fresh (2 days), still in cooldown
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();

    let held = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(held.status, Status::Held);
    assert_eq!(held.adoptable_target, None);
    assert_eq!(held.latest, Some(Version::new("v1.1.0")));
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

/// When the newest version is still cooling but an older one has matured, the row is `Adoptable` (you
/// can update to the matured one) — not `InCooldown`, which would wrongly read as "cannot update yet".
/// `latest` still reports the newest (cooling) version.
#[test]
fn matured_older_with_fresh_newest_is_adoptable() {
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
            Some("2026-06-01T00:00:00Z"), // matured (16 days)
            ReleaseQuality::Stable,
        ),
        rel(
            "v1.2.0",
            &[1, 2, 0],
            "v1",
            Some(UpdateKind::Minor),
            Some("2026-06-15T00:00:00Z"), // fresh (2 days), still in cooldown
            ReleaseQuality::Stable,
        ),
    ];
    let layers = layers_from(vec![]);
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Adoptable);
    assert_eq!(verdict.adoptable_target, Some(Version::new("v1.1.0")));
    assert_eq!(verdict.latest, Some(Version::new("v1.2.0")));
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

    // The row is `Adoptable` because the patch has matured (at 3d), even though the newest candidate
    // (v2.0.0) still cools at 30d — you can update to the patch now.
    assert_eq!(verdict.status, Status::Adoptable);
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

/// The fumadocs-core shape: a matured stable major (17.0.0) exists **above** the registry's
/// `latest` dist-tag (16.13.0) — a premature major the maintainer kept releasing below. It must not
/// become the adoptable target; the still-cooling in-tag minors drive the verdict instead, while
/// `latest` keeps surfacing the newest existing version as context.
#[test]
fn major_above_latest_tag_is_not_adoptable() {
    let d = dep("fumadocs-core", "16.11.4", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "16.11.4",
            &[1],
            "16",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        // The premature major: long matured, but above the current `latest` tag.
        above_tag(rel(
            "17.0.0",
            &[4],
            "17",
            Some(UpdateKind::Major),
            Some("2026-02-01T00:00:00Z"),
            ReleaseQuality::Stable,
        )),
        rel(
            "16.11.5",
            &[2],
            "16",
            Some(UpdateKind::Patch),
            Some("2026-06-14T00:00:00Z"), // 3d old — still cooling
            ReleaseQuality::Stable,
        ),
        rel(
            "16.13.0", // the `latest`-tagged version
            &[3],
            "16",
            Some(UpdateKind::Minor),
            Some("2026-06-16T00:00:00Z"), // 1d old — still cooling
            ReleaseQuality::Stable,
        ),
    ];
    let mut sorted = releases;
    sorted.sort_by(|a, b| a.order.cmp(&b.order));
    let layers = layers_from(vec![]);
    let h = ctx().major();

    let verdict = evaluate(&d, &sorted, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::InCooldown);
    assert_eq!(verdict.adoptable_target, None, "17.0.0 sits above the tag");
    // The newest existing version stays visible as context, exactly like the other ceilings.
    assert_eq!(verdict.latest, Some(Version::new("17.0.0")));
    assert!(
        verdict
            .candidates
            .iter()
            .all(|candidate| candidate.version != Version::new("17.0.0")),
        "the major above the tag must not be a candidate"
    );
}

/// With nothing newer inside the tag, the major above it yields `Held` naming the dist-tag ceiling,
/// mirroring `DeclaredBound`/`MaxMajor` — visible and explained, not silently up to date.
#[test]
fn only_above_tag_newer_is_held_with_dist_tag_reason() {
    let d = dep("fumadocs-core", "16.13.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "16.13.0",
            &[1],
            "16",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        above_tag(rel(
            "17.0.0",
            &[2],
            "17",
            Some(UpdateKind::Major),
            Some("2026-02-01T00:00:00Z"),
            ReleaseQuality::Stable,
        )),
    ];
    let layers = layers_from(vec![]);
    let h = ctx().major();

    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Held);
    assert_eq!(verdict.adoptable_target, None);
    assert_eq!(verdict.latest, Some(Version::new("17.0.0")));
    assert_eq!(
        verdict.held_reason,
        Some(HeldReason::DistTag("16.13.0".to_string())),
        "the hold names the tag version the registry recommends"
    );
}

/// A current pin already above the tag (a project deliberately riding a `next` line) deactivates
/// the ceiling ENTIRELY — not merely raising it to the pin's own line — so the project keeps
/// seeing newer releases instead of a downgrade-or-silence dead end: once the project has
/// knowingly passed the tag, the tag carries no guidance about where to stop.
#[test]
fn current_beyond_the_tag_deactivates_the_ceiling() {
    let d = dep("ex", "17.0.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "16.13.0",
            &[1],
            "16",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        above_tag(rel(
            "17.0.0",
            &[2],
            "17",
            None,
            Some("2026-02-01T00:00:00Z"),
            ReleaseQuality::Stable,
        )),
        above_tag(rel(
            "17.1.0",
            &[3],
            "17",
            Some(UpdateKind::Minor),
            Some("2026-05-01T00:00:00Z"), // matured
            ReleaseQuality::Stable,
        )),
        above_tag(rel(
            "18.0.0",
            &[4],
            "18",
            Some(UpdateKind::Major),
            Some("2026-04-01T00:00:00Z"), // matured
            ReleaseQuality::Stable,
        )),
    ];
    let layers = layers_from(vec![]);

    // Within the line: the newer minor is adoptable, no downgrade pressure toward the tag.
    let h = ctx();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Adoptable);
    assert_eq!(verdict.adoptable_target, Some(Version::new("17.1.0")));

    // Beyond the line under `--major`: the deactivated ceiling does not resurface as a cap at the
    // pin's own major — even 18.0.0, well above the tag's current position, stays adoptable.
    let h = ctx().major();
    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Adoptable);
    assert_eq!(verdict.adoptable_target, Some(Version::new("18.0.0")));
}

/// `respect-dist-tags = false` (`--no-respect-dist-tags`) is the deliberate escape hatch: the
/// major above the tag becomes an ordinary candidate again.
#[test]
fn ignore_dist_tags_escape_hatch_admits_the_major_above_the_tag() {
    let d = dep("fumadocs-core", "16.13.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "16.13.0",
            &[1],
            "16",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        above_tag(rel(
            "17.0.0",
            &[2],
            "17",
            Some(UpdateKind::Major),
            Some("2026-02-01T00:00:00Z"),
            ReleaseQuality::Stable,
        )),
    ];
    let layers = layers_from(vec![]);
    let h = ctx().major().ignore_dist_tags();

    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Adoptable);
    assert_eq!(verdict.adoptable_target, Some(Version::new("17.0.0")));
}

/// `evaluate_ceiling_hold` names the matured target the dist-tag hides, so `upgrade` can report the
/// hold (`DistTagHeld`) exactly like a declared bound or `max-major`.
#[test]
fn ceiling_hold_names_the_dist_tag_hidden_target() {
    let d = dep("fumadocs-core", "16.13.0", ReleaseQuality::Stable);
    let releases = vec![
        rel(
            "16.13.0",
            &[1],
            "16",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        above_tag(rel(
            "17.0.0",
            &[2],
            "17",
            Some(UpdateKind::Major),
            Some("2026-02-01T00:00:00Z"),
            ReleaseQuality::Stable,
        )),
    ];
    let layers = layers_from(vec![]);
    let h = ctx().major();

    let hold = evaluate_ceiling_hold(&d, &releases, &layers, &h.get(), now())
        .expect("the matured major above the tag is a hidden target");
    assert_eq!(hold.reason, CeilingReason::DistTag);
    assert_eq!(hold.target, Version::new("17.0.0"));
    assert_eq!(hold.update_kind, UpdateKind::Major);
}

/// When a declared manifest bound and the dist-tag both hide the same target — so no single
/// ceiling's removal exposes anything on its own — the declared bound, the author's own directly
/// editable constraint, names the hold as the first step of the staged guidance.
#[test]
fn declared_bound_outranks_the_dist_tag_as_held_reason() {
    let d = Dependency {
        declared_bound: Some("<17".to_string()),
        ..dep("fumadocs-core", "16.13.0", ReleaseQuality::Stable)
    };
    let mut beyond = above_tag(rel(
        "17.0.0",
        &[2],
        "17",
        Some(UpdateKind::Major),
        Some("2026-02-01T00:00:00Z"),
        ReleaseQuality::Stable,
    ));
    beyond.beyond_declared_bound = true;
    let releases = vec![
        rel(
            "16.13.0",
            &[1],
            "16",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        beyond,
    ];
    let layers = layers_from(vec![]);
    let h = ctx().major();

    let verdict = evaluate(&d, &releases, &layers, &h.get(), now());
    assert_eq!(verdict.status, Status::Held);
    assert_eq!(
        verdict.held_reason,
        Some(HeldReason::DeclaredBound("<17".to_string()))
    );
    let hold = evaluate_ceiling_hold(&d, &releases, &layers, &h.get(), now())
        .expect("the bound-hidden target is still named");
    assert_eq!(hold.reason, CeilingReason::DeclaredBound);
}

/// Stacked ceilings are probed individually: with the declared bound hiding a matured minor the
/// dist-tag still admits, and the tag hiding the major beyond both, the hold names the bound with
/// the target ITS removal exposes — `--rewrite` then actually reaches it. Naming the bound
/// against the jointly hidden major would promise a version the rewrite alone cannot expose (the
/// tag still holds it); that fallback shape is reported only when no single ceiling is causal
/// (see `declared_bound_outranks_the_dist_tag_as_held_reason`).
#[test]
fn stacked_ceiling_hold_reports_the_singly_exposed_target() {
    let d = Dependency {
        declared_bound: Some("<16.5".to_string()),
        ..dep("fumadocs-core", "16.0.0", ReleaseQuality::Stable)
    };
    let mut hidden_minor = rel(
        "16.13.0",
        &[2],
        "16",
        Some(UpdateKind::Minor),
        Some("2026-01-15T00:00:00Z"),
        ReleaseQuality::Stable,
    );
    hidden_minor.beyond_declared_bound = true;
    let mut hidden_major = above_tag(rel(
        "17.0.0",
        &[3],
        "17",
        Some(UpdateKind::Major),
        Some("2026-02-01T00:00:00Z"),
        ReleaseQuality::Stable,
    ));
    hidden_major.beyond_declared_bound = true;
    let releases = vec![
        rel(
            "16.0.0",
            &[1],
            "16",
            None,
            Some("2026-01-01T00:00:00Z"),
            ReleaseQuality::Stable,
        ),
        hidden_minor,
        hidden_major,
    ];
    let layers = layers_from(vec![]);

    let h = ctx().major();
    let hold = evaluate_ceiling_hold(&d, &releases, &layers, &h.get(), now())
        .expect("the bound alone hides a matured, tag-admitted target");
    assert_eq!(hold.reason, CeilingReason::DeclaredBound);
    assert_eq!(
        hold.target,
        Version::new("16.13.0"),
        "the target is what lifting the NAMED ceiling exposes, not the jointly hidden major"
    );
    assert_eq!(hold.update_kind, UpdateKind::Minor);

    // Step two of the staged guidance: with the bound rewritten away, the next run names the tag
    // and the major it hides.
    let h = ctx().major().rewrite_bounds();
    let hold = evaluate_ceiling_hold(&d, &releases, &layers, &h.get(), now())
        .expect("the tag still hides the major");
    assert_eq!(hold.reason, CeilingReason::DistTag);
    assert_eq!(hold.target, Version::new("17.0.0"));
}
