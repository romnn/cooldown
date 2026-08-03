//! Gitignore-aware project-root discovery shared by the tool adapters' `detect`.
//!
//! Each adapter declares a primary marker and may add a validation-only marker.
//! The shared walk descends from a root, skips excluded inputs, and collects both marker sets in one
//! traversal.
//! Centralizing it here keeps `.gitignore`, exclude, and workspace-root policy consistent.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::config::{compile_folder_globset, compile_package_globset};
use cooldown_core::{CoreError, ProjectDetection};
use globset::GlobSet;
use ignore::WalkBuilder;

/// Find every directory under `root` that directly contains a file named `marker`.
///
/// - `respect_gitignore`: when true (the default), ignore files (`.gitignore`, `.git/info/exclude`,
///   the global gitignore, and ripgrep's `.ignore`/`.rgignore`) prune which *directories* are
///   walked — skipping `target/`, vendored, generated, and cache trees (correct, and faster since
///   those often-huge trees are never descended). The marker is matched per *directory*, not by the
///   walk yielding the lockfile, so the rule is: a lockfile inside an ignored directory is skipped
///   (a stray `Cargo.lock` in a generated folder is not a project), but a lockfile that is itself
///   ignored at the file level is still detected — libraries routinely `.gitignore` their
///   `Cargo.lock`, and that must not make the project disappear.
/// - `exclude`: extra directory globs that are never scanned, in addition to gitignore, with
///   `.gitignore` semantics (see [`compile_folder_globset`]): a bare name (`"target"`, trailing
///   slash optional) prunes that directory at any depth, a leading slash (`"/build"`) anchors to
///   `root`, and an interior slash (`"third_party/grammars"`) is a root-relative path. `**` is
///   supported.
/// - `topmost_only`: when true, a match's descendants are not reported. A `Cargo.lock`/`uv.lock`
///   marks a workspace root that already owns its members, so nested lockfiles below it are skipped.
///
/// Hidden directories (dotfiles such as `.git`, `.venv`) are always skipped. Unreadable
/// directories are skipped rather than failing the whole scan.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if an `exclude` entry is not a valid glob.
pub fn find_marker_dirs(
    root: &Utf8Path,
    marker: &str,
    respect_gitignore: bool,
    exclude: &[String],
    topmost_only: bool,
) -> Result<Vec<Utf8PathBuf>, CoreError> {
    Ok(scan_marker_dirs(root, marker, None, respect_gitignore, exclude, topmost_only)?.primary)
}

/// Directly detected roots and validation-only roots found during one repository traversal.
pub(crate) struct ProjectMarkerDirs {
    pub(crate) primary: Vec<Utf8PathBuf>,
    pub(crate) validation_only: Vec<Utf8PathBuf>,
}

/// Finds an adapter's primary and validation-only markers during one filesystem traversal.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if an `exclude` entry is not a valid glob.
pub(crate) fn find_project_marker_dirs(
    root: &Utf8Path,
    detection: ProjectDetection,
    respect_gitignore: bool,
    exclude: &[String],
) -> Result<ProjectMarkerDirs, CoreError> {
    let primary = detection.primary();
    scan_marker_dirs(
        root,
        primary.lockfile,
        detection.validation_marker(),
        respect_gitignore,
        exclude,
        primary.workspace_root,
    )
}

fn scan_marker_dirs(
    root: &Utf8Path,
    primary_marker: &str,
    validation_marker: Option<&str>,
    respect_gitignore: bool,
    exclude: &[String],
    topmost_only: bool,
) -> Result<ProjectMarkerDirs, CoreError> {
    let excludes = compile_folder_globset(exclude)?;
    let root_owned = root.to_owned();

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore)
        .parents(respect_gitignore)
        // `.ignore`/`.rgignore` (ripgrep's files) prune directories too. Their file-level lock
        // patterns (repos routinely add `**/*.lock` to cut search noise) are harmless here because
        // we test the marker per *directory* below rather than trusting the walk to yield the
        // lockfile — so a directory entry like `testdata/` still prunes, but a hidden lockfile
        // inside a walked directory is never missed.
        .ignore(respect_gitignore)
        .require_git(true);
    builder.filter_entry(move |entry| {
        // Only directories are pruned; files always pass so we can match the marker on them.
        if entry.file_type().is_none_or(|t| !t.is_dir()) {
            return true;
        }
        !is_excluded(entry.path(), &root_owned, &excludes)
    });

    let mut primary = Vec::new();
    let mut validation_only = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(e) => {
                tracing::debug!(error = %e, "skipping unreadable path during scan");
                continue;
            }
        };
        // Test the marker against each walked *directory* rather than looking for the marker as a
        // yielded file. gitignore then prunes only which directories we descend into (skipping
        // `target/`, vendored, and cache trees); a project whose lockfile is itself gitignored —
        // common for libraries that don't commit `Cargo.lock` — is still detected.
        if entry.file_type().is_some_and(|t| t.is_dir())
            && let Some(dir) = Utf8Path::from_path(entry.path())
        {
            if marker_entry_exists(&dir.join(primary_marker)) {
                primary.push(dir.to_owned());
            }
            if validation_marker.is_some_and(|marker| marker_entry_exists(&dir.join(marker))) {
                validation_only.push(dir.to_owned());
            }
        }
    }

    primary.sort();
    primary.dedup();
    validation_only.sort();
    validation_only.dedup();
    if topmost_only {
        primary = keep_topmost(primary);
        validation_only.retain(|candidate| {
            !primary
                .iter()
                .any(|accepted| candidate.starts_with(accepted))
        });
    }
    Ok(ProjectMarkerDirs {
        primary,
        validation_only,
    })
}

/// Finds exact recovery artifacts without applying normal project-discovery ignore policy.
///
/// Hidden and gitignored projects remain visible because recovery must follow durable ownership
/// evidence rather than the current discovery configuration.
/// Known metadata, dependency, build, and cache trees are pruned to keep a repository-root safety
/// scan bounded.
pub fn find_recovery_marker_dirs(
    root: &Utf8Path,
    marker: &str,
) -> Result<Vec<Utf8PathBuf>, CoreError> {
    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .ignore(false)
        .require_git(false)
        .filter_entry(|entry| {
            if entry.depth() == 0 || entry.file_type().is_none_or(|kind| !kind.is_dir()) {
                return true;
            }
            !matches!(
                entry.file_name().to_str(),
                Some(
                    ".git"
                        | ".hg"
                        | ".svn"
                        | ".cache"
                        | ".venv"
                        | "node_modules"
                        | "target"
                        | "vendor"
                )
            )
        });
    let mut dirs = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                return Err(CoreError::Filesystem(format!(
                    "cannot inspect recovery artifacts below {root}: {error}"
                )));
            }
        };
        if entry.file_type().is_some_and(|kind| kind.is_dir())
            && let Some(dir) = Utf8Path::from_path(entry.path())
            && std::fs::symlink_metadata(dir.join(marker)).is_ok()
        {
            dirs.push(dir.to_owned());
        }
    }
    dirs.sort();
    dirs.dedup();
    Ok(dirs)
}

fn marker_entry_exists(path: &Utf8Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        let file_type = metadata.file_type();
        file_type.is_file() || file_type.is_symlink()
    })
}

/// Whether `path` (a directory) is excluded, matching its path relative to `root` against the
/// folder globset. Matching the relative path (rather than the bare name) is what lets a leading
/// slash anchor to the root: a bare name still prunes at any depth because
/// [`compile_folder_globset`] gives it the `**/` variant.
fn is_excluded(path: &std::path::Path, root: &Utf8Path, excludes: &GlobSet) -> bool {
    if excludes.is_empty() {
        return false;
    }
    Utf8Path::from_path(path)
        .and_then(|p| p.strip_prefix(root).ok())
        .is_some_and(|rel| !rel.as_str().is_empty() && excludes.is_match(rel.as_std_path()))
}

/// Drop any directory that has an ancestor already in the set (sorted input puts ancestors first).
fn keep_topmost(dirs: Vec<Utf8PathBuf>) -> Vec<Utf8PathBuf> {
    let mut kept: Vec<Utf8PathBuf> = Vec::new();
    for dir in dirs {
        if !kept.iter().any(|root| dir.starts_with(root)) {
            kept.push(dir);
        }
    }
    kept
}

/// `exclude-folders` compiled for filtering workspace members by *location*. A member is excluded
/// when its path — or any ancestor, so `packages/ts/luup` also excludes `packages/ts/luup/api` —
/// matches a folder glob (`.gitignore` semantics; see [`compile_folder_globset`]).
#[derive(Debug, Clone)]
pub(crate) struct FolderExcludeSet(GlobSet);

impl FolderExcludeSet {
    /// Compile the folder-exclude globs (an empty set matches nothing).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if a glob is invalid.
    pub(crate) fn compile(patterns: &[String]) -> Result<Self, CoreError> {
        Ok(Self(compile_folder_globset(patterns)?))
    }

    /// Whether a member living at `path` (or under an excluded ancestor) is excluded.
    #[must_use]
    pub(crate) fn excludes_path(&self, path: &Utf8Path) -> bool {
        if self.0.is_empty() {
            return false;
        }
        path.ancestors().any(|ancestor| {
            !ancestor.as_str().is_empty() && self.0.is_match(ancestor.as_std_path())
        })
    }
}

/// `exclude-packages` compiled for filtering workspace members by *package name*. A scoped glob like
/// `@luup/*` excludes every `@luup/...` member regardless of where it lives in the tree (see
/// [`compile_package_globset`]).
#[derive(Debug, Clone)]
pub(crate) struct PackageExcludeSet(GlobSet);

impl PackageExcludeSet {
    /// Compile the package-exclude globs (an empty set matches nothing).
    ///
    /// # Errors
    ///
    /// Returns [`CoreError::Config`] if a glob is invalid.
    pub(crate) fn compile(patterns: &[String]) -> Result<Self, CoreError> {
        Ok(Self(compile_package_globset(patterns)?))
    }

    /// Whether a member with package `name` is excluded.
    #[must_use]
    pub(crate) fn excludes_name(&self, name: &str) -> bool {
        !self.0.is_empty() && self.0.is_match(std::path::Path::new(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;

    fn utf8(p: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p.to_path_buf()).expect("utf8 path")
    }

    #[test]
    fn folder_exclude_set_matches_member_by_path_prefix() {
        let set = FolderExcludeSet::compile(&["packages/ts/luup".to_string()]).expect("compile");
        // A path under an excluded directory is excluded (ancestor match).
        assert!(set.excludes_path(Utf8Path::new("packages/ts/luup/api")));
        assert!(set.excludes_path(Utf8Path::new("packages/ts/luup")));
        // A sibling path is kept.
        assert!(!set.excludes_path(Utf8Path::new("apps/admin")));
        // The root importer (`.`) is never matched by a sub-path exclude.
        assert!(!set.excludes_path(Utf8Path::new(".")));
    }

    #[test]
    fn package_exclude_set_matches_member_by_name_glob() {
        let set = PackageExcludeSet::compile(&["@luup/*".to_string()]).expect("compile");
        // Excluded by scoped package-name glob regardless of where it lives.
        assert!(set.excludes_name("@luup/landingpage"));
        assert!(set.excludes_name("@luup/api"));
        // A different scope / unscoped name is kept.
        assert!(!set.excludes_name("@airtype/admin"));
        assert!(!set.excludes_name("root-pkg"));
    }

    #[test]
    fn empty_exclude_sets_match_nothing() {
        assert!(
            !FolderExcludeSet::compile(&[])
                .expect("compile")
                .excludes_path(Utf8Path::new("apps/admin"))
        );
        assert!(
            !PackageExcludeSet::compile(&[])
                .expect("compile")
                .excludes_name("@luup/api")
        );
    }

    fn touch(path: &Utf8Path) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("mkdir");
        }
        std::fs::write(path, "").expect("write");
    }

    #[test]
    fn topmost_only_skips_nested_markers() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        touch(&root.join("Cargo.lock"));
        touch(&root.join("vendored/grammar/Cargo.lock"));

        let found = find_marker_dirs(&root, "Cargo.lock", false, &[], true).expect("scan");
        assert_eq!(found, vec![root]);
    }

    #[test]
    fn project_scan_separates_primary_and_validation_only_markers() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir_all(root.join("with-lock"))?;
        std::fs::create_dir_all(root.join("custom-lock"))?;
        std::fs::write(root.join("with-lock/Cargo.lock"), "")?;
        std::fs::write(root.join("with-lock/Cargo.toml"), "")?;
        std::fs::write(root.join("custom-lock/Cargo.toml"), "")?;
        let detection = ProjectDetection::PrimaryWithValidation {
            primary: cooldown_core::ProjectMarker {
                lockfile: "Cargo.lock",
                manifest: "Cargo.toml",
                alternate_manifests: &[],
                workspace_root: true,
            },
            validation_marker: "Cargo.toml",
        };

        let found = find_project_marker_dirs(&root, detection, false, &[])?;

        assert_eq!(found.primary, vec![root.join("with-lock")]);
        assert_eq!(found.validation_only, vec![root.join("custom-lock")]);
        Ok(())
    }

    #[test]
    fn outer_validation_marker_does_not_hide_a_nested_validation_marker() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let nested = root.join("tools/custom");
        std::fs::create_dir_all(nested.join(".cargo"))?;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n")?;
        std::fs::write(nested.join("Cargo.toml"), "[package]\nname = \"custom\"\n")?;
        std::fs::write(
            nested.join(".cargo/config.toml"),
            "[resolver]\nlockfile-path = \"Custom.lock\"\n",
        )?;
        let detection = ProjectDetection::PrimaryWithValidation {
            primary: cooldown_core::ProjectMarker {
                lockfile: "Cargo.lock",
                manifest: "Cargo.toml",
                alternate_manifests: &[],
                workspace_root: true,
            },
            validation_marker: "Cargo.toml",
        };

        let found = find_project_marker_dirs(&root, detection, false, &[])?;

        assert!(found.primary.is_empty());
        assert_eq!(found.validation_only, vec![root, nested]);
        Ok(())
    }

    #[test]
    fn without_topmost_all_markers_are_reported() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        touch(&root.join("go.mod"));
        touch(&root.join("svc/api/go.mod"));

        let found = find_marker_dirs(&root, "go.mod", false, &[], false).expect("scan");
        assert_eq!(found, vec![root.clone(), root.join("svc/api")]);
    }

    #[test]
    fn exclude_by_bare_name_prunes_at_any_depth() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        touch(&root.join("uv.lock"));
        touch(&root.join("third_party/dep/uv.lock"));

        let excludes = vec!["third_party".to_string()];
        let found = find_marker_dirs(&root, "uv.lock", false, &excludes, false).expect("scan");
        assert_eq!(found, vec![root]);
    }

    #[test]
    fn exclude_with_trailing_slash_matches_like_bare_name() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        touch(&root.join("uv.lock"));
        touch(&root.join("examples/dep/uv.lock"));
        touch(&root.join("nested/examples/uv.lock"));

        // `"examples/"` is the natural directory-exclude idiom; the trailing slash must not change
        // its meaning. Like the bare name, it prunes `examples/` at any depth.
        let excludes = vec!["examples/".to_string()];
        let found = find_marker_dirs(&root, "uv.lock", false, &excludes, false).expect("scan");
        assert_eq!(found, vec![root]);
    }

    #[test]
    fn exclude_with_leading_slash_anchors_to_root() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        touch(&root.join("uv.lock"));
        touch(&root.join("examples/uv.lock"));
        touch(&root.join("nested/examples/uv.lock"));

        // `/examples` anchors to the repo root: the top-level examples is pruned, the nested one is
        // kept (unlike the bare name, which would prune both).
        let excludes = vec!["/examples".to_string()];
        let found = find_marker_dirs(&root, "uv.lock", false, &excludes, false).expect("scan");
        assert_eq!(found, vec![root.clone(), root.join("nested/examples")]);
    }

    #[test]
    fn lockfile_in_a_gitignored_directory_is_pruned() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        // A real git repo is required for .gitignore to take effect.
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        touch(&root.join("Cargo.lock"));
        // A generated/cache directory carrying a stray lockfile (e.g. a vendored grammar fixture).
        std::fs::write(root.join(".gitignore"), "_cache/\n").expect("gitignore");
        touch(&root.join("_cache/grammar/Cargo.lock"));

        let respected = find_marker_dirs(&root, "Cargo.lock", true, &[], false).expect("scan");
        assert_eq!(
            respected,
            vec![root.clone()],
            "_cache/ is gitignored, so its lock is skipped"
        );

        let unrespected = find_marker_dirs(&root, "Cargo.lock", false, &[], false).expect("scan");
        assert_eq!(
            unrespected,
            vec![root.clone(), root.join("_cache/grammar")],
            "with --no-gitignore the stray nested lock is found"
        );
    }

    #[test]
    fn lockfile_ignored_at_file_level_is_still_detected() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        touch(&root.join("Cargo.lock"));
        // Libraries routinely gitignore their own lockfile; a ripgrep `.ignore` may hide it from
        // search. Neither should make the project disappear — the marker is tested per directory.
        std::fs::write(root.join(".gitignore"), "Cargo.lock\n").expect("gitignore");
        std::fs::write(root.join(".ignore"), "**/*.lock\n").expect("rgignore");

        let found = find_marker_dirs(&root, "Cargo.lock", true, &[], true).expect("scan");
        assert_eq!(
            found,
            vec![root],
            "a file-level-ignored lockfile is still a project"
        );
    }

    #[test]
    fn invalid_exclude_glob_is_a_config_error() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        let err = find_marker_dirs(&root, "Cargo.lock", false, &["a/**/[".to_string()], false)
            .expect_err("bad glob");
        std::assert_matches!(err, CoreError::Config(_));
    }

    #[test]
    fn recovery_scan_finds_hidden_and_ignored_projects_but_prunes_bulk_trees() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        std::fs::create_dir_all(root.join(".git"))?;
        std::fs::write(root.join(".gitignore"), "ignored/\n")?;
        for marker in [
            "ignored/project/recovery",
            ".hidden/project/recovery",
            "target/fixture/recovery",
        ] {
            let marker = root.join(marker);
            std::fs::create_dir_all(
                marker
                    .parent()
                    .ok_or_else(|| eyre::eyre!("recovery marker has no parent"))?,
            )?;
            std::fs::write(marker, "")?;
        }

        let found = find_recovery_marker_dirs(root, "recovery")?;
        assert_eq!(
            found,
            vec![root.join(".hidden/project"), root.join("ignored/project")]
        );
        Ok(())
    }
}
