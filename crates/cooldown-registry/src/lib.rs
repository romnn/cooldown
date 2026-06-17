//! Shared registry plumbing: one HTTP client, an on-disk metadata cache hardened with a monotonic
//! publish-time floor, per-host concurrency, and a `PackageRegistry` fake for tests. Adapters are
//! *built from* this; the package manager is never the source of cooldown truth.

pub mod cache;
pub mod fake;
pub mod http;

pub use cache::{CacheEntry, GuardedTime, PublishStore};
pub use fake::FakeRegistry;
pub use http::{HttpOptions, HttpResponse, SharedHttp};

/// Cache TTLs: an immutable per-version `.info` can be cached for a long time; a mutable listing
/// (`@v/list`, `@latest`, index files) should refresh more often.
pub mod ttl {
    use std::time::Duration;

    /// A specific version's metadata is immutable; cache it for a week.
    pub const IMMUTABLE: Duration = Duration::from_secs(7 * 24 * 3600);
    /// A version listing can grow; refresh hourly.
    pub const LISTING: Duration = Duration::from_secs(3600);
}
