//! The [Hex] registry [`PackageRegistry`]: the release list and per-release publish times from
//! `api/packages/<name>`, whose `releases` array carries each version's `inserted_at` instant.
//!
//! [Hex]: https://github.com/hexpm/specifications/blob/main/apiary.apib

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;

const DEFAULT_BASE: &str = "https://hex.pm";

/// The registry name used to tag Hex-sourced [`PackageId`]s.
pub const HEXPM: &str = "hexpm";

/// A client for the [Hex] registry, implementing [`PackageRegistry`].
///
/// HTTP is shared and cached via [`SharedHttp`]; publish times pass through the store's monotonic
/// guard so a version's recorded time never moves backwards across runs.
///
/// [Hex]: https://github.com/hexpm/specifications/blob/main/apiary.apib
#[derive(Clone)]
pub struct Hex {
    http: SharedHttp,
    base: String,
}

#[derive(serde::Deserialize)]
struct Package {
    #[serde(default)]
    releases: Vec<HexRelease>,
}

#[derive(serde::Deserialize)]
struct HexRelease {
    version: String,
    #[serde(default)]
    inserted_at: Option<String>,
}

impl Hex {
    /// Creates a client against the public Hex instance (`https://hex.pm`).
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        Hex {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

    /// Returns this registry's name, [`HEXPM`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        HEXPM.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("hexpm|{name}@{vers}"), t)
                .effective,
        )
    }

    async fn get_package(&self, name: &str) -> Result<Option<Package>, CoreError> {
        let url = format!("{}/api/packages/{name}", self.base.trim_end_matches('/'));
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let parsed: Package = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{name}: {e}")))?;
        Ok(Some(parsed))
    }
}

#[async_trait]
impl PackageRegistry for Hex {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let Some(pkg) = self.get_package(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        Ok(pkg
            .releases
            .into_iter()
            .map(|rel| {
                let published_at = self.guard(
                    &package.name,
                    &rel.version,
                    rel.inserted_at
                        .as_deref()
                        .and_then(|s| s.parse::<Timestamp>().ok()),
                );
                RawRelease {
                    version: Version::new(rel.version),
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
        let Some(package) = self.get_package(&pkg.name).await? else {
            return Ok(None);
        };
        Ok(self.guard(
            &pkg.name,
            version.as_str(),
            package
                .releases
                .iter()
                .find(|rel| rel.version == version.as_str())
                .and_then(|rel| rel.inserted_at.as_deref())
                .and_then(|s| s.parse::<Timestamp>().ok()),
        ))
    }
}
