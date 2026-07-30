//! The cooldown decision — the single source of truth for every tool.
//!
//! [`evaluate`] drives `outdated`/`upgrade` over a candidate set; [`check_pin`] is the gate over
//! the currently-locked release. Both are pure: no concrete I/O, no clock (the `now` boundary is
//! passed in), and no version parsing (the tool hands back classified releases). "Unknown age
//! is never mature" is enforced here, once.

use crate::model::{
    Candidate, Dependency, HeldReason, MajorKey, PinVerdict, Release, ReleaseOrder, ReleaseQuality,
    Status, ToolId, UpdateKind, Verdict, Version,
};
use crate::policy::{
    MaxMajorPick, PolicyLayer, ResolveKind, ResolveQuery, resolve, resolve_max_major,
};
use camino::Utf8Path;
use jiff::Timestamp;

/// The context the core needs to build resolution queries and apply the candidate filter.
///
/// Threaded into both [`evaluate`] and [`check_pin`], it carries the per-invocation knobs that are
/// not properties of the [`Dependency`] itself: the tool, project, major scope, and whether an
/// upgrade may rewrite explicit declared bounds. It is `Copy`, so it is cheap to pass by value or
/// reference.
#[derive(Debug, Clone, Copy)]
pub struct ResolveContext<'a> {
    /// The tool being evaluated, used to build the [`ResolveQuery`](crate::ResolveQuery)
    /// for each candidate.
    pub tool: ToolId,
    /// The project root the policy cascade resolves against (matches `project=` selectors).
    pub project: &'a Utf8Path,
    /// `--major`: allow cross-major jumps as candidates (default: within the current major).
    pub allow_major: bool,
    /// Whether explicit manifest upper bounds constrain candidates.
    ///
    /// This is `false` only for `upgrade --rewrite`, whose contract explicitly permits rewriting
    /// and crossing such a bound.
    pub honor_declared_bounds: bool,
    /// Whether the registry's `latest` dist-tag caps candidates
    /// ([`Release::beyond_latest_tag`]).
    ///
    /// The cap applies only while the current pin sits at or below the tag. A pin already beyond
    /// it (a project deliberately riding a `next` line) deactivates the ceiling *entirely* — not
    /// merely raising it to the pin — so such a project never sees downgrade pressure or a silent
    /// "up to date", and even releases above its own line stay adoptable: once the project has
    /// knowingly passed the tag, the tag carries no guidance about where it should stop. This is
    /// `false` only when the user opts out (`respect-dist-tags = false` /
    /// `--no-respect-dist-tags`), electing to adopt releases above the registry's current `latest`
    /// tag.
    pub honor_latest_tag: bool,
}

/// Which package ceiling hid an otherwise adoptable release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CeilingReason {
    /// An explicit upper comparator in the declared manifest requirement.
    DeclaredBound,
    /// A package-scoped configured `max-major`.
    MaxMajor,
    /// The registry's `latest` dist-tag currently sits below the release — it is not what a plain
    /// install would resolve to today.
    DistTag,
}

/// A matured release that normal evaluation excludes because of a package ceiling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CeilingHold {
    /// The ceiling responsible for the hold.
    pub reason: CeilingReason,
    /// The newest matured release that would be adopted without package ceilings.
    pub target: Version,
    /// The target's update kind relative to the current release.
    pub update_kind: UpdateKind,
}

#[derive(Debug, Clone, Copy)]
struct CeilingFilters {
    declared_bound: bool,
    max_major: bool,
    latest_tag: bool,
}

impl CeilingFilters {
    fn standard(ctx: &ResolveContext<'_>) -> Self {
        Self {
            declared_bound: ctx.honor_declared_bounds,
            max_major: true,
            latest_tag: ctx.honor_latest_tag,
        }
    }

    const fn unbounded() -> Self {
        Self {
            declared_bound: false,
            max_major: false,
            latest_tag: false,
        }
    }
}

fn query<'a>(
    dep: &'a Dependency,
    ctx: &'a ResolveContext<'a>,
    kind: ResolveKind,
) -> ResolveQuery<'a> {
    ResolveQuery {
        tool: ctx.tool,
        package: &dep.package.name,
        registry: dep.package.registry.as_deref(),
        project: ctx.project,
        kind,
    }
}

/// Whether a release is visible as of `now`: a release dated **after** the evaluation instant does
/// not exist yet from the run's point of view, so it is neither a candidate nor the `latest`. Under
/// the real system clock no release is ever future-dated, so this is a no-op there; it only bites
/// when a fixed [`Clock`](crate::Clock) is injected to evaluate the registry "as of" an earlier
/// instant, keeping [`evaluate`]'s candidate and `latest` set honest — no versions from the future,
/// hence no negative candidate ages. ([`check_pin`] judges the already-locked pin directly, not a
/// candidate set, so it does not consult this.) A release with an unknown publish time is always
/// visible — it is judged [`UnknownAge`](Status::UnknownAge).
fn visible_at(r: &Release, now: Timestamp) -> bool {
    r.published_at.is_none_or(|published| published <= now)
}

/// Whether a candidate's quality makes it eligible: prereleases are excluded unless the current
/// pin is itself a prerelease; pseudo-versions (commit pins) are never normal upgrade targets.
fn quality_eligible(r: &Release, current_quality: ReleaseQuality) -> bool {
    match r.quality {
        ReleaseQuality::Stable | ReleaseQuality::Incompatible => true,
        ReleaseQuality::Prerelease => current_quality == ReleaseQuality::Prerelease,
        ReleaseQuality::Pseudo => false,
    }
}

/// Whether a candidate's major makes it eligible: cross-major jumps are admitted only under
/// `--major` (`allow_major`); otherwise a candidate must stay within the current pin's major.
///
/// "Within the current major" requires both that the candidate shares the current pin's
/// [`MajorKey`] *and* that it is not a semver-major jump ([`UpdateKind::Major`]). The `MajorKey`
/// alone is insufficient for Go's `+incompatible` versions: they keep the base module path (so they
/// share the empty `MajorKey`) yet bump the semver major (`v0.36.1` → `v11.0.0+incompatible`).
/// `kind_from_current` is the semver-accurate guard, so `--no-major`/`--minor` never plans a major.
fn major_eligible(r: &Release, current_major: &MajorKey, allow_major: bool) -> bool {
    allow_major || (r.major == *current_major && r.kind_from_current != Some(UpdateKind::Major))
}

/// The release order of the dependency's graph ceiling — the version a requirer pins it to with `==`
/// (its [`graph_ceiling`](Dependency::graph_ceiling)) — when that version is among `releases`.
/// [`evaluate`] excludes candidates ordered above it; `None` means no ceiling (or the ceiling version
/// is not present here), so candidates are uncapped. The upgrade-direction mirror of `graph_floor`.
fn graph_ceiling_order<'a>(dep: &Dependency, releases: &'a [Release]) -> Option<&'a ReleaseOrder> {
    let ceiling = dep.graph_ceiling.as_ref()?;
    releases
        .iter()
        .find(|r| r.version == *ceiling)
        .map(|r| &r.order)
}

fn within_max_major(release: &Release, max_major: Option<&MaxMajorPick>) -> bool {
    max_major.is_none_or(|pick| {
        release
            .major_number
            .is_some_and(|major| major <= pick.limit)
    })
}

fn active_max_major(
    current: &Release,
    dep: &Dependency,
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    enabled: bool,
) -> Option<MaxMajorPick> {
    enabled
        .then(|| resolve_max_major(layers, &query(dep, ctx, ResolveKind::CurrentPin)))
        .flatten()
        .filter(|pick| current.major_number.is_none_or(|major| major <= pick.limit))
}

fn empty_candidate_held_reason(
    dep: &Dependency,
    eligible: &[&Release],
    current_order: &ReleaseOrder,
    ceiling_order: Option<&ReleaseOrder>,
    max_major: Option<&MaxMajorPick>,
    latest_tagged: Option<&Release>,
    filters: CeilingFilters,
) -> Option<HeldReason> {
    let newer = eligible
        .iter()
        .copied()
        .filter(|release| release.order > *current_order);
    if dep.pinned && newer.clone().next().is_some() {
        return Some(HeldReason::ExactPin);
    }
    let graph_blocks =
        ceiling_order.is_some_and(|ceiling| newer.clone().any(|r| r.order > *ceiling));
    let declaration_blocks = filters.declared_bound
        && dep.declared_bound.is_some()
        && newer.clone().any(|release| release.beyond_declared_bound);
    let max_major_blocks = max_major.is_some_and(|pick| {
        newer
            .clone()
            .any(|release| !within_max_major(release, Some(pick)))
    });
    let dist_tag_blocks =
        latest_tagged.is_some() && newer.clone().any(|release| release.beyond_latest_tag);

    // The dist-tag comes last: it is the least user-actionable ceiling (the registry owns the tag),
    // so a coincident manifest bound or configured max-major names the hold the user can act on.
    if graph_blocks {
        Some(HeldReason::GraphCeiling)
    } else if declaration_blocks {
        dep.declared_bound
            .as_ref()
            .map(|bound| HeldReason::DeclaredBound(bound.clone()))
    } else if max_major_blocks {
        max_major.map(|pick| HeldReason::MaxMajor(pick.limit))
    } else if dist_tag_blocks {
        latest_tagged.map(|tagged| HeldReason::DistTag(tagged.version.to_string()))
    } else {
        None
    }
}

/// The release the registry's `latest` dist-tag names, recovered from the per-release markers:
/// every release ordered above the tag carries [`Release::beyond_latest_tag`], so the tagged
/// release is the greatest unflagged one. `None` when no release is flagged — no dist-tag data, or
/// the tag already sits at the newest release — in which case there is nothing to cap.
fn latest_tagged_release(releases: &[Release]) -> Option<&Release> {
    if !releases.iter().any(|release| release.beyond_latest_tag) {
        return None;
    }
    releases
        .iter()
        .filter(|release| !release.beyond_latest_tag)
        .max_by(|a, b| a.order.cmp(&b.order))
}

fn commit_pin_verdict(dep: &Dependency, releases: &[Release], now: Timestamp) -> Option<Verdict> {
    if dep.current_quality != ReleaseQuality::Pseudo {
        return None;
    }
    let latest = releases
        .iter()
        .filter(|r| r.quality.is_stable_like() && !r.yanked && visible_at(r, now))
        .max_by(|a, b| a.order.cmp(&b.order))
        .map(|r| r.version.clone());
    Some(Verdict {
        status: Status::Held,
        adoptable_target: None,
        latest,
        candidates: Vec::new(),
        held_reason: Some(HeldReason::CommitPin),
    })
}

/// Classify one newer release as a [`Candidate`]: resolve its per-kind cooldown window and judge its
/// publish instant against that window's cutoff at `now` ([`Exempt`](Status::Exempt) when an `allow`
/// rule waives it, [`UnknownAge`](Status::UnknownAge) when undated). `None` for an unclassifiable
/// jump (no `kind_from_current`) — the adapter classifies every real upgrade, so this only skips.
fn classify_candidate(
    r: &Release,
    dep: &Dependency,
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
) -> Option<Candidate> {
    let kind = r.kind_from_current?;
    let window = resolve(layers, &query(dep, ctx, ResolveKind::Candidate(kind)), now).window;
    let status = if window.exempt {
        Status::Exempt
    } else {
        match r.published_at {
            None => Status::UnknownAge,
            Some(p) if p <= window.cutoff(now) => Status::Adoptable,
            Some(_) => Status::InCooldown,
        }
    };
    Some(Candidate {
        version: r.version.clone(),
        kind,
        window,
        status,
        published_at: r.published_at,
    })
}

/// Evaluates a dependency against its classified releases, producing a per-candidate [`Verdict`].
///
/// This is the engine behind the `outdated` and `upgrade` commands. Given the currently-locked
/// [`Dependency`], the full set of classified [`Release`]s for that package, the resolved policy
/// `layers`, the [`ResolveContext`], and the `now` boundary, it decides for every newer eligible
/// release whether adoption may proceed and aggregates a headline [`Status`].
///
/// # Decision
///
/// Releases are first filtered to the *eligible* set — those adoption could target: stable-like
/// quality (with the prerelease rule from [`ReleaseQuality`], honouring the current pin), within
/// the current major unless [`ResolveContext::allow_major`] is set, and not yanked. Each eligible
/// release newer than the current pin becomes a [`Candidate`]: its per-kind cooldown window is
/// [`resolve`](crate::resolve)d, and its publish instant is judged against that window's
/// [`cutoff`](crate::ResolvedWindow::cutoff) at `now`:
///
/// - [`Status::Exempt`] — an `allow` rule waives the window.
/// - [`Status::UnknownAge`] — no publish time is known; *never* treated as mature (the core's
///   one conservative rule, enforced here).
/// - [`Status::Adoptable`] — published at or before the cutoff, i.e. matured past its window.
/// - [`Status::InCooldown`] — published after the cutoff, still too fresh.
///
/// # Returned verdict
///
/// The [`Verdict`] carries the per-candidate breakdown plus three rollups: `candidates` (ascending
/// by release order), `latest` (the newest eligible version, for context), and `adoptable_target`
/// (the newest candidate that is [`Adoptable`](Status::Adoptable) or [`Exempt`](Status::Exempt), or
/// `None`). The headline `status` is [`Status::Adoptable`] whenever any candidate has matured;
/// otherwise it is the newest candidate's status, or [`Status::UpToDate`] when no newer candidate
/// exists — except when the only newer releases lie above the dependency's
/// [`graph_ceiling`](Dependency::graph_ceiling), an explicit declared upper bound, a configured
/// `max-major`, or the registry's `latest` dist-tag ([`Release::beyond_latest_tag`]), which yields
/// [`Status::Held`] with `latest` still surfacing the newest version. Two
/// further cases override the rollup: exact manifest pins are [`Status::Held`] when there is a
/// candidate to review, and a commit pin (pseudo-version) has no tagged version to compare and
/// yields [`Status::Held`]. If the current pin is absent from `releases` the result is conservatively
/// [`Status::UpToDate`] (`check`, via [`check_pin`], is the real gate and does not rely on this).
///
/// # Examples
///
/// ```
/// use camino::Utf8Path;
/// use cooldown_core::{
///     ByKind, Dependency, ToolId, MajorKey, Origin, PackageId, PolicyLayer, Release,
///     ReleaseOrder, ReleaseQuality, ResolveContext, Rule, Selector, Status, UpdateKind, Version,
///     WindowSpec, evaluate,
/// };
/// use jiff::{SignedDuration, Timestamp};
///
/// // A package locked at 1.0.0 with a fresh 1.0.1 patch released "now".
/// let dep = Dependency {
///     package: PackageId::new(ToolId("cargo"), "widget", None),
///     current: Version::new("1.0.0"),
///     current_quality: ReleaseQuality::Stable,
///     direct: true,
///     artifacts: Vec::new(),
///     graph_floor: None,
///     graph_ceiling: None,
///     declared_bound: None,
///     members: Vec::new(),
///     pinned: false,
/// };
/// let now: Timestamp = "2026-01-08T00:00:00Z".parse()?;
/// let mature: Timestamp = "2026-01-01T00:00:00Z".parse()?;
/// let releases = vec![
///     Release {
///         version: Version::new("1.0.0"),
///         order: ReleaseOrder(vec![0]),
///         major: MajorKey("1".into()),
///         major_number: Some(1),
///         kind_from_current: None,
///         beyond_declared_bound: false,
///         beyond_latest_tag: false,
///         published_at: Some(mature),
///         yanked: false,
///         quality: ReleaseQuality::Stable,
///     },
///     Release {
///         version: Version::new("1.0.1"),
///         order: ReleaseOrder(vec![1]),
///         major: MajorKey("1".into()),
///         major_number: Some(1),
///         kind_from_current: Some(UpdateKind::Patch),
///         beyond_declared_bound: false,
///         beyond_latest_tag: false,
///         published_at: Some(now), // published right now → still cooling
///         yanked: false,
///         quality: ReleaseQuality::Stable,
///     },
/// ];
///
/// // A single 7-day `min-age` policy.
/// let mut layer = PolicyLayer::new(Origin::Default);
/// let mut rule = Rule::new(Selector::Default);
/// rule.window = ByKind::scalar(WindowSpec::MinAge(SignedDuration::from_hours(24 * 7)));
/// layer.rules.push(rule);
///
/// let ctx = ResolveContext {
///     tool: ToolId("cargo"),
///     project: Utf8Path::new("/repo"),
///     allow_major: false,
///     honor_declared_bounds: true,
///     honor_latest_tag: true,
/// };
/// let verdict = evaluate(&dep, &releases, &[layer], &ctx, now);
///
/// assert_eq!(verdict.status, Status::InCooldown);
/// assert_eq!(verdict.latest, Some(Version::new("1.0.1")));
/// assert!(verdict.adoptable_target.is_none()); // 1.0.1 is still too fresh
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
#[must_use]
pub fn evaluate(
    dep: &Dependency,
    releases: &[Release],
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
) -> Verdict {
    evaluate_with_filters(
        dep,
        releases,
        layers,
        ctx,
        now,
        CeilingFilters::standard(ctx),
    )
}

fn evaluate_with_filters(
    dep: &Dependency,
    releases: &[Release],
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
    filters: CeilingFilters,
) -> Verdict {
    debug_assert!(
        releases.is_sorted_by(|a, b| a.order <= b.order),
        "releases must be sorted ascending by ReleaseOrder"
    );

    // A commit pin (pseudo-version) has no tagged version to compare against, so it short-circuits to
    // Held with just the newest stable release as `latest` for context. An exact pin (`==`/`=`) is
    // also Held, but it *is* a tagged version, so it flows through normal candidate evaluation below.
    // That way its `adoptable_target` still reports the newest matured version, i.e. exactly which
    // version could be manually pinned to.
    if let Some(verdict) = commit_pin_verdict(dep, releases, now) {
        return verdict;
    }

    let Some(current) = releases.iter().find(|r| r.version == dep.current) else {
        // Defensive: the adapter is expected to include the current pin among `releases`. Without
        // its order we cannot classify upgrades, so we conservatively report up-to-date rather
        // than inventing spurious candidates (`check` is the real gate and does not rely on this).
        return Verdict {
            status: Status::UpToDate,
            adoptable_target: None,
            latest: Some(dep.current.clone()),
            candidates: Vec::new(),
            held_reason: None,
        };
    };
    let current_order = current.order.clone();
    let current_major = current.major.clone();

    // A requirer may pin this dependency exactly (`==`), capping it below newer releases; candidates
    // ordered above that ceiling are excluded (the upgrade-direction mirror of `graph_floor`). A
    // ceiling below the current version is not a real upper bound — the graph resolved past it — so
    // it is ignored, leaving a legal upgrade free rather than wrongly holding the dependency.
    let ceiling_order = graph_ceiling_order(dep, releases).filter(|order| **order >= current_order);
    let max_major = active_max_major(current, dep, layers, ctx, filters.max_major);
    // The dist-tag cap applies only while the current pin sits at or below the tag: a pin already
    // beyond it (a project deliberately riding a `next` line) deactivates the ceiling entirely, so
    // that project keeps seeing newer releases instead of a downgrade-or-silence dead end — once
    // the project has knowingly passed the tag, the tag carries no guidance about where to stop.
    let latest_tagged = if filters.latest_tag && !current.beyond_latest_tag {
        latest_tagged_release(releases)
    } else {
        None
    };

    // Eligible = the releases adoption could target (quality + major filter + not yanked, and not
    // dated after `now`), current included, so `latest` is well-defined even when up to date.
    let eligible: Vec<&Release> = releases
        .iter()
        .filter(|r| {
            quality_eligible(r, dep.current_quality)
                && major_eligible(r, &current_major, ctx.allow_major)
                && !r.yanked
                && (r.version == dep.current || visible_at(r, now))
        })
        .collect();

    let latest = eligible
        .iter()
        .max_by(|a, b| a.order.cmp(&b.order))
        .map(|r| r.version.clone())
        .or_else(|| Some(dep.current.clone()));

    // Each newer eligible release within the ceiling becomes a candidate; the headline status and
    // adoptable target are rolled up from this set below.
    let candidates: Vec<Candidate> = eligible
        .iter()
        .copied()
        .filter(|r| {
            r.order > current_order
                && ceiling_order.is_none_or(|c| r.order <= *c)
                && within_max_major(r, max_major.as_ref())
                && !(filters.declared_bound
                    && dep.declared_bound.is_some()
                    && r.beyond_declared_bound)
                && !(latest_tagged.is_some() && r.beyond_latest_tag)
        })
        .filter_map(|r| classify_candidate(r, dep, layers, ctx, now))
        .collect();

    // `candidates` is in ascending order (from sorted releases); the headline is the newest. An
    // empty candidate set means no newer *admissible* release — "up to date", unless a ceiling
    // excluded one. An exact pin remains the primary reason it cannot move even when a second
    // ceiling also applies; this keeps pin filtering and the human explanation consistent.
    let Some(headline) = candidates.last() else {
        let held_reason = empty_candidate_held_reason(
            dep,
            &eligible,
            &current_order,
            ceiling_order,
            max_major.as_ref(),
            latest_tagged,
            filters,
        );
        return Verdict {
            status: if held_reason.is_some() {
                Status::Held
            } else {
                Status::UpToDate
            },
            adoptable_target: None,
            latest,
            candidates,
            held_reason,
        };
    };
    let adoptable_target = candidates
        .iter()
        .rev()
        .find(|c| matches!(c.status, Status::Adoptable | Status::Exempt))
        .map(|c| c.version.clone());

    // The status reflects whether you can act *now*, not just the newest candidate's freshness. If
    // any candidate has matured past its window (`adoptable_target` is set), the row is `Adoptable`
    // even when the very newest version is still cooling — `upgrade` would take the matured one. So
    // `InCooldown` is reserved for "something newer exists but nothing has matured yet", the only case
    // that truly means "cannot update yet". Two overrides: an exact pin is `Held` (it won't move on
    // its own, though `adoptable_target` still shows what one could manually pin to); an `Exempt`
    // headline keeps its label (the cooldown was explicitly waived for it).
    let status = if dep.pinned {
        Status::Held
    } else if headline.status == Status::Exempt {
        Status::Exempt
    } else if adoptable_target.is_some() {
        Status::Adoptable
    } else {
        headline.status
    };

    Verdict {
        status,
        adoptable_target,
        latest,
        candidates,
        held_reason: dep.pinned.then_some(HeldReason::ExactPin),
    }
}

/// Finds the newest matured target hidden by a declared bound, a configured `max-major`, or the
/// registry's `latest` dist-tag.
///
/// The ordinary graph ceiling and the context's major scope remain active; only the package-owned
/// ceilings are probed. Each ceiling is probed *individually*: the reported hold names a ceiling
/// whose removal alone exposes the reported target, so the action its reason implies (rewriting
/// the bound, raising `max-major`, opting out via `respect-dist-tags = false`) is sufficient to
/// reach that target — with two ceilings stacked, naming the outer one against the jointly hidden
/// target would promise a version the named action cannot expose. Only when no single ceiling is
/// causal does the hold fall back to the first violated ceiling in the same actionability order
/// (dist-tag last — the registry owns the tag) against the jointly exposed target: staged
/// guidance, where lifting the named ceiling is necessary and the next run names the remaining
/// one.
#[must_use]
pub fn evaluate_ceiling_hold(
    dep: &Dependency,
    releases: &[Release],
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
) -> Option<CeilingHold> {
    if dep.pinned || dep.current_quality == ReleaseQuality::Pseudo {
        return None;
    }

    let filters = CeilingFilters::standard(ctx);
    let bounded = evaluate_with_filters(dep, releases, layers, ctx, now, filters);
    let unbounded =
        evaluate_with_filters(dep, releases, layers, ctx, now, CeilingFilters::unbounded());
    let target = unbounded.adoptable_target?;
    if bounded.adoptable_target.as_ref() == Some(&target) {
        return None;
    }
    let current = releases
        .iter()
        .find(|release| release.version == dep.current)?;

    let singly_causal = |reason: CeilingReason, lifted: CeilingFilters| -> Option<CeilingHold> {
        let probe = evaluate_with_filters(dep, releases, layers, ctx, now, lifted);
        let target = probe.adoptable_target?;
        if bounded.adoptable_target.as_ref() == Some(&target) {
            return None;
        }
        let update_kind = probe
            .candidates
            .iter()
            .find(|candidate| candidate.version == target)?
            .kind;
        Some(CeilingHold {
            reason,
            target,
            update_kind,
        })
    };
    if filters.declared_bound
        && dep.declared_bound.is_some()
        && let Some(hold) = singly_causal(
            CeilingReason::DeclaredBound,
            CeilingFilters {
                declared_bound: false,
                ..filters
            },
        )
    {
        return Some(hold);
    }
    if active_max_major(current, dep, layers, ctx, filters.max_major).is_some()
        && let Some(hold) = singly_causal(
            CeilingReason::MaxMajor,
            CeilingFilters {
                max_major: false,
                ..filters
            },
        )
    {
        return Some(hold);
    }
    if filters.latest_tag
        && !current.beyond_latest_tag
        && releases.iter().any(|release| release.beyond_latest_tag)
        && let Some(hold) = singly_causal(
            CeilingReason::DistTag,
            CeilingFilters {
                latest_tag: false,
                ..filters
            },
        )
    {
        return Some(hold);
    }

    // No single ceiling exposes anything by itself — only the stack's joint removal reaches
    // `target`. Name the first ceiling the target provably violates (staged guidance, see above).
    let target_release = releases.iter().find(|release| release.version == target)?;
    let reason = if filters.declared_bound
        && dep.declared_bound.is_some()
        && target_release.beyond_declared_bound
    {
        CeilingReason::DeclaredBound
    } else {
        let max_major = active_max_major(current, dep, layers, ctx, filters.max_major);
        if !within_max_major(target_release, max_major.as_ref()) {
            CeilingReason::MaxMajor
        } else if filters.latest_tag
            && !current.beyond_latest_tag
            && target_release.beyond_latest_tag
        {
            CeilingReason::DistTag
        } else {
            return None;
        }
    };
    let update_kind = unbounded
        .candidates
        .iter()
        .find(|candidate| candidate.version == target)?
        .kind;

    Some(CeilingHold {
        reason,
        target,
        update_kind,
    })
}

/// Judges the currently-locked release against the cooldown policy — the `check` gate.
///
/// Where [`evaluate`] reasons about *upgrade candidates*, `check_pin` reasons about the version
/// already in the lockfile: is the release the project currently depends on old enough to satisfy
/// the policy? Because a locked pin has no from→to [`UpdateKind`], it resolves the bare `min-age`
/// window (the [`ResolveKind::CurrentPin`](crate::ResolveKind) field) and judges `locked`'s
/// publish instant against that window's [`cutoff`](crate::ResolvedWindow::cutoff) at `now`.
///
/// # Decision
///
/// - [`Status::Exempt`] — an `allow` rule waives the window, or `locked` is a pseudo-version /
///   commit pin (no tagged version to quarantine against).
/// - [`Status::UnknownAge`] — the locked release has no known publish time; never mature.
/// - [`Status::UpToDate`] — published at or before the cutoff; the pin passes the gate.
/// - [`Status::CurrentInCooldown`] — published after the cutoff; the pin is too fresh, a violation.
///
/// # Returned verdict
///
/// The [`PinVerdict`] carries the `status`, the resolved [`window`](crate::ResolvedWindow), and the
/// `published_at` instant for rendering. It additionally annotates whether the resolved graph forces
/// this pin: when [`Dependency::graph_floor`] *or* [`Dependency::graph_ceiling`] equals the locked
/// version, `graph_held` is set (a ceiling comes from an exact requirer pin, which holds the version
/// from above and below alike). A graph-held but too-fresh pin is *still* a
/// [`Status::CurrentInCooldown`] violation — the flag lets it be baselined deliberately rather than
/// silently passed.
#[must_use]
pub fn check_pin(
    dep: &Dependency,
    locked: &Release,
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
) -> PinVerdict {
    let res = resolve(layers, &query(dep, ctx, ResolveKind::CurrentPin), now);
    let window = res.window;

    let status = if window.exempt || locked.quality == ReleaseQuality::Pseudo {
        // An `allow` exemption, or a pseudo-version/commit pin with no tagged version to
        // quarantine against → exempt.
        Status::Exempt
    } else {
        match locked.published_at {
            None => Status::UnknownAge,
            Some(p) if p <= window.cutoff(now) => Status::UpToDate, // mature: passes the gate
            Some(_) => Status::CurrentInCooldown,                   // a violation
        }
    };

    // A `graph_floor` equal to the locked version holds the pin from below; a `graph_ceiling` equal
    // to it holds it from above. Both of cooldown's ceilings come from exact (`==`/`=`) requirer pins,
    // which lock the version in *both* directions, so a ceiling at the locked version means the pin
    // cannot be downgraded either — `fix` must leave it for a human even when no floor was computed
    // (hex/rubygems/conda never compute a floor; uv skips editable/path requirers).
    let graph_held = matches!(&dep.graph_floor, Some(v) if *v == locked.version)
        || matches!(&dep.graph_ceiling, Some(v) if *v == locked.version);

    PinVerdict {
        status,
        window,
        graph_held,
        graph_floor: dep.graph_floor.clone(),
        published_at: locked.published_at,
    }
}

/// The downgrade plan for one dependency under `fix`: whether its currently-locked version violates
/// the cooldown and, if so, the newest already-matured version to roll back to.
#[derive(Debug, Clone)]
pub struct FixVerdict {
    /// The current pin's [`check_pin`] verdict. Only [`Status::CurrentInCooldown`] needs fixing;
    /// [`PinVerdict::graph_held`] means the graph itself requires the too-fresh version, so `fix`
    /// must leave it in place for a human to baseline or resolve upstream.
    pub current: PinVerdict,
    /// The newest matured version older than the current pin — the downgrade target. `None` when the
    /// pin is already compliant, no older version has matured, or the graph holds the pin at the
    /// violating version.
    pub target: Option<Version>,
}

/// Decide whether `dep`'s locked version is too fresh and, if so, the newest matured version older
/// than it to downgrade to — the dual of [`evaluate`].
///
/// Where [`evaluate`] searches *newer* releases for the newest one safe to adopt, this searches
/// *older* releases for the newest one already past the cooldown: the minimal downgrade that makes
/// [`check_pin`] pass for this dependency. The target stays within the current major unless
/// [`ResolveContext::allow_major`] is set, is quality-eligible and not yanked, and is judged against
/// the same current-pin window [`check_pin`] uses, so the chosen version is one `check` will accept.
#[must_use]
pub fn evaluate_fix(
    dep: &Dependency,
    releases: &[Release],
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
) -> FixVerdict {
    let Some(current) = releases.iter().find(|r| r.version == dep.current) else {
        // The adapter did not surface the locked version among the releases, so its age cannot be
        // judged here; `check` remains the real gate.
        return FixVerdict {
            current: unknown_pin_verdict(dep, layers, ctx, now),
            target: None,
        };
    };
    let pin = check_pin(dep, current, layers, ctx, now);
    if pin.status != Status::CurrentInCooldown || pin.graph_held {
        return FixVerdict {
            current: pin,
            target: None,
        };
    }
    let cutoff = pin.window.cutoff(now);
    let max_major = active_max_major(current, dep, layers, ctx, true);
    // Never roll below the graph floor: the resolved graph requires at least that version, so a lower
    // one would not actually be selected (and would be re-bumped on the next lock). When the floor is
    // not among the fetched releases, fall back to no lower bound.
    let floor_order = dep
        .graph_floor
        .as_ref()
        .and_then(|floor| releases.iter().find(|r| r.version == *floor))
        .map(|r| r.order.clone());
    let target = releases
        .iter()
        .filter(|r| r.order < current.order)
        .filter(|r| floor_order.as_ref().is_none_or(|floor| r.order >= *floor))
        .filter(|r| {
            quality_eligible(r, dep.current_quality)
                && major_eligible(r, &current.major, ctx.allow_major)
                && within_max_major(r, max_major.as_ref())
                && !(ctx.honor_declared_bounds
                    && dep.declared_bound.is_some()
                    && r.beyond_declared_bound)
                && !r.yanked
        })
        .filter(|r| matches!(r.published_at, Some(published) if published <= cutoff))
        .max_by(|a, b| a.order.cmp(&b.order))
        .map(|r| r.version.clone());
    FixVerdict {
        current: pin,
        target,
    }
}

fn unknown_pin_verdict(
    dep: &Dependency,
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
) -> PinVerdict {
    let res = resolve(layers, &query(dep, ctx, ResolveKind::CurrentPin), now);
    PinVerdict {
        status: Status::UnknownAge,
        window: res.window,
        graph_held: matches!(&dep.graph_floor, Some(v) if *v == dep.current)
            || matches!(&dep.graph_ceiling, Some(v) if *v == dep.current),
        graph_floor: dep.graph_floor.clone(),
        published_at: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ReleaseOrder, Version};

    fn release(version: &str, major: &str, kind: Option<UpdateKind>) -> Release {
        Release {
            version: Version::new(version),
            order: ReleaseOrder(Vec::new()),
            major: MajorKey(major.to_string()),
            major_number: major.parse().ok(),
            kind_from_current: kind,
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: None,
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
    }

    #[test]
    fn no_major_rejects_semver_major_jump_sharing_the_base_path() {
        // v0.36.1 → v11.0.0+incompatible shares the empty base-path MajorKey, but it is a semver
        // major jump. `--no-major` must reject it; `--major` admits it. Guards against a `+incompatible`
        // major slipping past the path-only `MajorKey` check.
        let candidate = release("v11.0.0+incompatible", "", Some(UpdateKind::Major));
        let base = MajorKey(String::new());
        assert!(!major_eligible(&candidate, &base, false));
        assert!(major_eligible(&candidate, &base, true));
    }

    #[test]
    fn no_major_admits_same_major_minor_and_patch() {
        let base = MajorKey(String::new());
        let minor = release("v0.37.0", "", Some(UpdateKind::Minor));
        let patch = release("v0.36.2", "", Some(UpdateKind::Patch));
        assert!(major_eligible(&minor, &base, false));
        assert!(major_eligible(&patch, &base, false));
    }

    fn dated(version: &str, order: u8, published: &str) -> Release {
        Release {
            version: Version::new(version),
            order: ReleaseOrder(vec![order]),
            major: MajorKey("1".into()),
            major_number: Some(1),
            kind_from_current: Some(UpdateKind::Patch),
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: Some(published.parse().expect("timestamp")),
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
    }

    fn classified(
        version: &str,
        order: u8,
        major: Option<u64>,
        kind: Option<UpdateKind>,
    ) -> Release {
        Release {
            version: Version::new(version),
            order: ReleaseOrder(vec![order]),
            major: MajorKey(major.map_or_else(String::new, |major| major.to_string())),
            major_number: major,
            kind_from_current: kind,
            beyond_declared_bound: false,
            beyond_latest_tag: false,
            published_at: Some("2025-12-01T00:00:00Z".parse().expect("timestamp")),
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
    }

    fn max_major_layer(limit: u64) -> PolicyLayer {
        let mut layer = PolicyLayer::new(crate::Origin::Repo("cooldown.toml".into()));
        let mut rule = crate::Rule::new(crate::Selector::Package {
            glob: crate::PatternGlob::new("widget").expect("glob"),
            tool: Some(ToolId("cargo")),
        });
        rule.max_major = Some(limit);
        layer.rules.push(rule);
        layer
    }

    fn fix_dep(current: &str) -> Dependency {
        Dependency {
            package: crate::PackageId::new(ToolId("cargo"), "widget", None),
            current: Version::new(current),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            declared_bound: None,
            members: Vec::new(),
            pinned: false,
        }
    }

    fn seven_day_layer() -> PolicyLayer {
        let mut layer = PolicyLayer::new(crate::Origin::Default);
        let mut rule = crate::Rule::new(crate::Selector::Default);
        rule.window = crate::ByKind::scalar(crate::WindowSpec::MinAge(
            jiff::SignedDuration::from_hours(24 * 7),
        ));
        layer.rules.push(rule);
        layer
    }

    fn ctx() -> ResolveContext<'static> {
        ResolveContext {
            tool: ToolId("cargo"),
            project: Utf8Path::new("/repo"),
            allow_major: false,
            honor_declared_bounds: true,
            honor_latest_tag: true,
        }
    }

    #[test]
    fn graph_ceiling_holds_a_transitive_pinned_at_its_current_version() {
        // A requirer pins this dependency `==1.0.0`, so the graph forbids moving up even though 1.0.1
        // has matured — the upgrade-direction mirror of `graph_floor`. The dep is `Held`, with
        // `latest` still surfacing the newer version for context and no adoptable target.
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"), // current, matured
            dated("1.0.1", 1, "2025-12-15T00:00:00Z"), // newer, matured — but above the ceiling
        ];
        let mut dep = fix_dep("1.0.0");
        dep.graph_ceiling = Some(Version::new("1.0.0"));
        let verdict = evaluate(&dep, &releases, &[seven_day_layer()], &ctx(), now);
        assert_eq!(verdict.status, Status::Held);
        assert_eq!(verdict.held_reason, Some(HeldReason::GraphCeiling));
        assert_eq!(verdict.latest, Some(Version::new("1.0.1")));
        assert_eq!(verdict.adoptable_target, None);
        assert!(verdict.candidates.is_empty());

        // Without the ceiling the same matured 1.0.1 is freely adoptable — the ceiling is the only
        // thing holding it.
        dep.graph_ceiling = None;
        let verdict = evaluate(&dep, &releases, &[seven_day_layer()], &ctx(), now);
        assert_eq!(verdict.status, Status::Adoptable);
        assert_eq!(verdict.adoptable_target, Some(Version::new("1.0.1")));
    }

    #[test]
    fn max_major_holds_cross_major_and_keeps_latest_context() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            classified("5.9.0", 0, Some(5), None),
            classified("6.0.0", 1, Some(6), Some(UpdateKind::Major)),
        ];
        let mut context = ctx();
        context.allow_major = true;
        let verdict = evaluate(
            &fix_dep("5.9.0"),
            &releases,
            &[seven_day_layer(), max_major_layer(5)],
            &context,
            now,
        );
        assert_eq!(verdict.status, Status::Held);
        assert_eq!(verdict.held_reason, Some(HeldReason::MaxMajor(5)));
        assert_eq!(verdict.latest, Some(Version::new("6.0.0")));
        assert!(verdict.candidates.is_empty());
    }

    #[test]
    fn ceilings_admit_matured_candidates_within_the_allowed_line() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let mut beyond = classified("7.0.2", 2, Some(7), Some(UpdateKind::Major));
        beyond.beyond_declared_bound = true;
        let releases = vec![
            classified("5.9.3", 0, Some(5), None),
            classified("5.9.4", 1, Some(5), Some(UpdateKind::Patch)),
            beyond,
        ];
        let mut dependency = fix_dep("5.9.3");
        dependency.declared_bound = Some("<6".to_string());
        let mut context = ctx();
        context.allow_major = true;
        let verdict = evaluate(
            &dependency,
            &releases,
            &[seven_day_layer(), max_major_layer(5)],
            &context,
            now,
        );
        assert_eq!(verdict.status, Status::Adoptable);
        assert_eq!(verdict.adoptable_target, Some(Version::new("5.9.4")));
        assert_eq!(verdict.latest, Some(Version::new("7.0.2")));
        assert_eq!(verdict.held_reason, None);
    }

    #[test]
    fn max_major_is_inert_when_the_current_release_already_exceeds_it() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            classified("6.0.0", 0, Some(6), None),
            classified("6.1.0", 1, Some(6), Some(UpdateKind::Minor)),
        ];
        let verdict = evaluate(
            &fix_dep("6.0.0"),
            &releases,
            &[seven_day_layer(), max_major_layer(5)],
            &ctx(),
            now,
        );
        assert_eq!(verdict.status, Status::Adoptable);
        assert_eq!(verdict.adoptable_target, Some(Version::new("6.1.0")));
    }

    #[test]
    fn max_major_conservatively_excludes_an_unknown_numeric_major() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            classified("5.9.0", 0, Some(5), None),
            classified("next", 1, None, Some(UpdateKind::Major)),
        ];
        let mut context = ctx();
        context.allow_major = true;
        let verdict = evaluate(
            &fix_dep("5.9.0"),
            &releases,
            &[seven_day_layer(), max_major_layer(5)],
            &context,
            now,
        );
        assert_eq!(verdict.held_reason, Some(HeldReason::MaxMajor(5)));
    }

    #[test]
    fn declared_bound_holds_unless_the_context_allows_rewriting_it() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let mut newer = classified("6.0.0", 1, Some(6), Some(UpdateKind::Major));
        newer.beyond_declared_bound = true;
        let releases = vec![classified("5.9.0", 0, Some(5), None), newer];
        let mut dependency = fix_dep("5.9.0");
        dependency.declared_bound = Some(">=5, <6".to_string());
        let mut context = ctx();
        context.allow_major = true;

        let held = evaluate(&dependency, &releases, &[seven_day_layer()], &context, now);
        assert_eq!(
            held.held_reason,
            Some(HeldReason::DeclaredBound(">=5, <6".to_string()))
        );

        context.honor_declared_bounds = false;
        let rewritten = evaluate(&dependency, &releases, &[seven_day_layer()], &context, now);
        assert_eq!(rewritten.status, Status::Adoptable);
        assert_eq!(rewritten.adoptable_target, Some(Version::new("6.0.0")));
    }

    #[test]
    fn held_reason_precedence_is_graph_then_bound_then_max_major() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let mut newer = classified("6.0.0", 1, Some(6), Some(UpdateKind::Major));
        newer.beyond_declared_bound = true;
        let releases = vec![classified("5.9.0", 0, Some(5), None), newer];
        let mut dependency = fix_dep("5.9.0");
        dependency.graph_ceiling = Some(Version::new("5.9.0"));
        dependency.declared_bound = Some("<6".to_string());
        let mut context = ctx();
        context.allow_major = true;
        let layers = [seven_day_layer(), max_major_layer(5)];

        let graph = evaluate(&dependency, &releases, &layers, &context, now);
        assert_eq!(graph.held_reason, Some(HeldReason::GraphCeiling));

        dependency.graph_ceiling = None;
        let bound = evaluate(&dependency, &releases, &layers, &context, now);
        assert_eq!(
            bound.held_reason,
            Some(HeldReason::DeclaredBound("<6".to_string()))
        );

        dependency.declared_bound = None;
        let max_major = evaluate(&dependency, &releases, &layers, &context, now);
        assert_eq!(max_major.held_reason, Some(HeldReason::MaxMajor(5)));
    }

    #[test]
    fn exact_pin_remains_the_reason_when_a_package_ceiling_also_blocks_candidates() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            classified("5.9.0", 0, Some(5), None),
            classified("6.0.0", 1, Some(6), Some(UpdateKind::Major)),
        ];
        let mut dependency = fix_dep("5.9.0");
        dependency.pinned = true;
        let mut context = ctx();
        context.allow_major = true;

        let verdict = evaluate(
            &dependency,
            &releases,
            &[seven_day_layer(), max_major_layer(5)],
            &context,
            now,
        );

        assert_eq!(verdict.status, Status::Held);
        assert_eq!(verdict.held_reason, Some(HeldReason::ExactPin));
    }

    #[test]
    fn ceiling_probe_reports_only_the_package_ceiling_hiding_the_matured_target() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let mut newer = classified("6.0.0", 1, Some(6), Some(UpdateKind::Major));
        newer.beyond_declared_bound = true;
        let releases = vec![classified("5.9.0", 0, Some(5), None), newer];
        let mut dependency = fix_dep("5.9.0");
        dependency.declared_bound = Some("<6".to_string());
        let mut context = ctx();
        context.allow_major = true;
        let layers = [seven_day_layer(), max_major_layer(5)];

        let declared = evaluate_ceiling_hold(&dependency, &releases, &layers, &context, now)
            .expect("declared hold");
        assert_eq!(declared.reason, CeilingReason::DeclaredBound);
        assert_eq!(declared.target, Version::new("6.0.0"));
        assert_eq!(declared.update_kind, UpdateKind::Major);

        context.honor_declared_bounds = false;
        let configured = evaluate_ceiling_hold(&dependency, &releases, &layers, &context, now)
            .expect("configured hold");
        assert_eq!(configured.reason, CeilingReason::MaxMajor);
    }

    #[test]
    fn fix_never_targets_a_release_beyond_either_ceiling() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let safe = classified("5.8.0", 0, Some(5), Some(UpdateKind::Minor));
        let mut beyond_bound = classified("5.8.1", 1, Some(5), Some(UpdateKind::Patch));
        beyond_bound.beyond_declared_bound = true;
        let beyond_max = classified("6.0.0", 2, Some(6), Some(UpdateKind::Major));
        let mut current = classified("5.9.0", 3, Some(5), None);
        current.published_at = Some("2026-01-07T00:00:00Z".parse().expect("timestamp"));

        let mut dependency = fix_dep("5.9.0");
        dependency.declared_bound = Some("<5.8.1".to_string());
        let mut context = ctx();
        context.allow_major = true;
        let verdict = evaluate_fix(
            &dependency,
            &[safe, beyond_bound, beyond_max, current],
            &[seven_day_layer(), max_major_layer(5)],
            &context,
            now,
        );

        assert_eq!(verdict.current.status, Status::CurrentInCooldown);
        assert_eq!(verdict.target, Some(Version::new("5.8.0")));
    }

    #[test]
    fn graph_ceiling_caps_candidates_but_admits_those_at_or_below_it() {
        // The graph permits up to 1.1.0 (a requirer's `==1.1.0`): 1.1.0 is an ordinary adoptable
        // candidate while 1.2.0 above the ceiling is excluded — `latest` still shows 1.2.0.
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"),
            dated("1.1.0", 1, "2025-12-15T00:00:00Z"), // matured, at the ceiling
            dated("1.2.0", 2, "2025-12-20T00:00:00Z"), // matured, above the ceiling
        ];
        let mut dep = fix_dep("1.0.0");
        dep.graph_ceiling = Some(Version::new("1.1.0"));
        let verdict = evaluate(&dep, &releases, &[seven_day_layer()], &ctx(), now);
        assert_eq!(verdict.status, Status::Adoptable);
        assert_eq!(verdict.adoptable_target, Some(Version::new("1.1.0")));
        assert_eq!(verdict.latest, Some(Version::new("1.2.0")));
        assert_eq!(verdict.candidates.len(), 1);
        assert_eq!(verdict.candidates[0].version, Version::new("1.1.0"));
    }

    #[test]
    fn fix_targets_newest_matured_version_older_than_a_too_fresh_pin() {
        // Window cutoff is 2026-01-01; 1.0.0 and 1.0.1 have matured, 1.0.2 (the pin) is too fresh.
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"),
            dated("1.0.1", 1, "2025-12-15T00:00:00Z"),
            dated("1.0.2", 2, "2026-01-07T00:00:00Z"),
        ];
        let verdict = evaluate_fix(
            &fix_dep("1.0.2"),
            &releases,
            &[seven_day_layer()],
            &ctx(),
            now,
        );
        assert_eq!(verdict.current.status, Status::CurrentInCooldown);
        assert_eq!(verdict.target, Some(Version::new("1.0.1")));
    }

    #[test]
    fn fix_leaves_a_compliant_pin_alone() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"),
            dated("1.0.1", 1, "2025-12-15T00:00:00Z"),
        ];
        // 1.0.1 matured on 2025-12-15, before the 2026-01-01 cutoff → already compliant.
        let verdict = evaluate_fix(
            &fix_dep("1.0.1"),
            &releases,
            &[seven_day_layer()],
            &ctx(),
            now,
        );
        assert_eq!(verdict.current.status, Status::UpToDate);
        assert_eq!(verdict.target, None);
    }

    #[test]
    fn fix_reports_no_target_when_no_older_version_has_matured() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        // Every release is younger than the cutoff, so there is nothing safe to downgrade to.
        let releases = vec![
            dated("1.0.0", 0, "2026-01-05T00:00:00Z"),
            dated("1.0.1", 1, "2026-01-07T00:00:00Z"),
        ];
        let verdict = evaluate_fix(
            &fix_dep("1.0.1"),
            &releases,
            &[seven_day_layer()],
            &ctx(),
            now,
        );
        assert_eq!(verdict.current.status, Status::CurrentInCooldown);
        assert_eq!(verdict.target, None);
    }

    #[test]
    fn releases_dated_after_now_are_not_yet_visible() {
        // With a fixed clock injected (an "as-of" view), a release published after `now` does not
        // exist yet: it is neither the `latest` nor a candidate, so the report stays honest — no
        // versions from the future and no negative ages. Under the real clock nothing is ever
        // future-dated, so this guard never fires in production.
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"), // the pin
            dated("1.0.1", 1, "2025-12-20T00:00:00Z"), // matured before the cutoff → adoptable
            dated("1.0.2", 2, "2026-02-01T00:00:00Z"), // published AFTER now → not yet visible
        ];
        let verdict = evaluate(
            &fix_dep("1.0.0"),
            &releases,
            &[seven_day_layer()],
            &ctx(),
            now,
        );
        assert_eq!(
            verdict.latest,
            Some(Version::new("1.0.1")),
            "the future-dated 1.0.2 must not become the latest"
        );
        assert_eq!(verdict.adoptable_target, Some(Version::new("1.0.1")));
        assert!(
            verdict
                .candidates
                .iter()
                .all(|c| c.version != Version::new("1.0.2")),
            "a release dated after now must not be a candidate"
        );
    }

    #[test]
    fn fix_target_never_rolls_below_the_graph_floor() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        // 1.0.0/1.0.1 matured; 1.0.2/1.0.3 are too fresh. The pin is 1.0.3.
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"),
            dated("1.0.1", 1, "2025-12-15T00:00:00Z"),
            dated("1.0.2", 2, "2026-01-06T00:00:00Z"),
            dated("1.0.3", 3, "2026-01-07T00:00:00Z"),
        ];

        // Floor 1.0.1: the newest matured version at or above the floor is 1.0.1 — that is the target.
        let mut at_floor = fix_dep("1.0.3");
        at_floor.graph_floor = Some(Version::new("1.0.1"));
        let verdict = evaluate_fix(&at_floor, &releases, &[seven_day_layer()], &ctx(), now);
        assert_eq!(verdict.target, Some(Version::new("1.0.1")));

        // Floor 1.0.2: the only matured older versions (1.0.0, 1.0.1) sit below the floor, so there
        // is nothing safe to roll back to — never pick a version the graph forbids.
        let mut below_floor = fix_dep("1.0.3");
        below_floor.graph_floor = Some(Version::new("1.0.2"));
        let verdict = evaluate_fix(&below_floor, &releases, &[seven_day_layer()], &ctx(), now);
        assert_eq!(verdict.current.status, Status::CurrentInCooldown);
        assert!(!verdict.current.graph_held, "floor 1.0.2 < pin 1.0.3");
        assert_eq!(verdict.target, None);
    }

    #[test]
    fn fix_does_not_target_graph_held_violation() {
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"),
            dated("1.0.1", 1, "2026-01-07T00:00:00Z"),
        ];
        let mut dep = fix_dep("1.0.1");
        dep.graph_floor = Some(Version::new("1.0.1"));
        let verdict = evaluate_fix(&dep, &releases, &[seven_day_layer()], &ctx(), now);
        assert_eq!(verdict.current.status, Status::CurrentInCooldown);
        assert!(verdict.current.graph_held);
        assert_eq!(verdict.target, None);
    }

    #[test]
    fn fix_does_not_downgrade_a_ceiling_held_pin_without_a_floor() {
        // A transitive dep pinned `==1.0.1` by a requirer carries only a `graph_ceiling` (hex/
        // rubygems/conda never compute a floor; uv skips editable requirers). The `==` locks it in
        // both directions, so `fix` must leave the too-fresh pin in place rather than plan a
        // downgrade the requirer would re-bump.
        let now: Timestamp = "2026-01-08T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("1.0.0", 0, "2025-12-01T00:00:00Z"),
            dated("1.0.1", 1, "2026-01-07T00:00:00Z"),
        ];
        let mut dep = fix_dep("1.0.1");
        dep.graph_floor = None;
        dep.graph_ceiling = Some(Version::new("1.0.1"));
        let verdict = evaluate_fix(&dep, &releases, &[seven_day_layer()], &ctx(), now);
        assert_eq!(verdict.current.status, Status::CurrentInCooldown);
        assert!(verdict.current.graph_held);
        assert_eq!(verdict.target, None);
    }

    #[test]
    fn cooldown_horizon_picks_latest_or_soonest_to_mature() {
        // Mirrors the ruff scenario: locked at 0.15.15 with three newer patches. With a 7-day window
        // and now = 2026-06-17 (cutoff 2026-06-10), 0.15.16 has matured (adoptable) while 0.15.17 and
        // 0.15.18 are still cooling. 0.15.18 is the freshest, but 0.15.17 matures three days sooner.
        let now: Timestamp = "2026-06-17T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("0.15.15", 0, "2026-01-01T00:00:00Z"),
            dated("0.15.16", 1, "2026-06-05T00:00:00Z"), // matured before the cutoff → adoptable
            dated("0.15.17", 2, "2026-06-13T00:00:00Z"), // cooling, matures 2026-06-20
            dated("0.15.18", 3, "2026-06-16T00:00:00Z"), // cooling, matures 2026-06-23 (the newest)
        ];
        let verdict = evaluate(
            &fix_dep("0.15.15"),
            &releases,
            &[seven_day_layer()],
            &ctx(),
            now,
        );

        // The horizon never moves the decision: 0.15.16 is adoptable, 0.15.18 is the latest.
        assert_eq!(verdict.adoptable_target, Some(Version::new("0.15.16")));
        assert_eq!(verdict.latest, Some(Version::new("0.15.18")));

        // `Latest` (the default) reports the newest candidate; `Soonest` reports the cooling
        // candidate that unlocks first — 0.15.17, not the freshest 0.15.18.
        let latest = verdict
            .cooldown_candidate(crate::CooldownHorizon::Latest, now)
            .expect("a candidate");
        assert_eq!(latest.version, Version::new("0.15.18"));
        let soonest = verdict
            .cooldown_candidate(crate::CooldownHorizon::Soonest, now)
            .expect("a candidate");
        assert_eq!(soonest.version, Version::new("0.15.17"));
    }

    #[test]
    fn soonest_horizon_falls_back_to_newest_when_nothing_cools() {
        // Every newer release has already matured (cutoff 2026-06-10), so there is no cooling
        // candidate to count down to — `Soonest` then matches `Latest` (the newest candidate).
        let now: Timestamp = "2026-06-17T00:00:00Z".parse().expect("now");
        let releases = vec![
            dated("0.15.15", 0, "2026-01-01T00:00:00Z"),
            dated("0.15.16", 1, "2026-06-01T00:00:00Z"),
            dated("0.15.17", 2, "2026-06-05T00:00:00Z"),
        ];
        let verdict = evaluate(
            &fix_dep("0.15.15"),
            &releases,
            &[seven_day_layer()],
            &ctx(),
            now,
        );
        let soonest = verdict
            .cooldown_candidate(crate::CooldownHorizon::Soonest, now)
            .expect("a candidate");
        assert_eq!(soonest.version, Version::new("0.15.17"));
    }
}
