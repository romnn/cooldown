use super::model::{
    ByKind, Origin, PolicyLayer, Resolution, ResolveKind, ResolveQuery, ResolvedWindow, Selector,
    TraceStep, WindowSpec,
};
use crate::duration::duration_as_days;
use jiff::{SignedDuration, Timestamp};

/// A field pick: which rule won for one window field.
struct FieldPick {
    layer_index: usize,
    specificity: u8,
    origin: Origin,
    selector: Selector,
    spec: WindowSpec,
}

/// Find the authority-first winner for a single window field: the highest layer with a matching
/// rule that sets it, tie-broken within the layer by selector specificity.
fn pick_field(
    layers: &[PolicyLayer],
    query: &ResolveQuery<'_>,
    extract: impl Fn(&ByKind) -> Option<&WindowSpec>,
) -> Option<FieldPick> {
    let mut best: Option<FieldPick> = None;
    for (layer_index, layer) in layers.iter().enumerate() {
        for rule in &layer.rules {
            if !rule.selector.matches(query) {
                continue;
            }
            let Some(spec) = extract(&rule.window) else {
                continue;
            };
            let specificity = rule.selector.specificity();
            let better = match &best {
                None => true,
                Some(best_pick) => {
                    (layer_index, specificity) > (best_pick.layer_index, best_pick.specificity)
                }
            };
            if better {
                best = Some(FieldPick {
                    layer_index,
                    specificity,
                    origin: layer.origin.clone(),
                    selector: rule.selector.clone(),
                    spec: spec.clone(),
                });
            }
        }
    }
    best
}

fn field_for_kind(kind: ResolveKind) -> fn(&ByKind) -> Option<&WindowSpec> {
    match kind {
        ResolveKind::CurrentPin | ResolveKind::EffectiveDefault => {
            |by_kind| by_kind.default.as_ref()
        }
        ResolveKind::Candidate(crate::model::UpdateKind::Major) => |by_kind| by_kind.major.as_ref(),
        ResolveKind::Candidate(crate::model::UpdateKind::Minor) => |by_kind| by_kind.minor.as_ref(),
        ResolveKind::Candidate(crate::model::UpdateKind::Patch) => |by_kind| by_kind.patch.as_ref(),
    }
}

fn field_name(kind: ResolveKind) -> &'static str {
    match kind {
        ResolveKind::CurrentPin | ResolveKind::EffectiveDefault => "default",
        ResolveKind::Candidate(crate::model::UpdateKind::Major) => "major",
        ResolveKind::Candidate(crate::model::UpdateKind::Minor) => "minor",
        ResolveKind::Candidate(crate::model::UpdateKind::Patch) => "patch",
    }
}

fn min_age_days_of(spec: &WindowSpec, now: Timestamp) -> f64 {
    match spec {
        WindowSpec::MinAge(duration) => duration_as_days(*duration),
        WindowSpec::Latest => 0.0,
        WindowSpec::Freeze(timestamp) => duration_as_days(crate::duration::since(now, *timestamp)),
    }
}

/// Resolves the effective window for `query` against `layers`, with a full derivation trace.
///
/// Each field is combined by its own rule: `min-age` (and the per-kind windows) is
/// **authority-first** — the highest layer that sets it wins, tie-broken within the layer by
/// selector specificity, with a per-kind fall-through to the bare `default`; `max-major` uses the
/// same authority-first ordering; `floor` is **max-clamped** across layers; and `allow` is a
/// floor-aware **union** that zeroes an ordinary window but bypasses a floor only when it is
/// co-declared in that floor's layer or is an audited env/CLI override. The returned
/// [`Resolution::trace`] records every rule considered and which one applied.
///
/// `layers` are expected low → high authority. If no layer sets the resolved field (e.g. the
/// caller omitted the built-in `Default` layer), a 7-day `min-age` safety net is used.
///
/// # Examples
///
/// ```
/// use cooldown_core::{
///     ByKind, ToolId, Origin, PolicyLayer, ResolveKind, ResolveQuery, Rule, Selector,
///     WindowSpec, resolve,
/// };
/// use camino::Utf8Path;
/// use jiff::{SignedDuration, Timestamp};
///
/// let mut layer = PolicyLayer::new(Origin::Cli);
/// let mut rule = Rule::new(Selector::Default);
/// rule.window = ByKind::scalar(WindowSpec::MinAge(SignedDuration::from_hours(24 * 14)));
/// layer.rules.push(rule);
///
/// let now: Timestamp = "2026-01-15T00:00:00Z".parse()?;
/// let query = ResolveQuery {
///     tool: ToolId("cargo"),
///     package: "serde",
///     registry: None,
///     project: Utf8Path::new("."),
///     kind: ResolveKind::CurrentPin,
/// };
///
/// let resolution = resolve(&[layer], &query, now);
/// assert_eq!(resolution.window.decided_by, Origin::Cli);
/// assert!((resolution.window.effective_min_age_days(now) - 14.0).abs() < 1e-9);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn resolve(layers: &[PolicyLayer], query: &ResolveQuery<'_>, now: Timestamp) -> Resolution {
    let mut trace: Vec<TraceStep> = Vec::new();
    let pick = pick_window(layers, query, now, &mut trace);
    trace_max_major(layers, query, &mut trace);
    let floors = collect_floor_candidates(layers, query, &mut trace);
    let allow = resolve_allows(layers, query, &floors, &mut trace);

    // An `allow` reflects as `spec = Latest` (base cutoff = now); a residual floor it could not
    // bypass still clamps. Fully exempt only when no residual floor remains.
    let spec = if allow.matched {
        WindowSpec::Latest
    } else {
        pick.spec.clone()
    };
    let exempt = allow.matched && allow.effective_floor.is_none();
    let (floor_duration, floor_origin) = match &allow.effective_floor {
        Some(floor) => (Some(floor.duration), Some(floor.origin.clone())),
        None => (None, None),
    };
    let advisory_floor = resolve_advisory_floor(layers, &allow.residual_floors, &mut trace);
    // Provenance: when an allow applied, point at the highest-layer matching allow; else the pick.
    let (decided_by, decided_selector, exempt_origin) = match allow.provenance {
        Some((origin, selector)) => (origin.clone(), selector, Some(origin)),
        None => (pick.origin.clone(), pick.selector.clone(), None),
    };

    let window = ResolvedWindow {
        spec,
        decided_by,
        decided_selector,
        floor: floor_duration,
        floor_origin,
        exempt,
        exempt_origin,
        // Only the advisory shorten mode ever sets this, downstream of ordinary resolution.
        shortened_by: None,
        advisory_floor: advisory_floor.as_ref().map(|floor| floor.duration),
        advisory_floor_origin: advisory_floor.map(|floor| floor.origin),
    };

    Resolution { window, trace }
}

/// The floor that would still clamp the advisory *security* window: `bypass-floor` is resolved
/// against every residual floor candidate **by exact layer** — the same per-candidate mechanism
/// `allow` uses — so a repo bypass co-declared with a repo floor still cannot escape a separate
/// org (global) floor, and one repo-cascade file cannot lift another cascade file's floor.
///
/// `advisories.bypass-floor` is traced *here* rather than during
/// [`resolve_advisory_policy`](crate::resolve_advisory_policy), because only this pass knows
/// what a declaration actually did: a bypass that lifts no floor of its own layer changed
/// nothing, however the rest of the stack resolved.
fn resolve_advisory_floor(
    layers: &[PolicyLayer],
    residual_floors: &[FloorCandidate],
    trace: &mut Vec<TraceStep>,
) -> Option<FloorCandidate> {
    let bypass_declared = |layer_index: usize| -> bool {
        layers.get(layer_index).is_some_and(|layer| {
            layer
                .advisories
                .as_ref()
                .is_some_and(|advisories| advisories.bypass_floor == Some(true))
        })
    };
    // One step per floor candidate the declaration lifts, not one per layer: a layer can declare
    // several matching floors (a bare `floor` beside a `[package."x"] floor`), and a single step
    // could name only one of their durations.
    for (layer_index, layer) in layers.iter().enumerate() {
        if !bypass_declared(layer_index) {
            continue;
        }
        let mut lifted = residual_floors
            .iter()
            .filter(|floor| floor.layer_index == layer_index)
            .peekable();
        if lifted.peek().is_none() {
            trace.push(TraceStep {
                layer: layer.origin.clone(),
                field: "advisories.bypass-floor".into(),
                selector: None,
                min_age_days: None,
                applied: false,
                // Not necessarily "declares no floor": an `allow` in this layer may already have
                // removed the one it declared, leaving this nothing to lift either way.
                note: "bypass-floor = true considered; no residual floor of this layer remains \
                       for it to lift"
                    .into(),
            });
            continue;
        }
        for floor in lifted {
            trace.push(TraceStep {
                layer: layer.origin.clone(),
                field: "advisories.bypass-floor".into(),
                selector: Some(floor.selector.clone()),
                min_age_days: Some(duration_as_days(floor.duration)),
                applied: true,
                note: "bypass-floor = true lifts this floor for the security window only".into(),
            });
        }
    }
    residual_floors
        .iter()
        .filter(|floor| !bypass_declared(floor.layer_index))
        .max_by(|a, b| (a.duration, a.layer_index).cmp(&(b.duration, b.layer_index)))
        .cloned()
}

/// The `package` selector globs the policy marks fully exempt from the cooldown — a `latest = true`
/// rule (a [`WindowSpec::Latest`] on any kind) or an `allow` entry.
///
/// These map directly onto a native per-package exemption list (pnpm's `minimumReleaseAgeExclude`,
/// which accepts the same glob patterns), so `sync` can bake the `cooldown.toml` `latest` packages
/// into a tool's native config beside the default window — otherwise a `latest`-exempt package stays
/// quarantined by the native rolling window even though cooldown's own policy exempts it. The raw glob
/// is emitted verbatim (`@scope/*`, `@typescript/native-preview`), deduplicated and sorted for a
/// deterministic, idempotent write. A `package` rule that sets a `min-age`/`freeze` (not `latest`) is
/// not exempt and is never listed.
#[must_use]
pub fn exempt_package_globs(layers: &[PolicyLayer], target: crate::model::ToolId) -> Vec<String> {
    let mut globs = std::collections::BTreeSet::new();
    for layer in layers {
        for rule in &layer.rules {
            let Selector::Package { glob, tool } = &rule.selector else {
                continue;
            };
            if tool.is_some_and(|tool| tool != target) {
                continue;
            }
            let latest = [
                &rule.window.default,
                &rule.window.major,
                &rule.window.minor,
                &rule.window.patch,
            ]
            .into_iter()
            .flatten()
            .any(|spec| matches!(spec, WindowSpec::Latest));
            if rule.allow || latest {
                globs.insert(glob.raw().to_string());
            }
        }
    }
    globs.into_iter().collect()
}

/// The authority-first `max-major` result for a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaxMajorPick {
    /// The inclusive numeric major ceiling.
    pub limit: u64,
    /// The winning policy layer.
    pub origin: Origin,
    /// The winning selector within that layer.
    pub selector: Selector,
}

#[derive(Debug, Clone)]
struct IndexedMaxMajorPick {
    pick: MaxMajorPick,
    layer_index: usize,
    rule_index: usize,
    specificity: u8,
}

fn pick_max_major(layers: &[PolicyLayer], query: &ResolveQuery<'_>) -> Option<IndexedMaxMajorPick> {
    let mut best: Option<IndexedMaxMajorPick> = None;
    for (layer_index, layer) in layers.iter().enumerate() {
        for (rule_index, rule) in layer.rules.iter().enumerate() {
            let Some(limit) = rule.max_major else {
                continue;
            };
            if !rule.selector.matches(query) {
                continue;
            }
            let specificity = rule.selector.specificity();
            let better = best.as_ref().is_none_or(|best| {
                (layer_index, specificity) > (best.layer_index, best.specificity)
            });
            if better {
                best = Some(IndexedMaxMajorPick {
                    pick: MaxMajorPick {
                        limit,
                        origin: layer.origin.clone(),
                        selector: rule.selector.clone(),
                    },
                    layer_index,
                    rule_index,
                    specificity,
                });
            }
        }
    }
    best
}

/// Resolves the inclusive package `max-major` ceiling by layer authority, then specificity.
#[must_use]
pub fn resolve_max_major(layers: &[PolicyLayer], query: &ResolveQuery<'_>) -> Option<MaxMajorPick> {
    pick_max_major(layers, query).map(|pick| pick.pick)
}

fn trace_max_major(layers: &[PolicyLayer], query: &ResolveQuery<'_>, trace: &mut Vec<TraceStep>) {
    let winner = pick_max_major(layers, query);
    for (layer_index, layer) in layers.iter().enumerate() {
        for (rule_index, rule) in layer.rules.iter().enumerate() {
            let Some(limit) = rule.max_major else {
                continue;
            };
            if !rule.selector.matches(query) {
                continue;
            }
            let applied = winner.as_ref().is_some_and(|winner| {
                winner.layer_index == layer_index && winner.rule_index == rule_index
            });
            trace.push(TraceStep {
                layer: layer.origin.clone(),
                field: "max-major".to_string(),
                selector: Some(rule.selector.clone()),
                min_age_days: None,
                applied,
                note: if applied {
                    format!("selected inclusive major ceiling {limit}")
                } else {
                    format!("considered inclusive major ceiling {limit}")
                },
            });
        }
    }
}

/// Picks the authority-first window field for `query` and traces every rule considered.
///
/// `min-age` (and the per-kind windows) is authority-first: the highest layer that sets it wins,
/// tie-broken within the layer by selector specificity, with a per-kind fall-through to the bare
/// `default`. The built-in `Default` layer always sets `default = 7d`, so a pick effectively always
/// exists; if a caller omits that layer, a 7-day safety net is used.
fn pick_window(
    layers: &[PolicyLayer],
    query: &ResolveQuery<'_>,
    now: Timestamp,
    trace: &mut Vec<TraceStep>,
) -> FieldPick {
    let kind_pick = pick_field(layers, query, field_for_kind(query.kind));
    let used_fallthrough = kind_pick.is_none()
        && !matches!(
            query.kind,
            ResolveKind::CurrentPin | ResolveKind::EffectiveDefault
        );
    let pick = kind_pick
        .or_else(|| pick_field(layers, query, |by_kind| by_kind.default.as_ref()))
        .unwrap_or(FieldPick {
            layer_index: 0,
            specificity: 0,
            origin: Origin::Default,
            selector: Selector::Default,
            spec: WindowSpec::MinAge(SignedDuration::from_hours(24 * 7)),
        });

    // Trace every rule that set the resolved field, marking the winner.
    let resolved_field = if used_fallthrough {
        ResolveKind::CurrentPin
    } else {
        query.kind
    };
    for layer in layers {
        for rule in &layer.rules {
            if !rule.selector.matches(query) {
                continue;
            }
            if let Some(spec) = field_for_kind(resolved_field)(&rule.window) {
                let is_winner = layer.origin == pick.origin
                    && rule.selector == pick.selector
                    && *spec == pick.spec;
                trace.push(TraceStep {
                    layer: layer.origin.clone(),
                    field: field_name(resolved_field).to_string(),
                    selector: Some(rule.selector.clone()),
                    min_age_days: Some(min_age_days_of(spec, now)),
                    applied: is_winner,
                    note: if is_winner {
                        "selected (highest layer, most specific selector)".into()
                    } else {
                        "considered".into()
                    },
                });
            }
        }
    }
    if used_fallthrough {
        trace.push(TraceStep {
            layer: pick.origin.clone(),
            field: field_name(query.kind).to_string(),
            selector: None,
            min_age_days: None,
            applied: false,
            note: format!(
                "no rule set the `{}` window; fell through to the bare `min-age`",
                field_name(query.kind)
            ),
        });
    }
    pick
}

/// One matching `floor` rule: the floor duration plus the declaring layer, which decides whether
/// an `allow` can bypass it (only a same-layer or audited env/CLI allow can).
#[derive(Debug, Clone)]
struct FloorCandidate {
    /// Index of the declaring layer in the resolution stack.
    layer_index: usize,
    /// The floor's minimum-age duration.
    duration: SignedDuration,
    /// The declaring layer's origin, for attribution.
    origin: Origin,
    /// The rule's selector, so a trace step can name *which* of a layer's floors it means.
    selector: Selector,
}

/// Collects every matching `floor` rule, tracing each as a floor candidate.
fn collect_floor_candidates(
    layers: &[PolicyLayer],
    query: &ResolveQuery<'_>,
    trace: &mut Vec<TraceStep>,
) -> Vec<FloorCandidate> {
    let mut floors: Vec<FloorCandidate> = Vec::new();
    for (layer_index, layer) in layers.iter().enumerate() {
        for rule in &layer.rules {
            if !rule.selector.matches(query) {
                continue;
            }
            if let Some(floor) = rule.floor {
                trace.push(TraceStep {
                    layer: layer.origin.clone(),
                    field: "floor".into(),
                    selector: Some(rule.selector.clone()),
                    min_age_days: Some(duration_as_days(floor)),
                    applied: false,
                    note: "floor candidate".into(),
                });
                floors.push(FloorCandidate {
                    layer_index,
                    duration: floor,
                    origin: layer.origin.clone(),
                    selector: rule.selector.clone(),
                });
            }
        }
    }
    floors
}

/// The outcome of applying `allow` exemptions: whether any matched, the residual binding floor (if
/// any), and the provenance (highest-layer matching allow) used to attribute the decision.
struct AllowOutcome {
    matched: bool,
    effective_floor: Option<FloorCandidate>,
    /// Every floor the matched allows could *not* bypass
    /// ([`effective_floor`](Self::effective_floor) is their maximum).
    ///
    /// The advisory `bypass-floor` interaction resolves against this full list, again per
    /// candidate, so lifting one layer's floor never erases another layer's.
    residual_floors: Vec<FloorCandidate>,
    provenance: Option<(Origin, Selector)>,
}

/// Accumulates `allow` exemptions, resolves the residual binding floor, and traces each allow plus
/// the floor that survives.
///
/// The floor-bypass rule is the security-load-bearing part: an `allow` always zeroes an ordinary
/// window, but it bypasses a *floor* only when it is the audited invocation override (env/CLI) or it
/// is **co-declared in the same layer** as that floor. Crucially this is decided PER FLOOR, not
/// against a single max-clamped binding floor — so a repo `allow` co-declared with a repo floor
/// still cannot escape a *separate* org (global) floor in a different layer; that residual floor
/// remains and clamps the window.
fn resolve_allows(
    layers: &[PolicyLayer],
    query: &ResolveQuery<'_>,
    floors: &[FloorCandidate],
    trace: &mut Vec<TraceStep>,
) -> AllowOutcome {
    let mut allows: Vec<(usize, Origin, Selector)> = Vec::new();
    for (layer_index, layer) in layers.iter().enumerate() {
        for rule in &layer.rules {
            if rule.selector.matches(query) && rule.allow {
                allows.push((layer_index, layer.origin.clone(), rule.selector.clone()));
            }
        }
    }
    let allow_matched = !allows.is_empty();
    let has_env_cli_allow = allows
        .iter()
        .any(|(_, origin, _)| matches!(origin, Origin::Env | Origin::Cli));
    let allow_layers: std::collections::HashSet<usize> = allows
        .iter()
        .map(|(layer_index, ..)| *layer_index)
        .collect();

    // A floor is bypassed only by an allow in its own layer or an audited env/CLI allow.
    let bypassed = |floor_layer_index: usize| -> bool {
        allow_matched && (has_env_cli_allow || allow_layers.contains(&floor_layer_index))
    };
    let residual_floors: Vec<FloorCandidate> = floors
        .iter()
        .filter(|floor| !bypassed(floor.layer_index))
        .cloned()
        .collect();
    let effective_floor = residual_floors
        .iter()
        .max_by(|a, b| (a.duration, a.layer_index).cmp(&(b.duration, b.layer_index)))
        .cloned();

    for (layer_index, origin, selector) in &allows {
        let note = if has_env_cli_allow {
            "exemption applies (audited env/CLI override bypasses all floors)"
        } else {
            "exemption zeroes the window; floors in other layers still bind (residual)"
        };
        trace.push(TraceStep {
            layer: origin.clone(),
            field: "allow".into(),
            selector: Some(selector.clone()),
            min_age_days: Some(0.0),
            applied: true,
            note: format!("{note} [layer {layer_index}]"),
        });
    }

    if let Some(floor) = &effective_floor {
        trace.push(TraceStep {
            layer: floor.origin.clone(),
            field: "floor".into(),
            selector: None,
            min_age_days: Some(duration_as_days(floor.duration)),
            applied: true,
            note: if allow_matched {
                "residual floor (not bypassable by the matched allow)".into()
            } else {
                "binding floor (maximum across layers)".into()
            },
        });
    }

    let provenance = allows
        .iter()
        .max_by_key(|(layer_index, ..)| *layer_index)
        .map(|(_, origin, selector)| (origin.clone(), selector.clone()));
    AllowOutcome {
        matched: allow_matched,
        effective_floor,
        residual_floors,
        provenance,
    }
}

#[cfg(test)]
mod exempt_tests {
    use super::exempt_package_globs;
    use crate::{ByKind, Origin, PatternGlob, PolicyLayer, Rule, Selector, WindowSpec};
    use jiff::SignedDuration;

    fn package_rule(glob: &str, window: ByKind, allow: bool) -> Rule {
        let mut rule = Rule::new(Selector::Package {
            glob: PatternGlob::new(glob).expect("glob"),
            tool: None,
        });
        rule.window = window;
        rule.allow = allow;
        rule
    }

    #[test]
    fn collects_only_latest_and_allow_package_selectors() {
        let mut layer = PolicyLayer::new(Origin::Cli);
        layer.rules.push(package_rule(
            "@scope/latest-pkg",
            ByKind::scalar(WindowSpec::Latest),
            false,
        ));
        layer
            .rules
            .push(package_rule("allowed-pkg", ByKind::default(), true));
        // A `min-age` package is NOT exempt — it stays under the cooldown, so it must not be listed.
        layer.rules.push(package_rule(
            "min-age-pkg",
            ByKind::scalar(WindowSpec::MinAge(SignedDuration::from_hours(24 * 30))),
            false,
        ));
        // A non-`package` selector that is `latest` (e.g. the project default) is not a per-package
        // exemption and must not leak into the list.
        let mut default_latest = Rule::new(Selector::Default);
        default_latest.window = ByKind::scalar(WindowSpec::Latest);
        layer.rules.push(default_latest);

        // Sorted + deduplicated; only the package-scoped latest/allow selectors.
        assert_eq!(
            exempt_package_globs(&[layer], crate::model::ToolId("pnpm")),
            vec!["@scope/latest-pkg".to_string(), "allowed-pkg".to_string(),]
        );
    }

    #[test]
    fn no_exemptions_yields_an_empty_list() {
        let mut layer = PolicyLayer::new(Origin::Repo("cooldown.toml".into()));
        layer.rules.push(package_rule(
            "plain",
            ByKind::scalar(WindowSpec::MinAge(SignedDuration::from_hours(24 * 14))),
            false,
        ));
        assert!(exempt_package_globs(&[layer], crate::model::ToolId("pnpm")).is_empty());
    }

    #[test]
    fn tool_qualified_exemptions_do_not_leak_to_other_tools() {
        let mut layer = PolicyLayer::new(Origin::Repo("cooldown.toml".into()));
        let mut rule = Rule::new(Selector::Package {
            glob: PatternGlob::new("shared").expect("glob"),
            tool: Some(crate::model::ToolId("npm")),
        });
        rule.allow = true;
        layer.rules.push(rule);

        assert_eq!(
            exempt_package_globs(std::slice::from_ref(&layer), crate::model::ToolId("npm")),
            vec!["shared".to_string()]
        );
        assert!(exempt_package_globs(&[layer], crate::model::ToolId("pnpm")).is_empty());
    }
}
