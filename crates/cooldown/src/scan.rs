//! Gitignore-aware project-root discovery shared by the tool adapters' `detect`.
//!
//! Each adapter looks for its own marker file (`Cargo.lock`, `go.mod`, `uv.lock`), but the walk is
//! identical: descend from a root, skip what shouldn't be scanned, and collect the directories that
//! hold the marker. Centralizing it here means `.gitignore` handling, the exclude list, and the
//! workspace-root rule are implemented (and tested) once.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::CoreError;
use globset::{Glob, GlobSet, GlobSetBuilder};
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
/// - `exclude`: extra directory globs that are never scanned, in addition to gitignore. A bare name
///   (`"target"`) excludes that directory at any depth; an entry with a `/` is a path glob relative
///   to `root` (`"third_party/grammars"`). `**` is supported.
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
    let excludes = build_globset(exclude)?;
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

    let mut dirs = Vec::new();
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
            && dir.join(marker).is_file()
        {
            dirs.push(dir.to_owned());
        }
    }

    dirs.sort();
    dirs.dedup();
    if topmost_only {
        dirs = keep_topmost(dirs);
    }
    Ok(dirs)
}

/// Whether `path` (a directory) is excluded by name (matched at any depth) or by its path relative
/// to `root`.
fn is_excluded(path: &std::path::Path, root: &Utf8Path, excludes: &GlobSet) -> bool {
    if excludes.is_empty() {
        return false;
    }
    let name_excluded = path
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|name| excludes.is_match(name));
    let rel_excluded = Utf8Path::from_path(path)
        .and_then(|p| p.strip_prefix(root).ok())
        .is_some_and(|rel| !rel.as_str().is_empty() && excludes.is_match(rel.as_std_path()));
    name_excluded || rel_excluded
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

/// Build a [`GlobSet`] from the exclude patterns. A bare name also matches that directory at any
/// depth (so `"target"` excludes every `target/`, not just one at the root).
fn build_globset(patterns: &[String]) -> Result<GlobSet, CoreError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let trimmed = pat.trim();
        if trimmed.is_empty() {
            continue;
        }
        builder.add(compile_glob(trimmed)?);
        if !trimmed.contains('/') {
            builder.add(compile_glob(&format!("**/{trimmed}"))?);
        }
    }
    builder
        .build()
        .map_err(|e| CoreError::Config(format!("invalid exclude set: {e}")))
}

fn compile_glob(pattern: &str) -> Result<Glob, CoreError> {
    Glob::new(pattern)
        .map_err(|e| CoreError::Config(format!("invalid exclude glob {pattern:?}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utf8(p: &std::path::Path) -> Utf8PathBuf {
        Utf8PathBuf::from_path_buf(p.to_path_buf()).expect("utf8 path")
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
        assert!(matches!(err, CoreError::Config(_)));
    }
}
