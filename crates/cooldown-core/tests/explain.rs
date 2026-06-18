//! Golden `explain` traces — the resolution derivation, pinned step-by-step so the semantics are
//! documented by example.

mod common;
use camino::{Utf8Path, Utf8PathBuf};
use common::*;
use cooldown_core::*;

fn trace_for(layers: &[PolicyLayer], pkg: &str, kind: ResolveKind) -> Resolution {
    let proj = Utf8PathBuf::from(".");
    let q = ResolveQuery {
        tool: GO,
        package: pkg,
        registry: None,
        project: Utf8Path::new(proj.as_str()),
        kind,
    };
    resolve(layers, &q, now())
}

/// Scenario 5 derivation: repo `min-age = 0d` for some-tool, clamped by a global floor of 7d.
#[test]
fn explain_floor_clamp_derivation() {
    let layers = vec![
        config::builtin_default_layer(),
        layer("floor = \"7d\"", Origin::Global),
        layer(
            "[package.\"some-tool\"]\nmin-age = \"0d\"",
            Origin::Repo(Utf8PathBuf::from("cooldown.toml")),
        ),
    ];
    let res = trace_for(&layers, "some-tool", ResolveKind::CurrentPin);

    // The window applied is the repo's 0d (highest layer that set the default field).
    let applied_window = res
        .trace
        .iter()
        .find(|s| s.field == "default" && s.applied)
        .expect("an applied window step");
    assert_eq!(
        applied_window.layer,
        Origin::Repo(Utf8PathBuf::from("cooldown.toml"))
    );
    assert_eq!(applied_window.min_age_days, Some(0.0));

    // A binding floor step from the global layer, applied, at 7 days.
    let applied_floor = res
        .trace
        .iter()
        .find(|s| s.field == "floor" && s.applied && s.selector.is_none())
        .expect("an applied binding-floor step");
    assert_eq!(applied_floor.layer, Origin::Global);
    assert_eq!(applied_floor.min_age_days, Some(7.0));

    // The built-in default 7d is present but only "considered", not applied.
    let default_considered = res
        .trace
        .iter()
        .find(|s| s.field == "default" && s.layer == Origin::Default)
        .expect("the built-in default step");
    assert!(!default_considered.applied);
    assert_eq!(default_considered.note, "considered");
}

/// A per-kind major window's trace shows the per-kind selection.
#[test]
fn explain_per_kind_selection() {
    let layers = vec![
        config::builtin_default_layer(),
        layer(
            "[package.\"ex\"]\nmin-age = { major = \"30d\" }",
            Origin::Global,
        ),
        layer(
            "min-age = \"14d\"",
            Origin::Repo(Utf8PathBuf::from("cooldown.toml")),
        ),
    ];
    let res = trace_for(&layers, "ex", ResolveKind::Candidate(UpdateKind::Major));
    let applied = res
        .trace
        .iter()
        .find(|s| s.field == "major" && s.applied)
        .expect("an applied major step");
    assert_eq!(applied.layer, Origin::Global);
    assert_eq!(applied.min_age_days, Some(30.0));
    assert_eq!(
        res.window.spec,
        WindowSpec::MinAge(jiff::SignedDuration::from_hours(24 * 30))
    );
}
