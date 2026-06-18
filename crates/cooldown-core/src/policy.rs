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
    #[must_use]
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
    /// Compiles a glob `pattern`, preserving the original text for display and equality.
    ///
    /// `*` is allowed to cross `/` (the separator is not treated as literal), so a prefix
    /// pattern such as `github.com/acme/*` matches nested paths and `@acme/*` matches a whole
    /// scope — the intended "everything under this prefix" semantics.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`](crate::CoreError::Config) when `pattern` is not a valid glob.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::PatternGlob;
    ///
    /// let glob = PatternGlob::new("github.com/acme/*")?;
    /// assert!(glob.is_match("github.com/acme/widget"));
    /// assert!(!glob.is_match("github.com/other/widget"));
    /// # Ok::<(), cooldown_core::CoreError>(())
    /// ```
    pub fn new(pattern: &str) -> Result<Self, crate::error::CoreError> {
        // `*` crosses `/` (literal_separator = false), so `github.com/acme/*` matches nested paths
        // and `@acme/*` matches a scope — the intended "everything under this prefix" semantics.
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(false)
            .build()
            .map_err(|e| {
                crate::error::CoreError::Config(format!("invalid glob {pattern:?}: {e}"))
            })?;
        Ok(PatternGlob {
            raw: pattern.to_string(),
            matcher: glob.compile_matcher(),
        })
    }

    /// Returns whether `s` matches the compiled glob.
    #[must_use]
    pub fn is_match(&self, s: &str) -> bool {
        self.matcher.is_match(s)
    }

    /// Returns the original pattern text the glob was compiled from.
    #[must_use]
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
    /// Matches every package; the catch-all with the lowest specificity.
    Default,
    /// Matches every package in one ecosystem (e.g. all Cargo dependencies).
    Lang(EcosystemId),
    /// Matches packages served by a specific registry, by registry identifier.
    Registry(String),
    /// Matches packages whose project path matches the [`PatternGlob`].
    Project(PatternGlob),
    /// Matches packages whose name matches the [`PatternGlob`]; the most specific selector.
    Package(PatternGlob),
}

impl Selector {
    /// Higher = more specific; breaks ties *within* a layer.
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
    #[must_use]
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
    /// The bare `min-age` window, used as the per-kind fallback and for an already-locked pin.
    pub default: Option<WindowSpec>,
    /// The window for major-version updates, when set.
    pub major: Option<WindowSpec>,
    /// The window for minor-version updates, when set.
    pub minor: Option<WindowSpec>,
    /// The window for patch-version updates, when set.
    pub patch: Option<WindowSpec>,
}

impl ByKind {
    /// Builds a [`ByKind`] that sets only the bare [`default`](ByKind::default) window to `spec`.
    ///
    /// This is the common case where a rule declares a single scalar `min-age` rather than a
    /// per-kind table.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::{ByKind, WindowSpec};
    /// use jiff::SignedDuration;
    ///
    /// let window = ByKind::scalar(WindowSpec::MinAge(SignedDuration::from_hours(24 * 7)));
    /// assert!(window.default.is_some());
    /// assert!(window.major.is_none());
    /// ```
    #[must_use]
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
    /// What the rule applies to.
    pub selector: Selector,
    /// The per-kind windows this rule sets.
    pub window: ByKind,
    /// Whether this selector is exempted from the cooldown (an `allow` entry).
    pub allow: bool,
    /// A hard minimum window contributed by this rule (max-clamped across layers).
    pub floor: Option<SignedDuration>,
}

impl Rule {
    /// Builds an empty rule for `selector`: no windows, not exempt, no floor.
    ///
    /// Set [`window`](Rule::window), [`allow`](Rule::allow), and [`floor`](Rule::floor) on the
    /// returned value to declare the policy this selector contributes.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::{ByKind, Rule, Selector, WindowSpec};
    /// use jiff::SignedDuration;
    ///
    /// let mut rule = Rule::new(Selector::Default);
    /// rule.window = ByKind::scalar(WindowSpec::MinAge(SignedDuration::from_hours(24 * 14)));
    /// assert!(!rule.allow);
    /// ```
    #[must_use]
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
    /// Where this layer's policy came from, which fixes its authority in a [`PolicyStack`].
    pub origin: Origin,
    /// The rules this layer contributes, in declaration order.
    pub rules: Vec<Rule>,
    /// A config layer may set `strict-native`; combined monotonically on the stack.
    pub strict_native: Option<bool>,
}

impl PolicyLayer {
    /// Builds an empty layer for `origin`: no rules and no `strict-native` setting.
    ///
    /// Push [`Rule`]s onto [`rules`](PolicyLayer::rules) to populate the layer.
    ///
    /// # Examples
    ///
    /// ```
    /// use cooldown_core::{Origin, PolicyLayer, Rule, Selector};
    ///
    /// let mut layer = PolicyLayer::new(Origin::Cli);
    /// layer.rules.push(Rule::new(Selector::Default));
    /// assert_eq!(layer.origin, Origin::Cli);
    /// ```
    #[must_use]
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
    /// The ecosystem the package belongs to.
    pub ecosystem: EcosystemId,
    /// The package name, matched against [`Selector::Package`] globs.
    pub package: &'a str,
    /// The registry serving the package, if known; matched against [`Selector::Registry`].
    pub registry: Option<&'a str>,
    /// The project path, matched against [`Selector::Project`] globs.
    pub project: &'a Utf8Path,
    /// Which window to resolve: the bare pin or a per-kind candidate.
    pub kind: ResolveKind,
}

/// One step of an `explain` derivation.
#[derive(Debug, Clone)]
pub struct TraceStep {
    /// The layer that contributed this step.
    pub layer: Origin,
    /// The field being resolved (e.g. `default`, `major`, `floor`, `allow`).
    pub field: String,
    /// The selector of the rule this step came from, if any.
    pub selector: Option<Selector>,
    /// The step's window expressed as a number of days, when applicable.
    pub min_age_days: Option<f64>,
    /// Whether this step won (was applied) rather than merely considered.
    pub applied: bool,
    /// A human-readable explanation of why the step was considered or applied.
    pub note: String,
}

/// The resolved window plus the field-by-field trace `explain` prints.
#[derive(Debug, Clone)]
pub struct Resolution {
    /// The window selected for the query.
    pub window: ResolvedWindow,
    /// The field-by-field derivation, in the order `explain` prints it.
    pub trace: Vec<TraceStep>,
}

/// The window selected for a package, with provenance and any floor clamp / exemption.
#[derive(Debug, Clone)]
pub struct ResolvedWindow {
    /// The selected window before any floor clamp.
    pub spec: WindowSpec,
    /// The layer+selector that decided the window (or the `allow` rule, when exempt).
    pub decided_by: Origin,
    /// The selector of the rule that decided the window, paired with [`decided_by`](Self::decided_by).
    pub decided_selector: Selector,
    /// The binding (maximum) floor across layers, if any.
    pub floor: Option<SignedDuration>,
    /// The origin of the binding [`floor`](Self::floor), if one applies.
    pub floor_origin: Option<Origin>,
    /// Whether an `allow` entry exempts this package (and bypasses any floor).
    pub exempt: bool,
    /// The origin of the `allow` entry that granted the exemption, if [`exempt`](Self::exempt).
    pub exempt_origin: Option<Origin>,
}

impl ResolvedWindow {
    /// The effective cutoff at `now`, applying the floor max-clamp. A release published at or
    /// before this instant is mature. An `allow` is reflected by `spec == Latest` (base = `now`),
    /// so a residual floor it could not bypass still clamps — the floor is never short-circuited.
    #[must_use]
    pub fn cutoff(&self, now: Timestamp) -> Timestamp {
        let base = self.spec.base_cutoff(now);
        match self.floor {
            // A longer floor pushes the cutoff *earlier* (stricter): keep the earliest.
            Some(floor) => base.min(now - floor),
            None => base,
        }
    }

    /// The origin of a floor that actually tightened the window at `now`, for the JSON `clampedBy`.
    #[must_use]
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
    #[must_use]
    pub fn effective_min_age_days(&self, now: Timestamp) -> f64 {
        let cutoff = self.cutoff(now);
        duration_as_days(since(now, cutoff))
    }

    /// The `minAgeSource` string: `<origin>` or `<origin>:<selector>`.
    #[must_use]
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

fn min_age_days_of(spec: &WindowSpec, now: Timestamp) -> f64 {
    match spec {
        WindowSpec::MinAge(d) => duration_as_days(*d),
        WindowSpec::Latest => 0.0,
        WindowSpec::Freeze(t) => duration_as_days(since(now, *t)),
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
///     ByKind, EcosystemId, Origin, PolicyLayer, ResolveKind, ResolveQuery, Rule, Selector,
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
///     ecosystem: EcosystemId("rust"),
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
    let (floor_dur, floor_origin) = match &allow.effective_floor {
        Some((_, d, o)) => (Some(*d), Some(o.clone())),
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
        floor: floor_dur,
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
    let used_fallthrough = kind_pick.is_none() && query.kind != ResolveKind::CurrentPin;
    let pick = kind_pick
        .or_else(|| pick_field(layers, query, |bk| bk.default.as_ref()))
        .unwrap_or(FieldPick {
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

    let provenance = allows
        .iter()
        .max_by_key(|(li, ..)| *li)
        .map(|(_, o, sel)| (o.clone(), sel.clone()));
    AllowOutcome {
        matched: allow_matched,
        effective_floor,
        provenance,
    }
}
