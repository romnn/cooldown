//! The [anaconda.org] registry [`PackageRegistry`] for conda packages: the version list and
//! per-version publish times from `api/anaconda.org/package/<channel>/<name>`. The response carries
//! a `versions` list plus a `files` array (one per build/platform), each file stamped with an
//! `upload_time`; a version's publish instant is the earliest upload across its files.
//!
//! Packages are resolved from the `conda-forge` channel — by far the dominant community channel.
//! A package only on another channel (e.g. `defaults`) reports as not found.
//!
//! [anaconda.org]: https://api.anaconda.org/docs

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;
use std::collections::HashMap;

const DEFAULT_BASE: &str = "https://api.anaconda.org";
const DEFAULT_CHANNEL: &str = "conda-forge";

/// The registry name used to tag conda-sourced [`PackageId`]s.
pub const CONDA: &str = "conda";

/// A client for the [anaconda.org] package API, implementing [`PackageRegistry`].
///
/// [anaconda.org]: https://api.anaconda.org/docs
#[derive(Clone)]
pub struct Conda {
    http: SharedHttp,
    base: String,
    channel: String,
}

#[derive(serde::Deserialize)]
struct PackageDoc {
    #[serde(default)]
    versions: Vec<String>,
    #[serde(default)]
    files: Vec<CondaFile>,
}

#[derive(serde::Deserialize)]
struct CondaFile {
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    upload_time: Option<String>,
}

/// Parses anaconda.org's `upload_time` (`2017-11-15 14:56:40.319000+00:00`) by promoting the
/// date/time separator to the RFC 3339 `T`.
fn parse_upload_time(s: &str) -> Option<Timestamp> {
    s.replacen(' ', "T", 1).parse::<Timestamp>().ok()
}

impl Conda {
    /// Creates a client against the public anaconda.org API, resolving from `conda-forge`.
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        Conda {
            http,
            base: DEFAULT_BASE.to_string(),
            channel: DEFAULT_CHANNEL.to_string(),
        }
    }

    /// Returns this registry's name, [`CONDA`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        CONDA.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("conda|{name}@{vers}"), t)
                .effective,
        )
    }

    async fn get_doc(&self, name: &str) -> Result<Option<PackageDoc>, CoreError> {
        let url = format!(
            "{}/package/{}/{name}",
            self.base.trim_end_matches('/'),
            self.channel
        );
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let doc: PackageDoc = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{name}: {e}")))?;
        Ok(Some(doc))
    }

    /// Builds a version → earliest-upload-time map from the per-build `files` list.
    fn earliest_times(files: Vec<CondaFile>) -> HashMap<String, Timestamp> {
        let mut earliest: HashMap<String, Timestamp> = HashMap::new();
        for file in files {
            let (Some(version), Some(time)) = (
                file.version,
                file.upload_time.as_deref().and_then(parse_upload_time),
            ) else {
                continue;
            };
            earliest
                .entry(version)
                .and_modify(|e| {
                    if time < *e {
                        *e = time;
                    }
                })
                .or_insert(time);
        }
        earliest
    }
}

#[async_trait]
impl PackageRegistry for Conda {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let Some(doc) = self.get_doc(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        let times = Self::earliest_times(doc.files);
        Ok(doc
            .versions
            .into_iter()
            .map(|vers| {
                let published_at = self.guard(&package.name, &vers, times.get(&vers).copied());
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
        let Some(doc) = self.get_doc(&pkg.name).await? else {
            return Ok(None);
        };
        let times = Self::earliest_times(doc.files);
        Ok(self.guard(
            &pkg.name,
            version.as_str(),
            times.get(version.as_str()).copied(),
        ))
    }
}
