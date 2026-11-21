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

const DEFAULT_RETRY_DELAY: Duration = Duration::from_millis(750);
const MAX_RETRY_AFTER: Duration = Duration::from_mins(1);
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

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
    /// `true` if the served cached copy was past its TTL — an offline hit on an aged entry, or
    /// the stale fallback after a network failure.
    ///
    /// Consumers whose data may only *loosen* a decision while fresh (the advisory feed) key
    /// off this; registry consumers can ignore it (their hardening is the monotonic
    /// publish-time floor instead).
    pub stale: bool,
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
    // This shared lease excludes pruning for the lifetime of every cloned HTTP client.
    #[expect(
        dead_code,
        reason = "the open file holds a shared cache-maintenance lock until the last client drops"
    )]
    cache_lease: Option<std::fs::File>,
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
        let cache_lease = crate::cache::protect_from_prune(&cache_dir);
        Ok(SharedHttp {
            inner: Arc::new(Inner {
                client,
                cache_dir,
                opts,
                publish,
                hosts: Mutex::new(HashMap::new()),
                pacers: Mutex::new(HashMap::new()),
                cache_lease,
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

    /// Returns the directory the client keeps its response cache in, which adapters may also use
    /// for scratch state of their own.
    #[must_use]
    pub fn cache_dir(&self) -> &std::path::Path {
        &self.inner.cache_dir
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
        self.request(url, url, ttl, RequestKind::Get).await
    }

    /// Performs a `POST` of a JSON `body` to `url`, cached like [`get`](Self::get) under a key
    /// derived from the URL *and* the request body.
    ///
    /// The cached entry stores the verbatim request body and is served only when it matches the
    /// outgoing one, so two different queries to one endpoint never alias — not even on a
    /// digest collision (querybatch responses match requests by index, so an aliased response
    /// would misattribute advisories across packages).
    /// Same offline/fresh semantics, per-host cap and pacing, 429/5xx backoff, and stale-cache
    /// fallback; no `ETag` revalidation (conditional requests are a `GET` concept).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::OfflineMiss`] for an offline cache miss; [`CoreError::Transient`]
    /// if a network failure has no cached copy to fall back on, or if the host keeps
    /// rate-limiting past the retry budget.
    pub async fn post_json(
        &self,
        url: &str,
        body: &str,
        ttl: Duration,
    ) -> Result<HttpResponse, CoreError> {
        let cache_key = format!(
            "{url}#{:016x}",
            cooldown_core::fs::fnv1a_64(&format!("{url}\n{body}"))
        );
        self.request(url, &cache_key, ttl, RequestKind::PostJson { body })
            .await
    }

    #[tracing::instrument(name = "registry_request", skip_all, fields(url, cache_key))]
    async fn request(
        &self,
        url: &str,
        cache_key: &str,
        ttl: Duration,
        request: RequestKind<'_>,
    ) -> Result<HttpResponse, CoreError> {
        if let Some(body) = request.body() {
            cooldown_core::trace::artifact(
                cooldown_core::trace::ArtifactSource::HttpRequest { url },
                body.as_bytes(),
            );
        }
        match self.request_inner(url, cache_key, ttl, request).await {
            Ok(Delivered {
                response,
                network_status,
            }) => {
                cooldown_core::trace::artifact(
                    cooldown_core::trace::ArtifactSource::Http {
                        key: cache_key,
                        status: response.status,
                        from_cache: response.from_cache,
                        stale: response.stale,
                        network_status,
                    },
                    response.body.as_bytes(),
                );
                Ok(response)
            }
            Err(err) => {
                tracing::debug!(error = %err, "registry request failed");
                Err(err)
            }
        }
    }

    async fn request_inner(
        &self,
        url: &str,
        cache_key: &str,
        ttl: Duration,
        request: RequestKind<'_>,
    ) -> Result<Delivered, CoreError> {
        let cached = read_entry(&self.inner.cache_dir, cache_key).filter(|entry| {
            request
                .body()
                .is_none_or(|body| entry.request_body.as_deref() == Some(body))
        });

        if self.inner.opts.offline {
            return match cached {
                Some(entry) if entry.status < 400 => {
                    let stale = !cache_is_fresh(&entry, ttl);
                    Ok(Delivered::from_cache(entry, stale))
                }
                _ => Err(CoreError::OfflineMiss(url.to_string())),
            };
        }

        if !self.inner.opts.fresh
            && let Some(entry) = &cached
            && entry.status < 400
            && cache_is_fresh(entry, ttl)
        {
            tracing::trace!(
                url,
                method = request.method(),
                status = entry.status,
                "cache hit"
            );
            return Ok(Delivered::from_cache(entry.clone(), false));
        }

        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|parsed| parsed.host_str().map(str::to_string))
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
            .map_err(|error| CoreError::transient(error.to_string()))?;

        let mut attempt = 0;
        loop {
            attempt += 1;
            tracing::debug!(
                url,
                host,
                method = request.method(),
                attempt,
                "http request"
            );
            match self
                .fetch_once(url, cache_key, cached.as_ref(), request)
                .await
            {
                Ok(Fetched {
                    response,
                    network_status,
                }) => {
                    tracing::trace!(
                        url,
                        status = network_status,
                        from_cache = response.from_cache,
                        "http response"
                    );
                    return Ok(Delivered {
                        response,
                        network_status: Some(network_status),
                    });
                }
                Err(FetchError::Backoff(delay)) if attempt <= self.inner.opts.max_retries => {
                    tokio::time::sleep(delay).await;
                }
                // Both registry GETs and advisory query POSTs are safe to retry because the latter
                // is a read-only query despite its transport verb.
                Err(FetchError::Transient(error)) if attempt == 1 => {
                    tracing::debug!(url, host, error = %error, "transient transport failure; retrying once");
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(FetchError::Transient(error)) => {
                    if !self.inner.opts.fresh
                        && let Some(entry) = &cached
                        && entry.status < 400
                    {
                        return Ok(Delivered::from_cache(entry.clone(), true));
                    }
                    return Err(CoreError::transient(error));
                }
                Err(FetchError::Fatal(error)) => return Err(error),
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
        cache_key: &str,
        cached: Option<&CacheEntry>,
        request: RequestKind<'_>,
    ) -> Result<Fetched, FetchError> {
        let mut outgoing = match request {
            RequestKind::Get => self.inner.client.get(url),
            RequestKind::PostJson { body } => self
                .inner
                .client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body.to_string()),
        };
        if request.uses_etag()
            && let Some(etag) = cached.and_then(|entry| entry.etag.as_deref())
        {
            outgoing = outgoing.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        let response = outgoing
            .send()
            .await
            .map_err(|error| FetchError::Transient(error.to_string()))?;
        let status = response.status().as_u16();

        if request.uses_etag()
            && status == 304
            && let Some(entry) = cached
        {
            let mut refreshed = entry.clone();
            refreshed.fetched_at = Timestamp::now().to_string();
            let _ = write_entry(&self.inner.cache_dir, &refreshed);
            return Ok(Fetched {
                response: cached_response(refreshed, false),
                network_status: status,
            });
        }

        if status == 429 || (500..600).contains(&status) {
            let delay = retry_after_delay(
                response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|header| header.to_str().ok()),
            );
            return Err(FetchError::Backoff(delay));
        }

        let etag = if request.uses_etag() {
            response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|header| header.to_str().ok())
                .map(str::to_string)
        } else {
            None
        };
        let body = read_limited_body(response, url).await?;
        if (200..300).contains(&status) {
            let entry = CacheEntry {
                url: cache_key.to_string(),
                fetched_at: Timestamp::now().to_string(),
                etag,
                status,
                body: body.clone(),
                request_body: request.body().map(str::to_string),
            };
            let _ = write_entry(&self.inner.cache_dir, &entry);
        }

        Ok(Fetched {
            response: HttpResponse {
                status,
                body,
                from_cache: false,
                stale: false,
            },
            network_status: status,
        })
    }
}

#[derive(Clone, Copy)]
enum RequestKind<'a> {
    Get,
    PostJson { body: &'a str },
}

impl<'a> RequestKind<'a> {
    const fn body(self) -> Option<&'a str> {
        match self {
            Self::Get => None,
            Self::PostJson { body } => Some(body),
        }
    }

    const fn method(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::PostJson { .. } => "POST",
        }
    }

    const fn uses_etag(self) -> bool {
        matches!(self, Self::Get)
    }
}

/// A response as [`SharedHttp::request_inner`] delivers it, before it is traced.
struct Delivered {
    response: HttpResponse,
    /// The status the server answered, or `None` when no response came from the network.
    network_status: Option<u16>,
}

impl Delivered {
    fn from_cache(entry: CacheEntry, stale: bool) -> Self {
        Delivered {
            response: cached_response(entry, stale),
            network_status: None,
        }
    }
}

/// The outcome of one network round trip that the server answered.
struct Fetched {
    response: HttpResponse,
    /// The status the server answered, which is `304` when `response` is the revalidated
    /// cached copy.
    network_status: u16,
}

fn cached_response(entry: CacheEntry, stale: bool) -> HttpResponse {
    HttpResponse {
        status: entry.status,
        body: entry.body,
        from_cache: true,
        stale,
    }
}

enum FetchError {
    Transient(String),
    Backoff(Duration),
    Fatal(CoreError),
}

fn retry_after_delay(value: Option<&str>) -> Duration {
    // RFC 9110 also allows an HTTP-date here. Treat that form as the short default rather than
    // adding clock-dependent parsing; only explicit second counts extend the delay, and they are
    // capped so one hostile response cannot stall the whole host queue for hours.
    value
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .map_or(DEFAULT_RETRY_DELAY, |delay| delay.min(MAX_RETRY_AFTER))
}

async fn read_limited_body(mut resp: reqwest::Response, url: &str) -> Result<String, FetchError> {
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| FetchError::Transient(e.to_string()))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(FetchError::Fatal(CoreError::Parse(format!(
                "response body for {url} exceeds {} MiB",
                MAX_RESPONSE_BYTES / 1024 / 1024
            ))));
        }
        body.extend_from_slice(&chunk);
    }
    String::from_utf8(body).map_err(|error| {
        FetchError::Fatal(CoreError::Parse(format!(
            "response body for {url} is not valid UTF-8: {error}"
        )))
    })
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
            request_body: None,
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
        std::assert_matches!(result, Err(CoreError::Transient(_)));
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

    /// A lone transport flicker (connection dropped mid-request) must not fail the run: the
    /// request is retried once and the second attempt's response is returned as a normal fetch.
    #[tokio::test]
    async fn transient_transport_failure_is_retried_once() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            // First connection: accepted and dropped without a response — the transport flicker.
            let (first, _) = listener.accept().await.expect("first connection");
            drop(first);
            // Second connection: a minimal 200.
            let (mut second, _) = listener.accept().await.expect("second connection");
            let mut buf = [0u8; 1024];
            let _ = second.read(&mut buf).await;
            let _ = second
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok")
                .await;
        });
        let dir = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(dir.path(), HttpOptions::default()).expect("shared http");

        let url = format!("http://{addr}/index");
        let response = http.get(&url, Duration::ZERO).await.expect("retried fetch");
        assert_eq!(response.body, "ok");
        assert!(!response.from_cache);
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

    /// The POST cache never serves a response for a *different* request body, even when the
    /// digest in the cache key collides — the stored verbatim body is the identity check, so a
    /// forged or colliding entry is a miss, not a hit.
    #[tokio::test]
    async fn post_cache_validates_the_request_body_not_just_the_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let url = "http://127.0.0.1:1/v1/querybatch";
        let body = r#"{"queries":[{"package":{"name":"a"}}]}"#;
        let cache_key = format!(
            "{url}#{:016x}",
            cooldown_core::fs::fnv1a_64(&format!("{url}\n{body}"))
        );
        // An entry filed under this request's key but recording a different request body — the
        // shape a 64-bit digest collision (or a tampered file) would produce.
        let mut entry = cache_entry(&cache_key);
        entry.request_body = Some("something else".to_string());
        write_entry(dir.path(), &entry).expect("cache entry");
        let offline = HttpOptions {
            offline: true,
            ..Default::default()
        };
        let http = SharedHttp::new(dir.path(), offline.clone()).expect("shared http");
        let miss = http.post_json(url, body, Duration::from_hours(1)).await;
        std::assert_matches!(miss, Err(CoreError::OfflineMiss(_)));

        // The genuine entry (matching request body) is served.
        entry.request_body = Some(body.to_string());
        write_entry(dir.path(), &entry).expect("cache entry");
        let http = SharedHttp::new(dir.path(), offline).expect("shared http");
        let hit = http
            .post_json(url, body, Duration::from_hours(1))
            .await
            .expect("cache hit");
        assert!(hit.from_cache);
        assert_eq!(hit.body, "cached");
    }

    /// Serves each request on a loopback port with the raw HTTP response `reply` builds, after
    /// `delay`, and counts the requests it receives.
    ///
    /// `reply` gets the zero-based request number and the lowercased request head.
    /// Every answer closes its connection, so the count is also the number of round trips.
    async fn serve(
        delay: Duration,
        reply: impl Fn(usize, &str) -> String + Send + Sync + 'static,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let reply = Arc::new(reply);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let hit = counter.fetch_add(1, Ordering::SeqCst);
                let reply = reply.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
                    let mut head = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => head.extend_from_slice(&buf[..read]),
                        }
                    }
                    let head = String::from_utf8_lossy(&head).to_lowercase();
                    tokio::time::sleep(delay).await;
                    let _ = stream.write_all(reply(hit, &head).as_bytes()).await;
                });
            }
        });
        (base, hits)
    }

    fn ok(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\netag: \"v1\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Answers `304` to a request revalidating the `"v1"` copy and `200` with `body` otherwise,
    /// like a registry whose document has not changed.
    fn unchanged_document(body: &'static str) -> impl Fn(usize, &str) -> String {
        move |_, head| {
            if head.contains("if-none-match: \"v1\"") {
                "HTTP/1.1 304 Not Modified\r\netag: \"v1\"\r\nconnection: close\r\n\r\n".to_string()
            } else {
                ok(body)
            }
        }
    }

    fn hits(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Each zero-TTL request revalidates its cached copy with the registry: the client never
    /// treats an earlier response as fresh beyond its TTL.
    #[tokio::test]
    async fn sequential_zero_ttl_requests_each_revalidate() {
        let (base, counter) = serve(Duration::ZERO, unchanged_document("doc")).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(dir.path(), HttpOptions::default()).expect("shared http");
        let url = format!("{base}/semver");

        let first = http.get(&url, Duration::ZERO).await.expect("download");
        let second = http.get(&url, Duration::ZERO).await.expect("revalidation");

        // The second request reached the server, which answered `304` for the cached copy.
        assert_eq!(hits(&counter), 2);
        assert!(!first.from_cache);
        assert!(second.from_cache && !second.stale);
        assert_eq!(second.body, "doc");
    }

    /// Offline mode serves the cache or fails with an offline miss, and never sends a request.
    #[tokio::test]
    async fn offline_mode_never_touches_the_network() {
        let (base, counter) = serve(Duration::ZERO, unchanged_document("doc")).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let cached_url = format!("{base}/cached");
        write_entry(dir.path(), &cache_entry(&cached_url)).expect("cache entry");
        let http = SharedHttp::new(
            dir.path(),
            HttpOptions {
                offline: true,
                ..Default::default()
            },
        )
        .expect("shared http");

        // An aged entry is served as stale rather than revalidated, twice.
        for _ in 0..2 {
            let response = http.get(&cached_url, Duration::ZERO).await.expect("cached");
            assert!(response.from_cache && response.stale);
        }
        let miss = http.get(&format!("{base}/missing"), Duration::ZERO).await;
        std::assert_matches!(miss, Err(CoreError::OfflineMiss(_)));
        assert_eq!(hits(&counter), 0);
    }

    /// The `trace_event` payloads of the run trace, as a recorder at the compact detail sees them.
    #[derive(Clone, Default)]
    struct Observed(Arc<Mutex<Vec<serde_json::Value>>>);

    impl Observed {
        fn install(&self) -> tracing::dispatcher::DefaultGuard {
            use tracing_subscriber::prelude::*;
            let filter = tracing_subscriber::filter::filter_fn(|metadata| {
                metadata.target() == cooldown_core::trace::TARGET
                    && *metadata.level() <= tracing::Level::DEBUG
            });
            tracing::dispatcher::set_default(&tracing::Dispatch::new(
                tracing_subscriber::registry().with(self.clone().with_filter(filter)),
            ))
        }

        /// The sources of the HTTP artifacts recorded so far, in order.
        fn http_sources(&self) -> Vec<serde_json::Value> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .filter(|event| event["kind"] == "artifact_started")
                .map(|event| event["source"].clone())
                .filter(|source| source["kind"] == "http")
                .collect()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Observed {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            struct Payload(Option<serde_json::Value>);
            impl tracing::field::Visit for Payload {
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    if field.name() == "trace_event" {
                        self.0 = serde_json::from_str(value).ok();
                    }
                }
                fn record_debug(&mut self, _: &tracing::field::Field, _: &dyn std::fmt::Debug) {}
            }
            let mut payload = Payload(None);
            event.record(&mut payload);
            if let Some(payload) = payload.0 {
                self.0
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(payload);
            }
        }
    }

    /// The trace tells a revalidation from a download: after a `304` the delivered cached copy
    /// keeps its `200`, and `networkStatus` carries the server's `304`.
    #[tokio::test]
    async fn the_trace_records_the_status_the_server_returned() {
        let (base, _) = serve(Duration::ZERO, unchanged_document("doc")).await;
        let dir = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(dir.path(), HttpOptions::default()).expect("shared http");
        let url = format!("{base}/semver");
        let observed = Observed::default();
        let _subscriber = observed.install();

        // Download, revalidate, then serve the copy from cache within its TTL.
        http.get(&url, Duration::ZERO).await.expect("download");
        http.get(&url, Duration::ZERO).await.expect("revalidation");
        http.get(&url, Duration::from_hours(1))
            .await
            .expect("cache hit");

        let sources = observed.http_sources();
        assert_eq!(sources.len(), 3);
        // The download
        assert_eq!(sources[0]["status"], 200);
        assert_eq!(sources[0]["networkStatus"], 200);
        assert_eq!(sources[0]["fromCache"], false);
        // The revalidation
        assert_eq!(sources[1]["status"], 200);
        assert_eq!(sources[1]["networkStatus"], 304);
        assert_eq!(sources[1]["fromCache"], true);
        // The cache hit, which never reached the server
        assert_eq!(sources[2]["status"], 200);
        assert!(sources[2].get("networkStatus").is_none());
        assert_eq!(sources[2]["fromCache"], true);
    }

    #[test]
    fn retry_after_seconds_are_capped_and_dates_use_default_delay() {
        assert_eq!(retry_after_delay(Some("3")), Duration::from_secs(3));
        assert_eq!(retry_after_delay(Some("100000")), MAX_RETRY_AFTER);
        assert_eq!(
            retry_after_delay(Some("Wed, 21 Oct 2015 07:28:00 GMT")),
            DEFAULT_RETRY_DELAY
        );
        assert_eq!(retry_after_delay(None), DEFAULT_RETRY_DELAY);
    }
}
