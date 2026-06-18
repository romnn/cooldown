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
/// selector specificity, with a per-kind fall-through to the bare `default`; `floor` is
/// **max-clamped** across layers; and `allow` is a floor-aware **union** that zeroes an ordinary
/// window but bypasses a floor only when it is co-declared in that floor's layer or is an audited
/// env/CLI override. The returned [`Resolution::trace`] records every rule considered and which one
/// applied.
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
        Some((_, duration, origin)) => (Some(*duration), Some(origin.clone())),
        None => (None, None),
    };
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
    };

    Resolution { window, trace }
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

/// Collects every matching `floor` rule (with its declaring layer index and origin), tracing each
/// as a floor candidate.
fn collect_floor_candidates(
    layers: &[PolicyLayer],
    query: &ResolveQuery<'_>,
    trace: &mut Vec<TraceStep>,
) -> Vec<(usize, SignedDuration, Origin)> {
    let mut floors: Vec<(usize, SignedDuration, Origin)> = Vec::new();
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
                floors.push((layer_index, floor, layer.origin.clone()));
            }
        }
    }
    floors
}

/// The outcome of applying `allow` exemptions: whether any matched, the residual binding floor (if
/// any), and the provenance (highest-layer matching allow) used to attribute the decision.
struct AllowOutcome {
    matched: bool,
    effective_floor: Option<(usize, SignedDuration, Origin)>,
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
    floors: &[(usize, SignedDuration, Origin)],
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
    let effective_floor = floors
        .iter()
        .filter(|(floor_layer_index, ..)| !bypassed(*floor_layer_index))
        .max_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)))
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

    if let Some((_, duration, origin)) = &effective_floor {
        trace.push(TraceStep {
            layer: origin.clone(),
            field: "floor".into(),
            selector: None,
            min_age_days: Some(duration_as_days(*duration)),
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
        provenance,
    }
}
