use crate::proxy::GoProxy;
use crate::semver;
use cooldown_core::{
    CandidateScope, Dependency, MajorKey, PackageId, PackageRegistry, RawRelease, Release,
    ReleaseOrder, ReleaseQuality, Result, UpdateKind,
};

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
    let mut releases: Vec<Release> = raw
        .into_iter()
        .filter(|(_, release)| semver::is_valid(release.version.as_str()))
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
) -> Result<Vec<Release>> {
    let module = &dep.package.name;

    // (source_path, raw_release) across the module's own path and discovered higher majors.
    let mut raw: Vec<(String, RawRelease)> = proxy
        .releases(&dep.package)
        .await?
        .into_iter()
        .map(|release| (module.clone(), release))
        .collect();
    if candidates == CandidateScope::AllowCrossMajor {
        for path in discover_major_paths(proxy, module).await? {
            let package = PackageId::new(super::GO_ID, path.clone(), registry.clone());
            for release in proxy.releases(&package).await? {
                raw.push((path.clone(), release));
            }
        }
    }

    // Ensure the current pin is present so the core can locate its order.
    let current = dep.current.as_str();
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

/// Discover higher major-version module paths (`prefix/v2`, `/v3`, …) for cross-major candidates.
async fn discover_major_paths(proxy: &GoProxy, module: &str) -> Result<Vec<String>> {
    let (prefix, path_major, ok) = semver::split_path_version(module);
    if !ok {
        return Ok(Vec::new());
    }
    let current_major: u32 = if path_major.is_empty() {
        1
    } else {
        path_major
            .trim_start_matches(['/', '.'])
            .trim_start_matches('v')
            .parse()
            .unwrap_or(1)
    };
    let mut found = Vec::new();
    let mut misses = 0;
    let mut next_major = current_major + 1;
    while misses < 2 && next_major <= current_major + 8 {
        let path = semver::major_path(&prefix, next_major);
        let list = proxy.list(&path).await?;
        if list.is_empty() {
            misses += 1;
        } else {
            found.push(path);
            misses = 0;
        }
        next_major += 1;
    }
    Ok(found)
}
