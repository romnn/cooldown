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
fn repo_at(path: &str, toml: &str) -> PolicyLayer {
    layer(toml, Origin::Repo(Utf8PathBuf::from(path)))
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

/// A full `[advisories]` table survives the trip from TOML text through parsing to the resolved
/// policy — the same path a real `cooldown.toml` takes.
#[test]
fn advisories_table_parses_and_resolves_from_toml() {
    use indoc::indoc;
    let layers = layers_from(vec![repo(indoc! {r#"
        [advisories]
        enabled = true
        source = "osv"
        mode = "shorten"
        min-age = "36h"
        severity = "moderate"
        bypass-floor = true
    "#})]);
    let policy = resolve_advisory_policy(&layers);
    assert!(policy.enabled);
    assert_eq!(policy.source, AdvisorySourceKind::Osv);
    assert_eq!(policy.mode, AdvisoryMode::Shorten);
    assert_eq!(policy.min_age, SignedDuration::from_hours(36));
    assert_eq!(
        policy.min_age_origin,
        Origin::Repo(Utf8PathBuf::from("cooldown.toml"))
    );
    assert_eq!(policy.severity, AdvisorySeverity::Moderate);
    // The derivation is auditable per field, not per layer: one step for each key the layer
    // set.
    // `bypass-floor` is absent because it resolves per floor candidate during window
    // resolution, which is the only pass that knows whether it lifted anything.
    let fields: Vec<&str> = policy
        .trace
        .iter()
        .map(|step| step.field.as_str())
        .collect();
    assert_eq!(
        fields,
        vec![
            "advisories.enabled",
            "advisories.source",
            "advisories.mode",
            "advisories.min-age",
            "advisories.severity",
        ]
    );
    assert!(policy.trace.iter().all(|step| step.applied));
}

/// A field whose value lost — an overwritten authority-first `min-age`, a `mode = shorten`
/// declined by an explicit `flag`, a `severity` below the ratchet — is traced as *considered*,
/// not applied, so `explain` never claims a layer's value took effect when it did not.
#[test]
fn advisory_trace_marks_losing_fields_as_not_applied() {
    use indoc::indoc;
    let layers = layers_from(vec![
        global(indoc! {r#"
            [advisories]
            enabled = true
            mode = "flag"
            min-age = "12h"
            severity = "critical"
        "#}),
        repo(indoc! {r#"
            [advisories]
            mode = "shorten"
            min-age = "48h"
            severity = "low"
        "#}),
    ]);
    let policy = resolve_advisory_policy(&layers);
    // The winners: repo's min-age (authority-first), global's flag (monotone), critical
    // (ratchet).
    assert_eq!(policy.min_age, SignedDuration::from_hours(48));
    assert_eq!(policy.mode, AdvisoryMode::Flag);
    assert_eq!(policy.severity, AdvisorySeverity::Critical);

    let step = |field: &str, layer: &str| {
        policy
            .trace
            .iter()
            .find(|step| step.field == field && step.layer.token() == layer)
            .unwrap_or_else(|| panic!("a {field} step for {layer}"))
    };
    assert!(!step("advisories.min-age", "global").applied);
    assert!(
        step("advisories.min-age", "global")
            .note
            .contains("overridden by a higher layer")
    );
    assert!(step("advisories.min-age", "repo:cooldown.toml").applied);
    assert!(!step("advisories.mode", "repo:cooldown.toml").applied);
    assert!(!step("advisories.severity", "repo:cooldown.toml").applied);
    assert!(step("advisories.severity", "global").applied);
}

/// The same two combine rules with the layers the other way round: neither `mode` nor `severity`
/// is authority-first, so a lower layer's value can still be the winner and the *higher* layer's
/// step is the one demoted.
///
/// A step is only decidable once the whole stack is folded, which is why the trace is finalized
/// afterwards rather than as each layer lands.
#[test]
fn advisory_trace_demotes_a_higher_layer_that_lost_its_field() {
    use indoc::indoc;
    let layers = layers_from(vec![
        global(indoc! {r#"
            [advisories]
            enabled = true
            mode = "shorten"
            severity = "critical"
        "#}),
        repo(indoc! {r#"
            [advisories]
            mode = "flag"
            severity = "low"
        "#}),
    ]);
    let policy = resolve_advisory_policy(&layers);
    assert_eq!(policy.mode, AdvisoryMode::Flag);
    assert_eq!(policy.severity, AdvisorySeverity::Critical);

    let step = |field: &str, layer: &str| {
        policy
            .trace
            .iter()
            .find(|step| step.field == field && step.layer.token() == layer)
            .unwrap_or_else(|| panic!("a {field} step for {layer}"))
    };
    // The repo's `flag` won even though the global layer set `shorten` first.
    assert!(step("advisories.mode", "repo:cooldown.toml").applied);
    assert!(!step("advisories.mode", "global").applied);
    assert!(
        step("advisories.mode", "global")
            .note
            .contains("declined; an explicit `flag` stands")
    );
    // The threshold ratchets up, so the *higher* layer's `low` is the demoted one.
    assert!(step("advisories.severity", "global").applied);
    assert!(!step("advisories.severity", "repo:cooldown.toml").applied);
    assert!(
        step("advisories.severity", "repo:cooldown.toml")
            .note
            .contains("ratchets up to critical")
    );
    // Exactly one applied step per field, never two claiming to have decided it.
    for field in ["advisories.mode", "advisories.severity"] {
        let applied = policy
            .trace
            .iter()
            .filter(|step| step.field == field && step.applied)
            .count();
        assert_eq!(applied, 1, "{field} must report a single winner");
    }
}

/// `bypass-floor` is traced where its outcome is decided — per floor *candidate*, during window
/// resolution — so a declaration reports each floor it lifted, and one that lifts nothing says so
/// instead of claiming it applied.
#[test]
fn advisory_bypass_floor_traces_what_it_actually_lifted() {
    use indoc::indoc;
    let project = Utf8PathBuf::from(".");
    let bypass_steps = |layers: &[PolicyLayer]| -> Vec<cooldown_core::TraceStep> {
        resolve(layers, &q("x", &project, ResolveKind::CurrentPin), now())
            .trace
            .into_iter()
            .filter(|step| step.field == "advisories.bypass-floor")
            .collect()
    };

    let with_floor = vec![
        config::builtin_default_layer(),
        repo(indoc! {r#"
            floor = "30d"

            [advisories]
            enabled = true
            mode = "shorten"
            bypass-floor = true
        "#}),
    ];
    let bypass = bypass_steps(&with_floor);
    assert_eq!(bypass.len(), 1);
    assert!(bypass[0].applied, "it lifted this layer's 30d floor");
    assert_eq!(bypass[0].min_age_days, Some(30.0));

    // A layer can declare several matching floors; each is lifted, and each is reported with the
    // selector and duration that identify it — one step could name only one of them.
    let two_floors = vec![
        config::builtin_default_layer(),
        repo(indoc! {r#"
            floor = "1d"

            [package."x"]
            floor = "30d"

            [advisories]
            enabled = true
            mode = "shorten"
            bypass-floor = true
        "#}),
    ];
    let bypass = bypass_steps(&two_floors);
    let mut lifted: Vec<f64> = bypass
        .iter()
        .filter(|step| step.applied)
        .filter_map(|step| step.min_age_days)
        .collect();
    lifted.sort_by(f64::total_cmp);
    assert_eq!(lifted, vec![1.0, 30.0], "every lifted floor is reported");
    assert!(
        bypass.iter().any(|step| step
            .selector
            .as_ref()
            .and_then(cooldown_core::Selector::token)
            .is_some_and(|token| token.contains('x'))),
        "the package floor's step names its selector: {bypass:?}"
    );

    // The same declaration in a layer whose floor never reached the candidate list changed
    // nothing.
    let no_floor = vec![
        config::builtin_default_layer(),
        global("floor = \"10d\""),
        repo(indoc! {r#"
            [advisories]
            enabled = true
            mode = "shorten"
            bypass-floor = true
        "#}),
    ];
    let bypass = bypass_steps(&no_floor);
    assert_eq!(bypass.len(), 1);
    assert!(!bypass[0].applied, "no floor of this layer to lift");
    assert!(bypass[0].note.contains("no residual floor"));
}

/// `bypass-floor` mirrors the `allow` rule exactly: it lifts the *declaring layer's* floor for
/// the security window, and every other layer's floor survives as the advisory floor — so a
/// repo cannot use a CVE as a lever against a separate org/global floor.
#[test]
fn advisory_bypass_lifts_only_the_declaring_layers_floor() {
    use indoc::indoc;
    let layers = vec![
        config::builtin_default_layer(),
        global("floor = \"10d\""),
        repo(indoc! {r#"
            floor = "30d"

            [advisories]
            enabled = true
            mode = "shorten"
            bypass-floor = true
        "#}),
    ];
    let w = win(&layers, "x", ResolveKind::CurrentPin);
    // Ordinary resolution is untouched: the 30d repo floor still binds normally.
    assert_eq!(w.floor, Some(days(30)));
    // For the security window the repo bypass lifts only the repo floor; the global 10d floor
    // remains the advisory floor.
    assert_eq!(w.advisory_floor, Some(days(10)));
    assert_eq!(
        w.advisory_floor_origin.as_ref().map(Origin::token),
        Some("global".to_string())
    );
}

/// Two repo-cascade files are two *layers*, not one authority band: a nested file's
/// `bypass-floor` cannot lift a floor declared in a different cascade file.
#[test]
fn advisory_bypass_does_not_cross_repo_cascade_files() {
    use indoc::indoc;
    let layers = vec![
        config::builtin_default_layer(),
        repo("floor = \"30d\""),
        repo_at(
            "nested/cooldown.toml",
            indoc! {r#"
            [advisories]
            enabled = true
            mode = "shorten"
            bypass-floor = true
        "#},
        ),
    ];
    let w = win(&layers, "x", ResolveKind::CurrentPin);
    // The other cascade file's floor survives for the security window.
    assert_eq!(w.advisory_floor, Some(days(30)));
}

/// `enabled` is authority-first: the nearest layer that sets it wins, so a repo can decline a
/// globally enabled feed — and an org can enable it over a repo's silence.
#[test]
fn advisories_enabled_is_authority_first() {
    use indoc::indoc;
    let layers = layers_from(vec![
        global(indoc! {r"
            [advisories]
            enabled = true
        "}),
        repo(indoc! {r"
            [advisories]
            enabled = false
        "}),
    ]);
    assert!(!resolve_advisory_policy(&layers).enabled);

    let layers = layers_from(vec![
        global(indoc! {r"
            [advisories]
            enabled = true
        "}),
        repo(""),
    ]);
    assert!(resolve_advisory_policy(&layers).enabled);
}

/// Declaring `--advisory-min-age` (or `--advisory-severity`) on the CLI/env enables the feed at
/// that layer: either flag without `--advisories` must not be a silent no-op.
#[test]
fn advisory_window_flags_enable_the_feed() {
    let fields = cooldown_core::config::WindowFields {
        advisory_min_age: Some("1d".to_string()),
        ..cooldown_core::config::WindowFields::default()
    };
    let cli_layer = cooldown_core::config::layer_from_fields(Origin::Cli, &fields)
        .expect("valid fields")
        .expect("a non-empty layer");
    let policy = resolve_advisory_policy(&[cli_layer]);
    assert!(policy.enabled, "declaring the window implies the feed");
    assert_eq!(policy.mode, AdvisoryMode::Shorten);
    assert_eq!(policy.min_age, SignedDuration::from_hours(24));
}

/// Unknown `[advisories]` tokens are config errors naming the field and the accepted values,
/// not silent defaults.
#[test]
fn advisories_bad_tokens_are_config_errors() {
    use indoc::indoc;
    let severity = indoc! {r#"
        [advisories]
        severity = "catastrophic"
    "#};
    let mode = indoc! {r#"
        [advisories]
        mode = "panic"
    "#};
    let source = indoc! {r#"
        [advisories]
        source = "ouija"
    "#};
    let min_age = indoc! {r#"
        [advisories]
        min-age = "soon"
    "#};
    for (toml, needle) in [
        (severity, "severity"),
        (mode, "mode"),
        (source, "source"),
        (min_age, "soon"),
    ] {
        let error = cooldown_core::config::parse_config(toml, Origin::Global)
            .expect_err("bad token must fail");
        assert!(
            error.to_string().contains(needle),
            "error for {toml:?} should mention {needle:?}: {error}"
        );
    }
}
