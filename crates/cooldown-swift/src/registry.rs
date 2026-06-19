//! The [GitHub Releases] registry [`PackageRegistry`] for Swift packages. `SwiftPM` has no central
//! package index with publish times — dependencies are git URLs and versions are git tags — so for
//! GitHub-hosted packages the publish instant is the release's `published_at`. A package's
//! [`PackageId`] name is its `owner/repo`.
//!
//! Limitations: only packages hosted on github.com that publish GitHub **Releases** (not bare tags)
//! resolve here; others report as not found and surface as `unknown-age`. Requests are
//! unauthenticated, so a project with many dependencies can hit GitHub's hourly rate limit.
//!
//! [GitHub Releases]: https://docs.github.com/en/rest/releases/releases

use crate::version;
use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;

const DEFAULT_BASE: &str = "https://api.github.com";

/// The registry name used to tag GitHub-sourced [`PackageId`]s.
pub const GITHUB: &str = "github";

/// A client for the [GitHub Releases] API, implementing [`PackageRegistry`].
///
/// [GitHub Releases]: https://docs.github.com/en/rest/releases/releases
#[derive(Clone)]
pub struct GitHubReleases {
    http: SharedHttp,
    base: String,
}

#[derive(serde::Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
}

impl GitHubReleases {
    /// Creates a client against the public GitHub API (`https://api.github.com`).
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        GitHubReleases {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

    /// Returns this registry's name, [`GITHUB`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        GITHUB.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("github|{name}@{vers}"), t)
                .effective,
        )
    }

    /// Fetches up to the 100 most recent releases of `owner/repo`.
    async fn get_releases(&self, repo: &str) -> Result<Option<Vec<GhRelease>>, CoreError> {
        let url = format!(
            "{}/repos/{repo}/releases?per_page=100",
            self.base.trim_end_matches('/')
        );
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let parsed: Vec<GhRelease> = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{repo}: {e}")))?;
        Ok(Some(parsed))
    }
}

#[async_trait]
impl PackageRegistry for GitHubReleases {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let Some(releases) = self.get_releases(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        Ok(releases
            .into_iter()
            .filter(|rel| !rel.draft)
            .map(|rel| {
                let vers = version::strip_tag_prefix(&rel.tag_name).to_string();
                let published_at = self.guard(
                    &package.name,
                    &vers,
                    rel.published_at
                        .as_deref()
                        .and_then(|s| s.parse::<Timestamp>().ok()),
                );
                RawRelease {
                    version: Version::new(vers),
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
        let Some(releases) = self.get_releases(&pkg.name).await? else {
            return Ok(None);
        };
        Ok(self.guard(
            &pkg.name,
            version.as_str(),
            releases
                .iter()
                .find(|rel| version::strip_tag_prefix(&rel.tag_name) == version.as_str())
                .and_then(|rel| rel.published_at.as_deref())
                .and_then(|s| s.parse::<Timestamp>().ok()),
        ))
    }
}
