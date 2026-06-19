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

/// The slice of the npm package document we consume: the `time` map of version → ISO-8601 publish
/// instant. Its keys are every published version plus two non-version meta-keys (`created` /
/// `modified`), which we skip; the heavy per-version `versions` blob is left unparsed.
#[derive(serde::Deserialize)]
struct Doc {
    #[serde(default)]
    time: HashMap<String, String>,
}

/// The non-version meta-keys npm includes in the `time` map alongside the per-version timestamps.
const TIME_META_KEYS: [&str; 2] = ["created", "modified"];

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
        let Some(Doc { time }) = self.get_doc(&package.name).await? else {
            return Err(CoreError::NotFound(package.name.clone()));
        };
        Ok(time
            .into_iter()
            .filter(|(vers, _)| !TIME_META_KEYS.contains(&vers.as_str()))
            .map(|(vers, when)| {
                let published_at = self.guard(&package.name, &vers, when.parse::<Timestamp>().ok());
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
        let Some(Doc { time }) = self.get_doc(&pkg.name).await? else {
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
