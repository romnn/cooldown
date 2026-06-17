//! On-disk metadata cache with provenance, plus the monotonic publish-time floor.
//!
//! **Trust hardening:** a cached publish time may never move *earlier* on refresh. A backdated
//! upstream timestamp (an attempt to make a fresh version look mature) is rejected and flagged, not
//! trusted — the stored value only ever ratchets later (younger), the conservative direction.

use cooldown_core::CoreError;
use jiff::Timestamp;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A cached HTTP response with provenance (the URL, when it was fetched, and its ETag).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CacheEntry {
    pub url: String,
    pub fetched_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub etag: Option<String>,
    pub status: u16,
    pub body: String,
}

impl CacheEntry {
    pub fn fetched_at(&self) -> Option<Timestamp> {
        self.fetched_at.parse().ok()
    }
}

/// A deterministic 64-bit FNV-1a hash, used to derive stable cache filenames across runs (the std
/// `DefaultHasher` is deterministic but we keep our own to be explicit and version-independent).
fn fnv1a(s: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn entry_path(cache_dir: &Path, url: &str) -> PathBuf {
    cache_dir
        .join("http")
        .join(format!("{:016x}.json", fnv1a(url)))
}

/// Read a cached entry for `url`, if present and parseable.
pub fn read_entry(cache_dir: &Path, url: &str) -> Option<CacheEntry> {
    let path = entry_path(cache_dir, url);
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Write a cache entry for `url`.
pub fn write_entry(cache_dir: &Path, entry: &CacheEntry) -> Result<(), CoreError> {
    let path = entry_path(cache_dir, &entry.url);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(entry)
        .map_err(|e| CoreError::Io(format!("serialize cache entry: {e}")))?;
    std::fs::write(&path, bytes)?;
    Ok(())
}

/// The result of guarding an observed publish time against the monotonic floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardedTime {
    /// The effective (trusted) publish instant — never earlier than any previously observed one.
    pub effective: Timestamp,
    /// `true` if the upstream value was earlier than the stored floor (a rejected backdating).
    pub backdated: bool,
}

/// A persisted store of the latest-trusted publish instant per key `(package@version@registry)`.
/// Loaded once per run; updated and saved as publish times are observed.
pub struct PublishStore {
    path: PathBuf,
    inner: Mutex<HashMap<String, String>>,
}

impl PublishStore {
    /// Load (or start empty) the store under `cache_dir`.
    pub fn load(cache_dir: &Path) -> Self {
        let path = cache_dir.join("publish-times.json");
        let inner = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<HashMap<String, String>>(&b).ok())
            .unwrap_or_default();
        PublishStore {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// Guard an observed publish instant: the effective value never moves earlier than the stored
    /// floor. Records the (possibly unchanged) floor and reports whether the observation was a
    /// rejected backdating.
    pub fn guard(&self, key: &str, observed: Timestamp) -> GuardedTime {
        let mut map = self.inner.lock().expect("publish store mutex");
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

    /// Persist the store to disk (best-effort).
    pub fn save(&self) -> Result<(), CoreError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let map = self.inner.lock().expect("publish store mutex");
        let bytes = serde_json::to_vec_pretty(&*map)
            .map_err(|e| CoreError::Io(format!("serialize publish store: {e}")))?;
        std::fs::write(&self.path, bytes)?;
        Ok(())
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
}
