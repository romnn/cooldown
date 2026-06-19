//! The [RubyGems] registry [`PackageRegistry`]: the version list and per-version publish times from
//! `api/v1/versions/<gem>.json`, which returns every version with its `created_at` instant in one
//! response.
//!
//! [RubyGems]: https://guides.rubygems.org/rubygems-org-api/

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;

const DEFAULT_BASE: &str = "https://rubygems.org";

/// The registry name used to tag `RubyGems`-sourced [`PackageId`]s.
pub const RUBYGEMS: &str = "rubygems";

/// A client for the [RubyGems] registry, implementing [`PackageRegistry`].
///
/// HTTP is shared and cached via [`SharedHttp`]; publish times pass through the store's monotonic
/// guard so a version's recorded time never moves backwards across runs.
///
/// [RubyGems]: https://guides.rubygems.org/rubygems-org-api/
#[derive(Clone)]
pub struct RubyGems {
    http: SharedHttp,
    base: String,
}

/// One entry of the `versions/<gem>.json` array: a version number and its publish instant.
#[derive(serde::Deserialize)]
struct GemVersion {
    number: String,
    #[serde(default)]
    created_at: Option<String>,
}

impl RubyGems {
    /// Creates a client against the public `RubyGems` instance (`https://rubygems.org`).
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        RubyGems {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

    /// Returns this registry's name, [`RUBYGEMS`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        RUBYGEMS.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("rubygems|{name}@{vers}"), t)
                .effective,
        )
    }

    async fn get_versions(&self, name: &str) -> Result<Option<Vec<GemVersion>>, CoreError> {
        let url = format!(
            "{}/api/v1/versions/{name}.json",
            self.base.trim_end_matches('/')
        );
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let parsed: Vec<GemVersion> = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{name}: {e}")))?;
        Ok(Some(parsed))
    }
}

#[async_trait]
impl PackageRegistry for RubyGems {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let Some(versions) = self.get_versions(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        Ok(versions
            .into_iter()
            .map(|gv| {
                let published_at = self.guard(
                    &package.name,
                    &gv.number,
                    gv.created_at
                        .as_deref()
                        .and_then(|s| s.parse::<Timestamp>().ok()),
                );
                RawRelease {
                    version: Version::new(gv.number),
                    published_at,
                    yanked: false,
                    artifacts: Vec::new(),
                }
            })
            .collect())
    }

    async fn published_at(
        &self,
        pkg: &PackageId,
        version: &Version,
        _artifacts: &[ArtifactId],
    ) -> Result<Option<Timestamp>, CoreError> {
        let Some(versions) = self.get_versions(&pkg.name).await? else {
            return Ok(None);
        };
        Ok(self.guard(
            &pkg.name,
            version.as_str(),
            versions
                .iter()
                .find(|gv| gv.number == version.as_str())
                .and_then(|gv| gv.created_at.as_deref())
                .and_then(|s| s.parse::<Timestamp>().ok()),
        ))
    }
}
