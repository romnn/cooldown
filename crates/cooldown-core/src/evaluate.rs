//! The cooldown decision — the single source of truth for every tool.
//!
//! [`evaluate`] drives `outdated`/`upgrade` over a candidate set; [`check_pin`] is the gate over
//! the currently-locked release. Both are pure: no concrete I/O, no clock (the `now` boundary is
//! passed in), and no version parsing (the tool hands back classified releases). "Unknown age
//! is never mature" is enforced here, once.

use crate::model::{
    Candidate, Dependency, MajorKey, PinVerdict, Release, ReleaseOrder, ReleaseQuality, Status,
    ToolId, UpdateKind, Verdict, Version,
};
use crate::policy::{PolicyLayer, ResolveKind, ResolveQuery, resolve};
use camino::Utf8Path;
use jiff::Timestamp;

/// The context the core needs to build resolution queries and apply the candidate filter.
///
/// Threaded into both [`evaluate`] and [`check_pin`], it carries the per-invocation knobs that
/// are not properties of the [`Dependency`] itself: which tool is being evaluated, which
/// project the policy cascade resolves against, and whether cross-major jumps are admissible
/// candidates. It is `Copy`, so it is cheap to pass by value or reference.
#[derive(Debug, Clone, Copy)]
pub struct ResolveContext<'a> {
    /// The tool being evaluated, used to build the [`ResolveQuery`](crate::ResolveQuery)
    /// for each candidate.
    pub tool: ToolId,
    /// The project root the policy cascade resolves against (matches `project=` selectors).
    pub project: &'a Utf8Path,
    /// `--major`: allow cross-major jumps as candidates (default: within the current major).
    pub allow_major: bool,
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
/// [`graph_ceiling`](Dependency::graph_ceiling) (a requirer's `==` pin), which yields
/// [`Status::Held`] with `latest` still surfacing the newest version. Two further cases override the
/// rollup: exact manifest pins are [`Status::Held`] when there is a candidate to review, and a commit
/// pin (pseudo-version) has no tagged version to compare and yields [`Status::Held`]. If the current
/// pin is absent from `releases` the result is conservatively [`Status::UpToDate`] (`check`, via
/// [`check_pin`], is the real gate and does not rely on this).
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
///         kind_from_current: None,
///         published_at: Some(mature),
///         yanked: false,
///         quality: ReleaseQuality::Stable,
///     },
///     Release {
///         version: Version::new("1.0.1"),
///         order: ReleaseOrder(vec![1]),
///         major: MajorKey("1".into()),
///         kind_from_current: Some(UpdateKind::Patch),
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
    debug_assert!(
        releases.is_sorted_by(|a, b| a.order <= b.order),
        "releases must be sorted ascending by ReleaseOrder"
    );

    // A commit pin (pseudo-version) has no tagged version to compare against, so it short-circuits to
    // Held with just the newest stable release as `latest` for context. An exact pin (`==`/`=`) is
    // also Held, but it *is* a tagged version, so it flows through normal candidate evaluation below
    // and is only relabelled `Held` at the end — that way its `adoptable_target` still reports the
    // newest matured version, i.e. exactly which version could be manually pinned to.
    if dep.current_quality == ReleaseQuality::Pseudo {
        let latest = releases
            .iter()
            .filter(|r| r.quality.is_stable_like() && !r.yanked && visible_at(r, now))
            .max_by(|a, b| a.order.cmp(&b.order))
            .map(|r| r.version.clone());
        return Verdict {
            status: Status::Held,
            adoptable_target: None,
            latest,
            candidates: Vec::new(),
        };
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
        };
    };
    let current_order = current.order.clone();
    let current_major = current.major.clone();

    // A requirer may pin this dependency exactly (`==`), capping it below newer releases; candidates
    // ordered above that ceiling are excluded (the upgrade-direction mirror of `graph_floor`). A
    // ceiling below the current version is not a real upper bound — the graph resolved past it — so
    // it is ignored, leaving a legal upgrade free rather than wrongly holding the dependency.
    let ceiling_order = graph_ceiling_order(dep, releases).filter(|order| **order >= current_order);

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
        .filter(|r| r.order > current_order && ceiling_order.is_none_or(|c| r.order <= *c))
        .filter_map(|r| classify_candidate(r, dep, layers, ctx, now))
        .collect();

    // `candidates` is in ascending order (from sorted releases); the headline is the newest. An
    // empty candidate set means no newer *admissible* release — "up to date", unless the graph
    // ceiling excluded a newer one: then the dependency is pinned at its current version by a
    // requirer's `==` (graph-held), with `latest` still showing the newest version for context.
    let Some(headline) = candidates.last() else {
        let blocked_by_ceiling = ceiling_order.is_some_and(|ceiling| {
            eligible
                .iter()
                .any(|r| r.order > current_order && r.order > *ceiling)
        });
        return Verdict {
            status: if blocked_by_ceiling {
                Status::Held
            } else {
                Status::UpToDate
            },
            adoptable_target: None,
            latest,
            candidates,
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
    }
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
            kind_from_current: kind,
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
            kind_from_current: Some(UpdateKind::Patch),
            published_at: Some(published.parse().expect("timestamp")),
            yanked: false,
            quality: ReleaseQuality::Stable,
        }
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
