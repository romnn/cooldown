//! The npm registry [`PackageRegistry`]: the full version list and per-version publish times from
//! the package document at `registry.npmjs.org/<pkg>`. npm serves one tarball per version (no
//! per-file split like PyPI), and the document's `time` map gives each version's publish instant.

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;
use std::collections::HashMap;

const DEFAULT_BASE: &str = "https://registry.npmjs.org";

/// The registry name used to tag npm-sourced [`PackageId`]s. Shared by every npm-compatible tool
/// (npm, pnpm, yarn, bun) since they all resolve from the same index.
pub const NPM: &str = "npm";

/// A client for the [npm registry], implementing [`PackageRegistry`].
///
/// It fetches the package document (the full version list plus the `time` map) and derives each
/// release's publish instant from `time[version]`. HTTP is shared and cached via [`SharedHttp`];
/// publish times pass through the store's monotonic guard so a version's recorded time never moves
/// backwards across runs.
///
/// [npm registry]: https://github.com/npm/registry/blob/main/docs/REGISTRY-API.md
#[derive(Clone)]
pub struct NpmRegistry {
    http: SharedHttp,
    base: String,
}

/// The slice of the npm package document we consume.
///
/// `versions` is the authoritative list of INSTALLABLE versions (its keys); `time` maps each version
/// to its ISO-8601 publish instant. The two diverge: a version that has been **unpublished** is removed
/// from `versions` but its timestamp LINGERS in `time` (npm never prunes the `time` map). So the
/// version list must come from `versions` — sourcing it from `time` would propose an unpublished
/// version the package manager cannot fetch (e.g. the unpublished `colors` 1.4.1/1.4.2), which then
/// fails the whole joint resolve. Only the `versions` keys are needed; the heavy per-version metadata
/// is discarded via [`IgnoredAny`](serde::de::IgnoredAny).
#[derive(serde::Deserialize)]
struct Doc {
    #[serde(default)]
    #[allow(
        clippy::zero_sized_map_values,
        reason = "the ZST `IgnoredAny` value makes serde skip the heavy per-version metadata; only \
                  the keys (the installable version list) are needed"
    )]
    versions: HashMap<String, serde::de::IgnoredAny>,
    #[serde(default)]
    time: HashMap<String, String>,
}

/// The INSTALLABLE releases of a package doc: each `versions` key paired with its publish instant from
/// `time`. A version present only in `time` — an unpublished version npm never pruned from the `time`
/// map — is excluded, so cooldown never proposes a version the package manager cannot fetch.
fn installable_releases(doc: &Doc) -> impl Iterator<Item = (&String, Option<Timestamp>)> {
    doc.versions.keys().map(|vers| {
        let when = doc
            .time
            .get(vers)
            .and_then(|when| when.parse::<Timestamp>().ok());
        (vers, when)
    })
}

impl NpmRegistry {
    /// Creates a client against the public npm registry (`https://registry.npmjs.org`).
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        NpmRegistry {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

    /// Returns this registry's name, [`NPM`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        NPM.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("npm|{name}@{vers}"), t)
                .effective,
        )
    }

    /// The package-document URL. A scoped name (`@scope/pkg`) keeps its leading `@` but the
    /// separating slash is percent-encoded, as the registry expects.
    fn doc_url(&self, name: &str) -> String {
        format!(
            "{}/{}",
            self.base.trim_end_matches('/'),
            name.replace('/', "%2f")
        )
    }

    /// Fetches and parses the package document, returning `None` on a 404 so callers can decide
    /// whether an absent package is a hard error (release listing) or simply unknown (publish-time
    /// lookup).
    async fn get_doc(&self, name: &str) -> Result<Option<Doc>, CoreError> {
        let url = self.doc_url(name);
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let doc: Doc = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{name}: {e}")))?;
        Ok(Some(doc))
    }
}

#[async_trait]
impl PackageRegistry for NpmRegistry {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let Some(doc) = self.get_doc(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        Ok(installable_releases(&doc)
            .map(|(vers, when)| {
                let published_at = self.guard(&package.name, vers, when);
                RawRelease {
                    version: Version::new(vers.clone()),
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
        let Some(Doc { time, .. }) = self.get_doc(&pkg.name).await? else {
            return Ok(None);
        };
        Ok(self.guard(
            &pkg.name,
            version.as_str(),
            time.get(version.as_str())
                .and_then(|s| s.parse::<Timestamp>().ok()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::Doc;

    /// A version unpublished from the registry is removed from `versions` but its timestamp lingers in
    /// `time`. The release list must come from `versions` so cooldown never proposes the unpublished
    /// version — the `colors` 1.4.1/1.4.2 case that otherwise fails the whole joint resolve.
    #[test]
    fn unpublished_versions_lingering_in_time_are_excluded() {
        let doc: Doc = serde_json::from_str(
            r#"{
                "versions": {
                    "1.3.0": { "name": "colors", "version": "1.3.0" },
                    "1.4.0": { "name": "colors", "version": "1.4.0" }
                },
                "time": {
                    "created": "2014-01-01T00:00:00.000Z",
                    "modified": "2022-01-09T00:00:00.000Z",
                    "1.3.0": "2018-01-01T00:00:00.000Z",
                    "1.4.0": "2020-04-30T00:00:00.000Z",
                    "1.4.1": "2022-01-08T00:00:00.000Z",
                    "1.4.2": "2022-01-09T00:00:00.000Z"
                }
            }"#,
        )
        .expect("parse doc");

        let mut versions: Vec<&str> = super::installable_releases(&doc)
            .map(|(vers, _)| vers.as_str())
            .collect();
        versions.sort_unstable();
        // Only the two versions still in `versions`; the unpublished 1.4.1/1.4.2 (time-only) are gone.
        assert_eq!(versions, vec!["1.3.0", "1.4.0"]);

        // The kept versions carry their publish instant from `time`.
        let with_time = super::installable_releases(&doc)
            .filter(|(_, when)| when.is_some())
            .count();
        assert_eq!(with_time, 2);
    }
}
