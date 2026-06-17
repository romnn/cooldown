//! The shared HTTP client: one `reqwest::Client`, an on-disk cache with ETag/TTL refresh, a
//! per-host concurrency cap with 429 backoff, and offline/fresh modes.
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
use std::time::Duration;
use tokio::sync::Semaphore;

/// Knobs for the shared client.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub offline: bool,
    pub fresh: bool,
    pub user_agent: String,
    pub per_host_concurrency: usize,
    pub request_timeout: Duration,
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
            per_host_concurrency: 8,
            request_timeout: Duration::from_secs(30),
            max_retries: 3,
        }
    }
}

/// A fetched response (from network or cache).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub from_cache: bool,
}

impl HttpResponse {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
    /// 404/410 are the single "absent" signal in the GOPROXY protocol and crates.io index.
    pub fn is_not_found(&self) -> bool {
        self.status == 404 || self.status == 410
    }
}

/// The shared HTTP client + cache + per-host limiter, cloneable via `Arc`.
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
}

impl SharedHttp {
    pub fn new(cache_dir: impl Into<PathBuf>, opts: HttpOptions) -> Result<Self, CoreError> {
        let cache_dir = cache_dir.into();
        let client = reqwest::Client::builder()
            .user_agent(&opts.user_agent)
            .gzip(true)
            .timeout(opts.request_timeout)
            .build()
            .map_err(|e| CoreError::Io(format!("build http client: {e}")))?;
        let publish = Arc::new(PublishStore::load(&cache_dir));
        Ok(SharedHttp {
            inner: Arc::new(Inner {
                client,
                cache_dir,
                opts,
                publish,
                hosts: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn publish_store(&self) -> Arc<PublishStore> {
        self.inner.publish.clone()
    }

    pub fn options(&self) -> &HttpOptions {
        &self.inner.opts
    }

    fn semaphore_for(&self, host: &str) -> Arc<Semaphore> {
        let mut hosts = self.inner.hosts.lock().expect("hosts mutex");
        hosts
            .entry(host.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.inner.opts.per_host_concurrency)))
            .clone()
    }

    /// GET `url`, honoring the cache TTL, offline/fresh modes, ETag refresh, and 429 backoff.
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
        if !self.inner.opts.fresh {
            if let Some(e) = &cached {
                if e.status < 400 && cache_is_fresh(e, ttl) {
                    return Ok(HttpResponse {
                        status: e.status,
                        body: e.body.clone(),
                        from_cache: true,
                    });
                }
            }
        }

        let host = reqwest::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| "unknown".to_string());
        let sem = self.semaphore_for(&host);
        let _permit = sem.acquire().await.expect("semaphore not closed");

        let etag = cached.as_ref().and_then(|e| e.etag.clone());
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.fetch_once(url, etag.as_deref(), cached.as_ref()).await {
                Ok(resp) => return Ok(resp),
                Err(FetchError::Backoff(delay)) if attempt <= self.inner.opts.max_retries => {
                    tokio::time::sleep(delay).await;
                }
                Err(FetchError::Transient(e)) => {
                    // Fall back to a stale cached copy if we have one; else propagate.
                    if let Some(c) = &cached {
                        if c.status < 400 {
                            return Ok(HttpResponse {
                                status: c.status,
                                body: c.body.clone(),
                                from_cache: true,
                            });
                        }
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

        if status == 304 {
            if let Some(c) = cached {
                let mut refreshed = c.clone();
                refreshed.fetched_at = Timestamp::now().to_string();
                let _ = write_entry(&self.inner.cache_dir, &refreshed);
                return Ok(HttpResponse {
                    status: c.status,
                    body: c.body.clone(),
                    from_cache: true,
                });
            }
        }

        if status == 429 || (500..600).contains(&status) {
            let delay = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|h| h.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(Duration::from_secs)
                .unwrap_or_else(|| Duration::from_millis(750));
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
    age >= 0 && (age as u64) < ttl.as_secs()
}
