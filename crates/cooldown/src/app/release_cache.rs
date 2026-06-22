//! The run-scoped release cache — a first-class stage of the resolve pipeline.
//!
//! Every command resolves the same shape of data: for a dependency, either its candidate releases
//! (`outdated`/`upgrade`/`fix`) or the publish time of its locked version (`check`). A real
//! workspace asks for the *same* package many times over — each member of a Cargo/pnpm workspace
//! shares one lock, so `serde` is read once per member, and `upgrade` re-resolves the whole graph
//! every fixpoint round. Left alone, that is N redundant registry round-trips for one answer.
//!
//! [`ReleaseCache`] is the single chokepoint that removes the redundancy. It sits in front of every
//! adapter release fetch (see [`fetch_candidate_releases`] / [`fetch_locked_releases`]) and resolves
//! each distinct key exactly once per run, with **single-flight** semantics: when several tasks race
//! for the same uncached key, one runs the fetch and the rest await its result instead of
//! duplicating it. A failed resolution is *not* cached, so a transient registry error stays
//! retryable rather than poisoning the key for the rest of the run.
//!
//! ## What makes a key
//!
//! Candidate releases are keyed by `(package, current version, major scope)` and a locked release by
//! `(package, version)` — plus a **project discriminator**. The version matters because candidate
//! annotations (is this a major/minor/patch step from here?) are relative to the current version, so
//! two projects pinned to different versions of one package get distinct entries. The artifact scope
//! is held constant for the whole run, so it is not in the key.
//!
//! Whether the *project* is part of the key is the fetcher's call, via
//! [`ReleaseFetcher::releases_are_project_scoped`]: a global registry index (cargo, npm, …) returns
//! the same releases for a package regardless of who asks, so its entries are shared across the whole
//! run; but Go resolves candidates from each module's own `go list -m -versions` and uv reads
//! per-project locked artifact times, so their answers are project-specific and must *not* be shared
//! across projects. The orchestrator does not guess this — the fetcher declares it, and
//! [`project_scope`] folds the project root into the key only when it does.
//!
//! [`fetch_candidate_releases`]: super::Workspace::fetch_candidate_releases
//! [`fetch_locked_releases`]: super::Workspace::fetch_locked_releases

use async_trait::async_trait;
use cooldown_core::{
    CandidateScope, Dependency, FetchContext, PackageId, Release, ReleaseFetcher, Result, Version,
};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::OnceCell;

/// A run-scoped, single-flight async memo keyed by `K`, caching a `V` per key.
///
/// The first caller for a key runs the resolver; concurrent callers for the same key await that one
/// resolution and clone its result. A resolver error is **not** stored, so a later caller retries.
struct SingleFlight<K, V> {
    cells: Mutex<HashMap<K, Arc<OnceCell<V>>>>,
    /// Total resolve requests, and the subset that actually ran the resolver. Their difference is
    /// the work the cache saved; both are surfaced through [`Stats`] for the resolve log.
    lookups: AtomicU64,
    resolved: AtomicU64,
}

impl<K, V> SingleFlight<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn new() -> Self {
        SingleFlight {
            cells: Mutex::new(HashMap::new()),
            lookups: AtomicU64::new(0),
            resolved: AtomicU64::new(0),
        }
    }

    /// Return `key`'s value, running `resolve` only if no prior or in-flight caller already is.
    ///
    /// The map lock is held just long enough to hand out the key's [`OnceCell`]; the resolver runs
    /// outside it, so distinct keys resolve concurrently and only same-key callers serialize.
    async fn get_or_resolve<F, Fut>(&self, key: K, resolve: F) -> Result<V>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<V>>,
    {
        self.lookups.fetch_add(1, Ordering::Relaxed);
        let cell = {
            let mut cells = self.cells.lock().unwrap_or_else(PoisonError::into_inner);
            Arc::clone(
                cells
                    .entry(key)
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        cell.get_or_try_init(|| async {
            self.resolved.fetch_add(1, Ordering::Relaxed);
            resolve().await
        })
        .await
        .cloned()
    }

    fn stats(&self) -> (u64, u64) {
        (
            self.lookups.load(Ordering::Relaxed),
            self.resolved.load(Ordering::Relaxed),
        )
    }
}

/// Identity of a candidate-release resolution. `package` is the full [`PackageId`] — it carries the
/// `tool` and `registry` alongside the name, so `foo` on npm and `foo` on JSR (or two registries of
/// one tool) never share an entry. `current` is the version the dep sits at (candidate annotations
/// are relative to it) and `scope` is the major scope. `project` is `Some(root)` only for a
/// project-scoped fetcher (see [`project_scope`]), so a project-dependent adapter's answer is never
/// shared across projects while a global registry adapter's `None` is shared across the whole run.
#[derive(Clone, PartialEq, Eq, Hash)]
struct CandidateKey {
    package: PackageId,
    current: Version,
    scope: CandidateScope,
    project: Option<String>,
}

/// Identity of a locked-release resolution: the full [`PackageId`], the locked `version`, and the
/// same `project` discriminator as [`CandidateKey`] — uv reads a locked version's publish time from
/// the asking project's own lock, so its locked releases are project-scoped.
#[derive(Clone, PartialEq, Eq, Hash)]
struct LockedKey {
    package: PackageId,
    version: Version,
    project: Option<String>,
}

/// The project discriminator for a cache key: `Some(project root)` when the fetcher's answer is
/// project-specific (so two projects sharing a `(package, version)` don't serve each other's
/// result), `None` when it is a pure function of the package and can be shared across the run.
fn project_scope(fetcher: &dyn ReleaseFetcher, fetch: &FetchContext<'_>) -> Option<String> {
    fetcher
        .releases_are_project_scoped()
        .then(|| fetch.project.root.to_string())
}

/// The run-scoped release cache shared by every command through its [`Workspace`](super::Workspace).
///
/// Owns one [`SingleFlight`] memo for candidate releases and one for locked-release publish times. A
/// workspace holds exactly one for the whole run; it is cheap to construct and safe to share behind
/// a `&` across the concurrent fetch fan-out.
pub(crate) struct ReleaseCache {
    candidates: SingleFlight<CandidateKey, Vec<Release>>,
    locked: SingleFlight<LockedKey, Release>,
}

impl ReleaseCache {
    pub(crate) fn new() -> Self {
        ReleaseCache {
            candidates: SingleFlight::new(),
            locked: SingleFlight::new(),
        }
    }
}

/// The release-resolution port the orchestrator depends on: resolve a dependency's candidate
/// releases or its locked release, going to the `fetcher` only when the answer isn't already known.
///
/// A port (not a concrete type) so the cache implementation is swappable and a test can drive the
/// orchestrator with a mock — the same ports-and-adapters seam the rest of the codebase uses.
/// [`ReleaseCache`] is the production implementation (run-scoped, single-flight); the orchestrator
/// holds a `dyn ReleaseResolver` and never the concrete type. The `fetcher` is passed in per call
/// (rather than owned) so one tool-agnostic resolver fronts every adapter's [`ReleaseFetcher`].
#[async_trait]
pub(crate) trait ReleaseResolver: Send + Sync {
    /// Resolve the candidate releases for `dep` under `scope`, delegating to `fetcher` only on the
    /// first request for that identity this run.
    async fn candidate_releases(
        &self,
        fetcher: &dyn ReleaseFetcher,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        scope: CandidateScope,
    ) -> Result<Vec<Release>>;

    /// Resolve the locked release for `dep`, delegating to `fetcher` only on the first request.
    async fn locked_release(
        &self,
        fetcher: &dyn ReleaseFetcher,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
    ) -> Result<Release>;

    /// A snapshot of resolution effectiveness so far, for the resolve log.
    fn stats(&self) -> Stats;
}

#[async_trait]
impl ReleaseResolver for ReleaseCache {
    async fn candidate_releases(
        &self,
        fetcher: &dyn ReleaseFetcher,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
        scope: CandidateScope,
    ) -> Result<Vec<Release>> {
        let key = CandidateKey {
            package: dep.package.clone(),
            current: dep.current.clone(),
            scope,
            project: project_scope(fetcher, fetch),
        };
        self.candidates
            .get_or_resolve(key, || fetcher.releases(dep, fetch, scope))
            .await
    }

    async fn locked_release(
        &self,
        fetcher: &dyn ReleaseFetcher,
        dep: &Dependency,
        fetch: &FetchContext<'_>,
    ) -> Result<Release> {
        let key = LockedKey {
            package: dep.package.clone(),
            version: dep.current.clone(),
            project: project_scope(fetcher, fetch),
        };
        self.locked
            .get_or_resolve(key, || fetcher.locked_release(dep, fetch))
            .await
    }

    fn stats(&self) -> Stats {
        let (candidate_lookups, candidate_resolved) = self.candidates.stats();
        let (locked_lookups, locked_resolved) = self.locked.stats();
        Stats {
            lookups: candidate_lookups + locked_lookups,
            resolved: candidate_resolved + locked_resolved,
        }
    }
}

/// A snapshot of [`ReleaseCache`] effectiveness: how many resolve requests were made and how many
/// actually ran a fetch. [`saved`](Stats::saved) is the redundant work the cache removed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Stats {
    pub(crate) lookups: u64,
    pub(crate) resolved: u64,
}

impl Stats {
    pub(crate) fn saved(self) -> u64 {
        self.lookups.saturating_sub(self.resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::CoreError;

    #[tokio::test]
    async fn single_flight_resolves_a_key_once_under_concurrency() {
        let cache = SingleFlight::<&'static str, u64>::new();
        let calls = AtomicU64::new(0);
        let run = || async {
            cache
                .get_or_resolve("k", || async {
                    calls.fetch_add(1, Ordering::Relaxed);
                    // Yield so a racing sibling observes the in-flight cell rather than an empty one.
                    tokio::task::yield_now().await;
                    Ok(7u64)
                })
                .await
        };
        let (a, b, c) = tokio::join!(run(), run(), run());
        assert_eq!(
            (
                a.expect("a"),
                b.expect("b"),
                c.expect("c"),
                calls.load(Ordering::Relaxed)
            ),
            // Three racers, one fetch; all share the value.
            (7, 7, 7, 1)
        );
        let (lookups, resolved) = cache.stats();
        assert_eq!((lookups, resolved), (3, 1));
    }

    #[tokio::test]
    async fn failed_resolution_is_not_cached_and_can_be_retried() {
        let cache = SingleFlight::<&'static str, u64>::new();
        let first = cache
            .get_or_resolve("k", || async { Err(CoreError::transient("boom")) })
            .await;
        assert!(first.is_err());
        // The error was not stored, so the next caller re-runs the resolver and succeeds.
        let second = cache.get_or_resolve("k", || async { Ok(9u64) }).await;
        assert_eq!(second.expect("retry resolves"), 9);
    }

    #[tokio::test]
    async fn distinct_keys_each_resolve_independently() {
        let cache = SingleFlight::<&'static str, u64>::new();
        let a = cache.get_or_resolve("a", || async { Ok(1u64) }).await;
        let b = cache.get_or_resolve("b", || async { Ok(2u64) }).await;
        // A repeat of "a" is served from cache, not re-resolved.
        let a_again = cache.get_or_resolve("a", || async { Ok(99u64) }).await;
        assert_eq!(
            (a.expect("a"), b.expect("b"), a_again.expect("a again")),
            (1, 2, 1)
        );
        assert_eq!(cache.stats(), (3, 2));
    }

    /// A stand-in resolver, proving the port is mockable without any registry, fetcher, or fixtures.
    struct StubResolver;

    #[async_trait]
    impl ReleaseResolver for StubResolver {
        async fn candidate_releases(
            &self,
            _: &dyn ReleaseFetcher,
            _: &Dependency,
            _: &FetchContext<'_>,
            _: CandidateScope,
        ) -> Result<Vec<Release>> {
            Ok(Vec::new())
        }
        async fn locked_release(
            &self,
            _: &dyn ReleaseFetcher,
            _: &Dependency,
            _: &FetchContext<'_>,
        ) -> Result<Release> {
            Err(CoreError::transient("stub"))
        }
        fn stats(&self) -> Stats {
            Stats {
                lookups: 0,
                resolved: 0,
            }
        }
    }

    #[test]
    fn cache_and_stub_are_interchangeable_through_the_resolver_port() {
        // Both the production cache and a test stub satisfy the port, so the orchestrator can be
        // driven with either — the swappability the trait buys us.
        let cache: Box<dyn ReleaseResolver> = Box::new(ReleaseCache::new());
        let stub: Box<dyn ReleaseResolver> = Box::new(StubResolver);
        assert_eq!(cache.stats().saved(), 0);
        assert_eq!(stub.stats().saved(), 0);
    }

    use cooldown_core::{ArtifactScope, Project, ReleaseQuality, ToolId};

    /// A fetcher that counts how many times it actually resolves, with a configurable project-scope.
    struct CountingFetcher {
        calls: AtomicU64,
        project_scoped: bool,
    }

    #[async_trait]
    impl ReleaseFetcher for CountingFetcher {
        async fn releases(
            &self,
            _dep: &Dependency,
            _fetch: &FetchContext<'_>,
            _candidates: CandidateScope,
        ) -> Result<Vec<Release>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
        async fn locked_release(
            &self,
            _dep: &Dependency,
            _fetch: &FetchContext<'_>,
        ) -> Result<Release> {
            Err(CoreError::transient("unused"))
        }
        fn releases_are_project_scoped(&self) -> bool {
            self.project_scoped
        }
    }

    fn test_dep() -> Dependency {
        Dependency {
            package: PackageId::new(ToolId("test"), "pkg".to_string(), None),
            current: Version::new("1.0.0".to_string()),
            current_quality: ReleaseQuality::Stable,
            direct: true,
            artifacts: Vec::new(),
            graph_floor: None,
            graph_ceiling: None,
            members: Vec::new(),
            pinned: false,
        }
    }

    fn test_project(root: &str) -> Project {
        Project {
            root: camino::Utf8PathBuf::from(root),
            kind: ToolId("test"),
            manifest: camino::Utf8PathBuf::from(root),
            exclude_newer: None,
        }
    }

    async fn resolve_in(cache: &ReleaseCache, fetcher: &CountingFetcher, project: &Project) {
        let fetch = FetchContext {
            project,
            artifacts: ArtifactScope::Environment,
        };
        let _ = cache
            .candidate_releases(
                fetcher,
                &test_dep(),
                &fetch,
                CandidateScope::CurrentMajorOnly,
            )
            .await;
    }

    #[tokio::test]
    async fn project_scoped_fetcher_is_not_shared_across_projects() {
        let cache = ReleaseCache::new();
        let fetcher = CountingFetcher {
            calls: AtomicU64::new(0),
            project_scoped: true,
        };
        resolve_in(&cache, &fetcher, &test_project("/a")).await;
        resolve_in(&cache, &fetcher, &test_project("/b")).await;
        // Same (package, version, scope) but a project-scoped fetcher → each project resolves, so a
        // multi-module Go / multi-uv-project run never serves one project's releases to another.
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn global_fetcher_is_shared_across_projects() {
        let cache = ReleaseCache::new();
        let fetcher = CountingFetcher {
            calls: AtomicU64::new(0),
            project_scoped: false,
        };
        resolve_in(&cache, &fetcher, &test_project("/a")).await;
        resolve_in(&cache, &fetcher, &test_project("/b")).await;
        // A global registry fetcher resolves once and shares across every project in the run.
        assert_eq!(fetcher.calls.load(Ordering::Relaxed), 1);
    }
}
