//! The shared HTTP client: one `reqwest::Client`, an on-disk cache with ETag/TTL refresh, a
//! per-host concurrency cap with optional per-host request pacing and 429 backoff, and
//! offline/fresh modes.
//!
//! - `--offline`: cache only; a miss is a hard [`CoreError::OfflineMiss`] (the caller maps it to
//!   `UnknownAge` — never a false "ok").
//! - `--fresh`/`--no-cache`: always hit the registry (CI gates use it).
//! - On a transient failure with a cached copy, the stale copy is served (better than failing an
//!   `outdated`); with no cache, the transient error propagates so `check` can fail closed.

use crate::cache::{CacheEntry, PublishStore, read_entry, write_entry};
use cooldown_core::CoreError;
use jiff::Timestamp;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

/// Configuration knobs for a [`SharedHttp`] client.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    /// Serve from cache only; a miss is a hard [`CoreError::OfflineMiss`] (the `--offline` mode).
    pub offline: bool,
    /// Always hit the registry, ignoring a fresh cache hit (the `--fresh`/`--no-cache` mode).
    pub fresh: bool,
    /// The `User-Agent` header sent on every request.
    pub user_agent: String,
    /// The maximum number of in-flight requests allowed per host.
    pub per_host_concurrency: usize,
    /// Per-host minimum spacing between outgoing requests, keyed by host (e.g. `"api.github.com"`).
    /// A listed host has its requests started at least this far apart, keeping a strict registry
    /// under its rate budget; a host with no entry is bounded only by [`per_host_concurrency`].
    ///
    /// [`per_host_concurrency`]: HttpOptions::per_host_concurrency
    pub per_host_min_interval: HashMap<String, Duration>,
    /// The per-request timeout applied by the underlying `reqwest::Client`.
    pub request_timeout: Duration,
    /// The number of times a 429/5xx backoff is retried before the request fails.
    pub max_retries: usize,
}

impl Default for HttpOptions {
    fn default() -> Self {
        HttpOptions {
            offline: false,
            fresh: false,
            user_agent: concat!(
                "cooldown/",
                env!("CARGO_PKG_VERSION"),
                " (+https://github.com/romnn/cooldown)"
            )
            .to_string(),
            per_host_concurrency: 16,
            per_host_min_interval: default_host_min_intervals(),
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
        }
    }
}

/// Built-in per-host request spacing. Only registries with a strict rate budget are listed;
/// everything else is governed by the concurrency cap alone. `api.github.com` (Swift package
/// discovery) has a 60-requests/hour unauthenticated budget and trips secondary limits on bursts,
/// so its requests are spaced ~1s apart — polite, and never bursty enough to draw a 403/429.
fn default_host_min_intervals() -> HashMap<String, Duration> {
    HashMap::from([("api.github.com".to_string(), Duration::from_secs(1))])
}

/// A response served by [`SharedHttp::get`], from the network or the on-disk cache.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The HTTP status code (a cached or revalidated copy carries its original status).
    pub status: u16,
    /// The response body as text.
    pub body: String,
    /// `true` if served from cache (a fresh hit, a 304 revalidation, or a stale fallback).
    pub from_cache: bool,
}

impl HttpResponse {
    /// Returns `true` if the status is a 2xx success.
    #[must_use]
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
    /// Returns `true` if the status signals "absent" (404 or 410).
    ///
    /// 404/410 are the single "absent" signal in the GOPROXY protocol and crates.io index.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.status == 404 || self.status == 410
    }
}

/// The shared HTTP client, on-disk cache, and per-host concurrency limiter.
///
/// Cheap to [`Clone`] — the state lives behind an [`Arc`], so every clone shares one
/// `reqwest::Client`, one cache directory, one [`PublishStore`], and one set of per-host
/// semaphores. Construct it with [`Self::new`] and fetch through [`Self::get`].
#[derive(Clone)]
pub struct SharedHttp {
    inner: Arc<Inner>,
}

struct Inner {
    client: reqwest::Client,
    cache_dir: PathBuf,
    opts: HttpOptions,
    publish: Arc<PublishStore>,
    hosts: Mutex<HashMap<String, Arc<Semaphore>>>,
    pacers: Mutex<HashMap<String, Arc<HostPacer>>>,
}

/// A per-host request pacer: requests to the host start at least `interval` apart, so a strict
/// registry is never hit in a burst. Cloned [`SharedHttp`] handles share one pacer per host (it
/// lives behind the [`Inner`] map), so the spacing holds across every adapter targeting that host.
struct HostPacer {
    interval: Duration,
    /// The earliest instant the next request may start. Behind an async mutex so callers queue on
    /// it — awaiting their turn — instead of each polling a clock.
    gate: tokio::sync::Mutex<Instant>,
}

impl HostPacer {
    fn new(interval: Duration) -> Self {
        HostPacer {
            interval,
            gate: tokio::sync::Mutex::new(Instant::now()),
        }
    }

    /// Await this host's turn, then claim the next slot. Callers serialize on the gate, and each
    /// sleeps only until its reserved instant — a timer wait, never a poll — so consecutive requests
    /// to the host begin at least `interval` apart.
    async fn throttle(&self) {
        let mut next = self.gate.lock().await;
        let start = (*next).max(Instant::now());
        tokio::time::sleep_until(tokio::time::Instant::from_std(start)).await;
        *next = start + self.interval;
    }
}

impl SharedHttp {
    /// Builds a client rooted at `cache_dir` with the given [`HttpOptions`].
    ///
    /// Loads the [`PublishStore`] from `cache_dir` eagerly so the monotonic publish-time floor is
    /// in effect from the first request.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::System`] if the underlying `reqwest::Client` cannot be built (for
    /// example, the platform TLS backend fails to initialize).
    pub fn new(cache_dir: impl Into<PathBuf>, opts: HttpOptions) -> Result<Self, CoreError> {
        let cache_dir = cache_dir.into();
        let client = reqwest::Client::builder()
            .user_agent(&opts.user_agent)
            .gzip(true)
            .timeout(opts.request_timeout)
            .build()
            .map_err(|e| CoreError::System(format!("build http client: {e}")))?;
        let publish = Arc::new(PublishStore::load(&cache_dir));
        Ok(SharedHttp {
            inner: Arc::new(Inner {
                client,
                cache_dir,
                opts,
                publish,
                hosts: Mutex::new(HashMap::new()),
                pacers: Mutex::new(HashMap::new()),
            }),
        })
    }

    /// Returns the shared [`PublishStore`] backing the monotonic publish-time floor.
    ///
    /// Adapters use it to [`guard`](PublishStore::guard) observed publish instants and to
    /// [`save`](PublishStore::save) the floor at the end of a run.
    #[must_use]
    pub fn publish_store(&self) -> Arc<PublishStore> {
        self.inner.publish.clone()
    }

    /// Returns the [`HttpOptions`] this client was built with.
    #[must_use]
    pub fn options(&self) -> &HttpOptions {
        &self.inner.opts
    }

    fn semaphore_for(&self, host: &str) -> Arc<Semaphore> {
        let mut hosts = self
            .inner
            .hosts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        hosts
            .entry(host.to_string())
            .or_insert_with(|| {
                Arc::new(Semaphore::new(self.inner.opts.per_host_concurrency.max(1)))
            })
            .clone()
    }

    /// The pacer for `host`, or `None` when the host has no configured minimum interval (the common
    /// case — only strict registries are paced). Built lazily and shared across clones.
    fn pacer_for(&self, host: &str) -> Option<Arc<HostPacer>> {
        let interval = self.inner.opts.per_host_min_interval.get(host).copied()?;
        let mut pacers = self
            .inner
            .pacers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Some(
            pacers
                .entry(host.to_string())
                .or_insert_with(|| Arc::new(HostPacer::new(interval)))
                .clone(),
        )
    }

    /// Performs a `GET` of `url`, honoring the cache `ttl`, offline/fresh modes, `ETag`
    /// revalidation, and 429/5xx backoff.
    ///
    /// Resolution order: an offline build serves a cached non-error copy or fails with
    /// [`CoreError::OfflineMiss`]; otherwise a fresh cached hit short-circuits (unless
    /// [`HttpOptions::fresh`] is set); otherwise the request goes out under the per-host
    /// concurrency cap, retrying transient backoff up to [`HttpOptions::max_retries`] and falling
    /// back to a stale cached copy when the network fails.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::OfflineMiss`] for an offline cache miss; [`CoreError::Transient`] if a
    /// network failure has no stale cache to fall back on, or if the host keeps rate-limiting past
    /// the retry budget.
    pub async fn get(&self, url: &str, ttl: Duration) -> Result<HttpResponse, CoreError> {
        let cached = read_entry(&self.inner.cache_dir, url);

        // Offline: serve cache or fail (never a false success).
        if self.inner.opts.offline {
            return match cached {
                Some(e) if e.status < 400 => Ok(HttpResponse {
                    status: e.status,
                    body: e.body,
                    from_cache: true,
                }),
                _ => Err(CoreError::OfflineMiss(url.to_string())),
            };
        }

        // Fresh cache hit (and not forced fresh): serve it.
        if !self.inner.opts.fresh
            && let Some(e) = &cached
            && e.status < 400
            && cache_is_fresh(e, ttl)
        {
            tracing::trace!(url, status = e.status, "cache hit");
            return Ok(HttpResponse {
                status: e.status,
                body: e.body.clone(),
                from_cache: true,
            });
        }

        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        // Pace a strict host before taking a concurrency permit, so the wait never occupies an
        // in-flight slot. Hosts with no configured interval skip this entirely.
        if let Some(pacer) = self.pacer_for(&host) {
            tracing::trace!(host, "awaiting host pace");
            pacer.throttle().await;
        }
        let sem = self.semaphore_for(&host);
        // The per-host semaphores are never closed, so `acquire` only fails if one were; treat
        // that impossible case as a transient error rather than panicking.
        let _permit = sem
            .acquire()
            .await
            .map_err(|e| CoreError::transient(e.to_string()))?;

        let etag = cached.as_ref().and_then(|e| e.etag.clone());
        let mut attempt = 0;
        loop {
            attempt += 1;
            tracing::debug!(url, host, attempt, "http request");
            match self.fetch_once(url, etag.as_deref(), cached.as_ref()).await {
                Ok(resp) => {
                    tracing::trace!(url, status = resp.status, "http response");
                    return Ok(resp);
                }
                Err(FetchError::Backoff(delay)) if attempt <= self.inner.opts.max_retries => {
                    tokio::time::sleep(delay).await;
                }
                Err(FetchError::Transient(e)) => {
                    // Fall back to a stale cached copy if we have one; else propagate.
                    if !self.inner.opts.fresh
                        && let Some(c) = &cached
                        && c.status < 400
                    {
                        return Ok(HttpResponse {
                            status: c.status,
                            body: c.body.clone(),
                            from_cache: true,
                        });
                    }
                    return Err(CoreError::transient(e));
                }
                Err(FetchError::Backoff(_)) => {
                    return Err(CoreError::Transient(
                        format!(
                            "rate-limited by {host} after {} retries",
                            self.inner.opts.max_retries
                        )
                        .into(),
                    ));
                }
            }
        }
    }

    async fn fetch_once(
        &self,
        url: &str,
        etag: Option<&str>,
        cached: Option<&CacheEntry>,
    ) -> Result<HttpResponse, FetchError> {
        let mut req = self.inner.client.get(url);
        if let Some(tag) = etag {
            req = req.header(reqwest::header::IF_NONE_MATCH, tag);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| FetchError::Transient(e.to_string()))?;
        let status = resp.status().as_u16();

        if status == 304
            && let Some(c) = cached
        {
            let mut refreshed = c.clone();
            refreshed.fetched_at = Timestamp::now().to_string();
            let _ = write_entry(&self.inner.cache_dir, &refreshed);
            return Ok(HttpResponse {
                status: c.status,
                body: c.body.clone(),
                from_cache: true,
            });
        }

        if status == 429 || (500..600).contains(&status) {
            let delay = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map_or_else(|| Duration::from_millis(750), Duration::from_secs);
            return Err(FetchError::Backoff(delay));
        }

        let new_etag = resp
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        let body = resp
            .text()
            .await
            .map_err(|e| FetchError::Transient(e.to_string()))?;

        if (200..300).contains(&status) {
            let entry = CacheEntry {
                url: url.to_string(),
                fetched_at: Timestamp::now().to_string(),
                etag: new_etag,
                status,
                body: body.clone(),
            };
            let _ = write_entry(&self.inner.cache_dir, &entry);
        }

        Ok(HttpResponse {
            status,
            body,
            from_cache: false,
        })
    }
}

enum FetchError {
    Transient(String),
    Backoff(Duration),
}

fn cache_is_fresh(entry: &CacheEntry, ttl: Duration) -> bool {
    let Some(fetched) = entry.fetched_at() else {
        return false;
    };
    let now = Timestamp::now();
    let age = now.as_second().saturating_sub(fetched.as_second());
    // A negative age means the entry was fetched "in the future" (clock skew); treat it as not
    // fresh. `try_from` both drops that case and converts without a sign-loss cast.
    u64::try_from(age).is_ok_and(|secs| secs < ttl.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_entry(url: &str) -> CacheEntry {
        CacheEntry {
            url: url.to_string(),
            fetched_at: "2026-06-18T00:00:00Z".into(),
            etag: None,
            status: 200,
            body: "cached".into(),
        }
    }

    #[tokio::test]
    async fn fresh_mode_does_not_fallback_to_stale_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = "http://127.0.0.1:1/test";
        write_entry(dir.path(), &cache_entry(url)).expect("cache entry");
        let http = SharedHttp::new(
            dir.path(),
            HttpOptions {
                fresh: true,
                request_timeout: Duration::from_millis(100),
                ..Default::default()
            },
        )
        .expect("shared http");

        let result = http.get(url, Duration::ZERO).await;
        assert!(matches!(result, Err(CoreError::Transient(_))));
    }

    #[tokio::test]
    async fn non_fresh_mode_can_fallback_to_stale_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = "http://127.0.0.1:1/test";
        write_entry(dir.path(), &cache_entry(url)).expect("cache entry");
        let http = SharedHttp::new(
            dir.path(),
            HttpOptions {
                fresh: false,
                request_timeout: Duration::from_millis(100),
                ..Default::default()
            },
        )
        .expect("shared http");

        let response = http.get(url, Duration::ZERO).await.expect("stale fallback");
        assert!(response.from_cache);
        assert_eq!(response.body, "cached");
    }

    #[tokio::test]
    async fn host_pacer_serializes_calls_by_the_interval() {
        let pacer = HostPacer::new(Duration::from_millis(50));
        let started = Instant::now();
        // The first call is immediate; the next two each await ~one more interval, so three calls
        // span ~two intervals end to end.
        pacer.throttle().await;
        pacer.throttle().await;
        pacer.throttle().await;
        assert!(started.elapsed() >= Duration::from_millis(90));
    }

    #[test]
    fn pacer_is_built_only_for_configured_hosts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(dir.path(), HttpOptions::default()).expect("shared http");
        // The built-in default paces GitHub (strict budget) but leaves CDN-backed registries free.
        assert!(http.pacer_for("api.github.com").is_some());
        assert!(http.pacer_for("index.crates.io").is_none());
    }
}
