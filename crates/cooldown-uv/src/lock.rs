//! `uv.lock` parsing. Modern uv.lock (revision ≥ 3) records an `upload-time` per wheel/sdist, so
//! the lock itself usually supplies the publish instant — `PyPI` is only a fallback for older locks,
//! non-registry sources, and indexes that omit the field.

use crate::artifact::{artifact_id_from_url, newest_or_none, published_at_for_artifacts};
use cooldown_core::{ArtifactId, CoreError, RawArtifact};
use jiff::Timestamp;
use std::collections::HashMap;

/// The `[package.source]` table — where uv resolved a package from.
///
/// Each variant field is the URL or path for one source kind; at most one is set
/// for a given package. A registry source is an installable PyPI-style index;
/// the `virtual`/`editable` markers identify the workspace root project itself.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Source {
    /// The index URL when the package comes from a package registry (e.g. PyPI).
    #[serde(default)]
    pub registry: Option<String>,
    /// The marker for a `virtual` workspace member (the root project, not installed).
    #[serde(default, rename = "virtual")]
    pub r#virtual: Option<String>,
    /// The path for an `editable` install (the root project in editable mode).
    #[serde(default)]
    pub editable: Option<String>,
    /// The path for a local directory source.
    #[serde(default)]
    pub directory: Option<String>,
    /// The repository URL for a git source.
    #[serde(default)]
    pub git: Option<String>,
}

impl Source {
    /// Returns `true` if the package was resolved from a package registry.
    ///
    /// Only registry packages have a meaningful PyPI publish time, so this gates
    /// which dependencies the adapter considers for a cooldown verdict.
    #[must_use]
    pub fn is_registry(&self) -> bool {
        self.registry.is_some()
    }

    /// Returns `true` if this source marks the workspace root project.
    ///
    /// The root is either a `virtual` member or an `editable` install; it is
    /// never a cooldown candidate but its dependency list defines the direct set.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.r#virtual.is_some() || self.editable.is_some()
    }

    /// Returns `true` if this source is the *project's own* package — its `virtual`/`editable` path
    /// points at `.`. Distinguishes the project from local path dependencies, which are also
    /// `editable` but point at sibling directories (`../other`). Used to name the project package.
    #[must_use]
    pub fn is_project_root(&self) -> bool {
        let at_dot = |path: &Option<String>| {
            path.as_deref()
                .is_some_and(|value| value == "." || value.is_empty())
        };
        at_dot(&self.r#virtual) || at_dot(&self.editable)
    }
}

/// A single distribution file (a wheel or sdist) recorded under a [`Package`].
///
/// The URL is parsed to derive a stable artifact identity, and the publish instant is parsed for
/// cooldown evaluation. Other recorded file metadata remains ignored.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct File {
    /// The file URL recorded in the lock, used to derive a stable artifact identity.
    #[serde(default)]
    pub url: Option<String>,
    /// The `upload-time` field (RFC 3339), present in uv.lock revision ≥ 3.
    ///
    /// `None` for older locks or indexes that omit it; parsed lazily by
    /// [`Package::newest_upload_time`].
    #[serde(default, rename = "upload-time")]
    pub upload_time: Option<String>,
}

/// A single entry in a package's dependency list (a referenced package name).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Dep {
    /// The depended-on package name, PEP 503-normalised by uv.
    pub name: String,
}

/// One `[[package]]` entry in `uv.lock` — a resolved node in the dependency graph.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Package {
    /// The PEP 503-normalised package name.
    pub name: String,
    /// The resolved version; `None` for some non-registry sources (e.g. the root).
    #[serde(default)]
    pub version: Option<String>,
    /// Where uv resolved this package from; `None` is treated as non-registry.
    #[serde(default)]
    pub source: Option<Source>,
    /// The package's runtime dependency edges (`[[package.dependencies]]`).
    #[serde(default)]
    pub dependencies: Vec<Dep>,
    /// `[package.dev-dependencies]` — PEP 735 groups (e.g. `dev = [...]`), keyed by group name.
    #[serde(default, rename = "dev-dependencies")]
    pub dev_dependencies: std::collections::HashMap<String, Vec<Dep>>,
    /// `[package.optional-dependencies]` — extras, keyed by extra name.
    #[serde(default, rename = "optional-dependencies")]
    pub optional_dependencies: std::collections::HashMap<String, Vec<Dep>>,
    /// The source distribution, if the resolution includes one.
    #[serde(default)]
    pub sdist: Option<File>,
    /// The resolved wheel files (one per selected platform/Python tag).
    #[serde(default)]
    pub wheels: Vec<File>,
}

impl Package {
    /// Yields every declared direct dependency name: normal + dev-group + optional-extra.
    pub fn all_direct_dep_names(&self) -> impl Iterator<Item = &str> {
        self.dependencies
            .iter()
            .chain(self.dev_dependencies.values().flatten())
            .chain(self.optional_dependencies.values().flatten())
            .map(|d| d.name.as_str())
    }
}

impl Package {
    /// The locked artifact identities this package resolved to in the current environment.
    ///
    /// When uv records wheels, they represent the environment-specific install set, so they take
    /// precedence over the sdist fallback. Only when no wheels are present do we fall back to the
    /// sdist identity.
    #[must_use]
    pub fn artifact_ids(&self) -> Vec<ArtifactId> {
        let mut ids = Vec::new();
        for artifact in self.selected_raw_artifacts() {
            if !ids.contains(&artifact.id) {
                ids.push(artifact.id);
            }
        }
        ids
    }

    /// The locked artifacts recorded for this package, each with its upload time when known.
    #[must_use]
    pub fn raw_artifacts(&self) -> Vec<RawArtifact> {
        self.file_artifacts().collect()
    }

    /// Newest upload time across this package's files, or `None` if any selected file lacks one
    /// (conservative — a partially-known release is never treated as mature).
    #[must_use]
    pub fn newest_upload_time(&self) -> Option<Timestamp> {
        newest_or_none(self.file_artifacts().map(|artifact| artifact.published_at))
    }

    /// Newest upload time across the selected artifact identities, or across all files when
    /// `artifacts` is empty.
    #[must_use]
    pub fn published_at_for_artifacts(&self, artifacts: &[ArtifactId]) -> Option<Timestamp> {
        published_at_for_artifacts(&self.raw_artifacts(), artifacts)
    }

    fn selected_raw_artifacts(&self) -> impl Iterator<Item = RawArtifact> + '_ {
        self.selected_files().filter_map(File::raw_artifact)
    }

    fn selected_files(&self) -> Box<dyn Iterator<Item = &File> + '_> {
        if self.wheels.is_empty() {
            Box::new(self.sdist.iter())
        } else {
            Box::new(self.wheels.iter())
        }
    }

    fn file_artifacts(&self) -> impl Iterator<Item = RawArtifact> + '_ {
        self.sdist
            .iter()
            .chain(self.wheels.iter())
            .filter_map(File::raw_artifact)
    }
}

impl File {
    fn raw_artifact(&self) -> Option<RawArtifact> {
        let id = self.url.as_deref().and_then(artifact_id_from_url)?;
        Some(RawArtifact {
            id,
            published_at: parse_time(self.upload_time.as_deref()),
            markers: Vec::new(),
        })
    }
}

fn parse_time(s: Option<&str>) -> Option<Timestamp> {
    s.and_then(|x| x.parse::<Timestamp>().ok())
}

#[derive(Debug, serde::Deserialize)]
struct UvLockToml {
    #[serde(default)]
    package: Vec<Package>,
}

/// A parsed `uv.lock` file: the flat list of resolved packages.
///
/// uv stores the resolution as a flat array of [`Package`] nodes plus their
/// dependency edges; this type wraps that array and offers the queries the
/// adapter needs (direct set, graph floors, lookup by name+version).
pub struct UvLock {
    /// Every resolved `[[package]]` entry, including the root project.
    pub packages: Vec<Package>,
}

impl UvLock {
    /// Parses the TOML contents of a `uv.lock` file.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::LockUnreadable`] if `content` is not valid `uv.lock`
    /// TOML (malformed syntax or a shape that does not match the expected schema).
    pub fn parse(content: &str) -> Result<Self, CoreError> {
        let raw: UvLockToml = toml::from_str(content)
            .map_err(|e| CoreError::LockUnreadable(format!("uv.lock: {e}")))?;
        Ok(UvLock {
            packages: raw.package,
        })
    }

    /// The root project's direct dependency names — normal + dev groups + optional extras (PEP 503
    /// normalised by uv already).
    #[must_use]
    pub fn direct_names(&self) -> Vec<String> {
        self.packages
            .iter()
            .filter(|p| p.source.as_ref().is_some_and(Source::is_root))
            .flat_map(|p| {
                p.all_direct_dep_names()
                    .map(std::string::ToString::to_string)
            })
            .collect()
    }

    /// The MVS-like floor: a package required by a non-root package is held by the graph.
    #[must_use]
    pub fn graph_floors(&self) -> HashMap<String, String> {
        let mut floors = HashMap::new();
        for pkg in &self.packages {
            let is_root = pkg.source.as_ref().is_some_and(Source::is_root);
            if is_root {
                continue;
            }
            for dep in &pkg.dependencies {
                // The resolved version of the dep is the lock's single entry for that name.
                if let Some(resolved) = self
                    .packages
                    .iter()
                    .find(|p| p.name == dep.name)
                    .and_then(|p| p.version.clone())
                {
                    floors.insert(dep.name.clone(), resolved);
                }
            }
        }
        floors
    }

    /// Finds the package resolved at exactly `name` and `version`, if present.
    ///
    /// `uv.lock` holds a single entry per name, so a version mismatch yields `None`.
    #[must_use]
    pub fn find(&self, name: &str, version: &str) -> Option<&Package> {
        self.packages
            .iter()
            .find(|p| p.name == name && p.version.as_deref() == Some(version))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_project_root_distinguishes_self_from_path_deps() {
        // The project's own package points at `.`; a local path dependency is also `editable` but
        // points at a sibling directory — it must not be mistaken for the project package.
        let own = Source {
            registry: None,
            r#virtual: None,
            editable: Some(".".to_string()),
            directory: None,
            git: None,
        };
        let path_dep = Source {
            registry: None,
            r#virtual: None,
            editable: Some("../airtype-common".to_string()),
            directory: None,
            git: None,
        };
        assert!(own.is_project_root());
        assert!(!path_dep.is_project_root());
        // Both still count as "root-ish" for the direct-set computation.
        assert!(own.is_root() && path_dep.is_root());
    }

    const SAMPLE: &str = r#"
version = 1
revision = 3
requires-python = ">=3.12"

[[package]]
name = "demo"
version = "0.1.0"
source = { virtual = "." }
dependencies = [{ name = "requests" }]

[[package]]
name = "requests"
version = "2.34.2"
source = { registry = "https://pypi.org/simple" }
dependencies = [{ name = "idna" }]
sdist = { url = "https://x/requests-2.34.2.tar.gz", upload-time = "2026-05-14T19:25:27.735Z" }
wheels = [{ url = "https://x/requests-2.34.2-py3-none-any.whl", upload-time = "2026-05-14T19:25:26.443Z" }]

[[package]]
name = "idna"
version = "3.10"
source = { registry = "https://pypi.org/simple" }
wheels = [{ url = "https://x/idna-3.10-py3-none-any.whl", upload-time = "2024-09-15T18:07:39.349Z" }]
"#;

    #[test]
    fn parses_and_derives_direct() {
        let lock = UvLock::parse(SAMPLE).unwrap();
        assert_eq!(lock.packages.len(), 3);
        assert_eq!(lock.direct_names(), vec!["requests".to_string()]);
    }

    #[test]
    fn newest_upload_time_picks_latest() {
        let lock = UvLock::parse(SAMPLE).unwrap();
        let req = lock.find("requests", "2.34.2").unwrap();
        // Newest of the sdist (19:25:27) and wheel (19:25:26).
        assert_eq!(
            req.newest_upload_time().unwrap().to_string(),
            "2026-05-14T19:25:27.735Z"
        );
    }

    #[test]
    fn artifact_ids_normalize_wheels_and_sdist() {
        let lock = UvLock::parse(SAMPLE).unwrap();
        let req = lock.find("requests", "2.34.2").unwrap();
        let mut artifacts = req.artifact_ids();
        artifacts.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            artifacts
                .into_iter()
                .map(|artifact| artifact.0)
                .collect::<Vec<_>>(),
            vec!["wheel:py3-none-any".to_string()]
        );
    }

    #[test]
    fn artifact_ids_fall_back_to_sdist_when_no_wheels_are_recorded() {
        let lock = UvLock::parse(
            r#"
[[package]]
name = "demo"
version = "0.1.0"
source = { virtual = "." }
dependencies = [{ name = "requests" }]

[[package]]
name = "requests"
version = "2.34.2"
source = { registry = "https://pypi.org/simple" }
sdist = { url = "https://x/requests-2.34.2.tar.gz", upload-time = "2026-05-14T19:25:27.735Z" }
"#,
        )
        .unwrap();
        let req = lock.find("requests", "2.34.2").unwrap();
        assert_eq!(
            req.artifact_ids()
                .into_iter()
                .map(|artifact| artifact.0)
                .collect::<Vec<_>>(),
            vec!["sdist".to_string()]
        );
    }

    #[test]
    fn direct_includes_dev_and_optional_groups() {
        let lock = UvLock::parse(
            r#"
[[package]]
name = "demo"
version = "0.1.0"
source = { virtual = "." }
dependencies = [{ name = "requests" }]

[package.dev-dependencies]
dev = [{ name = "pytest" }, { name = "ruff" }]

[package.optional-dependencies]
http2 = [{ name = "httpx" }]
"#,
        )
        .unwrap();
        let mut direct = lock.direct_names();
        direct.sort();
        assert_eq!(direct, vec!["httpx", "pytest", "requests", "ruff"]);
    }

    #[test]
    fn graph_floor_marks_transitive() {
        let lock = UvLock::parse(SAMPLE).unwrap();
        let floors = lock.graph_floors();
        // idna is required by requests (non-root) → held at 3.10.
        assert_eq!(floors.get("idna").map(String::as_str), Some("3.10"));
        // requests is required only by the root → not graph-held.
        assert!(!floors.contains_key("requests"));
    }
}
