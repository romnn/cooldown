//! The GOPROXY-backed [`PackageRegistry`]. Publish times come from `.info` `Time` fields parsed to
//! typed instants (never compared as strings) and passed through the monotonic publish-time floor.
//! Module paths are escaped with the `!`-lowercase rule; 404/410 is the single "absent" signal.

use crate::semver;
use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;

/// A Go module proxy client over the shared HTTP layer.
#[derive(Clone)]
pub struct GoProxy {
    http: SharedHttp,
    /// Proxy base URLs in order (from `GOPROXY`, minus `direct`/`off`).
    bases: Vec<String>,
}

/// The metadata for a single module version, parsed from a proxy `.info`/`@latest` response.
#[derive(Debug, Clone)]
pub struct ProxyInfo {
    /// The canonical version string reported by the proxy.
    pub version: String,
    /// The publish time, if the `.info` response carried a parseable `Time` field.
    pub time: Option<Timestamp>,
}

#[derive(serde::Deserialize)]
struct InfoJson {
    #[serde(rename = "Version")]
    version: String,
    #[serde(rename = "Time")]
    time: Option<String>,
}

impl GoProxy {
    /// Creates a proxy client over `http` that tries each base URL in `bases` in order.
    ///
    /// `bases` should already be filtered to HTTP(S) endpoints (the `direct`/`off`
    /// `GOPROXY` keywords are not valid base URLs); use [`GoProxy::from_env`] to derive
    /// them from the environment.
    #[must_use]
    pub fn new(http: SharedHttp, bases: Vec<String>) -> Self {
        GoProxy { http, bases }
    }

    /// Builds a proxy client from the `GOPROXY` environment variable.
    ///
    /// Defaults to `https://proxy.golang.org,direct` when `GOPROXY` is unset. The
    /// `direct` and `off` keywords are dropped, leaving the ordered HTTP(S) bases.
    #[must_use]
    pub fn from_env(http: SharedHttp) -> Self {
        let raw = std::env::var("GOPROXY")
            .unwrap_or_else(|_| "https://proxy.golang.org,direct".to_string());
        let bases = parse_goproxy(&raw);
        GoProxy { http, bases }
    }

    /// The reporting registry name (the first proxy host), for the JSON `registry` field.
    #[must_use]
    pub fn registry_name(&self) -> Option<String> {
        self.bases.first().and_then(|b| host_of(b))
    }

    /// Escape a module path or version per the `!`-lowercase rule.
    fn escape(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for c in s.chars() {
            if c.is_ascii_uppercase() {
                out.push('!');
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    async fn get_first(
        &self,
        suffix: &str,
        ttl: std::time::Duration,
    ) -> Result<Option<String>, CoreError> {
        let mut last_err: Option<CoreError> = None;
        for base in &self.bases {
            let url = format!("{}/{}", base.trim_end_matches('/'), suffix);
            match self.http.get(&url, ttl).await {
                Ok(resp) if resp.is_not_found() => {} // advance to the next proxy on 404/410
                Ok(resp) if resp.is_success() => return Ok(Some(resp.body)),
                Ok(resp) => {
                    last_err = Some(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
                }
                Err(CoreError::OfflineMiss(_)) => return Ok(None), // offline miss → unknown age
                Err(e) => last_err = Some(e),
            }
        }
        match last_err {
            Some(e) => Err(e),
            None => Ok(None), // all bases returned 404, or no bases configured
        }
    }

    /// Fetches `@v/list`: the module's tagged versions, one per line.
    ///
    /// Includes prereleases but not pseudo-versions. Returns an empty vector when no
    /// proxy has a listing for the module (a 404/410 from every base).
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError`] if a proxy responds with a non-success, non-404 status
    /// or the transport itself fails (see [`cooldown_core::CoreError`]).
    pub async fn list(&self, module: &str) -> Result<Vec<String>, CoreError> {
        let suffix = format!("{}/@v/list", Self::escape(module));
        match self.get_first(&suffix, ttl::LISTING).await? {
            Some(body) => Ok(body
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(std::string::ToString::to_string)
                .collect()),
            None => Ok(Vec::new()),
        }
    }

    /// Fetches `@v/<version>.info`: the metadata for a specific version.
    ///
    /// The response is immutable and cached long. Returns `None` when no proxy has the
    /// version (a 404/410 from every base).
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError::Parse`] if the response body is not valid `.info` JSON,
    /// or a transport-level [`CoreError`] (see [`list`](Self::list)).
    pub async fn info(&self, module: &str, version: &str) -> Result<Option<ProxyInfo>, CoreError> {
        let suffix = format!("{}/@v/{}.info", Self::escape(module), Self::escape(version));
        let Some(body) = self.get_first(&suffix, ttl::IMMUTABLE).await? else {
            return Ok(None);
        };
        let parsed: InfoJson = serde_json::from_str(&body)
            .map_err(|e| CoreError::Parse(format!("{module}@{version}.info: {e}")))?;
        let time = parsed
            .time
            .as_deref()
            .and_then(|t| t.parse::<Timestamp>().ok());
        Ok(Some(ProxyInfo {
            version: parsed.version,
            time,
        }))
    }

    /// Fetches `@latest`: the proxy's notion of the module's latest version.
    ///
    /// Used as a fallback when [`list`](Self::list) returns no tagged versions. Returns
    /// `None` when no proxy can resolve a latest version.
    ///
    /// # Errors
    ///
    /// Returns a [`CoreError::Parse`] if the response body is not valid `.info` JSON,
    /// or a transport-level [`CoreError`] (see [`list`](Self::list)).
    pub async fn latest(&self, module: &str) -> Result<Option<ProxyInfo>, CoreError> {
        let suffix = format!("{}/@latest", Self::escape(module));
        let Some(body) = self.get_first(&suffix, ttl::LISTING).await? else {
            return Ok(None);
        };
        let parsed: InfoJson = serde_json::from_str(&body)
            .map_err(|e| CoreError::Parse(format!("{module}/@latest: {e}")))?;
        let time = parsed
            .time
            .as_deref()
            .and_then(|t| t.parse::<Timestamp>().ok());
        Ok(Some(ProxyInfo {
            version: parsed.version,
            time,
        }))
    }

    /// Apply the monotonic publish-time floor and return the trusted instant.
    fn guard(&self, module: &str, version: &str, observed: Option<Timestamp>) -> Option<Timestamp> {
        let observed = observed?;
        let key = format!("go|{module}@{version}");
        Some(self.http.publish_store().guard(&key, observed).effective)
    }
}

#[async_trait]
impl PackageRegistry for GoProxy {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let module = &package.name;
        let mut versions = self.list(module).await?;
        if versions.is_empty()
            && let Some(latest) = self.latest(module).await?
        {
            versions.push(latest.version);
        }

        // Fetch .info for every listed version concurrently; the per-host semaphore bounds load.
        let futs = versions.iter().map(|v| async move {
            let info = self.info(module, v).await;
            (v.clone(), info)
        });
        let infos = futures::future::join_all(futs).await;

        let mut out = Vec::with_capacity(infos.len());
        for (version, info) in infos {
            let time = match info {
                Ok(Some(i)) => self.guard(module, &version, i.time),
                Err(e) if e.is_transient() => return Err(e),
                // A proxy 404 or a single non-transient `.info` failure → unknown age.
                Ok(None) | Err(_) => None,
            };
            out.push(RawRelease {
                version: Version::new(version),
                published_at: time,
                yanked: false, // Go has no version retraction in the proxy metadata
                artifacts: Vec::new(),
            });
        }
        Ok(out)
    }

    async fn published_at(
        &self,
        pkg: &PackageId,
        version: &Version,
        _artifacts: &[ArtifactId],
    ) -> Result<Option<Timestamp>, CoreError> {
        // Pseudo-versions encode their commit time; trust the embedded value (also passed through
        // the floor) without a network round-trip.
        if let Some(t) = semver::pseudo_time(version.as_str()) {
            return Ok(self.guard(&pkg.name, version.as_str(), Some(t)));
        }
        match self.info(&pkg.name, version.as_str()).await? {
            Some(info) => Ok(self.guard(&pkg.name, version.as_str(), info.time)),
            None => Ok(None),
        }
    }
}

/// Parse `GOPROXY` into ordered base URLs, dropping `direct`/`off` keywords.
fn parse_goproxy(raw: &str) -> Vec<String> {
    raw.split(['|', ','])
        .map(str::trim)
        .filter(|e| e.starts_with("http://") || e.starts_with("https://"))
        .map(std::string::ToString::to_string)
        .collect()
}

/// Extract the host from a base URL without pulling in a URL-parsing dependency.
fn host_of(url: &str) -> Option<String> {
    let after = url.split("://").nth(1)?;
    let authority = after.split('/').next()?;
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    (!host.is_empty()).then(|| host.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_uppercase() {
        assert_eq!(
            GoProxy::escape("github.com/Sirupsen/logrus"),
            "github.com/!sirupsen/logrus"
        );
        assert_eq!(
            GoProxy::escape("github.com/GoogleCloudPlatform/x"),
            "github.com/!google!cloud!platform/x"
        );
        assert_eq!(
            GoProxy::escape("v3.0.0+incompatible"),
            "v3.0.0+incompatible"
        );
    }

    #[test]
    fn parses_goproxy_list() {
        assert_eq!(
            parse_goproxy("https://proxy.golang.org,direct"),
            vec!["https://proxy.golang.org".to_string()]
        );
        assert_eq!(
            parse_goproxy("https://a.example|https://b.example|off"),
            vec![
                "https://a.example".to_string(),
                "https://b.example".to_string()
            ]
        );
        assert!(parse_goproxy("off").is_empty());
    }
}
