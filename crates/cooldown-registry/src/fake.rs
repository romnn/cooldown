//! A `PackageRegistry` fake for driving adapters in unit/conformance tests without network.

use async_trait::async_trait;
use cooldown_core::{
    ArtifactId, CoreError, PackageId, PackageRegistry, RawArtifact, RawRelease, Version,
};
use jiff::Timestamp;
use std::collections::HashMap;

/// An in-memory registry. Build it with the chained `with_*` helpers in tests.
#[derive(Default, Clone)]
pub struct FakeRegistry {
    releases: HashMap<String, Vec<RawRelease>>,
}

impl FakeRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a version-granular release (no per-file artifacts).
    pub fn with_release(
        mut self,
        pkg: &str,
        version: &str,
        published_at: Option<&str>,
        yanked: bool,
    ) -> Self {
        let rr = RawRelease {
            version: Version::new(version),
            published_at: published_at.map(parse),
            yanked,
            artifacts: Vec::new(),
        };
        self.releases.entry(pkg.to_string()).or_default().push(rr);
        self
    }

    /// Add an artifact-granular release (e.g. a PyPI release with several files).
    pub fn with_artifact_release(
        mut self,
        pkg: &str,
        version: &str,
        artifacts: &[(&str, Option<&str>)],
    ) -> Self {
        let arts: Vec<RawArtifact> = artifacts
            .iter()
            .map(|(id, t)| RawArtifact {
                id: ArtifactId((*id).to_string()),
                published_at: t.map(parse),
                markers: Vec::new(),
            })
            .collect();
        // Version-level published_at is the newest known artifact (or None if any unknown).
        let published_at = newest_or_none(arts.iter().map(|a| a.published_at));
        let rr = RawRelease {
            version: Version::new(version),
            published_at,
            yanked: false,
            artifacts: arts,
        };
        self.releases.entry(pkg.to_string()).or_default().push(rr);
        self
    }
}

fn parse(s: &str) -> Timestamp {
    s.parse().expect("valid RFC3339 in test fixture")
}

/// Newest of the times, but `None` if any is unknown (conservative).
fn newest_or_none(times: impl Iterator<Item = Option<Timestamp>>) -> Option<Timestamp> {
    let mut newest: Option<Timestamp> = None;
    for t in times {
        match t {
            None => return None,
            Some(t) => newest = Some(newest.map_or(t, |n| n.max(t))),
        }
    }
    newest
}

#[async_trait]
impl PackageRegistry for FakeRegistry {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        Ok(self
            .releases
            .get(&package.name)
            .cloned()
            .unwrap_or_default())
    }

    async fn published_at(
        &self,
        pkg: &PackageId,
        version: &Version,
        artifacts: &[ArtifactId],
    ) -> Result<Option<Timestamp>, CoreError> {
        let Some(list) = self.releases.get(&pkg.name) else {
            return Err(CoreError::NotFound(pkg.name.clone()));
        };
        let Some(rr) = list.iter().find(|r| &r.version == version) else {
            return Err(CoreError::NotFound(format!("{}@{version}", pkg.name)));
        };
        if artifacts.is_empty() || rr.artifacts.is_empty() {
            return Ok(rr.published_at);
        }
        // Newest of the requested artifacts; None if any selected artifact's time is unknown.
        let selected = rr
            .artifacts
            .iter()
            .filter(|a| artifacts.contains(&a.id))
            .map(|a| a.published_at);
        Ok(newest_or_none(selected))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cooldown_core::EcosystemId;

    #[tokio::test]
    async fn artifact_granular_unknown_poisons_release() {
        let reg = FakeRegistry::new().with_artifact_release(
            "pkg",
            "1.0.0",
            &[("a.whl", Some("2026-06-01T00:00:00Z")), ("b.whl", None)],
        );
        let pkg = PackageId::new(EcosystemId("python"), "pkg", None);
        let got = reg
            .published_at(
                &pkg,
                &Version::new("1.0.0"),
                &[ArtifactId("a.whl".into()), ArtifactId("b.whl".into())],
            )
            .await
            .unwrap();
        assert_eq!(
            got, None,
            "any unknown artifact time poisons the release time"
        );
    }
}
