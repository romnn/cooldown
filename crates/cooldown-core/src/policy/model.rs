use crate::duration::{duration_as_days, since};
use crate::model::{ToolId, UpdateKind};
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
            Origin::Repo(path) => format!("repo:{path}"),
            Origin::Config(path) => format!("config:{path}"),
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
            .map_err(|error| {
                crate::error::CoreError::Config(format!("invalid glob {pattern:?}: {error}"))
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

/// What a rule applies to. Specificity: `Package` > `Registry` > `Project` > `Tool` > `Default`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// Matches every package; the catch-all with the lowest specificity.
    Default,
    /// Matches every package managed by one tool/tool (e.g. all Cargo dependencies).
    Tool(ToolId),
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
            Selector::Tool(_) => 1,
            Selector::Default => 0,
        }
    }

    /// Whether this selector applies to the queried package.
    #[must_use]
    pub fn matches(&self, query: &ResolveQuery<'_>) -> bool {
        match self {
            Selector::Default => true,
            Selector::Tool(tool) => *tool == query.tool,
            Selector::Registry(registry) => query.registry == Some(registry.as_str()),
            Selector::Project(glob) => glob.is_match(query.project.as_str()),
            Selector::Package(glob) => {
                !matches!(query.kind, ResolveKind::EffectiveDefault) && glob.is_match(query.package)
            }
        }
    }

    /// The `<selector>` half of the `minAgeSource` string, e.g. `package=left-pad`.
    #[must_use]
    pub fn token(&self) -> Option<String> {
        match self {
            Selector::Default => None,
            Selector::Tool(tool) => Some(format!("tool={tool}")),
            Selector::Registry(registry) => Some(format!("registry={registry}")),
            Selector::Project(glob) => Some(format!("project={}", glob.raw())),
            Selector::Package(glob) => Some(format!("package={}", glob.raw())),
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
            WindowSpec::MinAge(duration) => now - *duration,
            WindowSpec::Latest => now,
            WindowSpec::Freeze(timestamp) => *timestamp,
        }
    }
}

/// Formats a [`WindowSpec`] as a tool-facing `exclude-newer`-style cutoff value, or `None` when the
/// spec excludes nothing.
///
/// This is the single renderer for the resolver cutoff cooldown hands to publish-time-aware tools
/// (uv's `--exclude-newer` / native `uv.toml`): both the per-project resolution window and `sync`'s
/// native write go through it, so they never disagree. An age becomes a *relative* span ("14 days",
/// "36 hours", "90 seconds"; singular "1 day") so the tool re-evaluates it against the current "now"
/// on each check and a re-check stays stable across runs — an absolute cutoff would drift every run
/// and report the lock perpetually stale. A freeze becomes its absolute RFC3339 instant. A
/// zero/negative age and [`WindowSpec::Latest`] exclude nothing, so they map to `None`.
///
/// # Examples
///
/// ```
/// use cooldown_core::{WindowSpec, window_exclude_newer};
/// use jiff::SignedDuration;
///
/// assert_eq!(
///     window_exclude_newer(&WindowSpec::MinAge(SignedDuration::from_hours(24 * 14))).as_deref(),
///     Some("14 days"),
/// );
/// assert_eq!(window_exclude_newer(&WindowSpec::Latest), None);
/// ```
#[must_use]
pub fn window_exclude_newer(spec: &WindowSpec) -> Option<String> {
    const SECS_PER_DAY: i64 = 86_400;
    const SECS_PER_HOUR: i64 = 3_600;
    match spec {
        WindowSpec::MinAge(duration) => {
            let secs = duration.as_secs();
            if secs <= 0 {
                return None;
            }
            let (count, unit) = if secs % SECS_PER_DAY == 0 {
                (secs / SECS_PER_DAY, "day")
            } else if secs % SECS_PER_HOUR == 0 {
                (secs / SECS_PER_HOUR, "hour")
            } else {
                (secs, "second")
            };
            Some(if count == 1 {
                format!("1 {unit}")
            } else {
                format!("{count} {unit}s")
            })
        }
        WindowSpec::Freeze(timestamp) => Some(timestamp.to_string()),
        WindowSpec::Latest => None,
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

/// Whether to resolve the bare window (the locked pin), the effective project default, or a
/// per-kind candidate window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolveKind {
    /// The `check` gate: an already-locked version has no from→to kind, so it uses the bare
    /// `min-age`.
    CurrentPin,
    /// The effective project default window, excluding package selectors.
    ///
    /// Used by `config` to report the policy a project defaults to before a package- or
    /// registry-specific selector narrows it further.
    EffectiveDefault,
    /// An `outdated`/`upgrade` candidate of the given kind.
    Candidate(UpdateKind),
}

/// A resolution query: which package, in which tool/registry/project, for which kind.
#[derive(Debug, Clone, Copy)]
pub struct ResolveQuery<'a> {
    /// The tool the package belongs to.
    pub tool: ToolId,
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

    /// The window's spec with any binding floor folded in, mirroring [`cutoff`](Self::cutoff)'s clamp.
    ///
    /// A longer floor pushes the cutoff earlier (stricter), so the returned spec is never weaker than
    /// cooldown's own gate: a `MinAge` rises to the longer of its age and the floor; a `Latest`
    /// (window = 0) becomes `MinAge(floor)` when a floor still binds; a `Freeze` becomes `MinAge(floor)`
    /// only when the floor's relative cutoff lands earlier than the freeze date, otherwise the absolute
    /// freeze stands. With no binding floor the spec is returned unchanged. This is the spec a
    /// resolver cutoff or native `exclude-newer` is rendered from, so a floor-protected window is never
    /// persisted weaker than [`cutoff`](Self::cutoff) enforces.
    #[must_use]
    pub fn effective_spec(&self, now: Timestamp) -> WindowSpec {
        let Some(floor) = self.floor else {
            return self.spec.clone();
        };
        match &self.spec {
            WindowSpec::MinAge(duration) => WindowSpec::MinAge((*duration).max(floor)),
            WindowSpec::Latest => WindowSpec::MinAge(floor),
            WindowSpec::Freeze(instant) if (now - floor) < *instant => WindowSpec::MinAge(floor),
            WindowSpec::Freeze(instant) => WindowSpec::Freeze(*instant),
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
            Some(selector) => format!("{}:{selector}", self.decided_by.token()),
            None => self.decided_by.token(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WindowSpec, window_exclude_newer};
    use jiff::{SignedDuration, Timestamp};

    #[test]
    fn window_exclude_newer_renders_relative_spans() {
        assert_eq!(
            window_exclude_newer(&WindowSpec::MinAge(SignedDuration::from_hours(24 * 14)))
                .as_deref(),
            Some("14 days")
        );
        assert_eq!(
            window_exclude_newer(&WindowSpec::MinAge(SignedDuration::from_hours(36))).as_deref(),
            Some("36 hours")
        );
        assert_eq!(
            window_exclude_newer(&WindowSpec::MinAge(SignedDuration::from_secs(90))).as_deref(),
            Some("90 seconds")
        );
    }

    #[test]
    fn window_exclude_newer_uses_singular_for_one() {
        assert_eq!(
            window_exclude_newer(&WindowSpec::MinAge(SignedDuration::from_hours(24))).as_deref(),
            Some("1 day")
        );
    }

    #[test]
    fn window_exclude_newer_maps_empty_windows_to_none() {
        assert_eq!(
            window_exclude_newer(&WindowSpec::MinAge(SignedDuration::ZERO)),
            None
        );
        assert_eq!(window_exclude_newer(&WindowSpec::Latest), None);
    }

    #[test]
    fn window_exclude_newer_renders_freeze_as_rfc3339() {
        let instant: Timestamp = "2026-06-01T00:00:00Z".parse().expect("timestamp");
        assert_eq!(
            window_exclude_newer(&WindowSpec::Freeze(instant)).as_deref(),
            Some("2026-06-01T00:00:00Z")
        );
    }
}
