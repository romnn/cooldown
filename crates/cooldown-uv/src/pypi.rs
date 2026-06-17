//! The PyPI JSON-API [`PackageRegistry`]: the full version list and per-file upload times
//! (`upload_time_iso_8601`). A release has several files (wheels per platform + an sdist), each
//! with its own time; the release time is the newest, but `None` if any file lacks one.

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{ttl, SharedHttp};
use jiff::Timestamp;
use std::collections::HashMap;

const DEFAULT_BASE: &str = "https://pypi.org";
pub const PYPI: &str = "pypi";

#[derive(Clone)]
pub struct PyPi {
    http: SharedHttp,
    base: String,
}

#[derive(serde::Deserialize)]
struct PyFile {
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

fn newest_or_none(files: &[PyFile]) -> Option<Timestamp> {
    if files.is_empty() {
        return None;
    }
    let mut newest: Option<Timestamp> = None;
    for f in files {
        match f
            .upload_time
            .as_deref()
            .and_then(|s| s.parse::<Timestamp>().ok())
        {
            None => return None,
            Some(t) => newest = Some(newest.map_or(t, |n| n.max(t))),
        }
    }
    newest
}

fn all_yanked(files: &[PyFile]) -> bool {
    !files.is_empty() && files.iter().all(|f| f.yanked)
}

impl PyPi {
    pub fn new(http: SharedHttp) -> Self {
        PyPi {
            http,
            base: DEFAULT_BASE.to_string(),
        }
    }

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
                let published_at = self.guard(&package.name, &vers, newest_or_none(&files));
                RawRelease {
                    version: Version::new(vers),
                    published_at,
                    yanked: all_yanked(&files),
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
        Ok(self.guard(&pkg.name, version.as_str(), newest_or_none(&parsed.urls)))
    }
}
