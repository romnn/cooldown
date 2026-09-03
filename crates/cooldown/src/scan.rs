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
use std::collections::BTreeSet;

/// Find every directory under `root` that directly contains a file named `marker`.
///
/// - `respect_gitignore`: when true (the default), ignore files (`.gitignore`, `.git/info/exclude`,
///   the global gitignore, and ripgrep's `.ignore`/`.rgignore`) prune which *directories* are
///   walked — skipping `target/`, vendored, generated, and cache trees (correct, and faster since
///   those often-huge trees are never descended).
///   The marker is matched per *directory*, not by the
///   walk yielding the lockfile, so the rule is: a lockfile inside an ignored directory is skipped
///   (a stray `Cargo.lock` in a generated folder is not a project), but a lockfile that is itself
///   ignored at the file level is still detected — libraries routinely `.gitignore` their
///   `Cargo.lock`, and that must not make the project disappear.
/// - `exclude`: extra directory globs that are never scanned, in addition to gitignore, with
///   `.gitignore` semantics (see [`compile_folder_globset`]): a bare name (`"target"`, trailing
///   slash optional) prunes that directory at any depth, a leading slash (`"/build"`) anchors to
///   `root`, and an interior slash (`"third_party/grammars"`) is a root-relative path.
///   `**` is
///   supported.
/// - `topmost_only`: when true, a match's descendants are not reported.
///   A `Cargo.lock`/`uv.lock`
///   marks a workspace root that already owns its members, so nested lockfiles below it are skipped.
///
/// Hidden directories (dotfiles such as `.git`, `.venv`) are skipped unless the selection lies
/// in or below one (see [`WalkPolicy`]).
/// Unreadable directories are skipped rather than failing the whole scan.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if an `exclude` entry is not a valid glob.
#[cfg(test)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectMarkerDirs {
    pub(crate) primary: Vec<Utf8PathBuf>,
    pub(crate) validation_only: Vec<Utf8PathBuf>,
    /// Lockfile roots dropped by the topmost-only rule for sitting below another primary root.
    /// Kept so the orchestrator can appeal them to the adapter's
    /// `nested_lockfile_root_escapes` — a nested workspace root the enclosing workspace merely
    /// excludes is a project of its own. Empty for markers without the topmost-only rule.
    pub(crate) nested: Vec<Utf8PathBuf>,
}

/// Finds an adapter's primary and validation-only markers during one filesystem traversal.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if an `exclude` entry is not a valid glob.
#[cfg(test)]
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

/// Which directories one marker walk may enter.
#[derive(Clone, Copy)]
pub(crate) struct WalkPolicy<'a> {
    /// Honor `.gitignore`/`.ignore` files (the default); off, only the exclude globs prune.
    pub(crate) respect_gitignore: bool,
    /// `exclude-folders` globs with `.gitignore` semantics (see [`compile_folder_globset`]).
    pub(crate) exclude: &'a [String],
    /// A directory under the root the invocation named explicitly (`-C`/`--dir`, or its own
    /// working directory below the scan root).
    ///
    /// The walk always enters it and the ancestors leading to it, hidden or excluded as they may
    /// be: the excludes trim the default scan, and naming a path outranks a glob.
    /// Inside an excluded ancestor entered only for that reason, the walk stays on the path to
    /// the selection and within the selection's own subtree, so lifting an ancestor never brings
    /// back the siblings the exclude meant to prune.
    /// Excludes below the selection apply as usual.
    /// Gitignore rules are not lifted (that is `--no-gitignore`), but a selection they hide is
    /// reported as an error instead of yielding an empty scan.
    pub(crate) selected: Option<&'a Utf8Path>,
}

/// Finds several adapters' marker sets during one filesystem traversal.
///
/// Every detection must use the same [`WalkPolicy`].
/// Results preserve the input order.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if an exclude entry is not a valid glob, or if the walk never
/// reached the selected directory (a gitignore rule hides it, or it is unreadable).
pub(crate) fn find_project_marker_dirs_batch(
    root: &Utf8Path,
    detections: &[ProjectDetection],
    policy: WalkPolicy<'_>,
) -> Result<Vec<ProjectMarkerDirs>, CoreError> {
    let scans = detections
        .iter()
        .map(|detection| {
            let primary = detection.primary();
            MarkerScan {
                primary: primary.lockfile,
                validation: detection.validation_marker(),
                topmost_only: primary.workspace_root,
            }
        })
        .collect::<Vec<_>>();
    scan_marker_groups(root, &scans, policy)
}

#[derive(Clone, Copy)]
struct MarkerScan<'a> {
    primary: &'a str,
    validation: Option<&'a str>,
    topmost_only: bool,
}

#[cfg(test)]
fn scan_marker_dirs(
    root: &Utf8Path,
    primary_marker: &str,
    validation_marker: Option<&str>,
    respect_gitignore: bool,
    exclude: &[String],
    topmost_only: bool,
) -> Result<ProjectMarkerDirs, CoreError> {
    let scan = MarkerScan {
        primary: primary_marker,
        validation: validation_marker,
        topmost_only,
    };
    scan_marker_groups(
        root,
        &[scan],
        WalkPolicy {
            respect_gitignore,
            exclude,
            selected: None,
        },
    )?
    .into_iter()
    .next()
    .ok_or_else(|| CoreError::System("marker scan produced no result".to_string()))
}

/// The directory filter of one scan walk.
///
/// Files always pass because the markers are matched per walked directory, never as yielded
/// files.
/// Hidden directories are pruned unless they lie on the spine, the selected directory and every
/// ancestor the walk must enter to reach it.
/// Excluded directories follow the same rule as a workspace member (see
/// [`FolderExcludeSet::excludes_path`]): on the spine and below the selection a glob matching at
/// or above the selection never counts, while beside the spine every glob counts, so a lifted
/// ancestor's other children stay pruned.
struct DirFilter {
    root: Utf8PathBuf,
    excludes: FolderExcludeSet,
    /// The selection relative to `root`.
    selected: Option<Utf8PathBuf>,
}

impl DirFilter {
    fn admits(&self, entry: &ignore::DirEntry) -> bool {
        // Only directories are pruned; files always pass so we can match the marker on them.
        if entry.file_type().is_none_or(|t| !t.is_dir()) {
            return true;
        }
        let Some(path) = Utf8Path::from_path(entry.path()) else {
            return true;
        };
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return true;
        };
        if rel.as_str().is_empty() {
            return true;
        }
        let selected = self.selected.as_deref();
        // The spine is the selection and every ancestor the walk must enter to reach it.
        let on_spine = selected.is_some_and(|selected| selected.starts_with(rel));
        // Dot-directories (`.git`, `.venv`) are never scanned unless the invocation named one, or
        // a path through one.
        if is_hidden(path) && !on_spine {
            return false;
        }
        // Naming a directory outranks a glob that would prune it or a directory above it, so the
        // globs are lifted on the spine and below the selection; beside the spine nothing is
        // lifted, which keeps a lifted ancestor's other children pruned (a broken config in one
        // of them must not fail a run that never asked for it).
        let lifted = selected.filter(|selected| on_spine || rel.starts_with(selected));
        if self.excludes.excludes_path(rel, lifted) {
            return false;
        }
        if on_spine && self.excludes.excludes_path(rel, None) {
            tracing::debug!(dir = %path, "entering an excluded directory on the selected path");
        }
        true
    }
}

fn scan_marker_groups(
    root: &Utf8Path,
    scans: &[MarkerScan<'_>],
    policy: WalkPolicy<'_>,
) -> Result<Vec<ProjectMarkerDirs>, CoreError> {
    let excludes = FolderExcludeSet::compile(policy.exclude)?;
    // A selection outside the root has nothing to lift and nothing to reach.
    let selected = policy
        .selected
        .filter(|selected| selected.starts_with(root))
        .map(Utf8Path::to_owned);
    let respect_gitignore = policy.respect_gitignore;
    let markers = scans
        .iter()
        .flat_map(|scan| std::iter::once(scan.primary).chain(scan.validation))
        .collect::<BTreeSet<_>>();

    let mut builder = WalkBuilder::new(root);
    builder
        // Hidden directories are pruned in `filter_entry` below, where the selection can lift
        // the rule, rather than by the walker, which offers no exception.
        .hidden(false)
        .git_ignore(respect_gitignore)
        .git_global(respect_gitignore)
        .git_exclude(respect_gitignore)
        .parents(respect_gitignore)
        // `.ignore`/`.rgignore` (ripgrep's files) prune directories too.
        // Their file-level lock
        // patterns (repos routinely add `**/*.lock` to cut search noise) are harmless here because
        // we test the marker per *directory* below rather than trusting the walk to yield the
        // lockfile — so a directory entry like `testdata/` still prunes, but a hidden lockfile
        // inside a walked directory is never missed.
        .ignore(respect_gitignore)
        .require_git(true);
    let filter = DirFilter {
        root: root.to_owned(),
        excludes,
        selected: selected
            .as_deref()
            .and_then(|selected| selected.strip_prefix(root).ok())
            .map(Utf8Path::to_owned),
    };
    builder.filter_entry(move |entry| filter.admits(entry));

    let mut found = scans
        .iter()
        .map(|_| ProjectMarkerDirs {
            primary: Vec::new(),
            validation_only: Vec::new(),
            nested: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut reached_selected = false;
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
            if selected.as_deref() == Some(dir) {
                reached_selected = true;
            }
            let present = present_markers(dir, &markers);
            for (scan, result) in scans.iter().zip(&mut found) {
                if present.contains(scan.primary) {
                    result.primary.push(dir.to_owned());
                }
                if scan
                    .validation
                    .is_some_and(|marker| present.contains(marker))
                {
                    result.validation_only.push(dir.to_owned());
                }
            }
        }
    }

    // The excludes and hidden-directory rule were lifted along the selected path, so a
    // selection the walk still never reached sits under a gitignore rule (or an unreadable
    // directory); say so instead of returning an empty scan the run would report as clean.
    if let Some(selected) = &selected
        && !reached_selected
    {
        let selected = selected.strip_prefix(root).unwrap_or(selected);
        return Err(CoreError::Config(if respect_gitignore {
            format!(
                "{selected} was not scanned: a .gitignore/.ignore rule ignores it or a directory \
                 above it; pass --no-gitignore to scan ignored directories"
            )
        } else {
            format!("{selected} was not scanned: it or a directory above it could not be read")
        }));
    }

    for (scan, result) in scans.iter().zip(&mut found) {
        result.primary.sort();
        result.primary.dedup();
        result.validation_only.sort();
        result.validation_only.dedup();
        if scan.topmost_only {
            let (topmost, nested) = split_topmost(std::mem::take(&mut result.primary));
            result.primary = topmost;
            result.nested = nested;
            result
                .validation_only
                .retain(|candidate| result.primary.binary_search(candidate).is_err());
        }
    }
    Ok(found)
}

/// Finds reserved recovery artifact names without applying normal project-discovery ignore policy.
///
/// Hidden and gitignored projects remain visible because recovery must follow durable ownership
/// evidence rather than the current discovery configuration.
/// Known metadata, dependency, build, and cache trees are pruned to keep a repository-root safety
/// scan bounded.
/// Unreadable entries are skipped rather than failing the scan, matching the marker walks the same
/// discovery pairs this with: one unreadable subtree (a root-owned volume mount) must not block
/// recovering every project the walk *can* see, and recovery inside a subtree this process cannot
/// read could not have proceeded anyway. The skip is only partially fail-closed, though: an
/// interrupted project behind the unreadable subtree is rediscovered through its recovery
/// authority anchor only when it shares the scan root's git common directory — a nested git
/// repository or submodule inside that subtree anchors under its own git directory, and orphan
/// artifacts with no anchor at all have nothing outside the subtree pointing at them. Both shapes
/// escape silently, so every skipped path is returned as a warning for the caller to surface.
pub(crate) fn find_recovery_artifact_dirs(
    root: &Utf8Path,
    is_artifact: impl Fn(&str) -> bool,
) -> RecoveryArtifactScan {
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
    let mut skipped_unreadable = Vec::new();
    for result in builder.build() {
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                // The walker's error display names the offending path when it is known; keep the
                // scan root as the fallback context for the rare pathless failure.
                skipped_unreadable.push(format!(
                    "recovery artifact scan under {root} skipped an unreadable path: {error}"
                ));
                continue;
            }
        };
        let Some(name) = entry.file_name().to_str() else {
            continue;
        };
        if is_artifact(name)
            && let Some(parent) = entry.path().parent().and_then(Utf8Path::from_path)
        {
            dirs.push(parent.to_owned());
        }
    }
    dirs.sort();
    dirs.dedup();
    RecoveryArtifactScan {
        dirs,
        skipped_unreadable,
    }
}

/// The outcome of [`find_recovery_artifact_dirs`]: the artifact-owning directories plus the
/// unreadable paths the walk had to skip (see the function's fail-closed caveats).
pub(crate) struct RecoveryArtifactScan {
    /// Directories that directly contain a reserved recovery artifact name.
    pub(crate) dirs: Vec<Utf8PathBuf>,
    /// One preformatted message per skipped unreadable path; the caller surfaces them (recovery
    /// reports them as user-visible warnings) rather than losing them to a debug trace.
    pub(crate) skipped_unreadable: Vec<String>,
}

fn present_markers<'a>(dir: &Utf8Path, markers: &BTreeSet<&'a str>) -> BTreeSet<&'a str> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::debug!(%error, %dir, "skipping unreadable directory markers");
            return BTreeSet::new();
        }
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let marker = markers.get(name).copied()?;
            let kind = entry.file_type().ok()?;
            if kind.is_file()
                || (kind.is_symlink()
                    && std::fs::metadata(entry.path()).is_ok_and(|metadata| metadata.is_file()))
            {
                Some(marker)
            } else {
                None
            }
        })
        .collect()
}

/// Whether `path` (a directory) is excluded, matching its path relative to `root` against the
/// folder globset.
/// Matching the relative path (rather than the bare name) is what lets a leading
/// slash anchor to the root: a bare name still prunes at any depth because
/// [`compile_folder_globset`] gives it the `**/` variant.
/// Whether `path` names a dot-directory (`.git`, `.venv`).
fn is_hidden(path: &Utf8Path) -> bool {
    path.file_name().is_some_and(|name| name.starts_with('.'))
}

/// Split the set into topmost directories and those with an ancestor already in the set (sorted
/// input puts ancestors first).
fn split_topmost(dirs: Vec<Utf8PathBuf>) -> (Vec<Utf8PathBuf>, Vec<Utf8PathBuf>) {
    let mut kept: Vec<Utf8PathBuf> = Vec::new();
    let mut nested: Vec<Utf8PathBuf> = Vec::new();
    for dir in dirs {
        if kept.iter().any(|root| dir.starts_with(root)) {
            nested.push(dir);
        } else {
            kept.push(dir);
        }
    }
    (kept, nested)
}

/// `exclude-folders` compiled for filtering workspace members by *location*.
/// A member is excluded
/// when its path — or any ancestor, so `packages/ts/luup` also excludes `packages/ts/luup/api` —
/// matches a folder glob (`.gitignore` semantics; see [`compile_folder_globset`]).
#[derive(Debug, Clone)]
pub(crate) struct FolderExcludeSet(GlobSet);

impl Default for FolderExcludeSet {
    /// The empty set, which excludes nothing.
    fn default() -> Self {
        Self(GlobSet::empty())
    }
}

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
    /// Directories at or above `lifted` never count: the invocation named that directory, which
    /// outranks a glob, and the scan walk lifted the same globs to reach it.
    #[must_use]
    pub(crate) fn excludes_path(&self, path: &Utf8Path, lifted: Option<&Utf8Path>) -> bool {
        if self.0.is_empty() {
            return false;
        }
        path.ancestors()
            .take_while(|ancestor| !lifted.is_some_and(|lifted| lifted.starts_with(ancestor)))
            .any(|ancestor| {
                !ancestor.as_str().is_empty() && self.0.is_match(ancestor.as_std_path())
            })
    }
}

/// `exclude-packages` compiled for filtering workspace members by *package name*.
/// A scoped glob like
/// `@luup/*` excludes every `@luup/...` member regardless of where it lives in the tree (see
/// [`compile_package_globset`]).
#[derive(Debug, Clone)]
pub(crate) struct PackageExcludeSet(GlobSet);

impl Default for PackageExcludeSet {
    /// The empty set, which excludes nothing.
    fn default() -> Self {
        Self(GlobSet::empty())
    }
}

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
        assert!(set.excludes_path(Utf8Path::new("packages/ts/luup/api"), None));
        assert!(set.excludes_path(Utf8Path::new("packages/ts/luup"), None));
        // A sibling path is kept.
        assert!(!set.excludes_path(Utf8Path::new("apps/admin"), None));
        // The root importer (`.`) is never matched by a sub-path exclude.
        assert!(!set.excludes_path(Utf8Path::new("."), None));
    }

    /// Selecting a directory lifts the globs at and above it, but not below it.
    #[test]
    fn folder_exclude_set_lifts_the_selection_and_its_ancestors() {
        let set = FolderExcludeSet::compile(&["incubator".to_string(), "lab".to_string()])
            .expect("compile");
        let selected = Some(Utf8Path::new("incubator"));
        // The selection itself, and a member below it whose only excluded ancestor is the
        // selection, are kept.
        assert!(!set.excludes_path(Utf8Path::new("incubator"), selected));
        assert!(!set.excludes_path(Utf8Path::new("incubator/tools"), selected));
        // A glob matching below the selection still applies.
        assert!(set.excludes_path(Utf8Path::new("incubator/lab"), selected));
        assert!(set.excludes_path(Utf8Path::new("incubator/lab/api"), selected));
        // A member above the selection (the owner) is never excluded, whatever matches it.
        assert!(!set.excludes_path(
            Utf8Path::new("incubator"),
            Some(Utf8Path::new("incubator/lab"))
        ));
        assert!(!set.excludes_path(Utf8Path::new(""), selected));
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
                .excludes_path(Utf8Path::new("apps/admin"), None)
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
    fn project_scan_carries_nested_lockfile_roots_for_appeal() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir_all(root.join("incubator"))?;
        std::fs::write(root.join("Cargo.lock"), "")?;
        std::fs::write(root.join("Cargo.toml"), "")?;
        std::fs::write(root.join("incubator/Cargo.lock"), "")?;
        std::fs::write(root.join("incubator/Cargo.toml"), "")?;
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

        assert_eq!(found.primary, vec![root.clone()]);
        assert_eq!(found.nested, vec![root.join("incubator")]);
        Ok(())
    }

    #[test]
    fn project_scan_batches_distinct_adapter_markers() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::create_dir_all(root.join("rust"))?;
        std::fs::create_dir_all(root.join("go/service"))?;
        std::fs::write(root.join("rust/Cargo.lock"), "")?;
        std::fs::write(root.join("rust/Cargo.toml"), "")?;
        std::fs::write(root.join("go/go.mod"), "")?;
        std::fs::write(root.join("go/service/go.mod"), "")?;
        let detections = [
            ProjectDetection::PrimaryWithValidation {
                primary: cooldown_core::ProjectMarker {
                    lockfile: "Cargo.lock",
                    manifest: "Cargo.toml",
                    alternate_manifests: &[],
                    workspace_root: true,
                },
                validation_marker: "Cargo.toml",
            },
            ProjectDetection::Primary(cooldown_core::ProjectMarker {
                lockfile: "go.mod",
                manifest: "go.mod",
                alternate_manifests: &[],
                workspace_root: false,
            }),
        ];

        let found = find_project_marker_dirs_batch(
            &root,
            &detections,
            WalkPolicy {
                respect_gitignore: false,
                exclude: &[],
                selected: None,
            },
        )?;

        assert_eq!(found[0].primary, vec![root.join("rust")]);
        assert!(found[0].validation_only.is_empty());
        assert_eq!(
            found[1].primary,
            vec![root.join("go"), root.join("go/service")]
        );
        assert!(found[1].validation_only.is_empty());
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
    fn primary_root_does_not_hide_a_nested_validation_marker() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let nested = root.join("tools/custom");
        std::fs::create_dir_all(nested.join(".cargo"))?;
        std::fs::write(root.join("Cargo.lock"), "")?;
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

        assert_eq!(found.primary, vec![root]);
        assert_eq!(found.validation_only, vec![nested]);
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

    fn cargo_detection() -> ProjectDetection {
        ProjectDetection::PrimaryWithValidation {
            primary: cooldown_core::ProjectMarker {
                lockfile: "Cargo.lock",
                manifest: "Cargo.toml",
                alternate_manifests: &[],
                workspace_root: true,
            },
            validation_marker: "Cargo.toml",
        }
    }

    fn policy<'a>(exclude: &'a [String], selected: Option<&'a Utf8Path>) -> WalkPolicy<'a> {
        WalkPolicy {
            respect_gitignore: false,
            exclude,
            selected,
        }
    }

    /// The scenario behind `cooldown -C incubator …`: the repo root excludes `incubator`, so the
    /// default scan never enters it.
    /// Naming it lifts the prune for that path (and the ancestors leading to it) — the nested
    /// lockfile root is found and carried for the adapter's nested-workspace appeal — while the
    /// same glob still prunes everywhere else, including below the selection.
    #[test]
    fn explicitly_selected_directory_is_not_pruned_by_exclude_folders() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = utf8(tmp.path());
        touch(&root.join("Cargo.lock"));
        touch(&root.join("Cargo.toml"));
        touch(&root.join("labs/incubator/Cargo.lock"));
        touch(&root.join("labs/incubator/Cargo.toml"));
        touch(&root.join("labs/incubator/vendor/incubator/Cargo.lock"));
        touch(&root.join("other/incubator/Cargo.lock"));
        let excludes = vec!["incubator".to_string()];

        let pruned =
            find_project_marker_dirs_batch(&root, &[cargo_detection()], policy(&excludes, None))?
                .remove(0);
        assert_eq!(pruned.primary, vec![root.clone()]);
        assert!(
            pruned.nested.is_empty(),
            "the default scan prunes every `incubator`"
        );

        let selected = root.join("labs/incubator");
        let found = find_project_marker_dirs_batch(
            &root,
            &[cargo_detection()],
            policy(&excludes, Some(&selected)),
        )?
        .remove(0);
        assert_eq!(found.primary, vec![root.clone()]);
        assert_eq!(
            found.nested,
            vec![selected],
            "only the selected `incubator` is entered; the sibling and the nested one stay pruned"
        );
        Ok(())
    }

    fn uv_detection() -> ProjectDetection {
        ProjectDetection::PrimaryWithValidation {
            primary: cooldown_core::ProjectMarker {
                lockfile: "uv.lock",
                manifest: "pyproject.toml",
                alternate_manifests: &[],
                workspace_root: false,
            },
            validation_marker: "pyproject.toml",
        }
    }

    /// Lifting an excluded ancestor opens only the path to the selection and the selection's own
    /// subtree: the ancestor's other children stay pruned, so a broken sibling can never fail a
    /// run that never asked for it.
    /// A selection outside the root lifts nothing.
    #[test]
    fn a_lifted_ancestor_admits_only_the_selected_path() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = utf8(tmp.path());
        touch(&root.join("apps/web/uv.lock"));
        touch(&root.join("apps/web/tools/uv.lock"));
        touch(&root.join("apps/api/uv.lock"));
        let excludes = vec!["apps".to_string()];

        let scan = |selected: Option<&Utf8Path>| -> eyre::Result<Vec<Utf8PathBuf>> {
            Ok(find_project_marker_dirs_batch(
                &root,
                &[uv_detection()],
                policy(&excludes, selected),
            )?
            .remove(0)
            .primary)
        };

        assert!(scan(None)?.is_empty());
        assert_eq!(
            scan(Some(&root.join("apps/web")))?,
            vec![root.join("apps/web"), root.join("apps/web/tools")],
            "the selection and what lies below it are found; the sibling `apps/api` is not"
        );
        assert!(scan(Some(Utf8Path::new("/elsewhere")))?.is_empty());
        Ok(())
    }

    /// A dot-directory is never scanned by default, but naming one (or a path through one) enters
    /// it like any other selection.
    #[test]
    fn a_hidden_selection_is_entered() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = utf8(tmp.path());
        touch(&root.join("uv.lock"));
        touch(&root.join(".scratch/proj/uv.lock"));
        touch(&root.join(".other/uv.lock"));

        let found =
            find_project_marker_dirs_batch(&root, &[uv_detection()], policy(&[], None))?.remove(0);
        assert_eq!(found.primary, vec![root.clone()]);

        let selected = root.join(".scratch/proj");
        let found =
            find_project_marker_dirs_batch(&root, &[uv_detection()], policy(&[], Some(&selected)))?
                .remove(0);
        assert_eq!(found.primary, vec![root.clone(), selected]);
        Ok(())
    }

    /// Gitignore rules are not lifted, but a selection they hide is an error naming the escape
    /// hatch rather than an empty scan the run would report as clean.
    #[test]
    fn a_gitignored_selection_is_an_error() -> eyre::Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = utf8(tmp.path());
        std::fs::create_dir(root.join(".git"))?;
        std::fs::write(root.join(".gitignore"), "vendor/\n")?;
        touch(&root.join("uv.lock"));
        touch(&root.join("vendor/proj/uv.lock"));
        let selected = root.join("vendor/proj");

        let error = find_project_marker_dirs_batch(
            &root,
            &[uv_detection()],
            WalkPolicy {
                respect_gitignore: true,
                exclude: &[],
                selected: Some(&selected),
            },
        )
        .expect_err("a gitignored selection must not scan to nothing");
        assert!(
            error.to_string().contains("--no-gitignore"),
            "the error names the escape hatch: {error}"
        );

        let found = find_project_marker_dirs_batch(
            &root,
            &[uv_detection()],
            WalkPolicy {
                respect_gitignore: false,
                exclude: &[],
                selected: Some(&selected),
            },
        )?
        .remove(0);
        assert_eq!(found.primary, vec![root.clone(), selected]);
        Ok(())
    }

    #[test]
    fn exclude_with_trailing_slash_matches_like_bare_name() {
        let tmp = tempfile::tempdir().expect("tmp");
        let root = utf8(tmp.path());
        touch(&root.join("uv.lock"));
        touch(&root.join("examples/dep/uv.lock"));
        touch(&root.join("nested/examples/uv.lock"));

        // `"examples/"` is the natural directory-exclude idiom; the trailing slash must not change
        // its meaning.
        // Like the bare name, it prunes `examples/` at any depth.
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
        // search.
        // Neither should make the project disappear because the marker is tested per directory.
        std::fs::write(root.join(".gitignore"), "Cargo.lock\n").expect("gitignore");
        std::fs::write(root.join(".ignore"), "**/*.lock\n").expect("rgignore");

        let found = find_marker_dirs(&root, "Cargo.lock", true, &[], true).expect("scan");
        assert_eq!(
            found,
            vec![root],
            "a file-level-ignored lockfile is still a project"
        );
    }

    #[cfg(unix)]
    #[test]
    fn dangling_lock_symlink_does_not_mark_a_project() -> eyre::Result<()> {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf())
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(root.join("Cargo.toml"), "[workspace]\n")?;
        symlink("missing.lock", root.join("Cargo.lock"))?;

        let found = find_marker_dirs(&root, "Cargo.lock", false, &[], true)?;

        assert!(found.is_empty());
        Ok(())
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
            "ignored/project/.recovery.123.0.publish",
            ".hidden/project/recovery",
            "target/fixture/.recovery.123.0.publish",
        ] {
            let marker = root.join(marker);
            std::fs::create_dir_all(
                marker
                    .parent()
                    .ok_or_else(|| eyre::eyre!("recovery marker has no parent"))?,
            )?;
            std::fs::write(marker, "")?;
        }

        let found = find_recovery_artifact_dirs(root, |name| {
            matches!(name, "recovery" | ".recovery.123.0.publish")
        });
        assert_eq!(
            found.dirs,
            vec![root.join(".hidden/project"), root.join("ignored/project")]
        );
        assert!(found.skipped_unreadable.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn recovery_scan_skips_an_unreadable_subtree_instead_of_failing() -> eyre::Result<()> {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir()?;
        let root = Utf8Path::from_path(tmp.path())
            .ok_or_else(|| eyre::eyre!("temporary path is not UTF-8"))?;
        let project = root.join("project");
        // Not in the pruned-name list — e.g. a root-owned `data/` volume mount inside the repo.
        let sealed = root.join("data");
        std::fs::create_dir_all(&project)?;
        std::fs::create_dir_all(&sealed)?;
        std::fs::write(project.join("recovery"), "")?;
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o000))?;

        let result = find_recovery_artifact_dirs(root, |name| name == "recovery");
        std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o755))?;

        assert_eq!(
            result.dirs,
            vec![project],
            "the interrupted project the walk can see is still recovered"
        );
        // The skip is not fully fail-closed (a nested repository or orphan artifacts behind the
        // unreadable subtree escape silently), so it must surface as a warning, not a debug trace.
        let warning = result
            .skipped_unreadable
            .first()
            .ok_or_else(|| eyre::eyre!("the unreadable subtree must be reported"))?;
        assert!(
            warning.contains(sealed.as_str()),
            "the warning names the unreadable path, got: {warning}"
        );
        Ok(())
    }
}
