//! Shared utilities for registry-backed adapters that classify releases and translate common
//! resolver/apply outcomes.

mod driver;
pub use driver::{Driver, program_on_path, resolve_program};

use camino::Utf8Path;
use cooldown_core::{
    Change, CoreError, LockStatus, LockVerifyReport, MajorKey, MemberRef, ProjectMutationJournal,
    RawRelease, Release, ReleaseOrder, ReleaseQuality, Result, SkipReason, Skipped, UpdateKind,
};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasher;

/// The workspace `roots` that reach `target` through the resolved dependency graph — directly or
/// transitively — for attributing a transitive dependency to the members that pull it in.
///
/// `edges` are `(from, to)` pairs meaning "`from` depends on `to`". A breadth-first walk over the
/// reversed adjacency from `target` visits every node that can reach it; the `roots` among those are
/// returned. `target` itself is excluded, so a dependency is never attributed to itself even when it
/// is a root. The visited set bounds the walk, so dependency cycles are safe. Tool-agnostic: any
/// adapter that can express its resolved graph as edges gets transitive "used by" attribution.
#[must_use]
pub fn reaching_roots<'a, S: BuildHasher>(
    edges: impl IntoIterator<Item = (&'a str, &'a str)>,
    roots: &HashSet<&'a str, S>,
    target: &'a str,
) -> Vec<&'a str> {
    // Reverse adjacency: each node's *dependents*, so a BFS from `target` walks back to its requirers.
    let mut dependents: HashMap<&str, Vec<&str>> = HashMap::new();
    for (from, to) in edges {
        dependents.entry(to).or_default().push(from);
    }
    let mut seen: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<&str> = VecDeque::from([target]);
    let mut roots_reaching = Vec::new();
    while let Some(node) = queue.pop_front() {
        if !seen.insert(node) {
            continue;
        }
        if node != target && roots.contains(node) {
            roots_reaching.push(node);
        }
        if let Some(preds) = dependents.get(node) {
            queue.extend(preds.iter().copied());
        }
    }
    roots_reaching
}

/// Map the workspace roots reaching `target` to sorted, deduplicated [`MemberRef`]s via `member_of`,
/// which resolves a graph node id to its `(name, path)` (returning `None` for non-member nodes). The
/// shared shape behind every adapter's transitive "used by" attribution.
#[must_use]
pub fn reaching_members<'a, S: BuildHasher>(
    edges: impl IntoIterator<Item = (&'a str, &'a str)>,
    roots: &HashSet<&'a str, S>,
    target: &'a str,
    member_of: impl Fn(&str) -> Option<MemberRef>,
) -> Vec<MemberRef> {
    let mut members: Vec<MemberRef> = reaching_roots(edges, roots, target)
        .into_iter()
        .filter_map(member_of)
        .collect();
    members.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.path.cmp(&b.path)));
    members.dedup();
    members
}

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

/// Map an apply error to a resolver-conflict skip, unless it reflects a broken local environment.
///
/// A broken environment — the tool could not be spawned, a filesystem/lock fault, or a spawned tool
/// that failed because its runtime/storage is broken — stays fatal, because the adapter could not
/// reliably ask the underlying resolver for a dependency-graph decision. Only a genuine resolver
/// rejection becomes a per-candidate [`SkipReason::ResolverConflict`].
///
/// # Errors
///
/// Returns the original [`CoreError`] when it is a [local environment
/// failure](CoreError::is_local_environment_failure) rather than a resolver conflict.
pub fn skipped_on_apply_error(change: &Change, error: CoreError) -> Result<Skipped> {
    if error.is_local_environment_failure() {
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
pub fn verify_current_report(ok: bool, ok_detail: &str, stale_detail: &str) -> LockVerifyReport {
    LockVerifyReport {
        status: if ok {
            LockStatus::Current
        } else {
            LockStatus::Stale
        },
        detail: if ok {
            ok_detail.to_string()
        } else {
            stale_detail.to_string()
        },
    }
}

/// Build a fail-closed lock-currency report for adapters that cannot prove currency yet.
#[must_use]
pub fn verify_current_unknown(lockfile: &str) -> LockVerifyReport {
    LockVerifyReport {
        status: LockStatus::Unknown,
        detail: format!(
            "{lockfile} currency cannot be verified by this adapter yet; refusing to assume it is current"
        ),
    }
}

#[cfg(test)]
mod reaching_tests {
    use super::{reaching_roots, skipped_on_apply_error};
    use cooldown_core::{
        Change, CoreError, PackageId, ToolId, ToolTermination, UpdateKind, Version,
    };
    use std::collections::HashSet;

    fn change() -> Change {
        Change {
            package: PackageId::new(ToolId("mock"), "dep".to_string(), None),
            from: Version::new("1.0.0"),
            to: Version::new("2.0.0"),
            kind: UpdateKind::Minor,
            downgrade: false,
            direct: true,
            members: Vec::new(),
        }
    }

    #[test]
    fn finds_roots_that_reach_a_transitive_target() {
        // a (root) → b → d ;  c (root) → d ;  e (root) → f.  d is transitive under a and c only.
        let edges = [("a", "b"), ("b", "d"), ("c", "d"), ("e", "f")];
        let roots: HashSet<&str> = ["a", "c", "e"].into_iter().collect();

        let mut reach_d = reaching_roots(edges, &roots, "d");
        reach_d.sort_unstable();
        assert_eq!(reach_d, vec!["a", "c"]);

        // A directly-required target resolves to its single root.
        assert_eq!(reaching_roots(edges, &roots, "b"), vec!["a"]);
        // A node not in the graph yields nothing (no panic).
        assert!(reaching_roots(edges, &roots, "zzz").is_empty());
        // Cycles are handled (the visited set bounds the walk).
        let cyclic = [("a", "b"), ("b", "c"), ("c", "b")];
        assert_eq!(reaching_roots(cyclic, &roots, "c"), vec!["a"]);
    }

    #[test]
    fn local_tool_environment_failures_do_not_become_resolver_skips() {
        let err = CoreError::Tool {
            tool: "pnpm".into(),
            termination: ToolTermination::ExitCode(1),
            stderr: "pnpm: unable to open database file".into(),
        };

        assert!(matches!(
            skipped_on_apply_error(&change(), err),
            Err(CoreError::Tool { .. })
        ));
    }
}
