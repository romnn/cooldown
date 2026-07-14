use crate::proxy::GoProxy;
use crate::semver;
use cooldown_core::{
    CandidateScope, Dependency, MajorKey, PackageId, PackageRegistry, RawRelease, Release,
    ReleaseOrder, ReleaseQuality, Result, UpdateKind, Version,
};
use std::{cmp::Ordering, time::Duration};

const MAJOR_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// Classify a version string into a [`ReleaseQuality`].
#[must_use]
pub(super) fn classify_quality(version: &str) -> ReleaseQuality {
    if semver::is_pseudo(version) {
        ReleaseQuality::Pseudo
    } else if semver::is_incompatible(version) {
        ReleaseQuality::Incompatible
    } else if !semver::prerelease(version).is_empty() {
        ReleaseQuality::Prerelease
    } else {
        ReleaseQuality::Stable
    }
}

/// The `MajorKey` for a module *path* — the `/vN` suffix (`""` for v0/v1/+incompatible base
/// paths).
pub(super) fn major_key_for_path(path: &str) -> MajorKey {
    let (_, path_major, _) = semver::split_path_version(path);
    MajorKey(path_major)
}

/// Classify a set of source-tagged raw releases into ordered, deduped [`Release`]s: assign quality
/// and `kind_from_current`, derive each release's `MajorKey` from the path it came from, sort by
/// semver, dedupe by canonical version, and assign a within-package order token. Pure (no I/O), so
/// the adapter's classification logic is unit-testable without network.
#[must_use]
pub(super) fn build_releases(current: &str, raw: Vec<(String, RawRelease)>) -> Vec<Release> {
    // Mirror Go's `go get -u`/`@latest` rule for `+incompatible` versions (major ≥ 2 tagged on a
    // path with no `/vN` suffix and no `go.mod`). The raw GOPROXY `@v/list` returns every such tag,
    // but Go only *selects* one when the module has not adopted module-style versioning on the line
    // you are on: a pin already on the `+incompatible` line (github.com/docker/cli
    // v29.5.2+incompatible) keeps moving within it, while a pin on a compatible, go.mod-bearing
    // version (k8s.io/client-go v0.36.1) never jumps to a bare `+incompatible` tag (its ancient
    // v11.0.0+incompatible). This matches what `go list -m -versions` reports, so cooldown only ever
    // suggests what Go would. The current pin is itself never dropped (a compatible pin is not
    // `+incompatible`; an incompatible pin keeps the whole line).
    let current_is_incompatible = semver::is_incompatible(current);
    let mut releases: Vec<Release> = raw
        .into_iter()
        .filter(|(_, release)| {
            semver::is_valid(release.version.as_str())
                && (current_is_incompatible || !semver::is_incompatible(release.version.as_str()))
        })
        .map(|(path, release)| {
            let version = release.version.as_str();
            Release {
                version: release.version.clone(),
                order: ReleaseOrder(Vec::new()),
                major: major_key_for_path(&path),
                kind_from_current: classify_kind(current, version),
                published_at: release.published_at,
                yanked: release.yanked,
                quality: classify_quality(version),
            }
        })
        .collect();

    // Deduplicate by canonical version (the same tag can appear from base + a /vN probe). Within
    // an equal-canonical group, sort a release that HAS a publish time ahead of one that does
    // not, so `dedup_by` (which keeps the first) preserves the dated record.
    releases.sort_by(|a, b| {
        semver::compare(a.version.as_str(), b.version.as_str())
            .then_with(|| a.published_at.is_none().cmp(&b.published_at.is_none()))
    });
    releases.dedup_by(|a, b| {
        semver::canonical_version(a.version.as_str())
            == semver::canonical_version(b.version.as_str())
    });
    for (index, release) in releases.iter_mut().enumerate() {
        // `index` is a release index, which cannot realistically approach `u32::MAX`; saturate
        // rather than truncate so the big-endian order token stays monotonic.
        let order = u32::try_from(index).unwrap_or(u32::MAX);
        release.order = ReleaseOrder(order.to_be_bytes().to_vec());
    }
    cooldown_core::debug_assert_sorted(&releases);
    releases
}

pub(super) async fn releases(
    proxy: &GoProxy,
    dep: &Dependency,
    candidates: CandidateScope,
    registry: Option<String>,
    go_versions: Option<&[String]>,
) -> Result<Vec<Release>> {
    let module = &dep.package.name;
    let current = dep.current.as_str();

    // The module's own-path version set: prefer what Go itself reports (`go list -m -versions`),
    // which already omits the ancient pre-module and `+incompatible` tags a module-aware module would
    // never resolve to (k8s.io/client-go lists only its `v0.x` line, not `v1.5.2` or
    // `v11.0.0+incompatible`). Fall back to the proxy's raw `@v/list` (then `@latest`) when Go reports
    // no versions for this module (or its probe failed) so discovery still works.
    let own_versions = match go_versions {
        Some(versions) if !versions.is_empty() => versions.to_vec(),
        _ => {
            let mut listed = proxy.list(module).await?;
            if listed.is_empty()
                && let Some(latest) = proxy.latest(module).await?
            {
                listed.push(latest.version);
            }
            listed
        }
    };
    // For a downgrade (`fix`/`upgrade` rolling a too-fresh pin back), the publish times of the
    // versions between the graph floor and the current pin must be known, so time down to the floor —
    // not just the current pin and newer. The floor is the current pin for a graph-held dep (no extra
    // work) and usually just below it otherwise, so this stays clear of the historical-tag burst the
    // timing skip in `own_path_releases` avoids.
    let timing_floor = dep
        .graph_floor
        .as_ref()
        .map_or(current, |floor| floor.as_str());
    let own_path = own_path_releases(proxy, module, timing_floor, own_versions).await?;
    // (source_path, raw_release) across the module's own path and discovered higher majors.
    let mut raw: Vec<(String, RawRelease)> = own_path
        .into_iter()
        .map(|release| (module.clone(), release))
        .collect();
    if candidates == CandidateScope::AllowCrossMajor {
        let mut cross_paths = discover_major_paths(proxy, module).await?;
        // Lower majors too, so `fix --major` can cross a major boundary downward.
        cross_paths.extend(discover_lower_major_paths(proxy, module).await?);
        for path in cross_paths {
            let package = PackageId::new(super::GO_ID, path.clone(), registry.clone());
            for release in proxy.releases(&package).await? {
                raw.push((path.clone(), release));
            }
        }
    }

    // Ensure the current pin is present so the core can locate its order.
    if !raw
        .iter()
        .any(|(_, release)| release.version.as_str() == current)
    {
        let time = proxy
            .published_at(&dep.package, &dep.current, &[])
            .await
            .unwrap_or(None);
        raw.push((
            module.clone(),
            RawRelease {
                version: dep.current.clone(),
                published_at: time,
                yanked: false,
                artifacts: Vec::new(),
            },
        ));
    }

    Ok(build_releases(current, raw))
}

/// Build the module's own-path [`RawRelease`]s, fetching publish times only for `timing_floor` and
/// newer.
///
/// `timing_floor` is the graph floor (or the current pin when there is no lower floor): `check` and
/// `outdated` only look up from the current pin, while `fix`/`upgrade` may roll a too-fresh pin down
/// to the newest matured version the graph still allows, so the publish times of the
/// `[floor, current)` window must be known too. Versions older than the floor can never be an upgrade
/// candidate ([`evaluate`] only considers releases ordered above the current pin), the checked pin,
/// nor a downgrade target (it would fall below the graph floor), so their publish time is never read.
/// Fetching `.info` for every historical tag just to discard it is what turned a many-versioned
/// module — the Azure SDK submodules carry ~100 tags each — into a ~100-request burst per module that
/// tripped the proxy's rate limit on a cold cache and surfaced as spurious `error` rows. Older
/// versions are still listed (untimed) so semver ordering and the current pin's position in the
/// release set stay intact.
///
/// [`evaluate`]: cooldown_core::evaluate
async fn own_path_releases(
    proxy: &GoProxy,
    module: &str,
    timing_floor: &str,
    versions: Vec<String>,
) -> Result<Vec<RawRelease>> {
    let (to_time, untimed): (Vec<String>, Vec<String>) = versions
        .into_iter()
        .partition(|version| semver::compare(version, timing_floor) != Ordering::Less);
    let mut releases = proxy.releases_for(module, to_time).await?;
    releases.extend(untimed.into_iter().map(|version| RawRelease {
        version: Version::new(version),
        published_at: None,
        yanked: false,
        artifacts: Vec::new(),
    }));
    Ok(releases)
}

pub(super) fn classify_kind(current: &str, candidate: &str) -> Option<UpdateKind> {
    use UpdateKind::{Major, Minor, Patch};
    if !semver::is_valid(current) || !semver::is_valid(candidate) {
        return None;
    }
    if semver::major(current) != semver::major(candidate) {
        Some(Major)
    } else if semver::major_minor(current) != semver::major_minor(candidate) {
        Some(Minor)
    } else {
        Some(Patch)
    }
}

/// The `(prefix, current major)` of a module *path* — `1` for a v0/v1/base path — or `None` when the
/// path is not a well-formed module path to discover other majors from.
fn path_current_major(module: &str) -> Option<(String, u32)> {
    let (prefix, path_major, ok) = semver::split_path_version(module);
    if !ok {
        return None;
    }
    let major = if path_major.is_empty() {
        1
    } else {
        path_major
            .trim_start_matches(['/', '.'])
            .trim_start_matches('v')
            .parse()
            .unwrap_or(1)
    };
    Some((prefix, major))
}

/// Discover higher major-version module paths (`prefix/v2`, `/v3`, …) for cross-major *upgrade*
/// candidates. Walks up from the current major, stopping after two consecutive misses (or `+8`).
async fn discover_major_paths(proxy: &GoProxy, module: &str) -> Result<Vec<String>> {
    let Some((prefix, current_major)) = path_current_major(module) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    let mut misses = 0;
    let mut next_major = current_major + 1;
    while misses < 2 && next_major <= current_major + 8 {
        let path = semver::major_path(&prefix, next_major);
        match list_major_discovery_path(proxy, &path).await? {
            Some(list) if !list.is_empty() => {
                found.push(path);
                misses = 0;
            }
            _ => misses += 1,
        }
        next_major += 1;
    }
    Ok(found)
}

/// Discover lower major-version module paths (`prefix/v2`, …, the base `prefix` for v1) so a `fix`
/// downgrade can roll a too-fresh pin back across a major boundary (`/v3` → `/v2`, or `/v2` → the v1
/// base path). The range is bounded by the current major, and a v0/v1 module has no lower path, so
/// this is a no-op for the common case and only probes the rare v2+ module.
async fn discover_lower_major_paths(proxy: &GoProxy, module: &str) -> Result<Vec<String>> {
    let Some((prefix, current_major)) = path_current_major(module) else {
        return Ok(Vec::new());
    };
    let mut found = Vec::new();
    for major in (1..current_major).rev() {
        // Go's v1 lives at the base path (`example.com/foo`) with no `/v1` suffix — except gopkg.in,
        // whose v1 is `gopkg.in/pkg.v1`, which `major_path` builds (it only special-cases the base).
        let path = if major == 1 && !prefix.starts_with("gopkg.in/") {
            prefix.clone()
        } else {
            semver::major_path(&prefix, major)
        };
        match list_major_discovery_path(proxy, &path).await? {
            Some(list) if !list.is_empty() => found.push(path),
            _ => {}
        }
    }
    Ok(found)
}

async fn list_major_discovery_path(proxy: &GoProxy, path: &str) -> Result<Option<Vec<String>>> {
    // Major-path discovery is speculative: cooldown does not know whether `vN+1` exists. A genuine
    // absence and a transient failure both mean "do not include this path", so these probes should
    // not be allowed to consume the full registry request timeout.
    match tokio::time::timeout(MAJOR_DISCOVERY_TIMEOUT, proxy.list(path)).await {
        Ok(Ok(list)) => Ok(Some(list)),
        Ok(Err(err)) if err.is_transient() => {
            tracing::debug!(%path, %err, "major-path discovery probe failed transiently; treating as absent");
            Ok(None)
        }
        Ok(Err(err)) => Err(err),
        Err(_) => {
            tracing::debug!(
                %path,
                timeout_ms = MAJOR_DISCOVERY_TIMEOUT.as_millis(),
                "major-path discovery probe timed out; treating as absent"
            );
            Ok(None)
        }
    }
}
