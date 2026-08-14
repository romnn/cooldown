//! Shared registry plumbing: one HTTP client, an on-disk metadata cache hardened with a monotonic
//! publish-time floor, per-host concurrency, and a `PackageRegistry` fake for tests. Adapters are
//! *built from* this; the package manager is never the source of cooldown truth.

pub mod cache;
pub mod fake;
pub mod http;
pub mod osv;

pub use cache::{CacheEntry, GuardedTime, PublishStore};
pub use fake::FakeRegistry;
pub use http::{HttpOptions, HttpResponse, SharedHttp};
pub use osv::{OSV_SOURCE_ID, OsvSource};

/// Cache TTLs: an immutable per-version `.info` can be cached for a long time; a mutable listing
/// (`@v/list`, `@latest`, index files) should refresh more often.
pub mod ttl {
    use std::time::Duration;

    /// A specific version's metadata is immutable; cache it for a week.
    pub const IMMUTABLE: Duration = Duration::from_hours(168);
    /// A version listing can grow; refresh hourly.
    pub const LISTING: Duration = Duration::from_hours(1);
    /// Advisory data may only *shorten* a window while fresh, so it gets a short TTL; a copy
    /// past it still annotates but never shortens (see [`crate::osv`]).
    pub const ADVISORY: Duration = Duration::from_hours(24);
}
