//! Throwaway recursive copies of a project tree, used by the non-mutating resolve probes.
//!
//! Both `outdated`'s blocked-verification (`resolve_held`) and the dependency-mutating `--dry-run`
//! preview need to run the real resolver against a project without touching its real
//! `uv.lock`/`pyproject.toml`. They copy the project into a temp directory, run the mutating apply
//! there, and discard the copy — so a single implementation of the copy lives here and is shared by
//! both paths.

use cooldown_core::{CoreError, Project};

/// A project copied into a throwaway temp directory. The copy is deleted when this value drops, so
/// the real project tree is never read for mutation or written. Hold it for as long as the copied
/// [`Project`] is in use.
pub(crate) struct ProjectCopy {
    /// Kept so the temp directory lives as long as the copy; dropping it removes the tree.
    _scratch: tempfile::TempDir,
    /// The copied project, rooted inside the temp directory.
    pub(crate) project: Project,
}

impl ProjectCopy {
    /// Recursively copy `project`'s tree into a fresh temp directory and return a [`Project`] rooted
    /// there, with its manifest path rebased onto the copy. The real tree is only read (never
    /// written), and the copy is discarded when the returned value drops.
    pub(crate) fn create(project: &Project) -> cooldown_core::Result<Self> {
        let scratch = tempfile::tempdir()?;
        let scratch_root = camino::Utf8Path::from_path(scratch.path()).ok_or_else(|| {
            CoreError::PathEncoding("temp dir path is not valid utf-8".to_string())
        })?;
        copy_project_tree(project.root.as_std_path(), scratch.path())?;

        let manifest_rel = project
            .manifest
            .strip_prefix(&project.root)
            .unwrap_or(&project.manifest);
        let copied = Project {
            root: scratch_root.to_owned(),
            kind: project.kind,
            manifest: scratch_root.join(manifest_rel),
            exclude_newer: project.exclude_newer.clone(),
        };
        Ok(ProjectCopy {
            _scratch: scratch,
            project: copied,
        })
    }
}

/// Recursively copy a project tree into `dest`, skipping directories that the resolver never needs and
/// that would make the copy expensive or self-referential — virtualenvs, VCS metadata, and bytecode
/// caches. The resolver reads only the manifests/lock and resolves dependency metadata from its own
/// global cache, so omitting these is safe and keeps the throwaway copy cheap.
fn copy_project_tree(src: &std::path::Path, dest: &std::path::Path) -> std::io::Result<()> {
    const SKIP_DIRS: &[&str] = &[
        ".venv",
        "venv",
        ".git",
        "__pycache__",
        "node_modules",
        "target",
    ];
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let from = entry.path();
        let to = dest.join(&name);
        if file_type.is_dir() {
            if name.to_str().is_some_and(|n| SKIP_DIRS.contains(&n)) {
                continue;
            }
            copy_project_tree(&from, &to)?;
        } else if file_type.is_file() {
            std::fs::copy(&from, &to)?;
        }
        // Symlinks and other special entries are irrelevant to resolution and are skipped.
    }
    Ok(())
}
