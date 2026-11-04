//! Config discovery: the repo root, the global user config, and the per-project repo cascade.
//!
//! The cascade is computed **per detected project**: every `cooldown.toml` from the repo root down
//! to that project's directory, merged so a nearer file wins (like `.editorconfig`). The walk
//! auto-stops at the repo root: a `.git` directory or file, else the nearest ancestor with a
//! `cooldown.toml`, else `$HOME`.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{ConfigDocument, ScanConfig};
use cooldown_core::{CoreError, EdgePolicy, Origin, PolicyLayer};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// The repo-level config file name (`cooldown.toml`), used for both the repo cascade and repo-root
/// detection.
pub const CONFIG_FILE: &str = "cooldown.toml";

/// Resolve the repo root from `start`, walking up.
///
/// The `$HOME` fallback is canonicalized when it resolves, so a canonical `start` and the root
/// it falls back to are spelled alike.
#[must_use]
pub fn find_repo_root(start: &Utf8Path) -> Utf8PathBuf {
    repo_root_from(start, home_dir())
}

fn repo_root_from(start: &Utf8Path, home: Option<Utf8PathBuf>) -> Utf8PathBuf {
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
    // The walk climbs a canonical workdir, so its markers are canonical; `HOME` is spelled however
    // the environment spells it, through a symlink or, on Windows, without the verbatim prefix.
    // Every consumer compares the root against canonical paths, so the fallback must match them.
    home.map_or_else(
        || start.to_owned(),
        |home| canonical_dir(&home).unwrap_or(home),
    )
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
    /// The layer the file forms, which names the file the way reports spell it.
    origin: Origin,
    document: ConfigDocument,
}

impl LoadedConfigFile {
    fn policy_layer(&self) -> Result<PolicyLayer, CoreError> {
        self.document.policy_layer(self.origin.clone())
    }

    fn scan_config(&self) -> Result<ScanConfig, CoreError> {
        self.document.scan_config(&self.origin)
    }
}

/// Spells the config files of one run the way their origin tokens name them.
///
/// A file inside the repository is named by its `/`-separated path relative to the repository
/// root, so `config`, `explain` and every report's `source` read the same on every machine and
/// platform.
/// A file outside it, such as a `--config` file elsewhere, keeps its absolute path: no relative
/// spelling exists for it, and the user needs the location to find the file.
#[derive(Debug, Clone)]
struct OriginSpelling {
    /// The canonical repository root, so a root reached through a symlink or a non-canonical
    /// `$HOME` fallback still contains the files below it.
    root: Utf8PathBuf,
}

impl OriginSpelling {
    fn repo(&self, path: &Utf8Path) -> Origin {
        Origin::Repo(self.spell(path))
    }

    fn explicit(&self, path: &Utf8Path) -> Origin {
        Origin::Config(self.spell(path))
    }

    fn spell(&self, path: &Utf8Path) -> Utf8PathBuf {
        let located = located(path);
        crate::report_path::relative_report_path(&located, &self.root).unwrap_or_else(|| {
            // A Windows canonical path is verbatim (`\\?\C:\…`); the plain form reads as the
            // user would type it, and `dunce` keeps the verbatim form where dropping it would
            // change the meaning.
            Utf8Path::from_path(dunce::simplified(located.as_std_path()))
                .map(Utf8Path::to_path_buf)
                .unwrap_or(located)
        })
    }
}

/// `path` with its directory canonicalized and its file name kept.
///
/// The path is first made absolute against the working directory, exactly as reading the file
/// resolves it, which on Windows also applies the normalization the read applies, such as dropping
/// a trailing `.` from the file name.
/// Its directory is then canonicalized so the path compares against the canonical repository root.
/// The file name is kept rather than canonicalizing the file itself, so a symlinked config is named
/// where the link sits, which is the file the user put there or passed.
/// The path is returned unchanged when it cannot be resolved.
fn located(path: &Utf8Path) -> Utf8PathBuf {
    let Some(absolute) = std::path::absolute(path)
        .ok()
        .and_then(|absolute| Utf8PathBuf::from_path_buf(absolute).ok())
    else {
        return path.to_owned();
    };
    let (Some(dir), Some(name)) = (absolute.parent(), absolute.file_name()) else {
        return path.to_owned();
    };
    canonical_dir(dir).map_or_else(|| path.to_owned(), |dir| dir.join(name))
}

fn canonical_dir(dir: &Utf8Path) -> Option<Utf8PathBuf> {
    let canonical = std::fs::canonicalize(dir).ok()?;
    Utf8PathBuf::from_path_buf(canonical).ok()
}

/// The config sources discovered for one run, each parsed at most once before being projected into
/// policy or scan/runtime settings.
#[derive(Debug, Clone)]
pub struct ConfigSources {
    /// The repository root the cascade walks down from, canonical when it resolves, so the walk
    /// recognizes it whichever spelling it and the project directory arrive in.
    root: Utf8PathBuf,
    spelling: OriginSpelling,
    global: Option<LoadedConfigFile>,
    repo_root: Option<LoadedConfigFile>,
    explicit: Option<LoadedConfigFile>,
    nested: Arc<Mutex<HashMap<Utf8PathBuf, Option<LoadedConfigFile>>>>,
}

/// The per-project projections of its repository config cascade.
#[derive(Debug)]
pub struct ProjectConfig {
    /// Policy layers from the repository root through the project directory.
    pub policy_layers: Vec<PolicyLayer>,
    /// The resolved Cargo edge policy from the config documents applicable to this project.
    pub cargo_edge_policy: Option<EdgePolicy>,
    /// The `[tool.pnpm] single-copy` names folded across the config documents applicable to this
    /// project, farthest first, with the exclude lists' merge modes: a nested file's plain array
    /// adds to the root's, so listing one runtime of its own never un-gates the root's, and `[]`
    /// or `{ replace = [...] }` are the explicit ways to drop inherited names.
    pub pnpm_single_copy: Vec<String>,
    /// The `[tool.cargo] generated-members` declaration from the nearest config document that
    /// sets it, like the edge policy: a declaration names *this* workspace's members, so a nearer
    /// file's list — even an empty one — replaces a farther file's rather than adding to it.
    pub cargo_generated_members: Option<DeclaredGeneratedMembers>,
}

/// A `[tool.cargo] generated-members` declaration and the config file it came from, so `config`
/// can show which file narrowed what cooldown proposes and reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredGeneratedMembers {
    /// The declared member package names; empty is a deliberate "none".
    pub names: Vec<String>,
    /// The config layer that set the declaration.
    pub origin: Origin,
}

impl DeclaredGeneratedMembers {
    /// The declaration as the project carries it.
    #[must_use]
    pub fn generated_members(&self) -> cooldown_core::GeneratedMembers {
        cooldown_core::GeneratedMembers::declared(self.names.clone())
    }
}

/// The declaration `document` makes at `origin`, if any.
fn declared_generated_members(
    document: &ConfigDocument,
    origin: Origin,
) -> Option<DeclaredGeneratedMembers> {
    document
        .cargo_generated_members()
        .map(|names| DeclaredGeneratedMembers { names, origin })
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
                Some(path) => read_document(&path, Origin::Global)?,
                None => None,
            }
        };
        let root = canonical_dir(repo_root).unwrap_or_else(|| repo_root.to_owned());
        let spelling = OriginSpelling { root: root.clone() };
        let repo_root_config = repo_root.join(CONFIG_FILE);
        let repo_root_doc = read_document(&repo_root_config, spelling.repo(&repo_root_config))?;
        let explicit = match explicit {
            Some(path) => match read_document(path, spelling.explicit(path))? {
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
            root,
            spelling,
            global,
            repo_root: repo_root_doc,
            explicit,
            nested: Arc::default(),
        })
    }

    /// The merged non-policy scan config (`[global]`/`[<command>]`/`[tool.*]` settings) that
    /// controls detection and run-wide defaults.
    ///
    /// Lowest precedence first: global, repo-root, explicit.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if a config names an unknown tool under `[tool.*]`.
    pub fn scan_config(&self) -> Result<ScanConfig, CoreError> {
        let mut scan = ScanConfig::default();
        for config in [&self.global, &self.repo_root, &self.explicit]
            .into_iter()
            .flatten()
        {
            scan = scan.merge(config.scan_config()?);
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
            .map(LoadedConfigFile::policy_layer)
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
            .map(LoadedConfigFile::policy_layer)
            .transpose()
    }

    /// The config projection for a project: policy layers from the repo root down to the project
    /// directory, plus the Cargo runtime setting resolved across global, that repository cascade,
    /// and explicit config.
    ///
    /// Directories without a `cooldown.toml` contribute no layer.
    /// The walk compares the canonical forms of the root and `project_dir`, so a symlinked
    /// checkout, a `$HOME` fallback or a Windows verbatim or 8.3 spelling collects every layer
    /// between them exactly once and reads nothing above the root.
    /// A project outside the root layers only its own file over the root's, and one whose
    /// directory cannot be resolved gets the root layer alone.
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Filesystem`] if a discovered `cooldown.toml` cannot be read, or
    /// [`CoreError::Config`] if one does not parse as valid config.
    pub fn project_config(&self, project_dir: &Utf8Path) -> Result<ProjectConfig, CoreError> {
        let root = self.root.as_path();
        let dirs: Vec<Utf8PathBuf> = match canonical_dir(project_dir) {
            Some(project) if project.starts_with(root) => {
                let mut dirs: Vec<Utf8PathBuf> = project
                    .ancestors()
                    .take_while(|dir| dir.starts_with(root))
                    .map(Utf8Path::to_path_buf)
                    .collect();
                dirs.reverse();
                dirs
            }
            // A project outside the root, such as one beside a `$HOME` fallback, has no ancestry
            // inside the repository to walk.
            Some(project) => vec![root.to_owned(), project],
            // A directory that does not resolve holds no config file, and its raw spelling, such
            // as one through `..`, could climb above the root.
            None => vec![root.to_owned()],
        };

        let mut policy_layers = Vec::new();
        let mut cargo_edge_policy = self
            .global
            .as_ref()
            .and_then(|config| config.document.cargo_edge_policy());
        let mut pnpm_single_copy = self
            .global
            .as_ref()
            .and_then(|config| config.document.pnpm_single_copy())
            .unwrap_or_default();
        let mut cargo_generated_members = self
            .global
            .as_ref()
            .and_then(|config| declared_generated_members(&config.document, config.origin.clone()));
        for dir in dirs {
            // The root file was read when the sources were loaded, through whichever spelling the
            // root arrived in.
            let maybe_doc = if dir == root {
                self.repo_root.clone()
            } else {
                self.nested_document(&dir.join(CONFIG_FILE))?
            };
            if let Some(config) = maybe_doc {
                cargo_edge_policy = config.document.cargo_edge_policy().or(cargo_edge_policy);
                cargo_generated_members =
                    declared_generated_members(&config.document, config.origin.clone())
                        .or(cargo_generated_members);
                if let Some(layer) = config.document.pnpm_single_copy() {
                    pnpm_single_copy = pnpm_single_copy.merge(layer);
                }
                policy_layers.push(config.policy_layer()?);
            }
        }
        cargo_edge_policy = self
            .explicit
            .as_ref()
            .and_then(|config| config.document.cargo_edge_policy())
            .or(cargo_edge_policy);
        cargo_generated_members = self
            .explicit
            .as_ref()
            .and_then(|config| declared_generated_members(&config.document, config.origin.clone()))
            .or(cargo_generated_members);
        if let Some(layer) = self
            .explicit
            .as_ref()
            .and_then(|config| config.document.pnpm_single_copy())
        {
            pnpm_single_copy = pnpm_single_copy.merge(layer);
        }
        Ok(ProjectConfig {
            policy_layers,
            cargo_edge_policy,
            pnpm_single_copy: pnpm_single_copy.patterns().to_vec(),
            cargo_generated_members,
        })
    }

    fn nested_document(&self, path: &Utf8Path) -> Result<Option<LoadedConfigFile>, CoreError> {
        let mut nested = self.nested.lock().map_err(|_| {
            CoreError::Filesystem("config document cache lock was poisoned".to_string())
        })?;
        if let Some(document) = nested.get(path) {
            return Ok(document.clone());
        }
        let origin = self.spelling.repo(path);
        let document = read_document(path, origin.clone())?;
        // Only the repo-root file (and the global and `--config` files) feed the scan config, so
        // an exclude or `include-hidden` list here would be validated and then ignored; refuse it
        // rather than let a nested file look like the place to override an inherited list.
        if let Some(loaded) = &document
            && loaded.scan_config()?.sets_scan_lists()
        {
            return Err(CoreError::Config(format!(
                "{}: `exclude-folders`/`exclude-packages`/`include-hidden` apply only from the \
                 repository root `{CONFIG_FILE}`, the global config, or a `--config` file; a \
                 nested config contributes policy only",
                origin.token()
            )));
        }
        nested.insert(path.to_owned(), document.clone());
        Ok(document)
    }
}

fn read_document(path: &Utf8Path, origin: Origin) -> Result<Option<LoadedConfigFile>, CoreError> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(LoadedConfigFile {
            document: ConfigDocument::parse(&content, &origin)?,
            origin,
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
/// Without `HOME`, as in a Windows shell, `~` is the platform's home directory, the user profile
/// on Windows, and without any home the cache lives in the system temp directory.
/// A relative `XDG_CACHE_HOME` or home is ignored, as the XDG spec has it, so the cache never
/// resolves against the working directory, which is usually the project the run reads.
#[must_use]
pub fn cache_dir() -> PathBuf {
    cache_dir_from(CacheBases {
        xdg_cache_home: std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from),
        home: std::env::var_os("HOME").map(PathBuf::from),
        platform_home: std::env::home_dir(),
        temp: std::env::temp_dir(),
    })
}

/// Where the cache directory may live, in order of preference.
struct CacheBases {
    xdg_cache_home: Option<PathBuf>,
    home: Option<PathBuf>,
    platform_home: Option<PathBuf>,
    temp: PathBuf,
}

/// The cache directory [`cache_dir`] picks among `bases`.
fn cache_dir_from(bases: CacheBases) -> PathBuf {
    let absolute = |path: Option<PathBuf>| path.filter(|path| path.is_absolute());
    let CacheBases {
        xdg_cache_home,
        home,
        platform_home,
        temp,
    } = bases;
    absolute(xdg_cache_home)
        .or_else(|| {
            absolute(home)
                .or_else(|| absolute(platform_home))
                .map(|home| home.join(".cache"))
        })
        .unwrap_or(temp)
        .join("cooldown")
}

#[cfg(test)]
mod tests {
    use super::*;
    use indoc::indoc;

    /// The cache follows `XDG_CACHE_HOME`, then `HOME`, then the platform's home, then the temp
    /// directory, and never a relative path, which would resolve inside the project being run on.
    #[test]
    fn the_cache_dir_is_never_relative() {
        let temp = std::env::temp_dir();
        let home = temp.join("home");
        let profile = temp.join("profile");
        let bases = |xdg: Option<&str>,
                     home: Option<&std::path::Path>,
                     profile: Option<&std::path::Path>| CacheBases {
            xdg_cache_home: xdg.map(PathBuf::from),
            home: home.map(std::path::Path::to_path_buf),
            platform_home: profile.map(std::path::Path::to_path_buf),
            temp: temp.clone(),
        };
        let xdg = temp.join("xdg");

        assert_eq!(
            cache_dir_from(bases(xdg.to_str(), Some(&home), Some(&profile))),
            xdg.join("cooldown")
        );
        assert_eq!(
            cache_dir_from(bases(Some(".cache"), Some(&home), Some(&profile))),
            home.join(".cache").join("cooldown")
        );
        assert_eq!(
            cache_dir_from(bases(None, None, Some(&profile))),
            profile.join(".cache").join("cooldown")
        );
        assert_eq!(
            cache_dir_from(bases(
                Some(""),
                Some(std::path::Path::new("")),
                Some(std::path::Path::new("home"))
            )),
            temp.join("cooldown")
        );
    }

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
        let config = configs.project_config(&proj).unwrap();
        assert_eq!(config.policy_layers.len(), 2);
        // Root first (lower authority), project last (higher), each named relative to the root.
        assert_eq!(
            config.policy_layers[0].origin,
            Origin::Repo(Utf8PathBuf::from("cooldown.toml"))
        );
        assert_eq!(
            config.policy_layers[1].origin,
            Origin::Repo(Utf8PathBuf::from("services/api/cooldown.toml"))
        );
    }

    #[test]
    fn explicit_config_missing_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        let err = ConfigSources::load(root, Some(&root.join("missing.toml")), true)
            .expect_err("missing config");
        std::assert_matches!(err, CoreError::Config(_));
    }

    #[test]
    fn cargo_edge_policy_resolves_per_project_from_the_nearest_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        std::fs::write(
            root.join(CONFIG_FILE),
            indoc! {r#"
                [tool.cargo]
                edge-policy = "preserve"
            "#},
        )
        .expect("root config");
        let canonical = root.join("apps/canonical");
        let inherited = root.join("apps/inherited");
        std::fs::create_dir_all(&canonical).expect("canonical project");
        std::fs::create_dir_all(&inherited).expect("inherited project");
        std::fs::write(
            canonical.join(CONFIG_FILE),
            indoc! {r#"
                [tool.cargo]
                edge-policy = "canonicalize"
            "#},
        )
        .expect("nested config");

        let configs = ConfigSources::load(root, None, true).expect("config sources");
        assert_eq!(
            configs
                .project_config(&canonical)
                .expect("canonical config")
                .cargo_edge_policy,
            Some(EdgePolicy::Canonicalize)
        );
        assert_eq!(
            configs
                .project_config(&inherited)
                .expect("inherited config")
                .cargo_edge_policy,
            Some(EdgePolicy::Preserve)
        );
    }

    /// `generated-members` resolves like the edge policy: the nearest declaring file decides,
    /// an explicit empty list is a decision of its own, and the origin of the decision is kept
    /// for `config`.
    #[test]
    fn cargo_generated_members_resolve_from_the_nearest_declaring_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        std::fs::write(
            root.join(CONFIG_FILE),
            indoc! {r#"
                [tool.cargo]
                generated-members = ["workspace-hack"]
            "#},
        )
        .expect("root config");
        let authored = root.join("apps/authored");
        let inherited = root.join("apps/inherited");
        std::fs::create_dir_all(&authored).expect("authored project");
        std::fs::create_dir_all(&inherited).expect("inherited project");
        std::fs::write(
            authored.join(CONFIG_FILE),
            indoc! {r"
                [tool.cargo]
                generated-members = []
            "},
        )
        .expect("nested config");

        let configs = ConfigSources::load(root, None, true).expect("config sources");
        assert_eq!(
            configs
                .project_config(&inherited)
                .expect("inherited config")
                .cargo_generated_members,
            Some(DeclaredGeneratedMembers {
                names: vec!["workspace-hack".to_string()],
                origin: Origin::Repo(Utf8PathBuf::from("cooldown.toml")),
            })
        );
        assert_eq!(
            configs
                .project_config(&authored)
                .expect("authored config")
                .cargo_generated_members,
            Some(DeclaredGeneratedMembers {
                names: Vec::new(),
                origin: Origin::Repo(Utf8PathBuf::from("apps/authored/cooldown.toml")),
            }),
            "a nearer empty list replaces the inherited declaration rather than adding to it"
        );

        let bare = tempfile::tempdir().expect("tempdir");
        let bare_root = Utf8Path::from_path(bare.path()).expect("utf8 root");
        std::fs::create_dir(bare_root.join(".git")).expect("git dir");
        let bare_configs = ConfigSources::load(bare_root, None, true).expect("config sources");
        assert_eq!(
            bare_configs
                .project_config(bare_root)
                .expect("no config")
                .cargo_generated_members,
            None,
            "no file declares the key"
        );
    }

    /// `single-copy` folds like the exclude lists: a nested plain array adds to the root's names,
    /// a nested `{ replace = [...] }` starts over, and a project with no list of its own inherits
    /// the enclosing one.
    #[test]
    fn pnpm_single_copy_folds_across_the_config_cascade() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        std::fs::write(
            root.join(CONFIG_FILE),
            indoc! {r#"
                [tool.pnpm]
                single-copy = ["solid-js", "react"]
            "#},
        )
        .expect("root config");
        let extending = root.join("apps/extending");
        let replacing = root.join("apps/replacing");
        let inherited = root.join("apps/inherited");
        for dir in [&extending, &replacing, &inherited] {
            std::fs::create_dir_all(dir).expect("project dir");
        }
        std::fs::write(
            extending.join(CONFIG_FILE),
            indoc! {r#"
                [tool.pnpm]
                single-copy = ["typescript"]
            "#},
        )
        .expect("extending config");
        std::fs::write(
            replacing.join(CONFIG_FILE),
            indoc! {r#"
                [tool.pnpm]
                single-copy = { replace = ["typescript"] }
            "#},
        )
        .expect("replacing config");

        let configs = ConfigSources::load(root, None, true).expect("config sources");
        let names = |dir: &Utf8Path| {
            configs
                .project_config(dir)
                .expect("project config")
                .pnpm_single_copy
        };
        assert_eq!(names(&extending), ["solid-js", "react", "typescript"]);
        assert_eq!(names(&replacing), ["typescript"]);
        assert_eq!(names(&inherited), ["solid-js", "react"]);
    }

    /// An explicit config inside the repository is named relative to the root like a cascade
    /// file, while one outside it keeps its absolute location.
    #[test]
    fn explicit_config_is_named_relative_to_the_root_only_inside_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(root.join("ci")).expect("ci dir");
        let inside = root.join("ci").join("cooldown.toml");
        std::fs::write(&inside, "min-age = \"3d\"\n").expect("inside config");
        let elsewhere = tempfile::tempdir().expect("tempdir");
        let outside = Utf8Path::from_path(elsewhere.path())
            .expect("utf8 dir")
            .join("shared.toml");
        std::fs::write(&outside, "min-age = \"3d\"\n").expect("outside config");

        let explicit_origin = |path: &Utf8Path| {
            ConfigSources::load(root, Some(path), true)
                .expect("config sources")
                .explicit_policy_layer()
                .expect("explicit layer")
                .expect("explicit config loaded")
                .origin
        };

        assert_eq!(
            explicit_origin(&inside),
            Origin::Config(Utf8PathBuf::from("ci/cooldown.toml"))
        );
        // No relative spelling exists outside the root, so the token names the file absolutely.
        let Origin::Config(external) = explicit_origin(&outside) else {
            panic!("an explicit config forms a config layer");
        };
        assert!(external.is_absolute(), "{external}");
        assert_eq!(
            std::fs::canonicalize(&external).expect("named file exists"),
            std::fs::canonicalize(&outside).expect("outside config exists")
        );
    }

    /// A `--config` argument Windows normalizes when reading it, such as one with a trailing `.`,
    /// is named by the file that was read rather than by a verbatim spelling of the argument.
    #[cfg(windows)]
    #[test]
    fn an_explicit_config_is_named_by_the_file_windows_reads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(root.join("ci")).expect("ci dir");
        std::fs::write(root.join("ci").join("cooldown.toml"), "min-age = \"3d\"\n")
            .expect("ci config");

        let configs =
            ConfigSources::load(root, Some(&root.join("ci").join("cooldown.toml.")), true)
                .expect("config sources");
        assert_eq!(
            configs
                .explicit_policy_layer()
                .expect("explicit layer")
                .map(|layer| layer.origin.token()),
            Some("config:ci/cooldown.toml".to_string())
        );
    }

    /// A root spelled through a symlink, as the `$HOME` fallback can be, still names the files
    /// below it relative to it when the project directory and `--config` arrive canonicalized.
    #[cfg(unix)]
    #[test]
    fn a_root_reached_through_a_symlink_names_its_files_relative_to_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base =
            Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).expect("canonical"))
                .expect("utf8 base");
        let real = base.join("real");
        std::fs::create_dir_all(real.join("apps").join("web")).expect("project dir");
        std::fs::write(real.join(CONFIG_FILE), "min-age = \"14d\"\n").expect("root config");
        std::fs::write(
            real.join("apps").join("web").join(CONFIG_FILE),
            "min-age = \"21d\"\n",
        )
        .expect("nested config");
        // A symlinked config file is named where the link sits, not by its target.
        std::fs::write(base.join("shared.toml"), "min-age = \"3d\"\n").expect("shared config");
        std::os::unix::fs::symlink(base.join("shared.toml"), real.join("linked.toml"))
            .expect("config symlink");
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("root symlink");

        // Detection hands over canonical project directories, while the root keeps the alias.
        let configs = ConfigSources::load(&alias, Some(&real.join("linked.toml")), true)
            .expect("config sources");
        let config = configs
            .project_config(&real.join("apps").join("web"))
            .expect("project config");
        let origins: Vec<_> = config
            .policy_layers
            .iter()
            .map(|layer| layer.origin.token())
            .collect();
        assert_eq!(
            origins,
            ["repo:cooldown.toml", "repo:apps/web/cooldown.toml"]
        );
        assert_eq!(
            configs
                .explicit_policy_layer()
                .expect("explicit layer")
                .map(|layer| layer.origin.token()),
            Some("config:linked.toml".to_string())
        );
    }

    /// Writes a root, an intermediate `apps` and a project `apps/web` config below `root`.
    ///
    /// The root file sets a scan list, which a nested file may not, so reading it a second time as
    /// a nested layer fails the projection instead of passing unnoticed.
    fn layered_tree(root: &Utf8Path) {
        let project = root.join("apps").join("web");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            root.join(CONFIG_FILE),
            indoc! {r#"
                min-age = "14d"

                [global]
                exclude-folders = ["vendor"]
            "#},
        )
        .expect("root config");
        std::fs::write(root.join("apps").join(CONFIG_FILE), "min-age = \"18d\"\n")
            .expect("intermediate config");
        std::fs::write(project.join(CONFIG_FILE), "min-age = \"21d\"\n").expect("project config");
    }

    /// The origin tokens of the policy layers `configs` collects for `project_dir`.
    fn layer_tokens(configs: &ConfigSources, project_dir: &Utf8Path) -> Vec<String> {
        configs
            .project_config(project_dir)
            .expect("project config")
            .policy_layers
            .iter()
            .map(|layer| layer.origin.token())
            .collect()
    }

    const ALL_LAYERS: [&str; 3] = [
        "repo:cooldown.toml",
        "repo:apps/cooldown.toml",
        "repo:apps/web/cooldown.toml",
    ];

    /// A root spelled through a symlinked checkout still bounds the walk of a canonical project
    /// directory, so the intermediate layer is collected and the root file is read once.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_collects_every_layer_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base =
            Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).expect("canonical"))
                .expect("utf8 base");
        let real = base.join("real");
        layered_tree(&real);
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&real, &alias).expect("root symlink");

        let configs = ConfigSources::load(&alias, None, true).expect("config sources");

        // Detection hands over canonical project directories, while the root keeps the alias.
        assert_eq!(
            layer_tokens(&configs, &real.join("apps").join("web")),
            ALL_LAYERS
        );
        // The root itself, in either spelling, is one layer rather than a root and a nested one.
        assert_eq!(layer_tokens(&configs, &real), ["repo:cooldown.toml"]);
        assert_eq!(layer_tokens(&configs, &alias), ["repo:cooldown.toml"]);
    }

    /// A project directory reached through a symlink from outside the checkout walks the
    /// repository directories its target sits in.
    #[cfg(unix)]
    #[test]
    fn a_project_reached_through_a_symlink_collects_every_layer_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base =
            Utf8PathBuf::from_path_buf(std::fs::canonicalize(tmp.path()).expect("canonical"))
                .expect("utf8 base");
        let root = base.join("repo");
        layered_tree(&root);
        let link = base.join("web");
        std::os::unix::fs::symlink(root.join("apps").join("web"), &link).expect("project symlink");

        let configs = ConfigSources::load(&root, None, true).expect("config sources");

        assert_eq!(layer_tokens(&configs, &link), ALL_LAYERS);
    }

    /// A root spelled with a `..` component, which a project directory can be a lexical prefix
    /// of, still bounds the walk, so no config above the root is read.
    #[test]
    fn a_root_spelled_through_a_parent_component_reads_nothing_above_it() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = Utf8Path::from_path(tmp.path()).expect("utf8 base");
        // This file sits above the repository, so collecting it would apply a foreign layer.
        std::fs::write(base.join(CONFIG_FILE), "min-age = \"1d\"\n").expect("outside config");
        let root = base.join("repo");
        layered_tree(&root);

        let configs =
            ConfigSources::load(&root.join("apps").join(".."), None, true).expect("config sources");

        assert_eq!(
            layer_tokens(&configs, &root.join("apps")),
            ["repo:cooldown.toml", "repo:apps/cooldown.toml"]
        );
    }

    /// A project directory that does not resolve gets the root layer alone, so a raw spelling
    /// through `..` cannot reach a config above the root, and a root that does not exist at all,
    /// as a `$HOME` fallback can be, yields no layer rather than an error.
    #[test]
    fn an_unresolvable_project_directory_gets_the_root_layer_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = Utf8Path::from_path(tmp.path()).expect("utf8 base");
        std::fs::write(base.join(CONFIG_FILE), "min-age = \"1d\"\n").expect("outside config");
        let root = base.join("repo");
        layered_tree(&root);

        let configs = ConfigSources::load(&root, None, true).expect("config sources");
        assert_eq!(
            layer_tokens(&configs, &root.join("..").join("missing")),
            ["repo:cooldown.toml"]
        );

        let missing = base.join("missing");
        let configs = ConfigSources::load(&missing, None, true).expect("config sources");
        assert_eq!(layer_tokens(&configs, &missing), Vec::<String>::new());
    }

    /// A root spelled the way the temporary directory is handed out, which is not the verbatim
    /// (and possibly an 8.3 short) form that canonical project directories take, still bounds the
    /// walk.
    #[cfg(windows)]
    #[test]
    fn a_root_without_the_verbatim_prefix_collects_every_layer_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        layered_tree(root);
        let canonical = |path: &Utf8Path| {
            Utf8PathBuf::from_path_buf(std::fs::canonicalize(path).expect("canonical"))
                .expect("utf8 path")
        };
        let canonical_root = canonical(root);
        assert!(
            canonical_root.as_str().starts_with(r"\\?\"),
            "{canonical_root} is verbatim"
        );

        let configs = ConfigSources::load(root, None, true).expect("config sources");

        assert_eq!(
            layer_tokens(&configs, &canonical(&root.join("apps").join("web"))),
            ALL_LAYERS
        );
        assert_eq!(
            layer_tokens(&configs, &canonical_root),
            ["repo:cooldown.toml"]
        );
    }

    /// The `$HOME` fallback resolves to the canonical directory the environment's spelling names,
    /// the form the workdir the walk starts from takes.
    #[test]
    fn the_home_fallback_root_is_canonical() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let base = Utf8Path::from_path(tmp.path()).expect("utf8 base");
        let workdir = base.join("home").join("work");
        std::fs::create_dir_all(&workdir).expect("workdir");
        std::fs::create_dir_all(base.join("other")).expect("other dir");
        let home = base.join("other").join("..").join("home");

        // Nothing above the temporary directory marks a repository, so the walk falls back.
        assert_eq!(
            repo_root_from(&workdir, Some(home)),
            Utf8PathBuf::from_path_buf(
                std::fs::canonicalize(base.join("home")).expect("canonical home")
            )
            .expect("utf8 home")
        );
    }

    #[test]
    fn explicit_config_overrides_the_project_edge_policy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        std::fs::write(
            root.join(CONFIG_FILE),
            indoc! {r#"
                [tool.cargo]
                edge-policy = "canonicalize"
            "#},
        )
        .expect("root config");
        let explicit = root.join("explicit.toml");
        std::fs::write(
            &explicit,
            indoc! {r#"
                [tool.cargo]
                edge-policy = "none"
            "#},
        )
        .expect("explicit config");

        let configs = ConfigSources::load(root, Some(&explicit), true).expect("config sources");
        assert_eq!(
            configs
                .project_config(root)
                .expect("project config")
                .cargo_edge_policy,
            Some(EdgePolicy::None)
        );
    }

    /// A nested `cooldown.toml` never feeds the scan config, so an exclude or `include-hidden`
    /// list there is refused with a pointer to the files that do, instead of being validated and
    /// then ignored.
    #[test]
    fn nested_exclude_lists_are_rejected_with_a_location_hint() {
        for nested in [
            indoc! {r"
                [tool.cargo]
                exclude-folders = []
            "},
            indoc! {r#"
                [global]
                include-hidden = [".agents"]
            "#},
        ] {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
            std::fs::create_dir(root.join(".git")).expect("git dir");
            let project = root.join("apps/api");
            std::fs::create_dir_all(&project).expect("project dir");
            std::fs::write(project.join(CONFIG_FILE), nested).expect("nested config");

            let configs = ConfigSources::load(root, None, true).expect("config sources");
            let error = configs
                .project_config(&project)
                .expect_err("nested scan list");
            assert!(
                error.to_string().contains("repository root"),
                "the error names where scan lists belong: {error}"
            );
        }
    }

    #[test]
    fn nested_misplaced_edge_policy_is_rejected_during_project_projection() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        let project = root.join("apps/api");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(
            project.join(CONFIG_FILE),
            indoc! {r#"
                [tool.npm]
                edge-policy = "preserve"
            "#},
        )
        .expect("nested config");

        let configs = ConfigSources::load(root, None, true).expect("config sources");
        let error = configs
            .project_config(&project)
            .expect_err("misplaced nested edge policy");
        assert!(error.to_string().contains("[tool.cargo]"), "{error}");
    }

    #[test]
    fn nested_config_is_parsed_once_for_the_run() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(tmp.path()).expect("utf8 root");
        std::fs::create_dir(root.join(".git")).expect("git dir");
        let project = root.join("apps/api");
        std::fs::create_dir_all(&project).expect("project dir");
        let nested = project.join(CONFIG_FILE);
        std::fs::write(
            &nested,
            indoc! {r#"
                [tool.cargo]
                edge-policy = "preserve"
            "#},
        )
        .expect("nested config");
        let configs = ConfigSources::load(root, None, true).expect("config sources");
        let first = configs.project_config(&project).expect("first projection");

        std::fs::write(
            &nested,
            indoc! {r#"
                [tool.cargo]
                edge-policy = "canonicalize"
            "#},
        )
        .expect("replace nested config");
        let second = configs.project_config(&project).expect("second projection");

        assert_eq!(first.cargo_edge_policy, Some(EdgePolicy::Preserve));
        assert_eq!(second.cargo_edge_policy, first.cargo_edge_policy);
    }
}
