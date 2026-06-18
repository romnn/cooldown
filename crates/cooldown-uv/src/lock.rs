//! `uv.lock` parsing. Modern uv.lock (revision ≥ 3) records an `upload-time` per wheel/sdist, so
//! the lock itself usually supplies the publish instant — `PyPI` is only a fallback for older locks,
//! non-registry sources, and indexes that omit the field.

use cooldown_core::CoreError;
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
}

/// A single distribution file (a wheel or sdist) recorded under a [`Package`].
///
/// Only the publish instant is parsed; the URL and hash fields uv records are
/// ignored because the adapter only needs the upload time for the cooldown check.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct File {
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
    /// Newest upload time across this package's files, or `None` if any selected file lacks one
    /// (conservative — a partially-known release is never treated as mature).
    #[must_use]
    pub fn newest_upload_time(&self) -> Option<Timestamp> {
        let mut times: Vec<Option<Timestamp>> = Vec::new();
        if let Some(s) = &self.sdist {
            times.push(parse_time(s.upload_time.as_deref()));
        }
        for w in &self.wheels {
            times.push(parse_time(w.upload_time.as_deref()));
        }
        if times.is_empty() {
            return None;
        }
        let mut newest: Option<Timestamp> = None;
        for t in times {
            match t {
                None => return None,
                Some(t) => newest = Some(newest.map_or(t, |n| n.max(t))),
            }
        }
        newest
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
sdist = { url = "https://x/requests.tar.gz", upload-time = "2026-05-14T19:25:27.735Z" }
wheels = [{ url = "https://x/requests.whl", upload-time = "2026-05-14T19:25:26.443Z" }]

[[package]]
name = "idna"
version = "3.10"
source = { registry = "https://pypi.org/simple" }
wheels = [{ url = "https://x/idna.whl", upload-time = "2024-09-15T18:07:39.349Z" }]
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
