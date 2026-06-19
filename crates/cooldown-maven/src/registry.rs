//! The [Maven Central] registry [`PackageRegistry`]: the version list and per-version publish times
//! from the Central search API. A package is identified by its `group:artifact` coordinate; the
//! `gav` search returns every version with its `timestamp` (epoch milliseconds).
//!
//! [Maven Central]: https://central.sonatype.org/search/rest-api-guide/

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;

const DEFAULT_BASE: &str = "https://search.maven.org";

/// The registry name used to tag Maven-sourced [`PackageId`]s.
pub const MAVEN_CENTRAL: &str = "maven-central";

/// A client for [Maven Central], implementing [`PackageRegistry`].
///
/// HTTP is shared and cached via [`SharedHttp`]; publish times pass through the store's monotonic
/// guard so a version's recorded time never moves backwards across runs.
///
/// [Maven Central]: https://central.sonatype.org/search/rest-api-guide/
#[derive(Clone)]
pub struct MavenCentral {
    http: SharedHttp,
    base: String,
}

#[derive(serde::Deserialize)]
struct SolrResponse {
    response: SolrBody,
}

#[derive(serde::Deserialize)]
struct SolrBody {
    #[serde(default)]
    docs: Vec<SolrDoc>,
}

#[derive(serde::Deserialize)]
struct SolrDoc {
    v: String,
    #[serde(default)]
    timestamp: Option<i64>,
}

/// Splits a `group:artifact` coordinate into its parts.
fn split_coord(coord: &str) -> Option<(&str, &str)> {
    let (group, artifact) = coord.split_once(':')?;
    if group.is_empty() || artifact.is_empty() {
        return None;
    }
    Some((group, artifact))
}

impl MavenCentral {
    /// Creates a client against the public Maven Central search API.
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        MavenCentral {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

    /// Returns this registry's name, [`MAVEN_CENTRAL`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        MAVEN_CENTRAL.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("maven|{name}@{vers}"), t)
                .effective,
        )
    }

    async fn get_versions(&self, coord: &str) -> Result<Option<Vec<SolrDoc>>, CoreError> {
        let Some((group, artifact)) = split_coord(coord) else {
            return Err(CoreError::Parse(format!(
                "{coord}: expected a `group:artifact` coordinate"
            )));
        };
        // The `gav` core lists one document per version, newest first; 200 rows covers a deep
        // release history while staying a single request.
        let url = format!(
            "{}/solrsearch/select?q=g:%22{group}%22+AND+a:%22{artifact}%22&core=gav&rows=200&wt=json",
            self.base.trim_end_matches('/')
        );
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let parsed: SolrResponse = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{coord}: {e}")))?;
        if parsed.response.docs.is_empty() {
            return Ok(None);
        }
        Ok(Some(parsed.response.docs))
    }
}

fn epoch_ms(ms: Option<i64>) -> Option<Timestamp> {
    Timestamp::from_millisecond(ms?).ok()
}

#[async_trait]
impl PackageRegistry for MavenCentral {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let Some(docs) = self.get_versions(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        Ok(docs
            .into_iter()
            .map(|doc| {
                let published_at = self.guard(&package.name, &doc.v, epoch_ms(doc.timestamp));
                RawRelease {
                    version: Version::new(doc.v),
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
        let Some(docs) = self.get_versions(&pkg.name).await? else {
            return Ok(None);
        };
        Ok(self.guard(
            &pkg.name,
            version.as_str(),
            docs.iter()
                .find(|doc| doc.v == version.as_str())
                .and_then(|doc| epoch_ms(doc.timestamp)),
        ))
    }
}
