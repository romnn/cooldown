//! Config discovery: the repo root, the global user config, and the per-project repo cascade.
//!
//! The cascade is computed **per detected project**: every `cooldown.toml` from the repo root down
//! to that project's directory, merged so a nearer file wins (like `.editorconfig`). The walk
//! auto-stops at the repo root: a `.git` directory or file, else the nearest ancestor with a
//! `cooldown.toml`, else `$HOME`.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{ScanConfig, parse_config, parse_scan_config};
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

/// Load the global config as a layer, if it exists.
///
/// Returns `Ok(None)` when no global config path can be derived or the file is absent.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] if the file exists but cannot be read, or
/// [`CoreError::Config`] if its contents do not parse as a valid config.
pub fn global_layer() -> Result<Option<PolicyLayer>, CoreError> {
    let Some(path) = global_config_path() else {
        return Ok(None);
    };
    read_layer(&path, Origin::Global)
}

/// Load an explicit `--config` / `COOLDOWN_CONFIG` file as a shared top file layer.
///
/// Unlike [`global_layer`], an explicit path that names a missing file is an error rather than a
/// silently-skipped layer: the user asked for this file by name.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if `path` does not exist, [`CoreError::Filesystem`] if it exists
/// but cannot be read, or [`CoreError::Config`] if its contents do not parse as a valid config.
pub fn explicit_config_layer(path: &Utf8Path) -> Result<PolicyLayer, CoreError> {
    match read_layer(path, Origin::Config(path.to_owned()))? {
        Some(layer) => Ok(layer),
        None => Err(CoreError::Config(format!(
            "--config file not found: {path}"
        ))),
    }
}

/// The repo cascade for a project: layers from the repo root down to the project dir, lowest
/// authority first (root) → highest (the project's own `cooldown.toml`).
///
/// Directories without a `cooldown.toml` contribute no layer. Both `repo_root` and `project_dir`
/// are expected to be absolute and to share a common root.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] if a discovered `cooldown.toml` cannot be read, or
/// [`CoreError::Config`] if one does not parse as a valid config.
pub fn repo_cascade_layers(
    repo_root: &Utf8Path,
    project_dir: &Utf8Path,
) -> Result<Vec<PolicyLayer>, CoreError> {
    let mut dirs: Vec<Utf8PathBuf> = Vec::new();
    // Collect project_dir and its ancestors up to (and including) repo_root.
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
    // Ensure repo_root is included even if the chain broke early.
    if !dirs.iter().any(|d| d == repo_root) {
        dirs.push(repo_root.to_owned());
    }
    dirs.reverse(); // root first → project last
    dirs.dedup();

    let mut layers = Vec::new();
    for d in dirs {
        let path = d.join(CONFIG_FILE);
        if let Some(layer) = read_layer(&path, Origin::Repo(path.clone()))? {
            layers.push(layer);
        }
    }
    Ok(layers)
}

/// Load the merged non-policy scan config (`[global]`/`[<command>]`/`[tool.*]` settings) that
/// controls detection: gitignore honoring, the exclude list, and per-command flag defaults.
///
/// Lowest precedence first: the global user config (unless `no_global`), then the repo-root
/// `cooldown.toml`, then an explicit `--config` file (which, like the policy layer, must exist if
/// named). Exclude lists concatenate; scalar overrides take the highest-precedence value.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`] if a present file cannot be read, [`CoreError::Config`] if one
/// does not parse or names an unknown tool under `[tool.*]`, or if an explicit `--config` file
/// is missing.
pub fn scan_config(
    repo_root: &Utf8Path,
    explicit: Option<&Utf8Path>,
    no_global: bool,
) -> Result<ScanConfig, CoreError> {
    let mut scan = ScanConfig::default();
    if !no_global
        && let Some(path) = global_config_path()
        && let Some(layer) = read_scan(&path, &Origin::Global)?
    {
        scan = scan.merge(layer);
    }
    let repo = repo_root.join(CONFIG_FILE);
    if let Some(layer) = read_scan(&repo, &Origin::Repo(repo.clone()))? {
        scan = scan.merge(layer);
    }
    if let Some(path) = explicit {
        match read_scan(path, &Origin::Config(path.to_owned()))? {
            Some(layer) => scan = scan.merge(layer),
            None => {
                return Err(CoreError::Config(format!(
                    "--config file not found: {path}"
                )));
            }
        }
    }
    Ok(scan)
}

fn read_scan(path: &Utf8Path, origin: &Origin) -> Result<Option<ScanConfig>, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(parse_scan_config(&content, origin)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(CoreError::Filesystem(format!("{path}: {e}"))),
    }
}

fn read_layer(path: &Utf8Path, origin: Origin) -> Result<Option<PolicyLayer>, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(parse_config(&content, origin)?)),
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

        let layers = repo_cascade_layers(root, &proj).unwrap();
        assert_eq!(layers.len(), 2);
        // Root first (lower authority), project last (higher).
        assert_eq!(layers[0].origin, Origin::Repo(root.join(CONFIG_FILE)));
        assert_eq!(layers[1].origin, Origin::Repo(proj.join(CONFIG_FILE)));
    }

    #[test]
    fn explicit_config_missing_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let err = explicit_config_layer(&root.join("missing.toml")).expect_err("missing config");
        assert!(matches!(err, CoreError::Config(_)));
    }
}
