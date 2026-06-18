//! Config discovery: the repo root, the global user config, and the per-project repo cascade.
//!
//! The cascade is computed **per detected project**: every `cooldown.toml` from the repo root down
//! to that project's directory, merged so a nearer file wins (like `.editorconfig`). The walk
//! auto-stops at the repo root: a `.git` directory or file, else the nearest ancestor with a
//! `cooldown.toml`, else `$HOME`.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{ConfigDocument, ScanConfig};
use cooldown_core::{CoreError, Origin, PolicyLayer};

/// The repo-level config file name (`cooldown.toml`), used for both the repo cascade and repo-root
/// detection.
pub const CONFIG_FILE: &str = "cooldown.toml";

/// Resolve the repo root from `start`, walking up.
#[must_use]
pub fn find_repo_root(start: &Utf8Path) -> Utf8PathBuf {
    let mut nearest_with_config: Option<Utf8PathBuf> = None;
    let mut dir = Some(start.to_owned());
    while let Some(d) = dir {
        // A `.git` directory or file marks the worktree root.
        if d.join(".git").exists() {
            return d;
        }
        if nearest_with_config.is_none() && d.join(CONFIG_FILE).is_file() {
            nearest_with_config = Some(d.clone());
        }
        dir = d.parent().map(std::borrow::ToOwned::to_owned);
    }
    if let Some(c) = nearest_with_config {
        return c;
    }
    home_dir().unwrap_or_else(|| start.to_owned())
}

/// The global config path: `${XDG_CONFIG_HOME:-~/.config}/cooldown/config.toml`.
///
/// Returns `None` only when neither `XDG_CONFIG_HOME` nor `HOME` is set, so no base directory can
/// be derived.
#[must_use]
pub fn global_config_path() -> Option<Utf8PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Utf8PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".config")))?;
    Some(base.join("cooldown").join("config.toml"))
}

/// One config file parsed once and projected into policy/scan views as needed.
#[derive(Debug, Clone)]
pub struct LoadedConfigFile {
    path: Utf8PathBuf,
    document: ConfigDocument,
}

impl LoadedConfigFile {
    fn policy_layer(&self, origin: Origin) -> Result<PolicyLayer, CoreError> {
        self.document.policy_layer(origin)
    }

    fn scan_config(&self, origin: &Origin) -> Result<ScanConfig, CoreError> {
        self.document.scan_config(origin)
    }
}

/// The config sources discovered for one run, each parsed at most once before being projected into
/// policy or scan/runtime settings.
#[derive(Debug, Clone, Default)]
pub struct ConfigSources {
    global: Option<LoadedConfigFile>,
    repo_root: Option<LoadedConfigFile>,
    explicit: Option<LoadedConfigFile>,
}

impl ConfigSources {
    /// Load the config sources relevant to one run: global, repo-root, and explicit.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] if a present file cannot be read, or
    /// [`CoreError::Config`] if an explicit path is missing or any file contains invalid config.
    pub fn load(
        repo_root: &Utf8Path,
        explicit: Option<&Utf8Path>,
        no_global: bool,
    ) -> Result<Self, CoreError> {
        let global = if no_global {
            None
        } else {
            match global_config_path() {
                Some(path) => read_document(&path, &Origin::Global)?,
                None => None,
            }
        };
        let repo_root_doc = read_document(
            &repo_root.join(CONFIG_FILE),
            &Origin::Repo(repo_root.join(CONFIG_FILE)),
        )?;
        let explicit = match explicit {
            Some(path) => match read_document(path, &Origin::Config(path.to_owned()))? {
                Some(document) => Some(document),
                None => {
                    return Err(CoreError::Config(format!(
                        "--config file not found: {path}"
                    )));
                }
            },
            None => None,
        };
        Ok(ConfigSources {
            global,
            repo_root: repo_root_doc,
            explicit,
        })
    }

    /// The merged non-policy scan config (`[global]`/`[<command>]`/`[tool.*]` settings) that
    /// controls detection and runtime defaults.
    ///
    /// Lowest precedence first: global, repo-root, explicit.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if a config names an unknown tool under `[tool.*]`.
    pub fn scan_config(&self) -> Result<ScanConfig, CoreError> {
        let mut scan = ScanConfig::default();
        if let Some(global) = &self.global {
            scan = scan.merge(global.scan_config(&Origin::Global)?);
        }
        if let Some(repo_root) = &self.repo_root {
            let origin = Origin::Repo(repo_root.path.clone());
            scan = scan.merge(repo_root.scan_config(&origin)?);
        }
        if let Some(explicit) = &self.explicit {
            let origin = Origin::Config(explicit.path.clone());
            scan = scan.merge(explicit.scan_config(&origin)?);
        }
        Ok(scan)
    }

    /// The global policy layer, if a global config was loaded.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if projecting the document into the policy model fails.
    pub fn global_policy_layer(&self) -> Result<Option<PolicyLayer>, CoreError> {
        self.global
            .as_ref()
            .map(|config| config.policy_layer(Origin::Global))
            .transpose()
    }

    /// The explicit `--config` policy layer, if one was loaded.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if projecting the document into the policy model fails.
    pub fn explicit_policy_layer(&self) -> Result<Option<PolicyLayer>, CoreError> {
        self.explicit
            .as_ref()
            .map(|config| config.policy_layer(Origin::Config(config.path.clone())))
            .transpose()
    }

    /// The repo cascade for a project: layers from the repo root down to the project dir, lowest
    /// authority first (root) → highest (the project's own `cooldown.toml`).
    ///
    /// Directories without a `cooldown.toml` contribute no layer. Both `repo_root` and
    /// `project_dir` are expected to be absolute and to share a common root.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] if a discovered `cooldown.toml` cannot be read, or
    /// [`CoreError::Config`] if one does not parse as valid config.
    pub fn repo_cascade_layers(
        &self,
        repo_root: &Utf8Path,
        project_dir: &Utf8Path,
    ) -> Result<Vec<PolicyLayer>, CoreError> {
        let mut dirs: Vec<Utf8PathBuf> = Vec::new();
        let mut cur = Some(project_dir.to_owned());
        while let Some(d) = cur {
            dirs.push(d.clone());
            if d == repo_root {
                break;
            }
            match d.parent() {
                Some(p) if p.starts_with(repo_root) || p == repo_root => cur = Some(p.to_owned()),
                Some(p) if repo_root.starts_with(&d) => cur = Some(p.to_owned()),
                _ => break,
            }
        }
        if !dirs.iter().any(|d| d == repo_root) {
            dirs.push(repo_root.to_owned());
        }
        dirs.reverse();
        dirs.dedup();

        let mut layers = Vec::new();
        let repo_root_config = repo_root.join(CONFIG_FILE);
        for dir in dirs {
            let path = dir.join(CONFIG_FILE);
            let maybe_doc = if path == repo_root_config {
                self.repo_root.clone()
            } else {
                read_document(&path, &Origin::Repo(path.clone()))?
            };
            if let Some(config) = maybe_doc {
                layers.push(config.policy_layer(Origin::Repo(config.path.clone()))?);
            }
        }
        Ok(layers)
    }
}

fn read_document(path: &Utf8Path, origin: &Origin) -> Result<Option<LoadedConfigFile>, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(LoadedConfigFile {
            path: path.to_owned(),
            document: ConfigDocument::parse(&content, origin)?,
        })),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CoreError::Filesystem(format!("{path}: {e}"))),
    }
}

fn home_dir() -> Option<Utf8PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Utf8PathBuf::from)
}

/// The XDG cache dir for cooldown: `${XDG_CACHE_HOME:-~/.cache}/cooldown`.
///
/// Falls back to a relative `.cache/cooldown` when neither `XDG_CACHE_HOME` nor `HOME` is set.
#[must_use]
pub fn cache_dir() -> Utf8PathBuf {
    std::env::var("XDG_CACHE_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(Utf8PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".cache")))
        .unwrap_or_else(|| Utf8PathBuf::from(".cache"))
        .join("cooldown")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_root_stops_at_git() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let sub = root.join("a/b/c");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(find_repo_root(&sub), root.to_owned());
    }

    #[test]
    fn cascade_root_to_project_order() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(tmp.path()).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join(CONFIG_FILE), "min-age = \"14d\"").unwrap();
        let proj = root.join("services/api");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join(CONFIG_FILE), "min-age = \"21d\"").unwrap();

        let configs = ConfigSources::load(root, None, true).unwrap();
        let layers = configs.repo_cascade_layers(root, &proj).unwrap();
        assert_eq!(layers.len(), 2);
        // Root first (lower authority), project last (higher).
        assert_eq!(layers[0].origin, Origin::Repo(root.join(CONFIG_FILE)));
        assert_eq!(layers[1].origin, Origin::Repo(proj.join(CONFIG_FILE)));
    }

    #[test]
    fn explicit_config_missing_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let err = ConfigSources::load(root, Some(&root.join("missing.toml")), true)
            .expect_err("missing config");
        assert!(matches!(err, CoreError::Config(_)));
    }
}
