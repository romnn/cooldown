//! On-disk metadata cache with provenance, plus the monotonic publish-time floor.
//!
//! **Trust hardening:** a cached publish time may never move *earlier* on refresh. A backdated
//! upstream timestamp (an attempt to make a fresh version look mature) is rejected and flagged, not
//! trusted — the stored value only ever ratchets later (younger), the conservative direction.

use cooldown_core::CoreError;
use cooldown_core::fs::{atomic_write, fnv1a_64};
use jiff::Timestamp;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A cached HTTP response with provenance: the URL, when it was fetched, and its `ETag`.
///
/// Serialized one-per-file under the cache directory (see [`read_entry`] and [`write_entry`]).
/// The provenance fields let the [`crate::http`] client decide freshness ([`Self::fetched_at`]
/// against a TTL) and revalidate cheaply with a conditional request ([`Self::etag`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    /// The absolute URL this entry was fetched from.
    pub url: String,
    /// The fetch instant as an RFC 3339 string; parse it with [`Self::fetched_at`].
    pub fetched_at: String,
    /// The response `ETag`, if the server sent one, used for `If-None-Match` revalidation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    /// The HTTP status code of the cached response.
    pub status: u16,
    /// The response body as text.
    pub body: String,
}

impl CacheEntry {
    /// Parses [`Self::fetched_at`] into a [`Timestamp`], or `None` if it is unparsable.
    #[must_use]
    pub fn fetched_at(&self) -> Option<Timestamp> {
        self.fetched_at.parse().ok()
    }
}

fn entry_path(cache_dir: &Path, url: &str) -> PathBuf {
    cache_dir
        .join("http")
        .join(format!("{:016x}.json", fnv1a_64(url)))
}

/// Reads the cached [`CacheEntry`] for `url` under `cache_dir`, if present and parseable.
///
/// Returns `None` on a cache miss or any read/parse failure — a missing or corrupt cache file is
/// not an error, it simply means "not cached".
#[must_use]
pub fn read_entry(cache_dir: &Path, url: &str) -> Option<CacheEntry> {
    let path = entry_path(cache_dir, url);
    let bytes = std::fs::read(&path).ok()?;
    let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
    if entry.url == url {
        Some(entry)
    } else {
        tracing::warn!(
            path = %path.display(),
            requested_url = url,
            cached_url = entry.url,
            "ignoring cache entry with mismatched URL provenance"
        );
        None
    }
}

/// Writes `entry` to the cache under `cache_dir`, creating the `http` subdirectory as needed.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] if the cache directory cannot be created or the file cannot
/// be written, or [`CoreError::Serialization`] if the entry cannot be serialized.
pub fn write_entry(cache_dir: &Path, entry: &CacheEntry) -> Result<(), CoreError> {
    let path = entry_path(cache_dir, &entry.url);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(entry)
        .map_err(|e| CoreError::Serialization(format!("serialize cache entry: {e}")))?;
    atomic_write(&path, &bytes)
}

/// The outcome of guarding an observed publish time against the monotonic floor.
///
/// Returned by [`PublishStore::guard`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedTime {
    /// The effective (trusted) publish instant — never earlier than any previously observed one.
    pub effective: Timestamp,
    /// `true` if the upstream value was earlier than the stored floor (a rejected backdating).
    pub backdated: bool,
}

/// A persisted store of the latest-trusted publish instant per key `(package@version@registry)`.
///
/// Loaded once per run with [`Self::load`], updated as publish times are observed
/// ([`Self::guard`]), and flushed with [`Self::save`]. The store is the home of the trust
/// hardening described at the module level: a stored instant only ever ratchets later (younger),
/// never earlier, so a backdated upstream timestamp cannot make a fresh version look mature.
///
/// # Examples
///
/// ```
/// use cooldown_registry::PublishStore;
/// use jiff::Timestamp;
///
/// // A fresh, empty directory: no prior floor, so the first observation is trusted as-is.
/// let dir = std::env::temp_dir().join(format!("cooldown-doctest-{}", std::process::id()));
/// let store = PublishStore::load(&dir);
/// let later: Timestamp = "2026-06-15T00:00:00Z".parse().unwrap();
/// let earlier: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
///
/// let first = store.guard("serde@1.0.0", later);
/// assert!(!first.backdated);
///
/// // A later refresh that claims an earlier instant is rejected; the floor holds.
/// let second = store.guard("serde@1.0.0", earlier);
/// assert!(second.backdated);
/// assert_eq!(second.effective, later);
/// ```
pub struct PublishStore {
    path: PathBuf,
    inner: Mutex<HashMap<String, String>>,
}

impl PublishStore {
    /// Loads the store under `cache_dir`, starting empty if it is absent or unreadable.
    ///
    /// A missing or corrupt `publish-times.json` is treated as an empty store rather than an
    /// error: the floor it protects is rebuilt as publish times are re-observed.
    #[must_use]
    pub fn load(cache_dir: &Path) -> Self {
        let path = cache_dir.join("publish-times.json");
        let inner = match std::fs::read(&path) {
            Ok(bytes) => {
                serde_json::from_slice::<HashMap<String, String>>(&bytes).unwrap_or_else(|error| {
                    tracing::warn!(
                        path = %path.display(),
                        error = %error,
                        "discarding corrupt publish-time store"
                    );
                    HashMap::new()
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "discarding unreadable publish-time store"
                );
                HashMap::new()
            }
        };
        PublishStore {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// Guards an observed publish instant against the stored monotonic floor for `key`.
    ///
    /// The effective value never moves earlier than any previously stored one. This records the
    /// (possibly unchanged) floor and reports, via [`GuardedTime::backdated`], whether the
    /// observation was a rejected backdating.
    ///
    /// A poisoned lock (a thread panicked while holding it) is recovered rather than propagated:
    /// the guarded map is a plain string-to-string table with no cross-entry invariant, so a
    /// partial write cannot leave it inconsistent.
    pub fn guard(&self, key: &str, observed: Timestamp) -> GuardedTime {
        let mut map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match map.get(key).and_then(|s| s.parse::<Timestamp>().ok()) {
            Some(stored) if stored > observed => GuardedTime {
                effective: stored,
                backdated: true,
            },
            _ => {
                map.insert(key.to_string(), observed.to_string());
                GuardedTime {
                    effective: observed,
                    backdated: false,
                }
            }
        }
    }

    /// Persists the store to `publish-times.json` under the cache directory.
    ///
    /// A poisoned lock is recovered rather than propagated, for the same reason as in
    /// [`Self::guard`]: the guarded map carries no invariant a partial write could break.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] if the cache directory cannot be created or the file
    /// cannot be written, or [`CoreError::Serialization`] if the store cannot be serialized.
    pub fn save(&self) -> Result<(), CoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let map = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let bytes = serde_json::to_vec_pretty(&*map)
            .map_err(|e| CoreError::Serialization(format!("serialize publish store: {e}")))?;
        atomic_write(&self.path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publish_floor_rejects_backdating() {
        let dir = tempfile::tempdir().unwrap();
        let store = PublishStore::load(dir.path());
        let later: Timestamp = "2026-06-15T00:00:00Z".parse().unwrap();
        let earlier: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

        let first = store.guard("pkg@v1", later);
        assert!(!first.backdated);
        assert_eq!(first.effective, later);

        // A later refresh claims an earlier time → rejected, floor held, flagged.
        let second = store.guard("pkg@v1", earlier);
        assert!(second.backdated);
        assert_eq!(second.effective, later);
    }

    #[test]
    fn publish_floor_allows_later() {
        let dir = tempfile::tempdir().unwrap();
        let store = PublishStore::load(dir.path());
        let t1: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
        let t2: Timestamp = "2026-06-10T00:00:00Z".parse().unwrap();
        store.guard("pkg@v1", t1);
        let g = store.guard("pkg@v1", t2);
        assert!(!g.backdated);
        assert_eq!(g.effective, t2);
    }

    #[test]
    fn cache_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let e = CacheEntry {
            url: "https://example.com/x".into(),
            fetched_at: "2026-06-17T00:00:00Z".into(),
            etag: Some("\"abc\"".into()),
            status: 200,
            body: "hello".into(),
        };
        write_entry(dir.path(), &e).unwrap();
        let got = read_entry(dir.path(), "https://example.com/x").unwrap();
        assert_eq!(got.body, "hello");
        assert_eq!(got.etag.as_deref(), Some("\"abc\""));
    }

    #[test]
    fn cache_read_rejects_mismatched_url_provenance() {
        let dir = tempfile::tempdir().unwrap();
        let requested = "https://example.com/victim";
        let path = entry_path(dir.path(), requested);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let e = CacheEntry {
            url: "https://example.com/attacker".into(),
            fetched_at: "2026-06-17T00:00:00Z".into(),
            etag: None,
            status: 200,
            body: "poison".into(),
        };
        std::fs::write(&path, serde_json::to_vec(&e).unwrap()).unwrap();

        assert!(read_entry(dir.path(), requested).is_none());
    }
}
