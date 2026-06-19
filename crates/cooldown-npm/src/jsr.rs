//! The [JSR] registry [`PackageRegistry`]: the version list and per-version publish times from a
//! package's `meta.json`. JSR is Deno's first-party registry; its packages are always scoped
//! (`@scope/name`) and versioned with `SemVer`, and each version's `createdAt` gives its publish
//! instant — so a single `meta.json` fetch yields everything the cooldown core needs.
//!
//! [JSR]: https://jsr.io

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;
use std::collections::HashMap;

const DEFAULT_BASE: &str = "https://jsr.io";

/// The registry name used to tag JSR-sourced [`PackageId`]s.
pub const JSR: &str = "jsr";

/// A client for the [JSR] registry, implementing [`PackageRegistry`] against `meta.json`.
///
/// HTTP is shared and cached via [`SharedHttp`]; publish times pass through the store's monotonic
/// guard so a version's recorded time never moves backwards across runs.
///
/// [JSR]: https://jsr.io
#[derive(Clone)]
pub struct JsrRegistry {
    http: SharedHttp,
    base: String,
}

/// The slice of a JSR `meta.json` we consume: the version map, each entry carrying its publish
/// instant (`createdAt`) and yank flag.
#[derive(serde::Deserialize)]
struct Meta {
    #[serde(default)]
    versions: HashMap<String, MetaVersion>,
}

#[derive(serde::Deserialize)]
struct MetaVersion {
    #[serde(default, rename = "createdAt")]
    created_at: Option<String>,
    #[serde(default)]
    yanked: bool,
}

impl JsrRegistry {
    /// Creates a client against the public JSR registry (`https://jsr.io`).
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        JsrRegistry {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

    /// Returns this registry's name, [`JSR`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        JSR.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("jsr|{name}@{vers}"), t)
                .effective,
        )
    }

    /// The `meta.json` URL for a scoped package name (`@scope/name`), which JSR serves verbatim in
    /// the path.
    fn meta_url(&self, name: &str) -> String {
        format!("{}/{}/meta.json", self.base.trim_end_matches('/'), name)
    }

    async fn get_meta(&self, name: &str) -> Result<Option<Meta>, CoreError> {
        let url = self.meta_url(name);
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let meta: Meta = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{name}: {e}")))?;
        Ok(Some(meta))
    }
}

#[async_trait]
impl PackageRegistry for JsrRegistry {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let Some(meta) = self.get_meta(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        Ok(meta
            .versions
            .into_iter()
            .map(|(vers, info)| {
                let published_at = self.guard(
                    &package.name,
                    &vers,
                    info.created_at
                        .as_deref()
                        .and_then(|s| s.parse::<Timestamp>().ok()),
                );
                RawRelease {
                    version: Version::new(vers),
                    published_at,
                    yanked: info.yanked,
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
        let Some(meta) = self.get_meta(&pkg.name).await? else {
            return Ok(None);
        };
        Ok(self.guard(
            &pkg.name,
            version.as_str(),
            meta.versions
                .get(version.as_str())
                .and_then(|info| info.created_at.as_deref())
                .and_then(|s| s.parse::<Timestamp>().ok()),
        ))
    }
}
