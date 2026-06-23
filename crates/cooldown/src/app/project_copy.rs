//! Throwaway recursive copies of a project tree, used by the non-mutating resolve probes.
//!
//! Both `outdated`'s blocked-verification (`resolve_held`) and the dependency-mutating `--dry-run`
//! preview need to run the real resolver against a project without touching its real
//! `uv.lock`/`pyproject.toml`. They copy the project into a temp directory, run the mutating apply
//! there, and discard the copy — so a single implementation of the copy lives here and is shared by
//! both paths.

use cooldown_core::{CoreError, Project, ResolveInputs};

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
    /// Copy only the files the resolver reads (`inputs`) from `project`'s tree into a fresh temp
    /// directory and return a [`Project`] rooted there, with its manifest path rebased onto the copy.
    ///
    /// Crucially this copies ONLY manifests, lockfiles, workspace/registry config, and (for Cargo/Go)
    /// source — never the full source/data tree. A blind recursive copy is catastrophic in a large
    /// monorepo: it would duplicate gigabytes of assets, model weights, and build data into the
    /// tempdir (often tmpfs/RAM), which the resolver never reads. The real tree is only read (never
    /// written), and the copy is discarded when the returned value drops.
    pub(crate) fn create(project: &Project, inputs: &ResolveInputs) -> cooldown_core::Result<Self> {
        let scratch = tempfile::tempdir()?;
        let scratch_root = camino::Utf8Path::from_path(scratch.path()).ok_or_else(|| {
            CoreError::PathEncoding("temp dir path is not valid utf-8".to_string())
        })?;
        copy_project_tree(project.root.as_std_path(), scratch.path(), inputs)?;

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

/// Recursively reproduce a project's directory *skeleton* into `dest`, copying ONLY the files the
/// resolver reads (`inputs`): manifests, lockfiles, workspace/registry config, and — for Cargo/Go —
/// source. Every other file (application source, assets, model weights, build data) is skipped, so the
/// throwaway copy is a few megabytes of metadata even for a multi-gigabyte monorepo. The directory
/// structure itself IS preserved (workspace members are located by path), but the bulk content is not.
///
/// Directories that the resolver never needs and that would make even a skeleton walk expensive or
/// self-referential are pruned outright: virtualenvs, VCS metadata, bytecode caches, `node_modules`,
/// build `target`, `vendor`/`testdata`, plus any dotfile-prefixed (`.git`, `.jj`, `.venv`, …) or
/// underscore-prefixed (`_data`, `_cache`, …) directory — the same set Go's own package discovery
/// skips. This keeps the walk off non-source trees that may be huge or unreadable (e.g. a `_data`
/// Docker volume owned by another user). Any entry that is unreadable (`PermissionDenied`) is skipped
/// quietly instead of failing the whole probe.
fn copy_project_tree(
    src: &std::path::Path,
    dest: &std::path::Path,
    inputs: &ResolveInputs,
) -> std::io::Result<()> {
    const SKIP_DIRS: &[&str] = &[
        ".venv",
        "venv",
        ".git",
        "__pycache__",
        "node_modules",
        "target",
        "vendor",
        "testdata",
    ];
    std::fs::create_dir_all(dest)?;
    let entries = match std::fs::read_dir(src) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            tracing::debug!(path = %src.display(), "skipping unreadable path while staging project copy");
            return Ok(());
        }
        Err(err) => return Err(err),
    };
    for entry in entries {
        let entry = entry?;
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                tracing::debug!(path = %entry.path().display(), "skipping unreadable path while staging project copy");
                continue;
            }
            Err(err) => return Err(err),
        };
        let name = entry.file_name();
        let from = entry.path();
        let to = dest.join(&name);
        if file_type.is_dir() {
            // Prune the resolver-irrelevant set: named skip dirs plus any dotfile/underscore-prefixed
            // directory (the latter is where container-owned `_data` volumes live).
            let pruned = name
                .to_str()
                .is_none_or(|n| SKIP_DIRS.contains(&n) || n.starts_with('.') || n.starts_with('_'));
            if pruned {
                continue;
            }
            copy_project_tree(&from, &to, inputs)?;
        } else if file_type.is_file() {
            // Copy ONLY resolver inputs — never the full source/data tree.
            if !is_resolver_input(&name, inputs) {
                continue;
            }
            match std::fs::copy(&from, &to) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                    tracing::debug!(path = %from.display(), "skipping unreadable path while staging project copy");
                }
                Err(err) => {
                    return Err(std::io::Error::new(
                        err.kind(),
                        format!("{}: {err}", from.display()),
                    ));
                }
            }
        }
        // Symlinks and other special entries are irrelevant to resolution and are skipped.
    }
    Ok(())
}

/// Whether a file is one the resolver reads: an exact manifest/lock/config basename, or a source file
/// whose extension a resolver validates against (`rs`, `go`). Everything else is skipped.
fn is_resolver_input(name: &std::ffi::OsStr, inputs: &ResolveInputs) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if inputs.filenames.contains(&name) {
        return true;
    }
    if let Some((_, extension)) = name.rsplit_once('.') {
        return inputs.source_extensions.contains(&extension);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::copy_project_tree;
    use cooldown_core::ResolveInputs;

    /// The skeleton copy reproduces the directory structure and the resolver inputs (manifests, locks,
    /// config) but NEVER the bulk source/data tree — the guarantee that keeps the throwaway probe cheap
    /// in a multi-gigabyte monorepo instead of cloning it into a tempdir.
    #[test]
    fn copies_only_resolver_inputs_not_the_full_tree() {
        let src = tempfile::tempdir().expect("src");
        let dest = tempfile::tempdir().expect("dest");
        let s = src.path();

        // Manifests / locks / config in a nested workspace — must be copied (the skeleton).
        std::fs::write(s.join("package.json"), "{}").expect("write");
        std::fs::write(s.join("pnpm-lock.yaml"), "lockfileVersion: '9.0'\n").expect("write");
        std::fs::write(s.join(".npmrc"), "registry=...\n").expect("write");
        std::fs::create_dir_all(s.join("packages/app")).expect("mkdir");
        std::fs::write(s.join("packages/app/package.json"), "{}").expect("write");

        // Bulk content — must NOT be copied.
        std::fs::write(s.join("packages/app/index.ts"), "export const x = 1;").expect("write");
        std::fs::write(s.join("model.bin"), vec![0u8; 4096]).expect("write");
        std::fs::write(s.join("README.md"), "# readme").expect("write");

        // Pruned dirs — never descended into.
        std::fs::create_dir_all(s.join("node_modules/foo")).expect("mkdir");
        std::fs::write(s.join("node_modules/foo/package.json"), "{}").expect("write");
        std::fs::create_dir_all(s.join("_data/vol")).expect("mkdir");
        std::fs::write(s.join("_data/vol/blob"), vec![0u8; 4096]).expect("write");

        copy_project_tree(s, dest.path(), &ResolveInputs::DEFAULT).expect("copy");
        let d = dest.path();

        // The skeleton: structure + resolver inputs are present.
        assert!(d.join("package.json").exists(), "root manifest copied");
        assert!(d.join("pnpm-lock.yaml").exists(), "lockfile copied");
        assert!(d.join(".npmrc").exists(), "registry config copied");
        assert!(
            d.join("packages/app/package.json").exists(),
            "member manifest copied (structure preserved)"
        );

        // The bulk: source, data, docs are skipped.
        assert!(
            !d.join("packages/app/index.ts").exists(),
            "source not copied"
        );
        assert!(!d.join("model.bin").exists(), "data blob not copied");
        assert!(!d.join("README.md").exists(), "docs not copied");

        // Pruned dirs are not reproduced at all.
        assert!(!d.join("node_modules").exists(), "node_modules pruned");
        assert!(!d.join("_data").exists(), "underscore data dir pruned");
    }

    /// `source_extensions` is opt-in per tool: Cargo/Go include their source (their resolve validates
    /// targets/imports against it), the declaration-only default does not.
    #[test]
    fn source_extensions_are_opt_in_per_tool() {
        let src = tempfile::tempdir().expect("src");
        let s = src.path();
        std::fs::write(s.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("write");
        std::fs::create_dir_all(s.join("src")).expect("mkdir");
        std::fs::write(s.join("src/lib.rs"), "").expect("write");

        // Cargo opts into `.rs`, so its source is copied.
        let cargo_inputs = ResolveInputs {
            source_extensions: &["rs"],
            ..ResolveInputs::DEFAULT
        };
        let dest = tempfile::tempdir().expect("dest");
        copy_project_tree(s, dest.path(), &cargo_inputs).expect("copy");
        assert!(dest.path().join("Cargo.toml").exists());
        assert!(
            dest.path().join("src/lib.rs").exists(),
            "cargo copies .rs source so the resolve sees crate targets"
        );

        // The default (no source extensions) skips it.
        let dest_default = tempfile::tempdir().expect("dest");
        copy_project_tree(s, dest_default.path(), &ResolveInputs::DEFAULT).expect("copy");
        assert!(
            !dest_default.path().join("src/lib.rs").exists(),
            "the declaration-only default never copies source"
        );
    }
}
