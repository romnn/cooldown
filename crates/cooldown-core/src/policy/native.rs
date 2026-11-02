//! Proofs that a `latest`/`allow` package selector may become a package-wide native exemption.
//!
//! A native exemption list (pnpm's `minimumReleaseAgeExclude`) exempts every version of every
//! package a selector names, so `sync` may write a selector there only when cooldown's own policy
//! exempts all of those releases too.
//! [`native_exemptions`] proves that per selector and withholds every selector it cannot prove.

use super::model::{
    ByKind, Origin, PatternGlob, PolicyLayer, ResolveKind, ResolveQuery, Rule, RuleRank, Selector,
    WindowSpec,
};
use super::resolve::resolve;
use crate::model::{ToolId, UpdateKind};
use camino::Utf8Path;
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};

/// The resolution kinds a native exemption covers: the locked pin and every update kind.
const KINDS: [ResolveKind; 4] = [
    ResolveKind::CurrentPin,
    ResolveKind::Candidate(UpdateKind::Major),
    ResolveKind::Candidate(UpdateKind::Minor),
    ResolveKind::Candidate(UpdateKind::Patch),
];

/// One policy stack and the project path its rules resolve against.
#[derive(Debug, Clone, Copy)]
pub struct PolicyContext<'a> {
    /// The policy layers, low to high authority, exactly as [`resolve`] takes them.
    pub layers: &'a [PolicyLayer],
    /// The repo-relative project path that [`Selector::Project`] rules match.
    pub project: &'a Utf8Path,
}

/// The policy's `latest`/`allow` package selectors, split by whether a native exemption list may
/// carry them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeExemptions {
    /// Selectors proven to exempt every release of every package they match, sorted and
    /// deduplicated.
    pub exempt: Vec<String>,
    /// Selectors the proof could not establish, sorted and deduplicated.
    ///
    /// cooldown may still exempt some or all of their releases, but a package-wide native entry
    /// could exempt a release cooldown still gates.
    pub withheld: Vec<String>,
}

/// Splits the `latest`/`allow` package selectors that apply to `target` into those that may become
/// package-wide native exemptions and those withheld.
///
/// A candidate is every [`Selector::Package`] rule, unqualified or qualified for `target`, that
/// sets `allow` or a [`WindowSpec::Latest`] window of any kind.
/// The candidates are collected from every context.
///
/// A candidate is exempt only when, in **every** context, each of its packages resolves to
/// [`WindowSpec::Latest`] with no binding floor for the locked pin and for every update kind, in
/// every registry.
/// Each context is proven against its own stack; the stacks are never merged.
/// Without any context nothing is proven.
///
/// The proof is sufficient, not exact: a withheld selector is not necessarily gated anywhere.
///
/// - A literal selector is resolved with [`resolve`] once per kind and per registry context the
///   layers distinguish (none, plus each [`Selector::Registry`] they name), which is exact.
/// - A glob selector is proven by dominance instead, since its packages cannot be enumerated.
///   It ranks rules exactly as [`resolve`] does, by layer, selector specificity, and then how
///   narrowly the pattern is spelled.
///   Every rule that may match one of its packages and ranks at least as high as the best rule
///   matching all of them must resolve the same way.
///   For example, a `@corp/ui` rule with a 30-day window outranks a same-layer `@corp/*` `latest`
///   rule, so the proof withholds `@corp/*`, while a `latest` on the narrower `@corp/ui*` can still
///   be proven beside a broad `@corp/*` window.
///   Rules of equal rank all count as possible winners, since the proof does not model which of
///   several identical declarations comes first.
///   Two patterns built only from literals and single `*` wildcards are intersected exactly; any
///   other pattern is assumed to overlap every pattern but its own.
///
/// `advisories.bypass-floor` and the advisory shorten mode change only the security window, which a
/// native exemption does not carry, so a floor they lift still withholds the selector.
///
/// # Examples
///
/// ```
/// use camino::Utf8Path;
/// use cooldown_core::config::{builtin_default_layer, parse_config};
/// use cooldown_core::{Origin, PolicyContext, ToolId, native_exemptions};
/// use indoc::indoc;
///
/// // Majors of `@corp/*` packages still wait 30 days, so pnpm must not exempt them outright.
/// let config = indoc! {r#"
///     [min-age]
///     default = "7d"
///     major = "30d"
///
///     [package."@corp/*"]
///     latest = true
/// "#};
/// let repo = parse_config(config, Origin::Repo("cooldown.toml".into()))?;
/// let layers = [builtin_default_layer(), repo];
/// let context = PolicyContext {
///     layers: &layers,
///     project: Utf8Path::new("."),
/// };
///
/// let exemptions = native_exemptions(&[context], ToolId("pnpm"), "2026-01-15T00:00:00Z".parse()?);
/// assert!(exemptions.exempt.is_empty());
/// assert_eq!(exemptions.withheld, ["@corp/*"]);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn native_exemptions(
    contexts: &[PolicyContext<'_>],
    target: ToolId,
    now: Timestamp,
) -> NativeExemptions {
    let mut candidates = BTreeMap::new();
    for context in contexts {
        for rule in context.layers.iter().flat_map(|layer| &layer.rules) {
            if let Some(glob) = candidate_glob(rule, target) {
                candidates.entry(glob.raw()).or_insert(glob);
            }
        }
    }
    let mut exemptions = NativeExemptions::default();
    for (raw, glob) in candidates {
        let proven = !contexts.is_empty()
            && contexts
                .iter()
                .all(|context| proven_in(*context, glob, target, now));
        let list = if proven {
            &mut exemptions.exempt
        } else {
            &mut exemptions.withheld
        };
        list.push(raw.to_string());
    }
    exemptions
}

/// The package glob of `rule` when it is a `latest`/`allow` package rule that applies to `target`.
fn candidate_glob(rule: &Rule, target: ToolId) -> Option<&PatternGlob> {
    let Selector::Package { glob, tool } = &rule.selector else {
        return None;
    };
    if tool.is_some_and(|tool| tool != target) {
        return None;
    }
    let latest = [
        &rule.window.default,
        &rule.window.major,
        &rule.window.minor,
        &rule.window.patch,
    ]
    .into_iter()
    .flatten()
    .any(|spec| *spec == WindowSpec::Latest);
    (rule.allow || latest).then_some(glob)
}

fn proven_in(
    context: PolicyContext<'_>,
    glob: &PatternGlob,
    target: ToolId,
    now: Timestamp,
) -> bool {
    if glob.is_literal() {
        literal_proven(context, glob.raw(), target, now)
    } else {
        glob_proven(context, glob, target)
    }
}

/// Whether every resolution of the package `name` in `context` is exempt with no binding floor.
fn literal_proven(context: PolicyContext<'_>, name: &str, target: ToolId, now: Timestamp) -> bool {
    // A registry no rule names resolves exactly like a package with no registry.
    let mut registries = vec![None];
    registries.extend(
        context
            .layers
            .iter()
            .flat_map(|layer| &layer.rules)
            .filter_map(|rule| match &rule.selector {
                Selector::Registry(registry) => Some(Some(registry.as_str())),
                _ => None,
            }),
    );
    registries.into_iter().all(|registry| {
        KINDS.into_iter().all(|kind| {
            let query = ResolveQuery {
                tool: target,
                package: name,
                registry,
                project: context.project,
                kind,
            };
            resolve(context.layers, &query, now)
                .window
                .effective_spec(now)
                == WindowSpec::Latest
        })
    })
}

/// How a rule's selector relates to the packages a glob matches in one context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Coverage {
    /// The selector matches none of them.
    Disjoint,
    /// The selector may match some of them, in some registry.
    Overlaps,
    /// The selector matches every one of them, in every registry.
    Covers,
}

/// A rule that may match one of the glob's packages.
struct RelevantRule<'a> {
    /// The rule's precedence, the same one [`resolve`] ranks it by.
    rank: RuleRank<'a>,
    origin: &'a Origin,
    rule: &'a Rule,
    /// Whether the rule matches every package the glob matches.
    covers: bool,
}

/// What every relevant rule for one window field implies about the field's pick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldOutcome {
    /// Every package picks a [`WindowSpec::Latest`] value for the field.
    Latest,
    /// A package picks either a [`WindowSpec::Latest`] value or nothing, falling through to the
    /// bare default.
    LatestOrUnset,
    /// Some package may pick a window that is not [`WindowSpec::Latest`].
    Unproven,
}

/// Whether every package `glob` matches resolves to an exemption with no binding floor in
/// `context`, by dominance over the rules that may match those packages.
fn glob_proven(context: PolicyContext<'_>, glob: &PatternGlob, target: ToolId) -> bool {
    let mut rules = Vec::new();
    for (layer_index, layer) in context.layers.iter().enumerate() {
        for rule in &layer.rules {
            let covers = match coverage(&rule.selector, glob, context.project, target) {
                Coverage::Disjoint => continue,
                Coverage::Overlaps => false,
                Coverage::Covers => true,
            };
            rules.push(RelevantRule {
                rank: RuleRank::of(layer_index, &rule.selector),
                origin: &layer.origin,
                rule,
                covers,
            });
        }
    }
    allow_proven(&rules) || window_proven(&rules)
}

fn coverage(
    selector: &Selector,
    glob: &PatternGlob,
    project: &Utf8Path,
    target: ToolId,
) -> Coverage {
    let covers_if = |matches: bool| {
        if matches {
            Coverage::Covers
        } else {
            Coverage::Disjoint
        }
    };
    match selector {
        Selector::Default => Coverage::Covers,
        Selector::Tool(tool) => covers_if(*tool == target),
        Selector::Project(pattern) => covers_if(pattern.is_match(project.as_str())),
        // A registry rule matches only the packages its registry serves, which may be any of them.
        Selector::Registry(_) => Coverage::Overlaps,
        Selector::Package {
            glob: rule_glob,
            tool,
        } => {
            if tool.is_some_and(|tool| tool != target) {
                Coverage::Disjoint
            } else if rule_glob.raw() == glob.raw() || rule_glob.raw() == "*" {
                // Identical text compiles to an identical matcher, and a bare `*` matches every name.
                Coverage::Covers
            } else if patterns_may_intersect(rule_glob, glob) {
                Coverage::Overlaps
            } else {
                Coverage::Disjoint
            }
        }
    }
}

/// Whether a covering `allow` exempts every package and bypasses every floor that may match one.
///
/// [`resolve`] bypasses a floor for an allow in the floor's own layer or for any env/CLI allow, and
/// a covering allow matches every package, so it applies to each floor in every resolution.
fn allow_proven(rules: &[RelevantRule<'_>]) -> bool {
    let covering_allows: Vec<&RelevantRule<'_>> = rules
        .iter()
        .filter(|relevant| relevant.covers && relevant.rule.allow)
        .collect();
    if covering_allows.is_empty() {
        return false;
    }
    let audited = covering_allows
        .iter()
        .any(|allow| matches!(allow.origin, Origin::Env | Origin::Cli));
    rules
        .iter()
        .filter(|relevant| relevant.rule.floor.is_some())
        .all(|floor| {
            audited
                || covering_allows
                    .iter()
                    .any(|allow| allow.rank.layer_index == floor.rank.layer_index)
        })
}

/// Whether every window field picks [`WindowSpec::Latest`] for every package, with no floor that
/// may match one.
///
/// A per-kind field that no rule sets falls through to the bare default in [`resolve`], so it
/// passes when the default does.
/// The default itself must be proven, since without a pick [`resolve`] uses a 7-day safety net.
fn window_proven(rules: &[RelevantRule<'_>]) -> bool {
    if rules.iter().any(|relevant| relevant.rule.floor.is_some()) {
        return false;
    }
    let kinds: [fn(&ByKind) -> Option<&WindowSpec>; 3] = [
        |by_kind| by_kind.major.as_ref(),
        |by_kind| by_kind.minor.as_ref(),
        |by_kind| by_kind.patch.as_ref(),
    ];
    field_outcome(rules, |by_kind| by_kind.default.as_ref()) == FieldOutcome::Latest
        && kinds
            .into_iter()
            .all(|extract| field_outcome(rules, extract) != FieldOutcome::Unproven)
}

/// Bounds the pick of one window field over every package the rules' glob matches.
///
/// A covering rule that sets the field matches every package, so each package's winner ranks at
/// least as high as the highest such rule.
/// The winner is then one of the relevant rules at or above that rank, compared by the full rank
/// [`resolve`] uses, including the direction of its raw-text tie-break.
/// An equal rank counts as a possible winner: identical declarations tie, and [`resolve`] keeps
/// the first of them, which the proof does not model.
/// Without a covering rule, any relevant rule that sets the field may win, or none may.
fn field_outcome(
    rules: &[RelevantRule<'_>],
    extract: impl Fn(&ByKind) -> Option<&WindowSpec>,
) -> FieldOutcome {
    let threshold = rules
        .iter()
        .filter(|relevant| relevant.covers && extract(&relevant.rule.window).is_some())
        .map(|relevant| relevant.rank)
        .max();
    let contenders_latest = rules
        .iter()
        .filter(|relevant| threshold.is_none_or(|threshold| relevant.rank >= threshold))
        .filter_map(|relevant| extract(&relevant.rule.window))
        .all(|spec| *spec == WindowSpec::Latest);
    match (threshold, contenders_latest) {
        (_, false) => FieldOutcome::Unproven,
        (Some(_), true) => FieldOutcome::Latest,
        (None, true) => FieldOutcome::LatestOrUnset,
    }
}

/// Whether some package name may match both patterns.
///
/// Exact when both patterns are simple ([`PatternGlob::is_simple_wildcard`]); any other pattern
/// is assumed to intersect.
fn patterns_may_intersect(left: &PatternGlob, right: &PatternGlob) -> bool {
    if !(left.is_simple_wildcard() && right.is_simple_wildcard()) {
        return true;
    }
    simple_patterns_intersect(left.raw(), right.raw())
}

/// A pair of positions, one in each pattern, reached by reading the same text with both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Position {
    left: usize,
    right: usize,
}

/// Whether some text matches both patterns, where `*` matches any run of characters and every
/// other character matches itself.
///
/// This searches the product of the two patterns' automata: a `*` may match nothing (advance its
/// pattern alone) or absorb one character the other pattern reads, and the patterns intersect
/// when both reach their end together.
fn simple_patterns_intersect(left: &str, right: &str) -> bool {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let mut seen = BTreeSet::new();
    let mut pending = vec![Position { left: 0, right: 0 }];
    while let Some(position) = pending.pop() {
        if !seen.insert(position) {
            continue;
        }
        let left_token = left.get(position.left).copied();
        let right_token = right.get(position.right).copied();
        let advance_left = Position {
            left: position.left + 1,
            ..position
        };
        let advance_right = Position {
            right: position.right + 1,
            ..position
        };
        match (left_token, right_token) {
            (None, None) => return true,
            // Each `*` may match nothing; two `*` absorbing a shared character change nothing.
            (Some('*'), Some('*')) => pending.extend([advance_left, advance_right]),
            // A `*` either matches nothing or absorbs the other pattern's next character.
            (Some('*'), _) => {
                pending.push(advance_left);
                if right_token.is_some() {
                    pending.push(advance_right);
                }
            }
            (_, Some('*')) => {
                pending.push(advance_right);
                if left_token.is_some() {
                    pending.push(advance_left);
                }
            }
            (Some(left_character), Some(right_character)) if left_character == right_character => {
                pending.push(Position {
                    left: position.left + 1,
                    right: position.right + 1,
                });
            }
            _ => {}
        }
    }
    false
}

#[cfg(test)]
mod tests;
