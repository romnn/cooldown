//! The cooldown decision — the single source of truth for every tool.
//!
//! [`evaluate`] drives `outdated`/`upgrade` over a candidate set; [`check_pin`] is the gate over
//! the currently-locked release. Both are pure: no concrete I/O, no clock (the `now` boundary is
//! passed in), and no version parsing (the tool hands back classified releases). "Unknown age
//! is never mature" is enforced here, once.

use crate::model::{
    Candidate, Dependency, MajorKey, PinVerdict, Release, ReleaseQuality, Status, ToolId,
    UpdateKind, Verdict,
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
/// exists. Two special cases override that: exact manifest pins are [`Status::Held`] when there is a
/// candidate to review, and a commit pin (pseudo-version) has no tagged version to compare and
/// yields [`Status::Held`]. If the current pin is absent from `releases` the result is
/// conservatively [`Status::UpToDate`] (`check`, via [`check_pin`], is the real gate and does not
/// rely on this).
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
            .filter(|r| r.quality.is_stable_like() && !r.yanked)
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

    // Eligible = the releases adoption could target (quality + major filter + not yanked), current
    // included, so `latest` is well-defined even when up to date.
    let eligible: Vec<&Release> = releases
        .iter()
        .filter(|r| {
            quality_eligible(r, dep.current_quality)
                && major_eligible(r, &current_major, ctx.allow_major)
                && !r.yanked
        })
        .collect();

    let latest = eligible
        .iter()
        .max_by(|a, b| a.order.cmp(&b.order))
        .map(|r| r.version.clone())
        .or_else(|| Some(dep.current.clone()));

    let mut candidates: Vec<Candidate> = Vec::new();
    for r in eligible.iter().filter(|r| r.order > current_order) {
        let Some(kind) = r.kind_from_current else {
            continue; // unclassifiable jump; skip (the adapter classifies every real upgrade)
        };
        let res = resolve(layers, &query(dep, ctx, ResolveKind::Candidate(kind)), now);
        let window = res.window;
        let status = if window.exempt {
            Status::Exempt
        } else {
            match r.published_at {
                None => Status::UnknownAge,
                Some(p) if p <= window.cutoff(now) => Status::Adoptable,
                Some(_) => Status::InCooldown,
            }
        };
        candidates.push(Candidate {
            version: r.version.clone(),
            kind,
            window,
            status,
            published_at: r.published_at,
        });
    }

    // `candidates` is in ascending order (from sorted releases); the headline is the newest. An
    // empty candidate set means no newer eligible release, i.e. up to date.
    let Some(headline) = candidates.last() else {
        return Verdict {
            status: Status::UpToDate,
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
/// this pin: when [`Dependency::graph_floor`] equals the locked version, `graph_held` is set. A
/// graph-held but too-fresh pin is *still* a [`Status::CurrentInCooldown`] violation — the flag lets
/// it be baselined deliberately rather than silently passed.
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

    let graph_held = matches!(&dep.graph_floor, Some(v) if *v == locked.version);

    PinVerdict {
        status,
        window,
        graph_held,
        graph_floor: dep.graph_floor.clone(),
        published_at: locked.published_at,
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
}
