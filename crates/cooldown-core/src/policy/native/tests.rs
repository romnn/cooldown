use super::{
    KINDS, NativeExemptions, PolicyContext, glob_proven, native_exemptions,
    simple_patterns_intersect,
};
use crate::advisory::AdvisoryPolicy;
use crate::config::{builtin_default_layer, parse_config};
use crate::model::ToolId;
use crate::{ByKind, Origin, PatternGlob, PolicyLayer, ResolveQuery, Rule, Selector, WindowSpec};
use camino::Utf8Path;
use indoc::indoc;
use jiff::{SignedDuration, Timestamp};

const PNPM: ToolId = ToolId("pnpm");
const NPM: ToolId = ToolId("npm");

fn now() -> Timestamp {
    "2026-10-21T00:00:00Z".parse().expect("timestamp")
}

fn days(count: i64) -> WindowSpec {
    WindowSpec::MinAge(SignedDuration::from_hours(24 * count))
}

fn package(glob: &str) -> Selector {
    Selector::Package {
        glob: PatternGlob::new(glob).expect("glob"),
        tool: None,
    }
}

fn qualified(glob: &str, tool: ToolId) -> Selector {
    Selector::Package {
        glob: PatternGlob::new(glob).expect("glob"),
        tool: Some(tool),
    }
}

fn window(selector: Selector, window: ByKind) -> Rule {
    let mut rule = Rule::new(selector);
    rule.window = window;
    rule
}

fn latest(selector: Selector) -> Rule {
    window(selector, ByKind::scalar(WindowSpec::Latest))
}

fn min_age(selector: Selector, count: i64) -> Rule {
    window(selector, ByKind::scalar(days(count)))
}

fn allow(selector: Selector) -> Rule {
    let mut rule = Rule::new(selector);
    rule.allow = true;
    rule
}

fn floor(selector: Selector, count: i64) -> Rule {
    let mut rule = Rule::new(selector);
    rule.floor = Some(SignedDuration::from_hours(24 * count));
    rule
}

fn layer(origin: Origin, rules: Vec<Rule>) -> PolicyLayer {
    let mut layer = PolicyLayer::new(origin);
    layer.rules = rules;
    layer
}

fn repo(rules: Vec<Rule>) -> PolicyLayer {
    layer(Origin::Repo("cooldown.toml".into()), rules)
}

fn global(rules: Vec<Rule>) -> PolicyLayer {
    layer(Origin::Global, rules)
}

/// The built-in 7-day default beneath `layers`.
fn stack(layers: Vec<PolicyLayer>) -> Vec<PolicyLayer> {
    let mut stack = vec![builtin_default_layer()];
    stack.extend(layers);
    stack
}

/// The exemptions of one root-project context for pnpm.
fn exemptions(layers: &[PolicyLayer]) -> NativeExemptions {
    exemptions_in(layers, ".")
}

fn exemptions_in(layers: &[PolicyLayer], project: &str) -> NativeExemptions {
    let context = PolicyContext {
        layers,
        project: Utf8Path::new(project),
    };
    native_exemptions(&[context], PNPM, now())
}

fn exempt(selectors: &[&str]) -> NativeExemptions {
    NativeExemptions {
        exempt: selectors.iter().map(ToString::to_string).collect(),
        withheld: Vec::new(),
    }
}

fn withheld(selectors: &[&str]) -> NativeExemptions {
    NativeExemptions {
        exempt: Vec::new(),
        withheld: selectors.iter().map(ToString::to_string).collect(),
    }
}

/// The effective spec `resolve` gives `name` as a locked pin of the root project.
fn resolved_pin(layers: &[PolicyLayer], name: &str) -> WindowSpec {
    let query = ResolveQuery {
        tool: PNPM,
        package: name,
        registry: None,
        project: Utf8Path::new("."),
        kind: crate::ResolveKind::CurrentPin,
    };
    crate::resolve(layers, &query, now())
        .window
        .effective_spec(now())
}

/// Only `latest`/`allow` package rules for the target are candidates; a window, a non-package
/// `latest`, and a rule qualified for another tool are not.
#[test]
fn candidates_are_latest_and_allow_package_rules_for_the_target() {
    let mut default_latest = Rule::new(Selector::Default);
    default_latest.window = ByKind::scalar(WindowSpec::Latest);
    let layers = stack(vec![repo(vec![
        latest(package("@scope/latest-pkg")),
        allow(package("allowed-pkg")),
        min_age(package("min-age-pkg"), 30),
        allow(qualified("npm-only", NPM)),
        latest(qualified("pnpm-only", PNPM)),
    ])]);
    assert_eq!(
        exemptions(&layers),
        exempt(&["@scope/latest-pkg", "allowed-pkg", "pnpm-only"])
    );

    // A bare `latest` default is no per-package exemption.
    let layers = stack(vec![repo(vec![default_latest])]);
    assert_eq!(exemptions(&layers), NativeExemptions::default());
}

/// Without any context there is nothing to prove a selector against.
#[test]
fn no_context_proves_nothing() {
    assert_eq!(
        native_exemptions(&[], PNPM, now()),
        NativeExemptions::default()
    );
}

/// The issue's regressions: an exemption of one update kind is no package-wide exemption, an
/// exemption of every kind is, and a later layer that restores a window withdraws it.
#[test]
fn only_an_exemption_of_every_kind_after_layering_is_exempt() {
    let patch_only = ByKind {
        patch: Some(WindowSpec::Latest),
        ..ByKind::default()
    };
    let layers = stack(vec![repo(vec![window(package("@corp/*"), patch_only)])]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*"]));

    let layers = stack(vec![repo(vec![latest(package("@corp/*"))])]);
    assert_eq!(exemptions(&layers), exempt(&["@corp/*"]));

    let layers = stack(vec![
        global(vec![latest(package("@corp/*"))]),
        repo(vec![min_age(package("@corp/*"), 14)]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*"]));
}

/// A per-kind `[min-age]` table holds majors to their own window whether it sits beside the
/// `latest` package table or in a lower-authority document.
#[test]
fn a_parsed_per_kind_window_withholds_a_latest_glob() {
    let per_kind = indoc! {r#"
        [min-age]
        default = "7d"
        major = "30d"
    "#};
    let package_latest = indoc! {r#"
        [package."@corp/*"]
        latest = true
    "#};
    let together = parse_config(
        &format!("{per_kind}\n{package_latest}"),
        Origin::Repo("cooldown.toml".into()),
    )
    .expect("config");
    assert_eq!(exemptions(&stack(vec![together])), withheld(&["@corp/*"]));

    let lower = parse_config(per_kind, Origin::Global).expect("global config");
    let higher =
        parse_config(package_latest, Origin::Repo("cooldown.toml".into())).expect("repo config");
    assert_eq!(
        exemptions(&stack(vec![lower, higher])),
        withheld(&["@corp/*"])
    );

    // Without the per-kind table the same selector stays exempt.
    let alone =
        parse_config(package_latest, Origin::Repo("cooldown.toml".into())).expect("repo config");
    assert_eq!(exemptions(&stack(vec![alone])), exempt(&["@corp/*"]));
}

/// A literal follows `resolve`: every kind exempt proves it, while a lower layer's per-kind window
/// (reached by the kind lookup before the default fall-through) or a higher default withholds it.
#[test]
fn a_literal_is_proven_against_every_kind() {
    let layers = stack(vec![repo(vec![latest(package("@corp/ui"))])]);
    assert_eq!(exemptions(&layers), exempt(&["@corp/ui"]));

    let major_window = ByKind {
        major: Some(days(30)),
        ..ByKind::default()
    };
    let layers = stack(vec![
        global(vec![window(Selector::Default, major_window)]),
        repo(vec![latest(package("@corp/ui"))]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/ui"]));

    let layers = stack(vec![
        repo(vec![latest(package("@corp/ui"))]),
        layer(Origin::Cli, vec![min_age(Selector::Default, 14)]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/ui"]));
}

/// A same-layer tie of equal specificity goes to the first rule in `resolve`: the literal proof
/// follows that winner in both orders.
#[test]
fn a_literal_follows_the_actual_tie_winner() {
    let first = stack(vec![repo(vec![
        latest(package("@corp/ui")),
        min_age(package("@corp/*"), 30),
    ])]);
    assert_eq!(resolved_pin(&first, "@corp/ui"), WindowSpec::Latest);
    assert_eq!(exemptions(&first), exempt(&["@corp/ui"]));

    let second = stack(vec![repo(vec![
        min_age(package("@corp/*"), 30),
        latest(package("@corp/ui")),
    ])]);
    assert_eq!(resolved_pin(&second, "@corp/ui"), days(30));
    assert_eq!(exemptions(&second), withheld(&["@corp/ui"]));
}

/// The glob proof withholds a selector a same-layer tie could go against in either order, even
/// when `resolve` actually picks the selector's own rule: a known false negative.
#[test]
fn a_glob_is_withheld_in_both_tie_orders() {
    let first = stack(vec![repo(vec![
        latest(package("@corp/*")),
        min_age(package("@corp/ui"), 30),
    ])]);
    assert_eq!(resolved_pin(&first, "@corp/ui"), WindowSpec::Latest);
    assert_eq!(exemptions(&first), withheld(&["@corp/*"]));

    let second = stack(vec![repo(vec![
        min_age(package("@corp/ui"), 30),
        latest(package("@corp/*")),
    ])]);
    assert_eq!(resolved_pin(&second, "@corp/ui"), days(30));
    assert_eq!(exemptions(&second), withheld(&["@corp/*"]));
}

/// A tool-qualified window outranks an unqualified exemption only for its own tool.
#[test]
fn a_tool_qualified_conflict_withholds_only_for_its_tool() {
    let layers = stack(vec![repo(vec![
        latest(package("@corp/ui")),
        min_age(qualified("@corp/ui", PNPM), 30),
        latest(package("@corp/*")),
        min_age(qualified("@corp/*", PNPM), 30),
    ])]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*", "@corp/ui"]));

    let layers = stack(vec![repo(vec![
        latest(package("@corp/ui")),
        min_age(qualified("@corp/ui", NPM), 30),
        latest(package("@corp/*")),
        min_age(qualified("@corp/*", NPM), 30),
    ])]);
    assert_eq!(exemptions(&layers), exempt(&["@corp/*", "@corp/ui"]));
}

/// A higher layer's project rule withholds the selector only in a project it matches.
#[test]
fn a_project_rule_withholds_only_where_it_matches() {
    let layers = stack(vec![
        repo(vec![
            latest(package("@corp/ui")),
            latest(package("@corp/*")),
        ]),
        layer(
            Origin::Cli,
            vec![min_age(
                Selector::Project(PatternGlob::new("apps/*").expect("glob")),
                30,
            )],
        ),
    ]);
    assert_eq!(
        exemptions_in(&layers, "apps/web"),
        withheld(&["@corp/*", "@corp/ui"])
    );
    assert_eq!(
        exemptions_in(&layers, "."),
        exempt(&["@corp/*", "@corp/ui"])
    );
}

/// A registry floor binds the packages that registry serves, which any selector may name.
#[test]
fn a_registry_floor_withholds() {
    let layers = stack(vec![
        global(vec![floor(Selector::Registry("internal".into()), 3)]),
        repo(vec![
            latest(package("@corp/ui")),
            latest(package("@corp/*")),
        ]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*", "@corp/ui"]));
}

/// An `allow` bypasses a floor declared in its own layer, so the selector stays exempt.
#[test]
fn an_allow_bypasses_a_same_layer_floor() {
    let layers = stack(vec![repo(vec![
        allow(package("@corp/ui")),
        floor(package("@corp/ui"), 3),
        allow(package("@corp/*")),
        floor(package("@corp/sensitive"), 3),
    ])]);
    assert_eq!(exemptions(&layers), exempt(&["@corp/*", "@corp/ui"]));
}

/// A floor in another layer survives a repository `allow`, and `advisories.bypass-floor` lifts it
/// only for the security window, which a native exemption does not carry.
#[test]
fn an_allow_does_not_bypass_another_layers_floor() {
    let layers = stack(vec![
        global(vec![
            floor(package("@corp/ui"), 3),
            floor(package("@corp/sensitive"), 3),
        ]),
        repo(vec![allow(package("@corp/ui")), allow(package("@corp/*"))]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*", "@corp/ui"]));

    let mut bypassing = layers.clone();
    for layer in &mut bypassing {
        layer.advisories = Some(AdvisoryPolicy {
            bypass_floor: Some(true),
            ..AdvisoryPolicy::default()
        });
    }
    assert_eq!(exemptions(&bypassing), withheld(&["@corp/*", "@corp/ui"]));
}

/// An audited env or CLI `allow` bypasses every floor.
#[test]
fn an_audited_allow_bypasses_every_floor() {
    for origin in [Origin::Env, Origin::Cli] {
        let layers = stack(vec![
            global(vec![
                floor(package("@corp/ui"), 3),
                floor(package("@corp/sensitive"), 3),
            ]),
            layer(
                origin,
                vec![allow(package("@corp/ui")), allow(package("@corp/*"))],
            ),
        ]);
        assert_eq!(exemptions(&layers), exempt(&["@corp/*", "@corp/ui"]));
    }
}

/// A later rule for a package inside the glob withholds it, while a rule for a disjoint glob does
/// not.
#[test]
fn a_glob_is_withheld_only_by_an_overlapping_window() {
    let layers = stack(vec![
        global(vec![latest(package("@corp/*"))]),
        repo(vec![min_age(package("@corp/ui"), 30)]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*"]));

    let layers = stack(vec![
        global(vec![latest(package("@corp/*"))]),
        repo(vec![min_age(package("@other/*"), 30)]),
    ]);
    assert_eq!(exemptions(&layers), exempt(&["@corp/*"]));
}

/// A pattern outside the simple `*` syntax is assumed to overlap every other pattern, yet it
/// still covers itself, since identical text matches identical names.
#[test]
fn a_non_simple_pattern_overlaps_everything_but_covers_itself() {
    let layers = stack(vec![
        global(vec![latest(package("@corp/*"))]),
        repo(vec![min_age(package("@corp/{ui,x}"), 30)]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*"]));

    // `@other/{a,b}` cannot match an `@corp/` name, but the proof does not parse braces.
    let layers = stack(vec![
        global(vec![latest(package("@corp/*"))]),
        repo(vec![min_age(package("@other/{a,b}"), 30)]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*"]));

    let layers = stack(vec![repo(vec![latest(package("@types/{node,react}"))])]);
    assert_eq!(exemptions(&layers), exempt(&["@types/{node,react}"]));
}

/// A lower layer's per-kind default window is reached before the bare default falls through.
#[test]
fn a_lower_layer_per_kind_default_withholds_a_glob() {
    let major_window = ByKind {
        major: Some(days(30)),
        ..ByKind::default()
    };
    let layers = stack(vec![
        global(vec![window(Selector::Default, major_window)]),
        repo(vec![latest(package("@corp/*"))]),
    ]);
    assert_eq!(exemptions(&layers), withheld(&["@corp/*"]));
}

/// A root `@corp/*` exemption is proven for the root project, but a child project that holds
/// `@corp/ui` to 30 days withholds it from every context that includes the child.
#[test]
fn a_selector_must_be_proven_in_every_context() {
    let root = stack(vec![layer(
        Origin::Repo("cooldown.toml".into()),
        vec![latest(package("@corp/*"))],
    )]);
    let mut child = root.clone();
    child.push(layer(
        Origin::Repo("packages/child/cooldown.toml".into()),
        vec![min_age(package("@corp/ui"), 30)],
    ));
    let root_context = PolicyContext {
        layers: &root,
        project: Utf8Path::new("."),
    };
    let child_context = PolicyContext {
        layers: &child,
        project: Utf8Path::new("packages/child"),
    };

    assert_eq!(
        native_exemptions(&[root_context], PNPM, now()),
        exempt(&["@corp/*"])
    );
    assert_eq!(
        native_exemptions(&[child_context], PNPM, now()),
        withheld(&["@corp/*"])
    );
    assert_eq!(
        native_exemptions(&[root_context, child_context], PNPM, now()),
        withheld(&["@corp/*"])
    );

    // A candidate only one stack names is still proven in every context.
    let mut exempting_child = root.clone();
    exempting_child.push(layer(
        Origin::Repo("packages/child/cooldown.toml".into()),
        vec![latest(package("left-pad"))],
    ));
    let exempting_context = PolicyContext {
        layers: &exempting_child,
        project: Utf8Path::new("packages/child"),
    };
    assert_eq!(
        native_exemptions(&[root_context, exempting_context], PNPM, now()),
        NativeExemptions {
            exempt: vec!["@corp/*".to_string()],
            withheld: vec!["left-pad".to_string()],
        }
    );
}

/// Simple patterns intersect exactly when some name matches both.
#[test]
fn simple_patterns_intersect_exactly() {
    for (left, right, intersect) in [
        ("@corp/*", "@corp/ui", true),
        ("*-plugin", "eslint-*", true),
        ("a*b", "ab", true),
        ("a*", "b*", false),
        ("left-pad", "right-pad", false),
        ("left-pad", "left-pad", true),
        ("*", "", true),
        ("a*", "", false),
        ("@corp/*", "@other/*", false),
        ("*a", "*b", false),
        ("a*c", "*b*", true),
        ("xy*", "*yx", true),
    ] {
        assert_eq!(
            simple_patterns_intersect(left, right),
            intersect,
            "{left} / {right}"
        );
        assert_eq!(
            simple_patterns_intersect(right, left),
            intersect,
            "{right} / {left}"
        );
    }
}

/// Every string over `alphabet` up to `max_length` characters, shortest first.
fn strings(alphabet: &[char], max_length: usize) -> Vec<String> {
    let mut all = vec![String::new()];
    let mut frontier = vec![String::new()];
    for _ in 0..max_length {
        let mut next = Vec::new();
        for prefix in &frontier {
            for character in alphabet {
                next.push(format!("{prefix}{character}"));
            }
        }
        all.extend(next.iter().cloned());
        frontier = next;
    }
    all
}

/// The automaton product agrees with brute force: any name both globs match is found, and a
/// claimed intersection always has a short witness.
#[test]
fn simple_patterns_intersect_agrees_with_the_glob_matcher() {
    // Patterns over `a`, `b` and single `*` up to length 3; names over `a` and `b` long enough to
    // witness any intersection of two such patterns.
    let patterns: Vec<String> = strings(&['a', 'b', '*'], 3)
        .into_iter()
        .filter(|pattern| !pattern.contains("**"))
        .collect();
    let names = strings(&['a', 'b'], 6);
    for left in &patterns {
        for right in &patterns {
            let left_glob = PatternGlob::new(left).expect("glob");
            let right_glob = PatternGlob::new(right).expect("glob");
            let witness = names
                .iter()
                .any(|name| left_glob.is_match(name) && right_glob.is_match(name));
            assert_eq!(
                simple_patterns_intersect(left, right),
                witness,
                "{left:?} / {right:?}"
            );
        }
    }
}

/// The building blocks of the generated soundness comparison.
fn rule_pool() -> Vec<Rule> {
    let every_kind = |spec: WindowSpec| ByKind {
        default: Some(spec.clone()),
        major: Some(spec.clone()),
        minor: Some(spec.clone()),
        patch: Some(spec),
    };
    vec![
        latest(package("@corp/*")),
        allow(package("@corp/*")),
        window(
            package("@corp/*"),
            ByKind {
                patch: Some(days(30)),
                ..ByKind::default()
            },
        ),
        min_age(package("@corp/ui"), 30),
        latest(package("@corp/ui")),
        floor(package("@corp/ui"), 3),
        window(
            Selector::Default,
            ByKind {
                major: Some(days(30)),
                ..ByKind::default()
            },
        ),
        window(Selector::Default, every_kind(WindowSpec::Latest)),
        floor(Selector::Default, 2),
        allow(package("*")),
        min_age(Selector::Registry("r".into()), 30),
        min_age(
            Selector::Project(PatternGlob::new("sub/*").expect("glob")),
            30,
        ),
        window(
            Selector::Tool(PNPM),
            ByKind {
                major: Some(WindowSpec::Latest),
                minor: Some(WindowSpec::Latest),
                ..ByKind::default()
            },
        ),
        min_age(Selector::Tool(NPM), 30),
        min_age(qualified("@corp/*", NPM), 30),
        min_age(qualified("@corp/*", PNPM), 30),
        window(
            package("@co*"),
            ByKind {
                minor: Some(days(30)),
                ..ByKind::default()
            },
        ),
        min_age(package("@corp/{ui,x}"), 30),
        min_age(package("@other/*"), 30),
    ]
}

/// Every ordered selection of at most two rules from `pool`.
fn selections(pool: &[Rule]) -> Vec<Vec<Rule>> {
    let mut selections = vec![Vec::new()];
    for (first_index, first) in pool.iter().enumerate() {
        selections.push(vec![first.clone()]);
        for (second_index, second) in pool.iter().enumerate() {
            if first_index != second_index {
                selections.push(vec![first.clone(), second.clone()]);
            }
        }
    }
    selections
}

/// The names every candidate glob is checked on: each literal a rule names plus synthetic ones,
/// including an empty scope suffix and a name only the `@co*` rule matches.
const NAMES: [&str; 6] = ["@corp/ui", "@corp/x", "@corp/", "@co", "@other/a", "zzz"];

/// Whenever the glob proof exports a selector, `resolve` exempts every enumerated name it matches
/// for every kind, registry and project.
///
/// The stacks enumerate ordered rule pairs in a lower layer (so same-layer ties occur in both
/// orders) under at most one rule in a higher layer, for two layer-origin pairs: a floor in a
/// global layer that a repository allow cannot bypass, and one in a repository layer that a CLI
/// allow can.
#[test]
fn the_glob_proof_is_sound_against_resolve() {
    let pool = rule_pool();
    let lower_selections = selections(&pool);
    // A higher layer overrides the lower one; these rules each override in a different way.
    let mut higher_selections = vec![Vec::new()];
    higher_selections.extend(
        [
            latest(package("@corp/*")),
            allow(package("*")),
            min_age(package("@corp/ui"), 30),
            min_age(qualified("@corp/*", PNPM), 30),
            window(
                Selector::Default,
                ByKind {
                    major: Some(days(30)),
                    ..ByKind::default()
                },
            ),
            floor(Selector::Default, 2),
            min_age(Selector::Registry("r".into()), 30),
            min_age(
                Selector::Project(PatternGlob::new("sub/*").expect("glob")),
                30,
            ),
        ]
        .map(|rule| vec![rule]),
    );
    let origins = [
        (Origin::Global, Origin::Repo("cooldown.toml".into())),
        (Origin::Repo("cooldown.toml".into()), Origin::Cli),
    ];
    let globs = ["@corp/*", "*"].map(|glob| PatternGlob::new(glob).expect("glob"));
    // A registry no rule names resolves like no registry at all.
    let registries = [None, Some("r")];
    let mut proven = 0_usize;
    let mut unproven = 0_usize;
    for (lower_origin, higher_origin) in &origins {
        for lower in &lower_selections {
            for higher in &higher_selections {
                let layers = stack(vec![
                    layer(lower_origin.clone(), lower.clone()),
                    layer(higher_origin.clone(), higher.clone()),
                ]);
                for project in [".", "sub/a"].map(Utf8Path::new) {
                    let context = PolicyContext {
                        layers: &layers,
                        project,
                    };
                    for glob in &globs {
                        if !glob_proven(context, glob, PNPM) {
                            unproven += 1;
                            continue;
                        }
                        proven += 1;
                        for name in NAMES.iter().filter(|name| glob.is_match(name)) {
                            for registry in registries {
                                for kind in KINDS {
                                    let query = ResolveQuery {
                                        tool: PNPM,
                                        package: name,
                                        registry,
                                        project,
                                        kind,
                                    };
                                    let spec = crate::resolve(&layers, &query, now())
                                        .window
                                        .effective_spec(now());
                                    assert_eq!(
                                        spec,
                                        WindowSpec::Latest,
                                        "{} proven, but {name} ({registry:?}, {kind:?}) in \
                                         {project} resolves otherwise under {layers:#?}",
                                        glob.raw()
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    // The enumeration exercises both outcomes, so the comparison is not vacuous.
    assert!(proven > 1_000, "only {proven} proofs succeeded");
    assert!(unproven > 1_000, "only {unproven} proofs failed");
}
