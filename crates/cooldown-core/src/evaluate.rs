//! The cooldown decision — the single source of truth for every ecosystem.
//!
//! [`evaluate`] drives `outdated`/`upgrade` over a candidate set; [`check_pin`] is the gate over
//! the currently-locked release. Both are pure: no concrete I/O, no clock (the `now` boundary is
//! passed in), and no version parsing (the ecosystem hands back classified releases). "Unknown age
//! is never mature" is enforced here, once.

use crate::model::{
    Candidate, Dependency, EcosystemId, MajorKey, PinVerdict, Release, ReleaseQuality, Status,
    Verdict, Version,
};
use crate::policy::{PolicyLayer, ResolveKind, ResolveQuery, resolve};
use camino::Utf8Path;
use jiff::Timestamp;

/// The context the core needs to build resolution queries and apply the candidate filter.
#[derive(Debug, Clone, Copy)]
pub struct ResolveContext<'a> {
    pub ecosystem: EcosystemId,
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
        ecosystem: ctx.ecosystem,
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

fn major_eligible(r: &Release, current_major: &MajorKey, allow_major: bool) -> bool {
    allow_major || r.major == *current_major
}

/// Evaluate a dependency against its classified releases, producing a per-candidate verdict.
pub fn evaluate(
    dep: &Dependency,
    releases: &[Release],
    layers: &[PolicyLayer],
    ctx: &ResolveContext<'_>,
    now: Timestamp,
) -> Verdict {
    debug_assert!(
        releases.windows(2).all(|w| w[0].order <= w[1].order),
        "releases must be sorted ascending by ReleaseOrder"
    );

    // A commit-pinned dependency has no tagged version to compare against → Held. We still surface
    // the newest stable release as `latest` for context.
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

    if candidates.is_empty() {
        return Verdict {
            status: Status::UpToDate,
            adoptable_target: None,
            latest,
            candidates,
        };
    }

    // `candidates` is in ascending order (from sorted releases); the headline is the newest.
    let adoptable_target = candidates
        .iter()
        .rev()
        .find(|c| matches!(c.status, Status::Adoptable | Status::Exempt))
        .map(|c| c.version.clone());

    let headline = candidates.last().expect("non-empty");
    let status = headline.status;

    Verdict {
        status,
        adoptable_target,
        latest,
        candidates,
    }
}

/// The `check` gate over the currently-locked release. Resolves the bare `min-age` (a pin has no
/// from→to kind) and judges the locked publish instant against it. A graph-pinned fresh pin is
/// still a violation, annotated `graph_held` — never a silent pass.
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

/// Construct a `Version` — small convenience re-export point for tests in sibling modules.
#[allow(dead_code)]
pub(crate) fn v(s: &str) -> Version {
    Version::new(s)
}
