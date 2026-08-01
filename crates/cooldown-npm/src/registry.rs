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
    revalidate_listings: bool,
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
    /// The registry's mutable tag → version pointers. Only `latest` is consumed: it is what a bare
    /// `npm install <pkg>` resolves to under npm's default `tag` configuration, i.e. the
    /// maintainer's own "this is current" declaration. (npm can still land below the tag — a
    /// reconfigured `tag`, or `--engine-strict` picking an engine-compatible older version — so
    /// the tag is a ceiling on adoption here, never a resolution promise.)
    #[serde(rename = "dist-tags", default)]
    dist_tags: HashMap<String, String>,
}

/// The slice of one npm package document the adapters consume: the installable releases plus the
/// version the mutable `latest` dist-tag names.
///
/// `latest_tag` is `None` when the registry serves no `latest` tag (a private mirror that strips
/// tags); the adapter then applies no dist-tag ceiling. The tag names a version that should be in
/// `releases`, but that is not guaranteed here — consumers must fail open when it is absent.
pub struct NpmPackument {
    /// The installable releases (see [`NpmRegistry`] for the `versions`-vs-`time` rule).
    pub releases: Vec<RawRelease>,
    /// The version string the `latest` dist-tag names, if the registry serves one.
    pub latest_tag: Option<String>,
}

/// One installable release of a package doc.
struct InstallableRelease<'a> {
    /// The version string — a key of the document's `versions` map.
    version: &'a str,
    /// The publish instant from the document's `time` map, when present and parseable.
    published_at: Option<Timestamp>,
}

/// The INSTALLABLE releases of a package doc: each `versions` key paired with its publish instant from
/// `time`. A version present only in `time` — an unpublished version npm never pruned from the `time`
/// map — is excluded, so cooldown never proposes a version the package manager cannot fetch.
fn installable_releases(doc: &Doc) -> impl Iterator<Item = InstallableRelease<'_>> {
    doc.versions.keys().map(|vers| {
        let when = doc
            .time
            .get(vers)
            .and_then(|when| when.parse::<Timestamp>().ok());
        InstallableRelease {
            version: vers,
            published_at: when,
        }
    })
}

impl NpmRegistry {
    /// Creates a client against the public npm registry (`https://registry.npmjs.org`).
    #[must_use]
    pub fn new(http: SharedHttp) -> Self {
        NpmRegistry {
            http,
            base: DEFAULT_BASE.to_string(),
            revalidate_listings: false,
        }
    }

    /// Makes every package-document fetch revalidate its cached copy against the registry (a
    /// conditional request the registry answers `304` when nothing changed) instead of trusting
    /// the listing TTL. Version-adopting runs that honor the `latest` dist-tag ceiling need this:
    /// the tag is mutable, and a cached copy up to an hour stale could otherwise authorize
    /// adopting a release the maintainer has since retagged below (`latest: 17 → 16`). Read-only
    /// commands keep the TTL — bounded staleness is acceptable where nothing is adopted. On a
    /// network failure the stale cached copy still serves (an outage must not hard-fail every
    /// run); the freshness guarantee applies to reachable registries.
    #[must_use]
    pub fn with_listing_revalidation(mut self, revalidate: bool) -> Self {
        self.revalidate_listings = revalidate;
        self
    }

    /// The cache TTL for package-document fetches: zero (always revalidate) when
    /// [`Self::with_listing_revalidation`] enabled it, the shared listing TTL otherwise.
    fn listing_ttl(&self) -> std::time::Duration {
        if self.revalidate_listings {
            std::time::Duration::ZERO
        } else {
            ttl::LISTING
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
        let resp = self.http.get(&url, self.listing_ttl()).await?;
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

impl NpmRegistry {
    /// Fetches the package document and returns both the installable releases and the version the
    /// `latest` dist-tag names — one HTTP round-trip for the two package-level facts adapters need.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::NotFound`] for an unknown package, or the underlying transport/parse
    /// error.
    pub async fn packument(&self, package: &PackageId) -> Result<NpmPackument, CoreError> {
        let Some(doc) = self.get_doc(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        let releases = installable_releases(&doc)
            .map(|release| {
                let published_at = self.guard(&package.name, release.version, release.published_at);
                RawRelease {
                    version: Version::new(release.version.to_string()),
                    published_at,
                    yanked: false,
                    artifacts: Vec::new(),
                }
            })
            .collect();
        Ok(NpmPackument {
            releases,
            latest_tag: doc.dist_tags.get("latest").cloned(),
        })
    }
}

#[async_trait]
impl PackageRegistry for NpmRegistry {
    async fn releases(&self, package: &PackageId) -> Result<Vec<RawRelease>, CoreError> {
        Ok(self.packument(package).await?.releases)
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
            .map(|release| release.version)
            .collect();
        versions.sort_unstable();
        // Only the two versions still in `versions`; the unpublished 1.4.1/1.4.2 (time-only) are gone.
        assert_eq!(versions, vec!["1.3.0", "1.4.0"]);

        // The kept versions carry their publish instant from `time`.
        let with_time = super::installable_releases(&doc)
            .filter(|release| release.published_at.is_some())
            .count();
        assert_eq!(with_time, 2);
    }

    /// The `latest` dist-tag is parsed from the same document (the fumadocs-core shape: `latest`
    /// deliberately below the semver-max `17.0.0`); a document without `dist-tags` parses too and
    /// simply yields no tag, so the adapter applies no ceiling.
    #[test]
    fn dist_tag_latest_is_parsed_and_optional() {
        let doc: Doc = serde_json::from_str(indoc::indoc! {r#"{
            "dist-tags": { "latest": "16.13.0", "next": "17.0.0" },
            "versions": {
                "16.13.0": { "name": "fumadocs-core", "version": "16.13.0" },
                "17.0.0": { "name": "fumadocs-core", "version": "17.0.0" }
            },
            "time": {}
        }"#})
        .expect("parse doc");
        assert_eq!(
            doc.dist_tags.get("latest").map(String::as_str),
            Some("16.13.0")
        );

        let bare: Doc = serde_json::from_str(r#"{ "versions": {}, "time": {} }"#).expect("parse");
        assert!(!bare.dist_tags.contains_key("latest"));
    }

    /// A minimal one-thread HTTP server whose packument's `latest` tag flips after the first
    /// request — the maintainer's downward retag, reproduced deterministically.
    fn serve_retagging_packument() -> String {
        use std::io::{Read as _, Write as _};

        fn doc(latest: &str) -> String {
            format!(
                r#"{{"dist-tags":{{"latest":"{latest}"}},"versions":{{"16.0.0":{{}},"17.0.0":{{}}}},"time":{{"16.0.0":"2025-01-01T00:00:00.000Z","17.0.0":"2025-02-01T00:00:00.000Z"}}}}"#
            )
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let base = format!("http://{}", listener.local_addr().expect("server addr"));
        std::thread::spawn(move || {
            for (hits, stream) in listener.incoming().enumerate() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let body = if hits == 0 {
                    doc("17.0.0")
                } else {
                    doc("16.0.0")
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
            }
        });
        base
    }

    /// The mutable-tag staleness trap, deterministically: `latest: 17 → 16` while the cached copy
    /// is still within the listing TTL. The read-only default keeps serving the cached tag
    /// (bounded staleness); a version-adopting run with listing revalidation must see the
    /// registry's *current* tag, so the demoted 17 can no longer be authorized by a stale cache.
    #[tokio::test]
    async fn listing_revalidation_sees_a_downward_retag_within_the_ttl() {
        use super::NpmRegistry;
        use cooldown_core::PackageId;
        use cooldown_registry::SharedHttp;

        let base = serve_retagging_packument();
        let dir = tempfile::tempdir().expect("tempdir");
        let http = SharedHttp::new(dir.path(), cooldown_registry::HttpOptions::default())
            .expect("shared http");
        let package = PackageId::new(cooldown_core::ToolId("npm"), "left-pad".to_string(), None);

        let cached = NpmRegistry {
            http: http.clone(),
            base: base.clone(),
            revalidate_listings: false,
        };
        let first = cached.packument(&package).await.expect("first fetch");
        assert_eq!(first.latest_tag.as_deref(), Some("17.0.0"));

        // Within the TTL the cached copy still answers — the registry is not consulted, so the
        // downward retag is invisible to the read-only default.
        let second = cached.packument(&package).await.expect("cached fetch");
        assert_eq!(second.latest_tag.as_deref(), Some("17.0.0"));

        // The version-adopting configuration revalidates and sees the retag.
        let revalidating = cached.clone().with_listing_revalidation(true);
        let live = revalidating.packument(&package).await.expect("revalidated");
        assert_eq!(live.latest_tag.as_deref(), Some("16.0.0"));
    }
}
