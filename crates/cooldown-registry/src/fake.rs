//! A `PackageRegistry` fake for driving adapters in unit/conformance tests without network.

use async_trait::async_trait;
use cooldown_core::{
    ArtifactId, CoreError, PackageId, PackageRegistry, RawArtifact, RawRelease, Version,
};
use jiff::Timestamp;
use std::collections::HashMap;

/// An in-memory [`PackageRegistry`] for tests, built with the chained `with_*` helpers.
///
/// It serves whatever releases the test registers, with no network access, so adapter and
/// conformance tests can exercise classification, the cooldown floor, and artifact-granular
/// publish-time logic deterministically.
///
/// # Examples
///
/// ```
/// use cooldown_registry::FakeRegistry;
///
/// let registry = FakeRegistry::new()
///     .with_release("serde", "1.0.0", Some("2026-01-01T00:00:00Z"), false)
///     .with_release("serde", "1.0.1", None, true);
/// ```
#[derive(Default, Clone)]
pub struct FakeRegistry {
    releases: HashMap<String, Vec<RawRelease>>,
}

impl FakeRegistry {
    /// Creates an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a version-granular release (no per-file artifacts) for `pkg`.
    ///
    /// `published_at` is an RFC 3339 instant, or `None` for an unknown time. An unparsable
    /// string is also treated as `None`, matching the conservative "unknown time" handling used
    /// throughout the registry.
    #[must_use]
    pub fn with_release(
        mut self,
        pkg: &str,
        version: &str,
        published_at: Option<&str>,
        yanked: bool,
    ) -> Self {
        let rr = RawRelease {
            version: Version::new(version),
            published_at: published_at.and_then(parse),
            yanked,
            artifacts: Vec::new(),
        };
        self.releases.entry(pkg.to_string()).or_default().push(rr);
        self
    }

    /// Registers an artifact-granular release for `pkg` (e.g. a `PyPI` release with several files).
    ///
    /// Each `artifacts` entry pairs an artifact id with its RFC 3339 upload time (`None`, or an
    /// unparsable string, for an unknown time). The version-level publish instant is derived as
    /// the newest known artifact time, or `None` if any artifact time is unknown.
    #[must_use]
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
                published_at: t.and_then(parse),
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

/// Parses an RFC 3339 instant, returning `None` for an unparsable string.
///
/// An unparsable fixture collapses to the conservative "unknown time" (`None`) rather than
/// panicking, keeping the test builders total and free of fallible-unwrap conversions.
fn parse(s: &str) -> Option<Timestamp> {
    s.parse().ok()
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
