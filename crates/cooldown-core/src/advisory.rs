//! The advisory feed: the security-relevant signal age alone cannot carry.
//!
//! A cooldown knows how *old* a version is and nothing about whether it is *security-relevant*,
//! so a candidate that fixes a known vulnerability waits out its window as a silent hold, and a
//! pin adopted from a security bump fails the very next `check`.
//! This module supplies the one missing input: advisories from a feed (OSV), matched against
//! the dependency's releases in the core.
//!
//! The feed only ever **annotates** a row (`mode = "flag"`, the default) or **shortens** its
//! window (`mode = "shorten"`, opt-in) — it never fails `check` because a pin is vulnerable.
//! Vulnerability gating stays with `govulncheck`/`cargo audit`; duplicating it here would make
//! exit 1 ambiguous.
//!
//! Versions stay opaque to the core: an [`AdvisorySource`] yields [`RawAdvisory`] values
//! carrying boundary version *strings*, and [`classify_advisory`] maps each boundary to a
//! [`ReleaseOrder`] token using the same release list the tool already ordered.
//! Membership is then a pure order comparison — no new place learns PEP 440, semver, and
//! `+incompatible`.
//! A boundary that cannot be ordered drops its *range*: range membership then never testifies,
//! and only exact-match evidence (enumerated affected versions, fix versions) remains — never a
//! silent pass.

use crate::error::Result;
use crate::model::{Release, ReleaseOrder, Version};
use crate::policy::{Origin, PolicyLayer, ResolvedWindow, Selector, TraceStep, WindowSpec};
use async_trait::async_trait;
use jiff::{SignedDuration, Timestamp};
use std::fmt;

/// A stable advisory identifier: `GHSA-…`, `CVE-…`, `RUSTSEC-…`, `GO-…`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct AdvisoryId(
    /// The verbatim identifier as the source reported it.
    pub String,
);

impl AdvisoryId {
    /// Returns the identifier string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AdvisoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// An advisory source identifier, registered by its adapter (e.g. `"osv"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AdvisorySourceId(
    /// The stable source name, e.g. `"osv"`.
    pub &'static str,
);

impl AdvisorySourceId {
    /// Returns the source's stable name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        self.0
    }
}

impl fmt::Display for AdvisorySourceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// A normalized advisory severity.
///
/// `Unknown` orders *below* every real rating, so an advisory with no rating never meets a
/// configured threshold — it annotates but never shortens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdvisorySeverity {
    /// The source reported no usable rating; never meets a threshold.
    Unknown,
    /// A low-severity rating.
    Low,
    /// A moderate/medium-severity rating.
    Moderate,
    /// A high-severity rating.
    High,
    /// A critical-severity rating.
    Critical,
}

impl AdvisorySeverity {
    /// Parses a severity token (case-insensitive; `medium` is accepted as `moderate`).
    ///
    /// Returns `None` for an unrecognised token so config validation can reject it; source
    /// adapters map unrecognised upstream ratings to [`Unknown`](AdvisorySeverity::Unknown)
    /// instead.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token.to_ascii_lowercase().as_str() {
            "low" => Some(AdvisorySeverity::Low),
            "moderate" | "medium" => Some(AdvisorySeverity::Moderate),
            "high" => Some(AdvisorySeverity::High),
            "critical" => Some(AdvisorySeverity::Critical),
            _ => None,
        }
    }

    /// The stable lowercase token this severity renders as.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AdvisorySeverity::Unknown => "unknown",
            AdvisorySeverity::Low => "low",
            AdvisorySeverity::Moderate => "moderate",
            AdvisorySeverity::High => "high",
            AdvisorySeverity::Critical => "critical",
        }
    }
}

impl fmt::Display for AdvisorySeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Which advisory feed the policy consults (`[advisories] source`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisorySourceKind {
    /// The OSV database (<https://osv.dev>) — the default.
    Osv,
    /// The GitHub Advisory Database.
    ///
    /// Parsed for forward compatibility; no source implements it yet, so selecting it yields no
    /// advisories — reported like any other unusable feed, never silently.
    Github,
    /// Annotate nothing — the explicit spelling for "no feed", distinct from an unreachable
    /// one.
    None,
}

impl AdvisorySourceKind {
    /// Parses a source token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "osv" => Some(AdvisorySourceKind::Osv),
            "github" => Some(AdvisorySourceKind::Github),
            "none" => Some(AdvisorySourceKind::None),
            _ => None,
        }
    }
}

/// What an advisory match does to a row (`[advisories] mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvisoryMode {
    /// Annotate only: verdicts unchanged, rows labeled (the safe default).
    Flag,
    /// Additionally resolve advisory-relevant rows against the security window (`[advisories]
    /// min-age`), which can only ever *shorten* the ordinary window.
    Shorten,
}

impl AdvisoryMode {
    /// Parses a mode token.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "flag" => Some(AdvisoryMode::Flag),
            "shorten" => Some(AdvisoryMode::Shorten),
            _ => None,
        }
    }
}

/// One event of an affected range, with its version still an opaque string.
///
/// The variants mirror the OSV `ranges[].events` keys.
/// A range's events are a *sequence*, not a set: an `Introduced` opens a span and the next
/// `Fixed`/`LastAffected` closes it, so the same version can appear in several events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawRangeEvent {
    /// Opens a span at the first affected version.
    Introduced(String),
    /// Closes the open span *below* this version (an exclusive upper bound).
    Fixed(String),
    /// Closes the open span *at* this version (an inclusive upper bound), for sources without a
    /// fix boundary.
    LastAffected(String),
    /// A `limit` boundary constraining every span of the range; a version containing `*`
    /// means unlimited.
    Limit(String),
}

/// One affected version range as the source reported it: the raw event list, unsorted.
///
/// The events are kept raw rather than pre-assembled into spans because assembling them
/// correctly needs a version *ordering*, which only exists once the boundaries are mapped onto a
/// release list — see [`classify_advisory`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RawAffectedRange {
    /// The range's events, in the order the source listed them.
    pub events: Vec<RawRangeEvent>,
}

/// An advisory as an [`AdvisorySource`] reports it: version boundaries still opaque strings,
/// not yet mapped onto a release list.
#[derive(Debug, Clone)]
pub struct RawAdvisory {
    /// The source's primary identifier (`GHSA-…`, `GO-…`, …).
    pub id: String,
    /// Cross-source aliases (`CVE-…`, `RUSTSEC-…`), the dedup key between feeds.
    pub aliases: Vec<String>,
    /// The normalized severity, [`Unknown`](AdvisorySeverity::Unknown) when the source has
    /// none.
    pub severity: AdvisorySeverity,
    /// Whether the source has withdrawn the advisory; a withdrawn advisory never flags or
    /// shortens.
    pub withdrawn: bool,
    /// A one-line human summary.
    pub summary: String,
    /// The affected version ranges, boundaries as version strings.
    pub ranges: Vec<RawAffectedRange>,
    /// Enumerated affected versions (exact matches), for sources that list them.
    pub affected_versions: Vec<String>,
    /// The versions that fix the advisory (the `fixed` boundaries), for display and the
    /// `check`-side "is this pin the fix?" test.
    pub fixes: Vec<String>,
}

/// The advisories affecting one package, as returned by an [`AdvisorySource`].
#[derive(Debug, Clone)]
pub struct PackageAdvisories {
    /// The package name exactly as queried.
    pub package: String,
    /// The advisories the source lists for it.
    pub advisories: Vec<RawAdvisory>,
}

/// One batched advisory fetch: the per-package advisories plus whether any of it was served
/// from a stale cache.
///
/// Stale advisory data may still annotate, but must never shorten a window — the feed is a
/// *loosening* input, so the trust rule is the inverse of the registry's monotonic floor.
#[derive(Debug, Clone)]
pub struct AdvisoryFetch {
    /// The advisories per queried package (packages with none are omitted).
    pub packages: Vec<PackageAdvisories>,
    /// Whether any part of the response was served from a cache past its TTL.
    pub stale: bool,
}

/// The advisory-feed port.
///
/// Batched deliberately: the whole package set is queried at once (OSV `querybatch`, plus one
/// document fetch per distinct advisory it names), never per dependency.
///
/// Failure is **fail-open on the window, loud in the output**: an unreachable feed can only
/// fail to shorten a window — the ordinary, stricter window stands — so the application
/// surfaces the error as a warning (or an error under `--fail-on-advisory-source`), never as a
/// false verdict.
#[async_trait]
pub trait AdvisorySource: Send + Sync {
    /// Returns the stable identifier of this source (e.g. `osv`).
    fn id(&self) -> AdvisorySourceId;

    /// Returns the advisories affecting `packages` within the advisory-database `ecosystem`
    /// (`Go`, `crates.io`, `PyPI`, `npm`, …; see
    /// [`Capabilities::advisory_ecosystem`](crate::Capabilities::advisory_ecosystem)).
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`](crate::CoreError) when the feed is unreachable or its response
    /// cannot be parsed.
    /// Callers treat this fail-open: windows stay un-shortened.
    async fn advisories(&self, ecosystem: &str, packages: &[String]) -> Result<AdvisoryFetch>;
}

/// One affected range with its boundaries mapped onto [`ReleaseOrder`] tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AffectedRange {
    /// The order of the first affected release, or `None` for "from the beginning".
    pub introduced: Option<ReleaseOrder>,
    /// The order of the first fixed release (exclusive upper bound).
    pub fixed: Option<ReleaseOrder>,
    /// The order of the last affected release (inclusive upper bound).
    pub last_affected: Option<ReleaseOrder>,
    /// `limit` boundaries: an order must be before *at least one* of them to be affected.
    ///
    /// Empty means unlimited — either the range declared no limit, or one of them was the `*`
    /// wildcard, which removes the ceiling however many finite limits sit beside it.
    pub limits: Vec<ReleaseOrder>,
}

impl AffectedRange {
    fn contains(&self, order: &ReleaseOrder) -> bool {
        // The OSV `BeforeLimits` rule: limits describe alternative branches, so being before any
        // one of them keeps the order in range; only being at or past *all* of them is out.
        if !self.limits.is_empty() && self.limits.iter().all(|limit| order >= limit) {
            return false;
        }
        if self.introduced.as_ref().is_some_and(|intro| order < intro) {
            return false;
        }
        if let Some(fixed) = &self.fixed {
            return order < fixed;
        }
        if let Some(last) = &self.last_affected {
            return order <= last;
        }
        // Open above: affected from `introduced` onward.
        true
    }
}

/// A classified advisory: boundaries mapped onto the dependency's own release order, so
/// membership is a pure comparison.
#[derive(Debug, Clone)]
pub struct Advisory {
    /// The source's primary identifier.
    pub id: AdvisoryId,
    /// Cross-source aliases.
    pub aliases: Vec<AdvisoryId>,
    /// The source that reported the advisory.
    pub source: AdvisorySourceId,
    /// The normalized severity.
    pub severity: AdvisorySeverity,
    /// Whether the source has withdrawn the advisory.
    pub withdrawn: bool,
    /// A one-line human summary.
    pub summary: String,
    /// The affected ranges whose boundaries could all be ordered against the release list.
    pub ranges: Vec<AffectedRange>,
    /// Enumerated affected versions, matched exactly.
    pub affected_versions: Vec<Version>,
    /// The fix versions, matched exactly (display + the `check`-side pin test).
    pub fixes: Vec<Version>,
    /// Whether any range boundary could not be ordered (that range was dropped).
    ///
    /// An unorderable advisory loses its range testimony: only exact-match evidence —
    /// enumerated affected versions, fix versions — still flags rows (and still earns the
    /// security window; an exact fix-version match is the strongest evidence there is, and
    /// needs no ordering).
    pub unorderable: bool,
}

impl Advisory {
    /// Whether `version` (at `order`, when it appears in the release list) is affected.
    ///
    /// Positive evidence only: an enumerated version match, or an orderable range containing
    /// the order.
    /// A version absent from the release list (`order = None`) can only match by enumeration.
    #[must_use]
    pub fn affects(&self, version: &Version, order: Option<&ReleaseOrder>) -> bool {
        if self.affected_versions.contains(version) {
            return true;
        }
        order.is_some_and(|order| self.ranges.iter().any(|range| range.contains(order)))
    }

    /// Whether `version` is one of the advisory's fix versions (exact match).
    #[must_use]
    pub fn fixed_by(&self, version: &Version) -> bool {
        self.fixes.contains(version)
    }
}

/// Maps a [`RawAdvisory`]'s version strings onto `releases`' order tokens, producing a
/// classified [`Advisory`].
///
/// `normalize` translates the advisory database's version spelling into the tool's display
/// spelling (e.g. OSV's `Go` ecosystem omits the `v` prefix the Go adapter uses); the identity
/// function is correct for most tools.
/// A range boundary that names a version absent from `releases` cannot be ordered, so that
/// range is dropped and the advisory is marked [`unorderable`](Advisory::unorderable) — it
/// degrades to annotate-only, never to a silent pass.
pub fn classify_advisory(
    raw: &RawAdvisory,
    source: AdvisorySourceId,
    releases: &[Release],
    normalize: &dyn Fn(&str) -> Version,
) -> Advisory {
    let order_of = |version: &str| -> Option<ReleaseOrder> {
        let version = normalize(version);
        releases
            .iter()
            .find(|release| release.version == version)
            .map(|release| release.order.clone())
    };
    let mut unorderable = false;
    let mut ranges = Vec::new();
    for raw_range in &raw.ranges {
        match map_range(raw_range, &order_of) {
            Some(spans) => ranges.extend(spans),
            None => unorderable = true,
        }
    }
    Advisory {
        id: AdvisoryId(raw.id.clone()),
        aliases: raw.aliases.iter().cloned().map(AdvisoryId).collect(),
        source,
        severity: raw.severity,
        withdrawn: raw.withdrawn,
        summary: raw.summary.clone(),
        ranges,
        affected_versions: raw.affected_versions.iter().map(|v| normalize(v)).collect(),
        fixes: raw.fixes.iter().map(|v| normalize(v)).collect(),
        unorderable,
    }
}

/// Maps one raw range onto release orders and assembles its spans, or `None` when a boundary
/// could not be ordered.
///
/// The events are **sorted before the walk**, as the OSV evaluation algorithm specifies:
/// publishers are only encouraged to emit them in order, so a source-order walk would pair the
/// wrong boundaries on a document that lists them any other way.
/// Sorting needs every boundary of the range mapped, so a single unfindable version drops the
/// whole range rather than a single span — the advisory keeps only its exact-match evidence
/// (see [`Advisory::unorderable`]), which is what an unorderable range already meant.
/// `limit` events apply to every span of the range rather than to one position in the sequence.
fn map_range(
    raw: &RawAffectedRange,
    order_of: &dyn Fn(&str) -> Option<ReleaseOrder>,
) -> Option<Vec<AffectedRange>> {
    // A limit *containing* `*` is infinity — the schema's wording, not just a literal `*`, so
    // a `2.*` limit is unlimited too — and it removes the ceiling however many finite limits
    // sit beside it: the OSV `BeforeLimits` rule admits anything before *any* limit.
    // It is scanned for *before* the finite limits are mapped: an unorderable one would
    // otherwise drop the range that the wildcard makes unlimited anyway, and whether it did
    // would depend on which limit the source happened to list first.
    let unlimited = raw
        .events
        .iter()
        .any(|event| matches!(event, RawRangeEvent::Limit(limit) if limit.contains('*')));
    let mut limits = Vec::new();
    if !unlimited {
        for event in &raw.events {
            if let RawRangeEvent::Limit(limit) = event {
                limits.push(order_of(limit)?);
            }
        }
    }

    // `(boundary, event)` pairs, with the beginning-of-time sentinel sorting below every
    // release.
    // An `Introduced` sorts before a boundary closing at the same version, so a span that
    // opens and closes on one release is empty rather than open-ended.
    let mut events: Vec<(Option<ReleaseOrder>, u8, &RawRangeEvent)> = Vec::new();
    for event in &raw.events {
        let (version, rank, opens) = match event {
            RawRangeEvent::Introduced(version) => (version, 0, true),
            RawRangeEvent::Fixed(version) | RawRangeEvent::LastAffected(version) => {
                (version, 1, false)
            }
            RawRangeEvent::Limit(_) => continue,
        };
        let boundary = if opens && is_beginning(version) {
            None
        } else {
            Some(order_of(version)?)
        };
        events.push((boundary, rank, event));
    }
    events.sort_by(|a, b| (&a.0, a.1).cmp(&(&b.0, b.1)));

    let blank = || AffectedRange {
        introduced: None,
        fixed: None,
        last_affected: None,
        limits: limits.clone(),
    };
    // The walk mirrors OSV's evaluator, which folds the sorted events into a single
    // vulnerable/not-vulnerable *state* rather than pairing them off: an event that does not
    // change that state changes nothing at all.
    // So a second `introduced` while a span is open is redundant (the range stays affected up to
    // the next close), and a close with nothing open leaves the version unaffected — evaluation
    // starts not-vulnerable, so a range whose first event closes describes no affected version.
    let mut spans = Vec::new();
    let mut open: Option<AffectedRange> = None;
    for (boundary, _, event) in events {
        match event {
            RawRangeEvent::Introduced(_) if open.is_none() => {
                let mut span = blank();
                span.introduced = boundary;
                open = Some(span);
            }
            RawRangeEvent::Fixed(_) => {
                if let Some(mut span) = open.take() {
                    span.fixed = boundary;
                    spans.push(span);
                }
            }
            RawRangeEvent::LastAffected(_) => {
                if let Some(mut span) = open.take() {
                    span.last_affected = boundary;
                    spans.push(span);
                }
            }
            RawRangeEvent::Introduced(_) | RawRangeEvent::Limit(_) => {}
        }
    }
    // An open span at the end is affected from its `introduced` onward.
    if let Some(span) = open.take() {
        spans.push(span);
    }
    Some(spans)
}

/// Whether an `introduced` boundary is the conventional "from the beginning" sentinel.
///
/// Ecosystems spell it differently — a bare `0`, semver's `0.0.0`, and the `0.0.0-0` the
/// `RustSec` database uses so the boundary also sorts below every prerelease — and none of them
/// need to name a real release: they are below every published version by construction, so they
/// map to "unbounded" instead of failing to order.
/// Only an `introduced` boundary is read this way; a zero `fixed`/`limit` is nonsense, and
/// treating it as unbounded would silently *widen* the affected set.
fn is_beginning(version: &str) -> bool {
    let core = version.strip_suffix("-0").unwrap_or(version);
    !core.is_empty() && core.split('.').all(|part| part == "0")
}

/// One config layer's `[advisories]` table.
///
/// Every field is optional so a layer only overrides what it names; [`resolve_advisory_policy`]
/// combines the layers per field.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdvisoryPolicy {
    /// Whether the feed is consulted at all (off unless some layer turns it on).
    pub enabled: Option<bool>,
    /// Which feed to consult.
    pub source: Option<AdvisorySourceKind>,
    /// Annotate only, or also shorten.
    pub mode: Option<AdvisoryMode>,
    /// The security window applied under [`AdvisoryMode::Shorten`].
    pub min_age: Option<SignedDuration>,
    /// The minimum severity that earns the security window.
    pub severity: Option<AdvisorySeverity>,
    /// Whether the security window may undercut a `floor` (honored only against a floor
    /// declared in the same layer; resolved per floor candidate during window resolution).
    pub bypass_floor: Option<bool>,
}

impl AdvisoryPolicy {
    /// Whether the table sets nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == AdvisoryPolicy::default()
    }
}

/// The effective advisory policy for one project, combined per field across the layer stack.
#[derive(Debug, Clone)]
pub struct ResolvedAdvisoryPolicy {
    /// Whether the feed is consulted (authority-first; default off).
    pub enabled: bool,
    /// The selected feed (authority-first; default OSV).
    pub source: AdvisorySourceKind,
    /// Flag vs shorten.
    ///
    /// Monotone toward [`AdvisoryMode::Flag`]: among the layers that set it, `flag` wins — the
    /// safe direction changes no verdict — so a repo can always decline a fast-track but a
    /// lower layer can never force one past an explicit `flag`.
    pub mode: AdvisoryMode,
    /// The security window (default 1 day).
    pub min_age: SignedDuration,
    /// The layer that decided [`min_age`](Self::min_age).
    pub min_age_origin: Origin,
    /// The severity threshold (max across layers — thresholds only ratchet up; default high).
    pub severity: AdvisorySeverity,
    /// The field-by-field derivation, appended to `explain` traces when the feed is enabled.
    ///
    /// `bypass-floor` does not resolve here: it is honored only against a floor declared in the
    /// same layer, decided per floor candidate during ordinary window resolution (see
    /// [`ResolvedWindow::advisory_floor`]) — so a repo cannot use a CVE as a lever against an
    /// org floor.
    pub trace: Vec<TraceStep>,
}

/// The built-in security window: one day.
const DEFAULT_SECURITY_WINDOW: SignedDuration = SignedDuration::from_hours(24);

/// Resolves the effective `[advisories]` policy across `layers` (low → high authority).
///
/// Per-field combine rules, in the same idiom as window resolution: `enabled`, `source`, and
/// `min-age` are **authority-first** (the highest layer that sets each wins); `mode` is
/// **monotone toward `flag`** (among layers that set it, an explicit `flag` wins — it changes
/// no verdict); `severity` is the **max** across layers (thresholds only ratchet up).
/// `bypass-floor` is absent because it never combines into one project-wide value: it is
/// resolved *and* traced during ordinary window resolution, per floor candidate, honored only in
/// the layer that declared the floor.
/// Defaults: disabled, OSV, flag, a 1-day window, high severity, no floor bypass.
#[must_use]
pub fn resolve_advisory_policy(layers: &[PolicyLayer]) -> ResolvedAdvisoryPolicy {
    let mut acc = PolicyAccumulator::default();
    for layer in layers {
        if let Some(policy) = &layer.advisories {
            acc.absorb(policy, &layer.origin);
        }
    }
    acc.finish()
}

/// The running `[advisories]` resolution: the winning value of each field so far plus the trace.
struct PolicyAccumulator {
    enabled: bool,
    source: AdvisorySourceKind,
    min_age: SignedDuration,
    min_age_origin: Origin,
    /// Every layer's `mode`, by trace index: the winner is only decidable once all are in.
    mode_steps: Vec<(usize, AdvisoryMode)>,
    /// Every layer's `severity`, by trace index (same reason).
    severity_steps: Vec<(usize, AdvisorySeverity)>,
    trace: Vec<TraceStep>,
}

impl Default for PolicyAccumulator {
    fn default() -> Self {
        PolicyAccumulator {
            enabled: false,
            source: AdvisorySourceKind::Osv,
            min_age: DEFAULT_SECURITY_WINDOW,
            min_age_origin: Origin::Default,
            mode_steps: Vec::new(),
            severity_steps: Vec::new(),
            trace: Vec::new(),
        }
    }
}

impl PolicyAccumulator {
    /// Folds one layer's table in, recording one step per field it sets.
    ///
    /// One trace step per (layer, field) rather than one per layer: a step says whether *that*
    /// field's value survived, so a layer whose `mode` lost to a `flag` elsewhere — or whose
    /// `min-age` was overwritten — is not reported as wholly applied.
    /// Every step is recorded as applied here and demoted in [`finish`](Self::finish): no
    /// combine rule can decide a winner from a prefix of the stack, since a later layer can
    /// overwrite an authority-first field, decline a `shorten`, or ratchet a threshold up.
    fn absorb(&mut self, policy: &AdvisoryPolicy, origin: &Origin) {
        if let Some(value) = policy.enabled {
            self.enabled = value;
            self.step(origin, "enabled", format!("enabled = {value}"), None);
        }
        if let Some(value) = policy.source {
            self.source = value;
            let token = match value {
                AdvisorySourceKind::Osv => "osv",
                AdvisorySourceKind::Github => "github",
                AdvisorySourceKind::None => "none",
            };
            self.step(origin, "source", format!("source = {token}"), None);
        }
        if let Some(value) = policy.mode {
            let token = match value {
                AdvisoryMode::Flag => "flag",
                AdvisoryMode::Shorten => "shorten",
            };
            self.mode_steps.push((self.trace.len(), value));
            self.step(origin, "mode", format!("mode = {token}"), None);
        }
        if let Some(value) = policy.min_age {
            self.min_age = value;
            self.min_age_origin = origin.clone();
            let days = crate::duration::duration_as_days(value);
            let note = format!("min-age = {days}d");
            self.step(origin, "min-age", note, Some(days));
        }
        if let Some(value) = policy.severity {
            self.severity_steps.push((self.trace.len(), value));
            self.step(origin, "severity", format!("severity = {value}"), None);
        }
    }

    /// Records one applied `advisories.<field>` derivation step for `origin`.
    fn step(&mut self, origin: &Origin, field: &str, note: String, min_age_days: Option<f64>) {
        self.trace.push(TraceStep {
            layer: origin.clone(),
            field: format!("advisories.{field}"),
            selector: None,
            min_age_days,
            applied: true,
            note,
        });
    }

    /// Marks the trace step at `index` as considered rather than applied, saying why.
    fn demote(&mut self, index: usize, reason: &str) {
        if let Some(step) = self.trace.get_mut(index) {
            step.applied = false;
            step.note = format!("{} ({reason})", step.note);
        }
    }

    /// Finalizes the resolution, leaving exactly one applied step per field.
    ///
    /// A [`TraceStep`] claims a value *won*, not that it was considered, so each field's losing
    /// steps are demoted here — where the whole stack is known — with the combine rule that beat
    /// them.
    fn finish(mut self) -> ResolvedAdvisoryPolicy {
        // Authority-first: the highest layer that set the field wins.
        for field in [
            "advisories.enabled",
            "advisories.source",
            "advisories.min-age",
        ] {
            let winner = self.trace.iter().rposition(|step| step.field == field);
            for (index, step) in self.trace.iter_mut().enumerate() {
                if step.field == field && Some(index) != winner {
                    step.applied = false;
                    step.note = format!("{} (overridden by a higher layer)", step.note);
                }
            }
        }

        // Monotone toward `flag`: an explicit `flag` in *any* layer declines shortening, so the
        // winner is not simply the highest layer that set the field.
        let mode = self
            .mode_steps
            .iter()
            .map(|(_, value)| *value)
            .reduce(|left, right| match (left, right) {
                (AdvisoryMode::Flag, _) | (_, AdvisoryMode::Flag) => AdvisoryMode::Flag,
                _ => AdvisoryMode::Shorten,
            });
        if let Some(mode) = mode {
            let winner = self
                .mode_steps
                .iter()
                .rev()
                .find(|(_, value)| *value == mode)
                .map(|(index, _)| *index);
            for (index, value) in std::mem::take(&mut self.mode_steps) {
                if Some(index) == winner {
                    continue;
                }
                let reason = if value == mode {
                    "also set by a higher layer".to_string()
                } else {
                    "declined; an explicit `flag` stands".to_string()
                };
                self.demote(index, &reason);
            }
        }

        // Max across layers: the threshold only ratchets up.
        let severity = self
            .severity_steps
            .iter()
            .map(|(_, value)| *value)
            .max()
            .unwrap_or(AdvisorySeverity::High);
        let winner = self
            .severity_steps
            .iter()
            .rev()
            .find(|(_, value)| *value == severity)
            .map(|(index, _)| *index);
        for (index, value) in std::mem::take(&mut self.severity_steps) {
            if Some(index) == winner {
                continue;
            }
            let reason = if value == severity {
                "also set by a higher layer".to_string()
            } else {
                format!("the threshold only ratchets up to {severity}")
            };
            self.demote(index, &reason);
        }

        ResolvedAdvisoryPolicy {
            enabled: self.enabled,
            source: self.source,
            mode: mode.unwrap_or(AdvisoryMode::Flag),
            min_age: self.min_age,
            min_age_origin: self.min_age_origin,
            severity,
            trace: self.trace,
        }
    }
}

/// Why a row is security-relevant and what the policy did about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurityRelevance {
    /// The advisories the row's version fixes (candidate side: adopting it resolves them; pin
    /// side: the locked version is their fix).
    pub fixes: Vec<AdvisoryId>,
    /// The highest severity among [`fixes`](Self::fixes).
    pub severity: AdvisorySeverity,
    /// The source the advisories came from.
    pub source: AdvisorySourceId,
    /// Whether the security window actually replaced the ordinary one for this row (`true` only
    /// under [`AdvisoryMode::Shorten`] when the shorter window changed the cutoff).
    pub applied: bool,
}

/// The advisory context threaded into the advised evaluation functions: the classified
/// advisories for one dependency plus the project's resolved advisory policy.
#[derive(Debug, Clone, Copy)]
pub struct AdvisoryContext<'a> {
    /// The classified advisories for the dependency under evaluation.
    pub advisories: &'a [Advisory],
    /// The project's resolved `[advisories]` policy.
    pub policy: &'a ResolvedAdvisoryPolicy,
}

/// Replaces `window` with the security window when that is *shorter*, or returns `None` when
/// the security window would not tighten adoption (it is a shortening mechanism only — a
/// security window longer than the ordinary one is a no-op).
///
/// The floor still clamps by default: `bypass-floor` lifts a floor only when it was declared
/// **in that floor's own layer** — the same per-candidate rule `allow` follows, resolved during
/// ordinary window resolution into [`ResolvedWindow::advisory_floor`] (the largest floor no
/// bypass could lift) — so a repo cannot use a CVE as a lever against an org floor, and lifting
/// one layer's floor never erases another layer's.
/// This bounds a poisoned feed's blast radius: the worst it can do is drop a window to the
/// surviving floor.
#[must_use]
pub fn apply_security_window(
    window: &ResolvedWindow,
    policy: &ResolvedAdvisoryPolicy,
    shortened_by: &AdvisoryId,
    now: Timestamp,
) -> Option<ResolvedWindow> {
    if window.exempt {
        // Already exempt — there is nothing left to shorten.
        return None;
    }
    let shortened = ResolvedWindow {
        spec: WindowSpec::MinAge(policy.min_age),
        decided_by: policy.min_age_origin.clone(),
        decided_selector: Selector::Default,
        floor: window.advisory_floor,
        floor_origin: window.advisory_floor_origin.clone(),
        exempt: false,
        exempt_origin: None,
        shortened_by: Some(shortened_by.clone()),
        advisory_floor: window.advisory_floor,
        advisory_floor_origin: window.advisory_floor_origin.clone(),
    };
    // Only a *later* effective cutoff (a shorter wait) counts as shortening; the floor clamp is
    // folded in on both sides so a floor no bypass could lift is honored.
    (shortened.cutoff(now) > window.cutoff(now)).then_some(shortened)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MajorKey, ReleaseQuality};

    fn release(version: &str, order: u8) -> Release {
        Release {
            version: Version::new(version),
            order: ReleaseOrder(vec![order]),
            major: MajorKey("1".into()),
            major_number: Some(1),
            kind_from_current: None,
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: None,
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
    }

    /// An advisory with one range built from `events`, and every `fixed` event also listed as a
    /// fix version (as every source does).
    fn raw_with(id: &str, events: Vec<RawRangeEvent>) -> RawAdvisory {
        let fixes = events
            .iter()
            .filter_map(|event| match event {
                RawRangeEvent::Fixed(version) => Some(version.clone()),
                _ => None,
            })
            .collect();
        RawAdvisory {
            id: id.to_string(),
            aliases: Vec::new(),
            severity: AdvisorySeverity::High,
            withdrawn: false,
            summary: String::new(),
            ranges: vec![RawAffectedRange { events }],
            affected_versions: Vec::new(),
            fixes,
        }
    }

    fn raw(id: &str, introduced: Option<&str>, fixed: Option<&str>) -> RawAdvisory {
        let events = introduced
            .map(|version| RawRangeEvent::Introduced(version.to_string()))
            .into_iter()
            .chain(fixed.map(|version| RawRangeEvent::Fixed(version.to_string())))
            .collect();
        raw_with(id, events)
    }

    const OSV: AdvisorySourceId = AdvisorySourceId("osv");

    fn identity(version: &str) -> Version {
        Version::new(version)
    }

    #[test]
    fn classify_maps_range_boundaries_onto_release_orders() {
        let releases = [
            release("1.0.0", 0),
            release("1.0.1", 1),
            release("1.0.2", 2),
        ];
        let advisory = classify_advisory(
            &raw("GHSA-x", Some("0"), Some("1.0.2")),
            OSV,
            &releases,
            &identity,
        );
        assert!(!advisory.unorderable);
        // 1.0.0 and 1.0.1 are affected; the 1.0.2 fix is not.
        assert!(advisory.affects(&Version::new("1.0.0"), Some(&ReleaseOrder(vec![0]))));
        assert!(advisory.affects(&Version::new("1.0.1"), Some(&ReleaseOrder(vec![1]))));
        assert!(!advisory.affects(&Version::new("1.0.2"), Some(&ReleaseOrder(vec![2]))));
        assert!(advisory.fixed_by(&Version::new("1.0.2")));
        assert!(!advisory.fixed_by(&Version::new("1.0.1")));
    }

    #[test]
    fn unfindable_boundary_drops_the_range_and_marks_unorderable() {
        let releases = [release("1.0.0", 0), release("1.0.1", 1)];
        // The fix version 9.9.9 is not among the fetched releases (another major line), so the
        // range cannot be ordered: no range membership, but the exact fixes test still works.
        let advisory = classify_advisory(
            &raw("GHSA-y", Some("0"), Some("9.9.9")),
            OSV,
            &releases,
            &identity,
        );
        assert!(advisory.unorderable);
        assert!(advisory.ranges.is_empty());
        assert!(!advisory.affects(&Version::new("1.0.0"), Some(&ReleaseOrder(vec![0]))));
        assert!(advisory.fixed_by(&Version::new("9.9.9")));
    }

    #[test]
    fn enumerated_versions_match_without_ranges() {
        let mut raw = raw("GHSA-z", None, None);
        raw.ranges.clear();
        raw.affected_versions = vec!["1.0.1".to_string()];
        let advisory = classify_advisory(&raw, OSV, &[], &identity);
        assert!(advisory.affects(&Version::new("1.0.1"), None));
        assert!(!advisory.affects(&Version::new("1.0.0"), None));
    }

    #[test]
    fn last_affected_is_an_inclusive_upper_bound() {
        let releases = [
            release("1.0.0", 0),
            release("1.0.1", 1),
            release("1.0.2", 2),
        ];
        let raw = raw_with(
            "GHSA-w",
            vec![
                RawRangeEvent::Introduced("0".to_string()),
                RawRangeEvent::LastAffected("1.0.1".to_string()),
            ],
        );
        let advisory = classify_advisory(&raw, OSV, &releases, &identity);
        assert!(advisory.affects(&Version::new("1.0.1"), Some(&ReleaseOrder(vec![1]))));
        assert!(!advisory.affects(&Version::new("1.0.2"), Some(&ReleaseOrder(vec![2]))));
    }

    /// Every ecosystem's "from the beginning" sentinel maps to an unbounded lower boundary —
    /// including the `RustSec` database's `0.0.0-0`, which names no real release and would
    /// otherwise be
    /// unorderable.
    #[test]
    fn zero_sentinels_open_a_range_without_naming_a_release() {
        let releases = [release("1.0.0", 0), release("1.0.1", 1)];
        for sentinel in ["0", "0.0.0", "0.0.0-0"] {
            let raw = raw("GHSA-0", Some(sentinel), Some("1.0.1"));
            let advisory = classify_advisory(&raw, OSV, &releases, &identity);
            assert!(!advisory.unorderable, "{sentinel} is a known sentinel");
            assert!(advisory.affects(&Version::new("1.0.0"), Some(&ReleaseOrder(vec![0]))));
            assert!(!advisory.affects(&Version::new("1.0.1"), Some(&ReleaseOrder(vec![1]))));
        }
        // Only an `introduced` reads as the beginning: an unbounded `fixed` would widen the
        // affected set instead of opening it.
        let zero_fix = raw("GHSA-0f", Some("1.0.0"), Some("0"));
        let advisory = classify_advisory(&zero_fix, OSV, &releases, &identity);
        assert!(advisory.unorderable);
        assert!(advisory.ranges.is_empty());
    }

    /// The OSV `BeforeLimits` rule: limits describe alternative branches, so an order before
    /// *any* of them is in range.
    /// A limit that cannot be ordered poisons its range — never a silent open range.
    #[test]
    fn limit_boundaries_cap_range_membership() {
        let releases = [
            release("1.0.0", 0),
            release("1.1.0", 1),
            release("1.2.0", 2),
        ];
        let limited = raw_with(
            "GHSA-l",
            vec![
                RawRangeEvent::Introduced("1.0.0".to_string()),
                RawRangeEvent::Limit("1.2.0".to_string()),
            ],
        );
        let advisory = classify_advisory(&limited, OSV, &releases, &identity);
        assert!(!advisory.unorderable);
        assert!(advisory.affects(&Version::new("1.1.0"), Some(&ReleaseOrder(vec![1]))));
        assert!(
            !advisory.affects(&Version::new("1.2.0"), Some(&ReleaseOrder(vec![2]))),
            "at the limit is out"
        );

        // Two limits: the widest one decides, so 1.1.0 stays in range even though it is past
        // the other.
        let two_limits = raw_with(
            "GHSA-l3",
            vec![
                RawRangeEvent::Introduced("1.0.0".to_string()),
                RawRangeEvent::Limit("1.1.0".to_string()),
                RawRangeEvent::Limit("1.2.0".to_string()),
            ],
        );
        let advisory = classify_advisory(&two_limits, OSV, &releases, &identity);
        assert!(
            advisory.affects(&Version::new("1.1.0"), Some(&ReleaseOrder(vec![1]))),
            "before at least one limit is in range"
        );
        assert!(!advisory.affects(&Version::new("1.2.0"), Some(&ReleaseOrder(vec![2]))));

        // A limit containing `*` is infinity — the schema allows any version containing the
        // string, not only a bare `*` — so it removes the ceiling the finite limit beside it
        // would impose: including an unorderable one, and whichever side of the wildcard it is
        // listed on.
        for wildcard in ["*", "2.*"] {
            for finite in ["1.1.0", "9.9.9"] {
                for events in [
                    vec![
                        RawRangeEvent::Limit(finite.to_string()),
                        RawRangeEvent::Limit(wildcard.to_string()),
                    ],
                    vec![
                        RawRangeEvent::Limit(wildcard.to_string()),
                        RawRangeEvent::Limit(finite.to_string()),
                    ],
                ] {
                    let mut all = vec![RawRangeEvent::Introduced("1.0.0".to_string())];
                    all.extend(events);
                    let unlimited = raw_with("GHSA-l4", all);
                    let advisory = classify_advisory(&unlimited, OSV, &releases, &identity);
                    assert!(
                        !advisory.unorderable,
                        "a range {wildcard} makes unlimited never needs {finite} ordered"
                    );
                    assert!(advisory.affects(&Version::new("1.2.0"), Some(&ReleaseOrder(vec![2]))));
                }
            }
        }

        let unfindable = raw_with(
            "GHSA-l2",
            vec![
                RawRangeEvent::Introduced("1.0.0".to_string()),
                RawRangeEvent::Limit("9.9.9".to_string()),
            ],
        );
        let advisory = classify_advisory(&unfindable, OSV, &releases, &identity);
        assert!(advisory.unorderable, "an unorderable limit drops the range");
        assert!(advisory.ranges.is_empty());
    }

    /// The events fold into one vulnerable/not-vulnerable state, so a redundant event changes
    /// nothing: a second `introduced` must not open a second span (leaving the first unbounded),
    /// and a close with nothing open describes no affected version at all.
    #[test]
    fn redundant_events_do_not_widen_a_range() {
        let releases = [
            release("1.0.0", 0),
            release("2.0.0", 1),
            release("3.0.0", 2),
            release("4.0.0", 3),
        ];
        let repeated = raw_with(
            "GHSA-r",
            vec![
                RawRangeEvent::Introduced("1.0.0".to_string()),
                RawRangeEvent::Introduced("2.0.0".to_string()),
                RawRangeEvent::Fixed("3.0.0".to_string()),
            ],
        );
        let advisory = classify_advisory(&repeated, OSV, &releases, &identity);
        assert!(advisory.affects(&Version::new("2.0.0"), Some(&ReleaseOrder(vec![1]))));
        assert!(
            !advisory.affects(&Version::new("3.0.0"), Some(&ReleaseOrder(vec![2]))),
            "the fix closes the one open span rather than leaving it unbounded"
        );
        assert!(!advisory.affects(&Version::new("4.0.0"), Some(&ReleaseOrder(vec![3]))));

        // Evaluation starts *not* vulnerable, so a range that only ever closes affects nothing.
        let closes_only = raw_with("GHSA-c", vec![RawRangeEvent::Fixed("3.0.0".to_string())]);
        let advisory = classify_advisory(&closes_only, OSV, &releases, &identity);
        assert!(advisory.ranges.is_empty());
        assert!(!advisory.affects(&Version::new("1.0.0"), Some(&ReleaseOrder(vec![0]))));
        assert!(
            advisory.fixed_by(&Version::new("3.0.0")),
            "the exact fix-version evidence is untouched"
        );
    }

    /// OSV's evaluation algorithm sorts a range's events before folding them; publishers are
    /// only *encouraged* to emit them in order, so a source-order fold would close the wrong
    /// span and invert the range.
    #[test]
    fn unsorted_events_are_ordered_before_the_state_fold() {
        let releases = [
            release("1.0.0", 0),
            release("1.1.0", 1),
            release("2.0.0", 2),
            release("2.1.0", 3),
        ];
        let shuffled = raw_with(
            "GHSA-s",
            vec![
                RawRangeEvent::Fixed("2.1.0".to_string()),
                RawRangeEvent::Introduced("1.0.0".to_string()),
                RawRangeEvent::Fixed("1.1.0".to_string()),
                RawRangeEvent::Introduced("2.0.0".to_string()),
            ],
        );
        let advisory = classify_advisory(&shuffled, OSV, &releases, &identity);
        assert!(!advisory.unorderable);
        // The spans are [1.0.0, 1.1.0) and [2.0.0, 2.1.0), not whatever the listed order pairs.
        assert!(advisory.affects(&Version::new("1.0.0"), Some(&ReleaseOrder(vec![0]))));
        assert!(!advisory.affects(&Version::new("1.1.0"), Some(&ReleaseOrder(vec![1]))));
        assert!(advisory.affects(&Version::new("2.0.0"), Some(&ReleaseOrder(vec![2]))));
        assert!(!advisory.affects(&Version::new("2.1.0"), Some(&ReleaseOrder(vec![3]))));
    }

    #[test]
    fn severity_parses_ratings_and_orders_unknown_below_all() {
        assert_eq!(
            AdvisorySeverity::parse("HIGH"),
            Some(AdvisorySeverity::High)
        );
        assert_eq!(
            AdvisorySeverity::parse("medium"),
            Some(AdvisorySeverity::Moderate)
        );
        assert_eq!(AdvisorySeverity::parse("bogus"), None);
        assert!(AdvisorySeverity::Unknown < AdvisorySeverity::Low);
        assert!(AdvisorySeverity::Critical > AdvisorySeverity::High);
    }

    fn layer_with(origin: Origin, policy: AdvisoryPolicy) -> PolicyLayer {
        let mut layer = PolicyLayer::new(origin);
        layer.advisories = Some(policy);
        layer
    }

    #[test]
    fn advisory_policy_defaults_are_disabled_flag_osv() {
        let policy = resolve_advisory_policy(&[PolicyLayer::new(Origin::Default)]);
        assert!(!policy.enabled);
        assert_eq!(policy.source, AdvisorySourceKind::Osv);
        assert_eq!(policy.mode, AdvisoryMode::Flag);
        assert_eq!(policy.min_age, SignedDuration::from_hours(24));
        assert_eq!(policy.severity, AdvisorySeverity::High);
    }

    #[test]
    fn advisory_mode_is_monotone_toward_flag() {
        // The org (global) turns shortening on; the repo declines with an explicit `flag` — the
        // safe direction wins regardless of layer order.
        let layers = [
            layer_with(
                Origin::Global,
                AdvisoryPolicy {
                    enabled: Some(true),
                    mode: Some(AdvisoryMode::Shorten),
                    ..AdvisoryPolicy::default()
                },
            ),
            layer_with(
                Origin::Repo("cooldown.toml".into()),
                AdvisoryPolicy {
                    mode: Some(AdvisoryMode::Flag),
                    ..AdvisoryPolicy::default()
                },
            ),
        ];
        assert_eq!(resolve_advisory_policy(&layers).mode, AdvisoryMode::Flag);

        // The reverse order too: a later shorten cannot override an explicit flag.
        let layers = [
            layer_with(
                Origin::Global,
                AdvisoryPolicy {
                    mode: Some(AdvisoryMode::Flag),
                    ..AdvisoryPolicy::default()
                },
            ),
            layer_with(
                Origin::Repo("cooldown.toml".into()),
                AdvisoryPolicy {
                    mode: Some(AdvisoryMode::Shorten),
                    ..AdvisoryPolicy::default()
                },
            ),
        ];
        assert_eq!(resolve_advisory_policy(&layers).mode, AdvisoryMode::Flag);
    }

    #[test]
    fn advisory_severity_threshold_ratchets_up_across_layers() {
        let layers = [
            layer_with(
                Origin::Global,
                AdvisoryPolicy {
                    severity: Some(AdvisorySeverity::Critical),
                    ..AdvisoryPolicy::default()
                },
            ),
            layer_with(
                Origin::Repo("cooldown.toml".into()),
                AdvisoryPolicy {
                    severity: Some(AdvisorySeverity::Low),
                    ..AdvisoryPolicy::default()
                },
            ),
        ];
        assert_eq!(
            resolve_advisory_policy(&layers).severity,
            AdvisorySeverity::Critical
        );
    }

    #[test]
    fn advisory_min_age_is_authority_first() {
        let layers = [
            layer_with(
                Origin::Global,
                AdvisoryPolicy {
                    min_age: Some(SignedDuration::from_hours(12)),
                    ..AdvisoryPolicy::default()
                },
            ),
            layer_with(
                Origin::Repo("cooldown.toml".into()),
                AdvisoryPolicy {
                    min_age: Some(SignedDuration::from_hours(48)),
                    ..AdvisoryPolicy::default()
                },
            ),
        ];
        let policy = resolve_advisory_policy(&layers);
        assert_eq!(policy.min_age, SignedDuration::from_hours(48));
        assert_eq!(policy.min_age_origin, Origin::Repo("cooldown.toml".into()));
    }

    fn window_with_floor(floor_origin: Option<Origin>) -> ResolvedWindow {
        let floor = floor_origin
            .is_some()
            .then_some(SignedDuration::from_hours(24 * 7));
        ResolvedWindow {
            spec: WindowSpec::MinAge(SignedDuration::from_hours(24 * 14)),
            decided_by: Origin::Default,
            decided_selector: Selector::Default,
            floor,
            floor_origin: floor_origin.clone(),
            exempt: false,
            exempt_origin: None,
            shortened_by: None,
            // No bypass declared: the security window faces the same floor.
            advisory_floor: floor,
            advisory_floor_origin: floor_origin,
        }
    }

    fn shorten_policy() -> ResolvedAdvisoryPolicy {
        ResolvedAdvisoryPolicy {
            enabled: true,
            source: AdvisorySourceKind::Osv,
            mode: AdvisoryMode::Shorten,
            min_age: SignedDuration::from_hours(24),
            min_age_origin: Origin::Repo("cooldown.toml".into()),
            severity: AdvisorySeverity::High,
            trace: Vec::new(),
        }
    }

    #[test]
    fn security_window_shortens_but_stops_at_the_advisory_floor() {
        let now: Timestamp = "2026-06-17T00:00:00Z".parse().expect("now");
        let ghsa = AdvisoryId("GHSA-x".to_string());

        // No floor: the 14d window drops to the 1d security window.
        let plain = window_with_floor(None);
        let shortened =
            apply_security_window(&plain, &shorten_policy(), &ghsa, now).expect("shortened");
        assert_eq!(shortened.cutoff(now), now - SignedDuration::from_hours(24));
        assert_eq!(shortened.shortened_by, Some(ghsa.clone()));

        // A global (org) floor without bypass: the window stops at the 7d floor, not 1d — the
        // poisoned-feed bound (a hostile feed can at worst drop a window to the floor).
        let floored = window_with_floor(Some(Origin::Global));
        let clamped =
            apply_security_window(&floored, &shorten_policy(), &ghsa, now).expect("clamped");
        assert_eq!(
            clamped.cutoff(now),
            now - SignedDuration::from_hours(24 * 7)
        );

        // The declaring layer's bypass lifted its own floor during resolution and a *smaller*
        // floor from another layer survived as the advisory floor: the security window stops
        // there, not at zero — lifting one layer's floor never erases another layer's.
        let residual = ResolvedWindow {
            advisory_floor: Some(SignedDuration::from_hours(24 * 3)),
            advisory_floor_origin: Some(Origin::Global),
            ..window_with_floor(Some(Origin::Repo("cooldown.toml".into())))
        };
        let clamped =
            apply_security_window(&residual, &shorten_policy(), &ghsa, now).expect("clamped");
        assert_eq!(
            clamped.cutoff(now),
            now - SignedDuration::from_hours(24 * 3)
        );
        assert_eq!(clamped.floor_origin, Some(Origin::Global));

        // Every floor's layer declared a bypass: no advisory floor remains and the security
        // window applies in full.
        let lifted_window = ResolvedWindow {
            advisory_floor: None,
            advisory_floor_origin: None,
            ..window_with_floor(Some(Origin::Global))
        };
        let lifted =
            apply_security_window(&lifted_window, &shorten_policy(), &ghsa, now).expect("lifted");
        assert_eq!(lifted.cutoff(now), now - SignedDuration::from_hours(24));
    }

    #[test]
    fn security_window_never_extends_an_ordinary_window() {
        let now: Timestamp = "2026-06-17T00:00:00Z".parse().expect("now");
        let ghsa = AdvisoryId("GHSA-x".to_string());
        // Ordinary window 12h, security window 24h: longer, so it is a no-op.
        let short = ResolvedWindow {
            spec: WindowSpec::MinAge(SignedDuration::from_hours(12)),
            ..window_with_floor(None)
        };
        assert!(apply_security_window(&short, &shorten_policy(), &ghsa, now).is_none());
    }

    #[test]
    fn security_window_leaves_an_exempt_row_alone() {
        let now: Timestamp = "2026-06-17T00:00:00Z".parse().expect("now");
        let ghsa = AdvisoryId("GHSA-x".to_string());
        let exempt = ResolvedWindow {
            exempt: true,
            spec: WindowSpec::Latest,
            ..window_with_floor(None)
        };
        assert!(apply_security_window(&exempt, &shorten_policy(), &ghsa, now).is_none());
    }
}
