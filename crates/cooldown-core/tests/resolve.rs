//! The precedence-matrix suite: layer × selector × field, including the floor max-clamp and the
//! allow-vs-floor security rule. These pin authority-first resolution by example.

mod common;
use common::*;
use cooldown_core::*;

use camino::{Utf8Path, Utf8PathBuf};
use jiff::SignedDuration;

fn days(n: i64) -> SignedDuration {
    SignedDuration::from_hours(24 * n)
}

fn repo(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Repo(Utf8PathBuf::from("cooldown.toml")))
}
fn global(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Global)
}
fn native(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Native)
}
fn cli(toml: &str) -> PolicyLayer {
    layer(toml, Origin::Cli)
}

fn q<'a>(pkg: &'a str, project: &'a Utf8Path, kind: ResolveKind) -> ResolveQuery<'a> {
    ResolveQuery {
        tool: GO,
        package: pkg,
        registry: None,
        project,
        kind,
    }
}

fn win(layers: &[PolicyLayer], pkg: &str, kind: ResolveKind) -> ResolvedWindow {
    let proj = Utf8PathBuf::from(".");
    resolve(layers, &q(pkg, &proj, kind), now()).window
}

fn effective_default(layers: &[PolicyLayer]) -> ResolvedWindow {
    let proj = Utf8PathBuf::from(".");
    resolve(
        layers,
        &ResolveQuery {
            tool: GO,
            package: "",
            registry: None,
            project: Utf8Path::new(proj.as_str()),
            kind: ResolveKind::EffectiveDefault,
        },
        now(),
    )
    .window
}

/// The plan's worked example: a global, *specific* `[package."left-pad"] = 30d` LOSES to a repo,
/// *general* top-level `min-age = 14d`. Layer dominates selector.
#[test]
fn authority_first_layer_dominates_selector() {
    let layers = vec![
        config::builtin_default_layer(),
        global("[package.\"left-pad\"]\nmin-age = \"30d\""),
        repo("min-age = \"14d\""),
    ];
    let w = win(&layers, "left-pad", ResolveKind::CurrentPin);
    assert_eq!(w.spec, WindowSpec::MinAge(days(14)));
    assert_eq!(
        w.decided_by,
        Origin::Repo(Utf8PathBuf::from("cooldown.toml"))
    );
}

/// Within one layer, the most specific selector wins.
#[test]
fn within_layer_specificity_breaks_tie() {
    let layers = vec![
        config::builtin_default_layer(),
        repo("min-age = \"14d\"\n[package.\"left-pad\"]\nmin-age = \"30d\""),
    ];
    assert_eq!(
        win(&layers, "left-pad", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(30))
    );
    assert_eq!(
        win(&layers, "other", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(14))
    );
}

#[test]
fn tool_scoped_package_rules_resolve_independently() {
    let layer = repo(
        r#"
        [tool.go.package."shared"]
        max-major = 2

        [tool.cargo.package."shared"]
        max-major = 5
        "#,
    );
    let project = Utf8Path::new(".");
    let go = ResolveQuery {
        tool: GO,
        package: "shared",
        registry: None,
        project,
        kind: ResolveKind::CurrentPin,
    };
    let cargo = ResolveQuery {
        tool: ToolId("cargo"),
        ..go
    };

    let go_pick = resolve_max_major(std::slice::from_ref(&layer), &go).expect("go ceiling");
    let cargo_pick = resolve_max_major(&[layer], &cargo).expect("cargo ceiling");
    assert_eq!(go_pick.limit, 2);
    assert_eq!(cargo_pick.limit, 5);
    assert_eq!(go_pick.selector.specificity(), 5);
    assert_eq!(
        go_pick.selector.token().as_deref(),
        Some("package=go:shared")
    );
}

#[test]
fn max_major_is_authority_first_then_specificity() {
    let layers = vec![
        global(
            r#"
            [tool.go.package."widget"]
            max-major = 3
            "#,
        ),
        repo(
            r#"
            [package."*"]
            max-major = 6

            [tool.go.package."widget"]
            max-major = 5
            "#,
        ),
    ];
    let query = q("widget", Utf8Path::new("."), ResolveKind::CurrentPin);
    let pick = resolve_max_major(&layers, &query).expect("ceiling");
    assert_eq!(pick.limit, 5, "repo authority wins, then tool specificity");
    std::assert_matches!(pick.origin, Origin::Repo(_));

    let resolution = resolve(&layers, &query, now());
    let applied = resolution
        .trace
        .iter()
        .find(|step| step.field == "max-major" && step.applied)
        .expect("max-major trace winner");
    assert!(applied.note.contains('5'));
}

#[test]
fn max_major_trace_marks_exactly_one_duplicate_selector_as_applied() {
    let mut layer = PolicyLayer::new(Origin::Repo(Utf8PathBuf::from("cooldown.toml")));
    for limit in [2, 3] {
        let mut rule = Rule::new(Selector::Package {
            glob: PatternGlob::new("widget").expect("glob"),
            tool: Some(GO),
        });
        rule.max_major = Some(limit);
        layer.rules.push(rule);
    }
    let query = q("widget", Utf8Path::new("."), ResolveKind::CurrentPin);

    let resolution = resolve(std::slice::from_ref(&layer), &query, now());
    let applied: Vec<_> = resolution
        .trace
        .iter()
        .filter(|step| step.field == "max-major" && step.applied)
        .collect();

    assert_eq!(
        resolve_max_major(&[layer], &query).map(|pick| pick.limit),
        Some(2)
    );
    assert_eq!(applied.len(), 1);
    assert!(applied[0].note.contains('2'));
}

#[test]
fn max_major_and_nested_packages_are_rejected_outside_package_rules() {
    for config in [
        "max-major = 5",
        "[tool.go]\nmax-major = 5",
        "[registry.crates.package.foo]\nmax-major = 5",
        "[project.\".\".package.foo]\nmax-major = 5",
    ] {
        let error = config::parse_config(config, Origin::Global).expect_err("invalid config");
        let message = error.to_string();
        assert!(
            message.contains("max-major")
                || message.contains("nested `package` tables are only supported under [tool.*]"),
            "unexpected error: {message}"
        );
    }
}

#[test]
fn non_tool_selectors_reject_misplaced_exclude_lists_with_a_location_hint() {
    for table in ["package.foo", "registry.example", "project.\".\""] {
        for key in ["exclude-folders", "exclude-packages"] {
            let config = format!("[{table}]\n{key} = [\"ignored\"]");
            let error = config::ConfigDocument::parse(&config, &Origin::Global)
                .expect_err("invalid config");
            let message = error.to_string();
            assert!(message.contains(key), "unexpected error: {message}");
            assert!(
                message.contains("exclusion lists live under [tool.*]"),
                "unexpected error: {message}"
            );
        }
    }
}

/// `repo > native > global > default` for `min-age`.
#[test]
fn layer_authority_order() {
    let base = vec![config::builtin_default_layer(), global("min-age = \"10d\"")];
    assert_eq!(
        win(&base, "x", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(10))
    );

    let mut with_native = base.clone();
    with_native.push(native("min-age = \"20d\""));
    assert_eq!(
        win(&with_native, "x", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(20))
    );

    let mut with_repo = with_native.clone();
    with_repo.push(repo("min-age = \"14d\""));
    assert_eq!(
        win(&with_repo, "x", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(14))
    );
}

/// A floor max-clamps the window up; a repo `min-age = 0d` is clamped by a global `floor`.
#[test]
fn floor_max_clamps_window() {
    let layers = vec![
        config::builtin_default_layer(),
        global("floor = \"7d\""),
        repo("[package.\"some-tool\"]\nmin-age = \"0d\""),
    ];
    let w = win(&layers, "some-tool", ResolveKind::CurrentPin);
    assert_eq!(w.spec, WindowSpec::MinAge(days(0)), "selected window is 0d");
    assert!(
        (w.effective_min_age_days(now()) - 7.0).abs() < 1e-9,
        "but clamped up to 7d"
    );
    assert_eq!(
        w.clamped_by(now()).map(cooldown_core::Origin::token),
        Some("global".to_string())
    );
}

/// An `allow` from any layer exempts against an ordinary window (no floor present).
#[test]
fn allow_union_exempts_against_window() {
    let layers = vec![
        config::builtin_default_layer(),
        global("allow = [\"left-pad\"]"),
        repo("min-age = \"14d\""),
    ];
    let w = win(&layers, "left-pad", ResolveKind::CurrentPin);
    assert!(w.exempt);
    assert!(
        w.effective_min_age_days(now()).abs() < 1e-9,
        "an exempt window resolves to a 0-day cooldown"
    );
}

/// The security rule: a repo `allow` cannot undercut an org (global) floor; a co-declared global
/// `allow` can; an explicit CLI `allow` always can.
#[test]
fn allow_vs_floor_security_rule() {
    let layers = vec![
        config::builtin_default_layer(),
        global("floor = \"7d\"\nallow = [\"github.com/acme/*\"]"),
        repo("allow = [\"some-tool\"]"),
    ];

    // Repo allow for some-tool is a different layer than the global floor → cannot bypass.
    let st = win(&layers, "some-tool", ResolveKind::CurrentPin);
    assert!(!st.exempt, "repo allow must not bypass the org floor");
    assert!((st.effective_min_age_days(now()) - 7.0).abs() < 1e-9);

    // Global allow for @acme/* is co-declared with the floor → bypasses.
    let acme = win(&layers, "github.com/acme/widget", ResolveKind::CurrentPin);
    assert!(
        acme.exempt,
        "co-declared global allow bypasses its own floor"
    );

    // CLI allow always bypasses (audited human override).
    let with_cli = vec![
        config::builtin_default_layer(),
        global("floor = \"7d\""),
        cli("allow = [\"some-tool\"]"),
    ];
    let st2 = win(&with_cli, "some-tool", ResolveKind::CurrentPin);
    assert!(st2.exempt, "CLI allow must always bypass a floor");
}

/// Per-kind fallthrough: a per-kind window wins for its kind; absent one, the bare `min-age`
/// applies. `CurrentPin` always uses the bare `min-age`.
#[test]
fn per_kind_fallthrough() {
    let layers = vec![
        config::builtin_default_layer(),
        global("[package.\"ex\"]\nmin-age = { major = \"30d\" }"),
        repo("min-age = \"14d\""),
    ];
    // Major: only the global per-kind sets `major` → 30d (per-kind wins for its kind).
    assert_eq!(
        win(&layers, "ex", ResolveKind::Candidate(UpdateKind::Major)).spec,
        WindowSpec::MinAge(days(30))
    );
    // Minor: nobody set `minor` → fall through to the bare `min-age` → repo's 14d.
    assert_eq!(
        win(&layers, "ex", ResolveKind::Candidate(UpdateKind::Minor)).spec,
        WindowSpec::MinAge(days(14))
    );
    // The pin always uses the bare `min-age` → 14d.
    assert_eq!(
        win(&layers, "ex", ResolveKind::CurrentPin).spec,
        WindowSpec::MinAge(days(14))
    );
}

/// The `minAgeSource` string is `<origin>:<selector>`.
#[test]
fn min_age_source_string() {
    let layers = vec![
        config::builtin_default_layer(),
        repo("[package.\"left-pad\"]\nmin-age = \"30d\""),
    ];
    let w = win(&layers, "left-pad", ResolveKind::CurrentPin);
    assert_eq!(w.source(), "repo:cooldown.toml:package=left-pad");
}

/// The effective project default excludes package selectors, even broad ones.
#[test]
fn effective_default_ignores_package_selectors() {
    let layers = vec![
        config::builtin_default_layer(),
        repo("min-age = \"14d\"\n[package.\"*\"]\nmin-age = \"30d\""),
    ];
    let window = effective_default(&layers);
    assert_eq!(window.spec, WindowSpec::MinAge(days(14)));
    assert_eq!(window.source(), "repo:cooldown.toml");
}

/// A `tool` selector applies only to its tool.
#[test]
fn tool_selector_scopes_by_tool() {
    let layers = vec![
        config::builtin_default_layer(),
        repo("[tool.go]\nmin-age = \"21d\"\n[tool.uv]\nmin-age = \"30d\""),
    ];
    // GO query picks the go tool rule (21d), not uv's.
    let w = win(&layers, "x", ResolveKind::CurrentPin);
    assert_eq!(w.spec, WindowSpec::MinAge(days(21)));
}

/// Regression (critical): a repo `allow` co-declared with a repo `floor` must NOT escape a
/// separate, lower-layer org/global floor. The global floor remains as a residual clamp.
#[test]
fn codeclared_allow_cannot_escape_a_separate_global_floor() {
    let layers = vec![
        config::builtin_default_layer(),
        global("floor = \"10d\""),
        repo("floor = \"30d\"\nallow = [\"evil-pkg\"]"),
    ];
    let w = win(&layers, "evil-pkg", ResolveKind::CurrentPin);
    // The repo allow bypasses the repo 30d floor (same layer) but NOT the global 10d floor.
    assert!(
        !w.exempt,
        "must not be fully exempt while an org floor remains"
    );
    assert!(
        (w.effective_min_age_days(now()) - 10.0).abs() < 1e-9,
        "residual global floor of 10d must still clamp, got {}",
        w.effective_min_age_days(now())
    );
    assert_eq!(
        w.clamped_by(now()).map(cooldown_core::Origin::token),
        Some("global".to_string())
    );
}
