//! Throwaway recursive copies of a project tree, used by the non-mutating resolve probes.
//!
//! Both `outdated`'s policy-complete upgrade preview and the dependency-mutating `--dry-run` need to
//! run the real resolver against a project without touching its real lockfiles or manifests.
//! They copy the project into a temp directory, run the mutating policy flow there, and discard the
//! copy — so a single implementation of the copy lives here and is shared by both paths.

use cooldown_core::{CoreError, Project, ResolveInputs};

const SKIP_DIRS: &[&str] = &[
    ".venv",
    "venv",
    ".git",
    ".jj",
    ".hg",
    ".svn",
    "__pycache__",
    "node_modules",
    "target",
    "vendor",
    "testdata",
];

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
    /// directory and return a [`Project`] with a canonical root and its manifest rebased onto the
    /// copy.
    ///
    /// Crucially this copies ONLY manifests, lockfiles, workspace/registry config, and (for Cargo/Go)
    /// source — never the full source/data tree. A blind recursive copy is catastrophic in a large
    /// monorepo: it would duplicate gigabytes of assets, model weights, and build data into the
    /// tempdir (often tmpfs/RAM), which the resolver never reads. The real tree is only read (never
    /// written), and the copy is discarded when the returned value drops.
    pub(crate) fn create(project: &Project, inputs: &ResolveInputs) -> cooldown_core::Result<Self> {
        let manifest_rel = project.manifest.strip_prefix(&project.root).map_err(|_| {
            CoreError::System(format!(
                "project manifest {} is outside project root {}",
                project.manifest, project.root
            ))
        })?;
        let scratch = tempfile::tempdir()?;
        let scratch_path = std::fs::canonicalize(scratch.path())?;
        let scratch_root = camino::Utf8PathBuf::from_path_buf(scratch_path).map_err(|path| {
            CoreError::PathEncoding(format!(
                "temp dir path is not valid UTF-8: {}",
                path.display()
            ))
        })?;
        copy_project_tree(
            project.root.as_std_path(),
            scratch_root.as_std_path(),
            inputs,
        )?;

        let copied_manifest = scratch_root.join(manifest_rel);
        let copied = Project {
            root: scratch_root,
            kind: project.kind,
            manifest: copied_manifest,
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
/// build `target`, and vendored/testdata trees. Dot-prefixed config directories are traversed only
/// when they are ancestors of explicit [`ResolveInputs::path_prefixes`] entries such as
/// `.cargo/config.toml`; underscore-prefixed source directories are not pruned generically because
/// they are legitimate module/package locations in several ecosystems. Any entry that is unreadable
/// (`PermissionDenied`) is skipped quietly instead of failing the whole probe.
fn copy_project_tree(
    src: &std::path::Path,
    dest: &std::path::Path,
    inputs: &ResolveInputs,
) -> std::io::Result<()> {
    copy_project_tree_inner(src, dest, std::path::Path::new(""), inputs)
}

fn copy_project_tree_inner(
    src: &std::path::Path,
    dest: &std::path::Path,
    rel: &std::path::Path,
    inputs: &ResolveInputs,
) -> std::io::Result<()> {
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
        let child_rel = rel.join(&name);
        if file_type.is_dir() {
            if should_prune_dir(&name, &child_rel, inputs) {
                continue;
            }
            copy_project_tree_inner(&from, &to, &child_rel, inputs)?;
        } else if file_type.is_file() {
            // Copy ONLY resolver inputs — never the full source/data tree.
            if !is_resolver_input(&child_rel, &name, inputs) {
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

fn should_prune_dir(name: &std::ffi::OsStr, rel: &std::path::Path, inputs: &ResolveInputs) -> bool {
    let Some(name) = name.to_str() else {
        return true;
    };
    if SKIP_DIRS.contains(&name) {
        return true;
    }
    if rel_is_under_dot_dir(rel) {
        return !path_prefix_relevant(rel, inputs);
    }
    false
}

/// Whether a file is one the resolver reads: an exact manifest/lock/config basename, or a source file
/// whose extension a resolver validates against (`rs`, `go`). Everything else is skipped.
fn is_resolver_input(
    rel: &std::path::Path,
    name: &std::ffi::OsStr,
    inputs: &ResolveInputs,
) -> bool {
    if path_prefix_matches(rel, inputs) {
        return true;
    }
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

fn rel_is_under_dot_dir(rel: &std::path::Path) -> bool {
    rel.components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .is_some_and(|name| name.starts_with('.'))
}

fn path_prefix_relevant(rel: &std::path::Path, inputs: &ResolveInputs) -> bool {
    let Some(rel) = rel_slash(rel) else {
        return false;
    };
    inputs.path_prefixes.iter().any(|prefix| {
        rel == *prefix
            || rel
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
            || prefix
                .strip_prefix(rel.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn path_prefix_matches(rel: &std::path::Path, inputs: &ResolveInputs) -> bool {
    let Some(rel) = rel_slash(rel) else {
        return false;
    };
    inputs.path_prefixes.iter().any(|prefix| {
        rel == *prefix
            || rel
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn rel_slash(rel: &std::path::Path) -> Option<String> {
    Some(
        rel.to_str()?
            .trim_start_matches(std::path::MAIN_SEPARATOR)
            .replace(std::path::MAIN_SEPARATOR, "/"),
    )
}

#[cfg(test)]
mod tests {
    use super::{ProjectCopy, copy_project_tree};
    use cooldown_core::{Project, ResolveInputs, ToolId};

    #[test]
    fn copied_project_uses_a_canonical_root() {
        let src = tempfile::tempdir().expect("src");
        std::fs::write(src.path().join("package.json"), "{}").expect("write manifest");
        let root = camino::Utf8PathBuf::from_path_buf(src.path().to_path_buf())
            .expect("UTF-8 source path");
        let project = Project {
            root: root.clone(),
            kind: ToolId("test"),
            manifest: root.join("package.json"),
            exclude_newer: None,
        };

        let copy = ProjectCopy::create(&project, &ResolveInputs::DEFAULT).expect("copy project");
        let canonical_root = std::fs::canonicalize(copy.project.root.as_std_path())
            .expect("canonicalize copied root");

        assert_eq!(copy.project.root.as_std_path(), canonical_root.as_path());
        assert_eq!(
            copy.project.manifest,
            copy.project.root.join("package.json")
        );
        assert!(copy.project.manifest.exists());
    }

    #[test]
    fn copied_project_rejects_a_manifest_outside_its_root() {
        let src = tempfile::tempdir().expect("src");
        let outside = tempfile::tempdir().expect("outside");
        let root = camino::Utf8PathBuf::from_path_buf(src.path().to_path_buf())
            .expect("UTF-8 source path");
        let manifest = camino::Utf8PathBuf::from_path_buf(outside.path().join("package.json"))
            .expect("UTF-8 manifest path");
        let project = Project {
            root,
            kind: ToolId("test"),
            manifest,
            exclude_newer: None,
        };

        let error = ProjectCopy::create(&project, &ResolveInputs::DEFAULT)
            .err()
            .expect("an outside-root manifest must be rejected");

        assert!(
            matches!(error, cooldown_core::CoreError::System(detail) if detail.contains("outside project root"))
        );
    }

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
        // Underscore dirs are legitimate package/source locations in several ecosystems, so they are
        // no longer pruned generically; bulk files inside them still do not match resolver inputs.
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

        // Pruned dirs are not reproduced at all; underscore data is walked but bulk files are skipped.
        assert!(!d.join("node_modules").exists(), "node_modules pruned");
        assert!(
            !d.join("_data/vol/blob").exists(),
            "underscore data blob skipped"
        );
    }

    #[test]
    fn copies_resolver_config_below_pruned_dot_dirs() {
        let src = tempfile::tempdir().expect("src");
        let dest = tempfile::tempdir().expect("dest");
        let s = src.path();
        std::fs::create_dir_all(s.join(".cargo")).expect("mkdir");
        std::fs::write(s.join(".cargo/config.toml"), "[source.crates-io]\n").expect("write");
        std::fs::create_dir_all(s.join(".cargo/bin")).expect("mkdir");
        std::fs::write(s.join(".cargo/bin/cargo-helper"), "binary").expect("write");
        std::fs::create_dir_all(s.join(".yarn/releases")).expect("mkdir");
        std::fs::write(s.join(".yarn/releases/yarn-4.0.0.cjs"), "yarn").expect("write");
        std::fs::create_dir_all(s.join(".yarn/cache")).expect("mkdir");
        std::fs::write(s.join(".yarn/cache/left-pad.zip"), "cache").expect("write");
        std::fs::create_dir_all(s.join(".swiftpm/configuration")).expect("mkdir");
        std::fs::write(
            s.join(".swiftpm/configuration/registries.json"),
            "{\"registries\":{}}\n",
        )
        .expect("write");

        copy_project_tree(s, dest.path(), &ResolveInputs::DEFAULT).expect("copy");
        let d = dest.path();

        assert!(d.join(".cargo/config.toml").exists());
        assert!(!d.join(".cargo/bin/cargo-helper").exists());
        assert!(d.join(".yarn/releases/yarn-4.0.0.cjs").exists());
        assert!(!d.join(".yarn/cache/left-pad.zip").exists());
        assert!(d.join(".swiftpm/configuration/registries.json").exists());
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
        std::fs::create_dir_all(s.join("_internal")).expect("mkdir");
        std::fs::write(s.join("_internal/helper.rs"), "").expect("write");

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
        assert!(
            dest.path().join("_internal/helper.rs").exists(),
            "underscore source dirs are valid resolver input locations"
        );

        // The default (no source extensions) skips it.
        let dest_default = tempfile::tempdir().expect("dest");
        copy_project_tree(s, dest_default.path(), &ResolveInputs::DEFAULT).expect("copy");
        assert!(
            !dest_default.path().join("src/lib.rs").exists(),
            "the declaration-only default never copies source"
        );
        assert!(
            !dest_default.path().join("_internal/helper.rs").exists(),
            "underscore source still requires an opted-in source extension"
        );
    }
}
