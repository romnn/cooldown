//! The `PyPI` JSON-API [`PackageRegistry`]: the full version list and per-file upload times
//! (`upload_time_iso_8601`). A release has several files (wheels per platform + an sdist), each
//! with its own time; the release time is the newest, but `None` if any file lacks one.

use crate::artifact::{artifact_id_from_filename, newest_or_none, published_at_for_artifacts};
use async_trait::async_trait;
use cooldown_core::{
    ArtifactId, CoreError, PackageId, PackageRegistry, RawArtifact, RawRelease, Version,
};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;
use std::collections::HashMap;

const DEFAULT_BASE: &str = "https://pypi.org";

/// The registry name used to tag PyPI-sourced [`PackageId`]s.
pub const PYPI: &str = "pypi";

/// A client for the [PyPI JSON API], implementing [`PackageRegistry`].
///
/// It fetches the full version list and, per release, the newest per-file upload
/// time (PyPI's PEP 700 `upload_time_iso_8601`). HTTP is shared and cached via
/// [`SharedHttp`]; publish times pass through the store's monotonic guard so a
/// version's recorded time never moves backwards across runs.
///
/// [PyPI JSON API]: https://docs.pypi.org/api/json/
#[derive(Clone)]
pub struct PyPi {
    http: SharedHttp,
    base: String,
}

#[derive(serde::Deserialize)]
struct PyFile {
    #[serde(default)]
    filename: Option<String>,
    #[serde(default, rename = "upload_time_iso_8601")]
    upload_time: Option<String>,
    #[serde(default)]
    yanked: bool,
}

#[derive(serde::Deserialize)]
struct AllJson {
    #[serde(default)]
    releases: HashMap<String, Vec<PyFile>>,
}

#[derive(serde::Deserialize)]
struct VersionJson {
    #[serde(default)]
    urls: Vec<PyFile>,
}

fn all_yanked(files: &[PyFile]) -> bool {
    !files.is_empty() && files.iter().all(|f| f.yanked)
}

fn raw_artifacts(files: &[PyFile]) -> Vec<RawArtifact> {
    files
        .iter()
        .filter_map(|file| {
            let id = file.filename.as_deref().map(artifact_id_from_filename)?;
            Some(RawArtifact {
                id,
                published_at: file
                    .upload_time
                    .as_deref()
                    .and_then(|value| value.parse::<Timestamp>().ok()),
                markers: Vec::new(),
            })
        })
        .collect()
}

fn file_upload_times(files: &[PyFile]) -> impl Iterator<Item = Option<Timestamp>> + '_ {
    files.iter().map(|file| {
        file.upload_time
            .as_deref()
            .and_then(|value| value.parse::<Timestamp>().ok())
    })
}

impl PyPi {
    /// Creates a client against the public PyPI instance (`https://pypi.org`).
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        PyPi {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

    /// Returns this registry's name, [`PYPI`], for tagging [`PackageId`]s.
    #[must_use]
    pub fn registry_name(&self) -> String {
        PYPI.to_string()
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("pypi|{name}@{vers}"), t)
                .effective,
        )
    }
}

#[async_trait]
impl PackageRegistry for PyPi {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let url = format!(
            "{}/pypi/{}/json",
            self.base.trim_end_matches('/'),
            package.name
        );
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Err(CoreError::NotFound(package.name.clone()));
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let parsed: AllJson = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{}: {e}", package.name)))?;
        Ok(parsed
            .releases
            .into_iter()
            .map(|(vers, files)| {
                let artifacts = raw_artifacts(&files);
                let published_at = self.guard(
                    &package.name,
                    &vers,
                    if artifacts.is_empty() {
                        newest_or_none(file_upload_times(&files))
                    } else {
                        newest_or_none(artifacts.iter().map(|artifact| artifact.published_at))
                    },
                );
                RawRelease {
                    version: Version::new(vers),
                    published_at,
                    yanked: all_yanked(&files),
                    artifacts,
                }
            })
            .collect())
    }

    async fn published_at(
        &self,
        pkg: &PackageId,
        version: &Version,
        artifacts: &[ArtifactId],
    ) -> Result<Option<Timestamp>, CoreError> {
        let url = format!(
            "{}/pypi/{}/{}/json",
            self.base.trim_end_matches('/'),
            pkg.name,
            version
        );
        let resp = self.http.get(&url, ttl::IMMUTABLE).await?;
        if resp.is_not_found() {
            return Ok(None);
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let parsed: VersionJson = serde_json::from_str(&resp.body)
            .map_err(|e| CoreError::Parse(format!("{}@{version}: {e}", pkg.name)))?;
        let release_artifacts = raw_artifacts(&parsed.urls);
        let published_at = if artifacts.is_empty() || release_artifacts.is_empty() {
            newest_or_none(file_upload_times(&parsed.urls))
        } else {
            published_at_for_artifacts(&release_artifacts, artifacts)
        };
        Ok(self.guard(&pkg.name, version.as_str(), published_at))
    }
}
