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

/// The trace steps that applied for `field`.
fn applied<'a>(resolution: &'a Resolution, field: &str) -> Vec<&'a TraceStep> {
    resolution
        .trace
        .iter()
        .filter(|step| step.field == field && step.applied)
        .collect()
}

/// The issue's example: the trace marks the literal as the one window winner and the one ceiling
/// winner, matching the resolved provenance, even with the scope glob declared first.
#[test]
fn explain_marks_the_more_specific_pattern_as_the_only_winner() {
    use indoc::indoc;
    let repo = layer(
        indoc! {r#"
            [package."@corp/*"]
            latest = true
            max-major = 3

            [package."@corp/ui"]
            min-age = "30d"
            max-major = 5
        "#},
        Origin::Repo(Utf8PathBuf::from("cooldown.toml")),
    );
    let mut reversed = repo.clone();
    reversed.rules.reverse();
    for repo in [repo, reversed] {
        let layers = layers_from(vec![repo]);
        let resolution = trace_for(&layers, "@corp/ui", ResolveKind::CurrentPin);

        let window = applied(&resolution, "default");
        assert_eq!(window.len(), 1, "{:#?}", resolution.trace);
        let selector = window.first().and_then(|step| step.selector.as_ref());
        assert_eq!(selector, Some(&resolution.window.decided_selector));
        assert_eq!(
            resolution.window.source(),
            "repo:cooldown.toml:package=@corp/ui"
        );

        let ceiling = applied(&resolution, "max-major");
        assert_eq!(ceiling.len(), 1, "{:#?}", resolution.trace);
        let selector = ceiling
            .first()
            .and_then(|step| step.selector.as_ref())
            .and_then(Selector::token);
        assert_eq!(selector.as_deref(), Some("package=@corp/ui"));

        // The glob is still traced, as considered.
        let considered = resolution
            .trace
            .iter()
            .filter(|step| {
                step.selector.as_ref().and_then(Selector::token).as_deref()
                    == Some("package=@corp/*")
            })
            .count();
        assert_eq!(considered, 2);
    }
}

/// Identical declarations, which only a programmatic layer can hold, tie in rank: the first one
/// wins and is the only one the trace marks, although the others carry the same value.
#[test]
fn explain_marks_one_of_several_identical_declarations() {
    let mut repo = PolicyLayer::new(Origin::Repo(Utf8PathBuf::from("cooldown.toml")));
    for _ in 0..2 {
        let mut rule = Rule::new(Selector::Package {
            glob: PatternGlob::new("@corp/ui").expect("glob"),
            tool: None,
        });
        rule.window = ByKind::scalar(WindowSpec::MinAge(jiff::SignedDuration::from_hours(
            24 * 30,
        )));
        rule.max_major = Some(5);
        repo.rules.push(rule);
    }
    let layers = layers_from(vec![repo]);
    let resolution = trace_for(&layers, "@corp/ui", ResolveKind::CurrentPin);

    assert_eq!(applied(&resolution, "default").len(), 1);
    assert_eq!(applied(&resolution, "max-major").len(), 1);
    // Both declarations are still listed.
    let listed = resolution
        .trace
        .iter()
        .filter(|step| step.field == "default" && step.layer != Origin::Default)
        .count();
    assert_eq!(listed, 2);
}
