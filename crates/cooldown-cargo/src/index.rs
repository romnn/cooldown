//! The crates.io sparse-index [`PackageRegistry`]. The modern sparse index carries a `pubtime` per
//! version (RFC3339), so one fetch per crate yields versions + yanked + publish time — no API call
//! and no rate-limit gymnastics. Publish times pass through the monotonic floor.

use async_trait::async_trait;
use cooldown_core::{ArtifactId, CoreError, PackageId, PackageRegistry, RawRelease, Version};
use cooldown_registry::{SharedHttp, ttl};
use jiff::Timestamp;

const DEFAULT_INDEX: &str = "https://index.crates.io";
pub const CRATES_IO: &str = "crates.io";

/// A crates.io sparse-index client over the shared HTTP layer.
#[derive(Clone)]
pub struct CratesIoIndex {
    http: SharedHttp,
    base: String,
}

#[derive(serde::Deserialize)]
struct IndexLine {
    vers: String,
    #[serde(default)]
    yanked: bool,
    #[serde(default)]
    pubtime: Option<String>,
}

impl CratesIoIndex {
    pub fn new(http: SharedHttp) -> Self {
        CratesIoIndex {
            http,
            base: DEFAULT_INDEX.to_string(),
        }
    }

    pub fn registry_name(&self) -> String {
        CRATES_IO.to_string()
    }

    /// The sparse-index path for a crate name (lowercased): `1/a`, `2/ip`, `3/l/log`, `se/rd/serde`.
    fn index_path(name: &str) -> String {
        let n = name.to_lowercase();
        match n.len() {
            0 => n,
            1 => format!("1/{n}"),
            2 => format!("2/{n}"),
            3 => format!("3/{}/{n}", &n[0..1]),
            _ => format!("{}/{}/{n}", &n[0..2], &n[2..4]),
        }
    }

    async fn fetch_lines(&self, name: &str) -> Result<Vec<IndexLine>, CoreError> {
        let url = format!(
            "{}/{}",
            self.base.trim_end_matches('/'),
            Self::index_path(name)
        );
        let resp = self.http.get(&url, ttl::LISTING).await?;
        if resp.is_not_found() {
            return Err(CoreError::NotFound(name.to_string()));
        }
        if !resp.is_success() {
            return Err(CoreError::transient(format!("{url}: HTTP {}", resp.status)));
        }
        let mut out = Vec::new();
        for line in resp.body.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parsed: IndexLine = serde_json::from_str(line)
                .map_err(|e| CoreError::Parse(format!("{name} index line: {e}")))?;
            out.push(parsed);
        }
        Ok(out)
    }

    fn guard(&self, name: &str, vers: &str, t: Option<Timestamp>) -> Option<Timestamp> {
        let t = t?;
        Some(
            self.http
                .publish_store()
                .guard(&format!("cargo|{name}@{vers}"), t)
                .effective,
        )
    }
}

#[async_trait]
impl PackageRegistry for CratesIoIndex {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        let lines = self.fetch_lines(&package.name).await?;
        Ok(lines
            .into_iter()
            .map(|l| {
                let t = l
                    .pubtime
                    .as_deref()
                    .and_then(|s| s.parse::<Timestamp>().ok());
                let published_at = self.guard(&package.name, &l.vers, t);
                RawRelease {
                    version: Version::new(l.vers),
                    published_at,
                    yanked: l.yanked,
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
        let lines = self.fetch_lines(&pkg.name).await?;
        match lines.into_iter().find(|l| l.vers == version.as_str()) {
            Some(l) => {
                let t = l
                    .pubtime
                    .as_deref()
                    .and_then(|s| s.parse::<Timestamp>().ok());
                Ok(self.guard(&pkg.name, version.as_str(), t))
            }
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_path_layout() {
        assert_eq!(CratesIoIndex::index_path("a"), "1/a");
        assert_eq!(CratesIoIndex::index_path("ip"), "2/ip");
        assert_eq!(CratesIoIndex::index_path("log"), "3/l/log");
        assert_eq!(CratesIoIndex::index_path("serde"), "se/rd/serde");
        assert_eq!(CratesIoIndex::index_path("Serde"), "se/rd/serde"); // lowercased
    }
}
