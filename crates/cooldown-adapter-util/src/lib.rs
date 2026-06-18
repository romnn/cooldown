//! Shared utilities for registry-backed adapters that classify releases and translate common
//! resolver/apply outcomes.

use camino::Utf8Path;
use cooldown_core::{
    Change, CoreError, MajorKey, ProjectMutationJournal, RawRelease, Release, ReleaseOrder,
    ReleaseQuality, Result, SkipReason, Skipped, UpdateKind, VerifyReport,
};
use std::cmp::Ordering;

/// Build sorted, deduplicated releases for a versioned registry-backed adapter.
///
/// `is_valid` filters invalid version strings, `compare` defines ascending version order, and the
/// remaining callbacks project adapter-specific release metadata into the core model.
#[must_use]
pub fn build_registry_releases(
    current: &str,
    raw: Vec<RawRelease>,
    is_valid: impl Fn(&str) -> bool,
    compare: impl Fn(&str, &str) -> Ordering,
    major_key: impl Fn(&str) -> MajorKey,
    classify_kind: impl Fn(&str, &str) -> Option<UpdateKind>,
    classify_quality: impl Fn(&str) -> ReleaseQuality,
) -> Vec<Release> {
    let mut releases: Vec<Release> = raw
        .into_iter()
        .filter(|release| is_valid(release.version.as_str()))
        .map(|release| {
            let version = release.version;
            let version_text = version.as_str().to_string();
            Release {
                version,
                order: ReleaseOrder(Vec::new()),
                major: major_key(&version_text),
                kind_from_current: classify_kind(current, &version_text),
                published_at: release.published_at,
                yanked: release.yanked,
                quality: classify_quality(&version_text),
            }
        })
        .collect();
    releases.sort_by(|a, b| compare(a.version.as_str(), b.version.as_str()));
    releases.dedup_by(|a, b| a.version == b.version);
    for (index, release) in releases.iter_mut().enumerate() {
        let token = u32::try_from(index).unwrap_or(u32::MAX);
        release.order = ReleaseOrder(token.to_be_bytes().to_vec());
    }
    cooldown_core::debug_assert_sorted(&releases);
    releases
}

/// Map a non-spawn apply error to a resolver-conflict skip.
///
/// Spawn failures stay fatal because the adapter could not even run the underlying tool.
///
/// # Errors
///
/// Returns the original [`CoreError`] when the underlying tool could not be spawned at all.
pub fn skipped_on_apply_error(change: &Change, error: CoreError) -> Result<Skipped> {
    if error.is_tool_spawn_failure() {
        return Err(error);
    }
    Ok(Skipped {
        change: change.clone(),
        reason: SkipReason::ResolverConflict,
        offending: Some(change.package.clone()),
    })
}

/// Capture one project-relative lock file as the mutation journal for a single-lock adapter.
///
/// # Errors
///
/// Returns a [`CoreError`](cooldown_core::CoreError) if the lock file state cannot be captured.
pub fn single_lock_journal(root: &Utf8Path, lockfile: &Utf8Path) -> Result<ProjectMutationJournal> {
    Ok(ProjectMutationJournal {
        files: vec![ProjectMutationJournal::capture_file(root, lockfile)?],
    })
}

/// Build a standard lock-currency verification report from a boolean probe.
#[must_use]
pub fn verify_current_report(ok: bool, ok_detail: &str, stale_detail: &str) -> VerifyReport {
    VerifyReport {
        ok,
        detail: if ok {
            ok_detail.to_string()
        } else {
            stale_detail.to_string()
        },
    }
}
