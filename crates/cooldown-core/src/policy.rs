//! Policy types and the pure resolver.
//!
//! Two orthogonal axes decide a value: **layers** (where a value comes from, low→high authority)
//! and **selectors** (what it applies to, most→least specific). Resolution is *per field*, and
//! each field has its own combine rule:
//!
//! - `min-age` / per-kind windows — **authority-first**: the highest layer that sets the field
//!   wins; within a layer the most specific selector breaks the tie. Layer dominates selector.
//! - `floor` — **max-clamp**: the effective window clamps up to `max(floor)` over all layers.
//! - `allow` — **accumulated union** that zeroes an ordinary window, but bypasses a floor only
//!   per-floor: a floor is escaped only by an allow co-declared in that floor's own layer, or by an
//!   audited env/CLI allow. A floor in any other layer remains as a residual clamp — so a repo
//!   `allow` cannot undercut a separate org/global floor.
//! - `strict-native` — **security-monotone** OR across layers (handled on [`PolicyStack`]).

use crate::duration::{duration_as_days, since};
use crate::model::{EcosystemId, UpdateKind};
use camino::Utf8Path;
use jiff::{SignedDuration, Timestamp};
use std::fmt;

/// Where a policy value comes from, lowest → highest authority when compared by layer index in a
/// [`PolicyStack`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// The built-in `min-age = 7d` default.
    Default,
    /// The global user config (`~/.config/cooldown/config.toml`).
    Global,
    /// A native manifest config, normalised into the unified model.
    Native,
    /// A repo/project `cooldown.toml` at the given path (nearer wins within the cascade).
    Repo(camino::Utf8PathBuf),
    /// An explicit `--config` / `COOLDOWN_CONFIG` file (one shared top file layer).
    Config(camino::Utf8PathBuf),
    /// `COOLDOWN_*` environment variables.
    Env,
    /// CLI flags.
    Cli,
}

impl Origin {
    /// The stable token used in the JSON `minAgeSource` (`default|global|native|repo:<path>|…`).
    pub fn token(&self) -> String {
        match self {
            Origin::Default => "default".into(),
            Origin::Global => "global".into(),
            Origin::Native => "native".into(),
            Origin::Repo(p) => format!("repo:{p}"),
            Origin::Config(p) => format!("config:{p}"),
            Origin::Env => "env".into(),
            Origin::Cli => "cli".into(),
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.token())
    }
}

/// A compiled glob with its original pattern preserved for display/equality.
#[derive(Debug, Clone)]
pub struct PatternGlob {
    raw: String,
    matcher: globset::GlobMatcher,
}

impl PatternGlob {
    pub fn new(pattern: &str) -> Result<Self, crate::error::CoreError> {
        // `*` crosses `/` (literal_separator = false), so `github.com/acme/*` matches nested paths
        // and `@acme/*` matches a scope — the intended "everything under this prefix" semantics.
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map_err(|e| {
                crate::error::CoreError::Parse(format!("invalid glob {pattern:?}: {e}"))
            })?;
        Ok(PatternGlob {
            raw: pattern.to_string(),
            matcher: glob.compile_matcher(),
        })
    }

    pub fn is_match(&self, s: &str) -> bool {
        self.matcher.is_match(s)
    }

    pub fn raw(&self) -> &str {
        &self.raw
    }
}

impl PartialEq for PatternGlob {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl Eq for PatternGlob {}

/// What a rule applies to. Specificity: `Package` > `Registry` > `Project` > `Lang` > `Default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Default,
    Lang(EcosystemId),
    Registry(String),
    Project(PatternGlob),
    Package(PatternGlob),
}

impl Selector {
    /// Higher = more specific; breaks ties *within* a layer.
    pub fn specificity(&self) -> u8 {
        match self {
            Selector::Package(_) => 4,
            Selector::Registry(_) => 3,
            Selector::Project(_) => 2,
            Selector::Lang(_) => 1,
            Selector::Default => 0,
        }
    }

    /// Whether this selector applies to the queried package.
    pub fn matches(&self, q: &ResolveQuery<'_>) -> bool {
        match self {
            Selector::Default => true,
            Selector::Lang(e) => *e == q.ecosystem,
            Selector::Registry(r) => q.registry == Some(r.as_str()),
            Selector::Project(g) => g.is_match(q.project.as_str()),
            Selector::Package(g) => g.is_match(q.package),
        }
    }

    /// The `<selector>` half of the `minAgeSource` string, e.g. `package=left-pad`.
    pub fn token(&self) -> Option<String> {
        match self {
            Selector::Default => None,
            Selector::Lang(e) => Some(format!("lang={e}")),
            Selector::Registry(r) => Some(format!("registry={r}")),
            Selector::Project(g) => Some(format!("project={}", g.raw())),
            Selector::Package(g) => Some(format!("package={}", g.raw())),
        }
    }
}

/// A resolved window: a rolling minimum age, the explicit opt-out, or an absolute cutoff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowSpec {
    /// A rolling minimum release age.
    MinAge(SignedDuration),
    /// The explicit, audited opt-out (window = 0).
    Latest,
    /// An absolute cutoff instead of a rolling window (reproducible).
    Freeze(Timestamp),
}

impl WindowSpec {
    /// The cutoff instant *before* any floor clamp: a release published at or before it is mature.
    pub fn base_cutoff(&self, now: Timestamp) -> Timestamp {
        match self {
            WindowSpec::MinAge(d) => now - *d,
            WindowSpec::Latest => now,
            WindowSpec::Freeze(t) => *t,
        }
    }
}

/// Per-kind windows mapping the `min-age` table field-for-field. A fixed-field struct: no `Ord` on
/// `UpdateKind`, no heap alloc.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ByKind {
    pub default: Option<WindowSpec>,
    pub major: Option<WindowSpec>,
    pub minor: Option<WindowSpec>,
    pub patch: Option<WindowSpec>,
}

impl ByKind {
    pub fn scalar(spec: WindowSpec) -> Self {
        ByKind {
            default: Some(spec),
            ..Default::default()
        }
    }
}

/// A single config rule: a selector and the values it sets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub selector: Selector,
    pub window: ByKind,
    /// Whether this selector is exempted from the cooldown (an `allow` entry).
    pub allow: bool,
    /// A hard minimum window contributed by this rule (max-clamped across layers).
    pub floor: Option<SignedDuration>,
}

impl Rule {
    pub fn new(selector: Selector) -> Self {
        Rule {
            selector,
            window: ByKind::default(),
            allow: false,
            floor: None,
        }
    }
}

/// One layer of policy from a single origin.
#[derive(Debug, Clone)]
pub struct PolicyLayer {
    pub origin: Origin,
    pub rules: Vec<Rule>,
    /// A config layer may set `strict-native`; combined monotonically on the stack.
    pub strict_native: Option<bool>,
}

impl PolicyLayer {
    pub fn new(origin: Origin) -> Self {
        PolicyLayer {
            origin,
            rules: Vec::new(),
            strict_native: None,
        }
    }
}

/// The full layered policy for one project, plus the monotone-combined `strict_native`.
#[derive(Debug, Clone)]
pub struct PolicyStack {
    /// Low → high authority: `[Default, Global, Native, RepoCascade…, Config, Env, Cli]`.
    pub layers: Vec<PolicyLayer>,
    /// `true` if any layer set it (CLI `--no-fail-on-stricter-native` forces it off during loading).
    pub strict_native: bool,
}

/// Whether to resolve the bare window (the locked pin) or a per-kind candidate window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKind {
    /// The `check` gate: an already-locked version has no from→to kind, so it uses the bare
    /// `min-age`.
    CurrentPin,
    /// An `outdated`/`upgrade` candidate of the given kind.
    Candidate(UpdateKind),
}

/// A resolution query: which package, in which ecosystem/registry/project, for which kind.
#[derive(Debug, Clone, Copy)]
pub struct ResolveQuery<'a> {
    pub ecosystem: EcosystemId,
    pub package: &'a str,
    pub registry: Option<&'a str>,
    pub project: &'a Utf8Path,
    pub kind: ResolveKind,
}

/// One step of an `explain` derivation.
#[derive(Debug, Clone)]
pub struct TraceStep {
    pub layer: Origin,
    pub field: String,
    pub selector: Option<Selector>,
    pub min_age_days: Option<f64>,
    pub applied: bool,
    pub note: String,
}

/// The resolved window plus the field-by-field trace `explain` prints.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub window: ResolvedWindow,
    pub trace: Vec<TraceStep>,
}

/// The window selected for a package, with provenance and any floor clamp / exemption.
#[derive(Debug, Clone)]
pub struct ResolvedWindow {
    /// The selected window before any floor clamp.
    pub spec: WindowSpec,
    /// The layer+selector that decided the window (or the `allow` rule, when exempt).
    pub decided_by: Origin,
    pub decided_selector: Selector,
    /// The binding (maximum) floor across layers, if any.
    pub floor: Option<SignedDuration>,
    pub floor_origin: Option<Origin>,
    /// Whether an `allow` entry exempts this package (and bypasses any floor).
    pub exempt: bool,
    pub exempt_origin: Option<Origin>,
}

impl ResolvedWindow {
    /// The effective cutoff at `now`, applying the floor max-clamp. A release published at or
    /// before this instant is mature. An `allow` is reflected by `spec == Latest` (base = `now`),
    /// so a residual floor it could not bypass still clamps — the floor is never short-circuited.
    pub fn cutoff(&self, now: Timestamp) -> Timestamp {
        let base = self.spec.base_cutoff(now);
        match self.floor {
            // A longer floor pushes the cutoff *earlier* (stricter): keep the earliest.
            Some(floor) => base.min(now - floor),
            None => base,
        }
    }

    /// The origin of a floor that actually tightened the window at `now`, for the JSON `clampedBy`.
    pub fn clamped_by(&self, now: Timestamp) -> Option<&Origin> {
        let floor = self.floor?;
        let base = self.spec.base_cutoff(now);
        if (now - floor) < base {
            self.floor_origin.as_ref()
        } else {
            None
        }
    }

    /// The effective window as a float number of days, for display.
    pub fn effective_min_age_days(&self, now: Timestamp) -> f64 {
        let cutoff = self.cutoff(now);
        duration_as_days(since(now, cutoff))
    }

    /// The `minAgeSource` string: `<origin>` or `<origin>:<selector>`.
    pub fn source(&self) -> String {
        match self.decided_selector.token() {
            Some(sel) => format!("{}:{}", self.decided_by.token(), sel),
            None => self.decided_by.token(),
        }
    }
}

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
    for (li, layer) in layers.iter().enumerate() {
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
                Some(b) => (li, specificity) > (b.layer_index, b.specificity),
            };
            if better {
                best = Some(FieldPick {
                    layer_index: li,
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
        ResolveKind::CurrentPin => |bk| bk.default.as_ref(),
        ResolveKind::Candidate(UpdateKind::Major) => |bk| bk.major.as_ref(),
        ResolveKind::Candidate(UpdateKind::Minor) => |bk| bk.minor.as_ref(),
        ResolveKind::Candidate(UpdateKind::Patch) => |bk| bk.patch.as_ref(),
    }
}

fn field_name(kind: ResolveKind) -> &'static str {
    match kind {
        ResolveKind::CurrentPin => "default",
        ResolveKind::Candidate(UpdateKind::Major) => "major",
        ResolveKind::Candidate(UpdateKind::Minor) => "minor",
        ResolveKind::Candidate(UpdateKind::Patch) => "patch",
    }
}

fn min_age_days_of(spec: &WindowSpec, now: Timestamp) -> Option<f64> {
    match spec {
        WindowSpec::MinAge(d) => Some(duration_as_days(*d)),
        WindowSpec::Latest => Some(0.0),
        WindowSpec::Freeze(t) => Some(duration_as_days(since(now, *t))),
    }
}

/// Resolve the effective window for a query, with a full trace. Per-field combine:
/// authority-first `min-age`, max-clamp `floor`, union `allow` (floor-aware).
pub fn resolve(layers: &[PolicyLayer], query: &ResolveQuery<'_>, now: Timestamp) -> Resolution {
    let mut trace: Vec<TraceStep> = Vec::new();

    // --- min-age / per-kind window (authority-first, with per-kind fallthrough to bare default).
    let kind_pick = pick_field(layers, query, field_for_kind(query.kind));
    let used_fallthrough = kind_pick.is_none() && query.kind != ResolveKind::CurrentPin;
    let pick = kind_pick.or_else(|| pick_field(layers, query, |bk| bk.default.as_ref()));

    // The built-in Default layer always sets `default = 7d`, so `pick` is effectively always Some;
    // fall back to a 7d safety net if a caller passes layers without it.
    let pick = pick.unwrap_or(FieldPick {
        layer_index: 0,
        specificity: 0,
        origin: Origin::Default,
        selector: Selector::Default,
        spec: WindowSpec::MinAge(SignedDuration::from_hours(24 * 7)),
    });

    // Trace every rule that set the resolved field, marking the winner.
    let resolved_field = if used_fallthrough {
        ResolveKind::CurrentPin // i.e. the `default` field
    } else {
        query.kind
    };
    for layer in layers.iter() {
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
                    min_age_days: min_age_days_of(spec, now),
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

    // --- floors: every matching floor rule, with its declaring layer.
    let mut floors: Vec<(usize, SignedDuration, Origin)> = Vec::new();
    for (li, layer) in layers.iter().enumerate() {
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
                floors.push((li, floor, layer.origin.clone()));
            }
        }
    }

    // --- allows (accumulated union). The floor-bypass rule is the security-load-bearing part: an
    // `allow` always zeroes an ordinary window, but it bypasses a *floor* only when it is the
    // audited invocation override (env/CLI) or it is **co-declared in the same layer** as that
    // floor. Crucially this is decided PER FLOOR, not against a single max-clamped binding floor —
    // so a repo `allow` co-declared with a repo floor still cannot escape a *separate* org (global)
    // floor in a different layer. That residual floor remains and clamps the window.
    let mut allows: Vec<(usize, Origin, Selector)> = Vec::new();
    for (li, layer) in layers.iter().enumerate() {
        for rule in &layer.rules {
            if rule.selector.matches(query) && rule.allow {
                allows.push((li, layer.origin.clone(), rule.selector.clone()));
            }
        }
    }
    let allow_matched = !allows.is_empty();
    let has_env_cli_allow = allows
        .iter()
        .any(|(_, o, _)| matches!(o, Origin::Env | Origin::Cli));
    let allow_layers: std::collections::HashSet<usize> =
        allows.iter().map(|(li, ..)| *li).collect();

    // A floor is bypassed only by an allow in its own layer or an audited env/CLI allow.
    let bypassed = |floor_li: usize| -> bool {
        allow_matched && (has_env_cli_allow || allow_layers.contains(&floor_li))
    };
    let effective_floor = floors
        .iter()
        .filter(|(fl, ..)| !bypassed(*fl))
        .max_by(|a, b| (a.1, a.0).cmp(&(b.1, b.0)))
        .cloned();

    for (li, origin, selector) in &allows {
        let note = if !allow_matched {
            unreachable!()
        } else if has_env_cli_allow {
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
            note: format!("{note} [layer {li}]"),
        });
    }

    if let Some((_, d, o)) = &effective_floor {
        trace.push(TraceStep {
            layer: o.clone(),
            field: "floor".into(),
            selector: None,
            min_age_days: Some(duration_as_days(*d)),
            applied: true,
            note: if allow_matched {
                "residual floor (not bypassable by the matched allow)".into()
            } else {
                "binding floor (maximum across layers)".into()
            },
        });
    }

    // An `allow` reflects as `spec = Latest` (base cutoff = now); a residual floor it could not
    // bypass still clamps. Fully exempt only when no residual floor remains.
    let spec = if allow_matched {
        WindowSpec::Latest
    } else {
        pick.spec.clone()
    };
    let exempt = allow_matched && effective_floor.is_none();
    let (floor_dur, floor_origin) = match &effective_floor {
        Some((_, d, o)) => (Some(*d), Some(o.clone())),
        None => (None, None),
    };
    // Provenance: when an allow applied, point at the highest-layer matching allow; else the pick.
    let allow_provenance = allows.iter().max_by_key(|(li, ..)| *li);
    let (decided_by, decided_selector, exempt_origin) = match allow_provenance {
        Some((_, o, sel)) => (o.clone(), sel.clone(), Some(o.clone())),
        None => (pick.origin.clone(), pick.selector.clone(), None),
    };

    let window = ResolvedWindow {
        spec,
        decided_by,
        decided_selector,
        floor: floor_dur,
        floor_origin,
        exempt,
        exempt_origin,
    };

    Resolution { window, trace }
}
