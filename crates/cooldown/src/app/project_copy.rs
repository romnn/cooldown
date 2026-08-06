//! Heuristic throwaway copies used for previews by adapters without tool-specific staging.
//!
//! `outdated` and `--dry-run` run real resolvers without touching source lockfiles or manifests.
//! Adapters that publish isolated mutations provide their own faithful staging strategy instead.

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

/// A project copied into a throwaway temp directory.
///
/// The copy is deleted when this value drops, so resolver mutations never reach the source tree.
/// Hold it for as long as the copied [`Project`] is in use.
pub(crate) struct ProjectCopy {
    /// Kept so the temp directory lives as long as the copy; dropping it removes the tree.
    #[expect(dead_code, reason = "the field keeps the copied project tree alive")]
    scratch: tempfile::TempDir,
    /// The copied project, rooted inside the temp directory.
    pub(crate) project: Project,
}

impl ProjectCopy {
    /// Copy only the files the resolver reads (`inputs`) from `project`'s tree into a fresh temp
    /// directory and return a [`Project`] with a canonical root and its manifest rebased onto the
    /// copy.
    ///
    /// This copies only the manifests, lockfiles, configuration, and source extensions declared by
    /// `inputs`.
    /// A blind recursive copy could duplicate gigabytes of unrelated monorepo data into a tempdir.
    /// The source tree is read but never written, and the heuristic copy is discarded on drop.
    ///
    /// `external_roots` are the out-of-tree local dependency sources the resolver must read
    /// ([`cooldown_core::ToolWrite::external_resolve_roots`]): the copy then reproduces the shared
    /// topology — the project and each root staged at their positions relative to the deepest
    /// common ancestor — so a relative reference (`editable+../sibling`) resolves inside the copy
    /// instead of dangling at a path only the real tree has.
    pub(crate) fn create(
        project: &Project,
        inputs: &ResolveInputs,
        external_roots: &[camino::Utf8PathBuf],
    ) -> cooldown_core::Result<Self> {
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

        // External roots arrive canonical (the adapter resolves `../` references through the real
        // tree), so ancestor computation must compare against the equally-canonical project root
        // or a symlinked component would push the ancestor to `/`.
        let canonical_root = std::fs::canonicalize(project.root.as_std_path())
            .ok()
            .and_then(|path| camino::Utf8PathBuf::from_path_buf(path).ok())
            .unwrap_or_else(|| project.root.clone());
        let project_dest = match staging_ancestor(&canonical_root, external_roots) {
            Some(ancestor) => {
                let rebase = |root: &camino::Utf8Path| {
                    root.strip_prefix(&ancestor)
                        .map(|rel| scratch_root.join(rel))
                };
                for root in external_roots {
                    let Ok(dest) = rebase(root) else { continue };
                    // A single-file root (a local wheel/sdist archive) is reproduced verbatim:
                    // the selective tree copy would filter it out as neither manifest nor source,
                    // yet the archive itself is exactly what the resolver reads.
                    if root.is_file() {
                        if let Some(parent) = dest.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::copy(root.as_std_path(), dest.as_std_path())?;
                    } else {
                        copy_project_tree(root.as_std_path(), dest.as_std_path(), inputs)?;
                    }
                }
                rebase(&canonical_root).unwrap_or_else(|_| scratch_root.clone())
            }
            None => scratch_root,
        };
        copy_project_tree(
            project.root.as_std_path(),
            project_dest.as_std_path(),
            inputs,
        )?;

        let copied_manifest = project_dest.join(manifest_rel);
        let copied = Project {
            root: project_dest,
            kind: project.kind,
            manifest: copied_manifest,
            exclude_newer: project.exclude_newer.clone(),
        };
        Ok(ProjectCopy {
            scratch,
            project: copied,
        })
    }
}

/// The deepest common ancestor of the project root and every external dependency root — the
/// directory whose layout the copy must reproduce for relative references between them to
/// resolve. `None` without external roots (the copy stays rooted at the project, the common
/// case), or when the paths share no prefix at all (a Windows-style second drive; the copy then
/// degrades to the plain project-rooted shape, which at worst reproduces the dangling-reference
/// failure this staging exists to avoid). The filesystem root is a legitimate ancestor: a
/// container layout (`/app` project, `/deps` editable) has no deeper one, and only the declared
/// roots are staged at their `/`-relative positions — never the whole filesystem — so the copy
/// stays bounded.
fn staging_ancestor(
    project_root: &camino::Utf8Path,
    external_roots: &[camino::Utf8PathBuf],
) -> Option<camino::Utf8PathBuf> {
    if external_roots.is_empty() {
        return None;
    }
    let mut ancestor = project_root.to_owned();
    for root in external_roots {
        while !root.starts_with(&ancestor) {
            if !ancestor.pop() {
                tracing::debug!(
                    project = %project_root,
                    "external dependency roots share no path prefix with the project; preview copy stays project-rooted"
                );
                return None;
            }
        }
    }
    Some(ancestor)
}

/// Recursively copies the inputs selected by a generic preview adapter into `dest`.
///
/// Only the directory structure needed to place selected files is recreated.
/// Virtual environments, VCS metadata, dependency stores, build output, and vendored/testdata trees
/// are pruned.
/// Dot-prefixed directories are traversed only for explicit [`ResolveInputs::path_prefixes`].
/// An unreadable entry is skipped because this copy supports best-effort previews, not publication.
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
    use super::{ProjectCopy, copy_project_tree, staging_ancestor};
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

        let copy =
            ProjectCopy::create(&project, &ResolveInputs::DEFAULT, &[]).expect("copy project");
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
    fn copy_stages_external_dependency_roots_at_their_relative_positions() {
        // A uv project whose editable dependencies live in sibling directories: the copy must
        // reproduce the shared topology so `editable+../../packages/python/lib` resolves inside
        // the temp tree instead of dangling at a path only the real tree has.
        let repo = tempfile::tempdir().expect("repo");
        let base = repo.path();
        std::fs::create_dir_all(base.join("services/app")).expect("mkdir");
        std::fs::create_dir_all(base.join("packages/python/lib")).expect("mkdir");
        std::fs::write(base.join("services/app/pyproject.toml"), "[project]").expect("write");
        std::fs::write(base.join("services/app/uv.lock"), "version = 1").expect("write");
        std::fs::write(base.join("packages/python/lib/pyproject.toml"), "[project]")
            .expect("write");
        let canonical = |path: std::path::PathBuf| {
            camino::Utf8PathBuf::from_path_buf(
                std::fs::canonicalize(path).expect("canonicalize fixture path"),
            )
            .expect("UTF-8 fixture path")
        };
        let root = canonical(base.join("services/app"));
        let external = canonical(base.join("packages/python/lib"));
        let project = Project {
            root: root.clone(),
            kind: ToolId("test"),
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        };

        let copy = ProjectCopy::create(&project, &ResolveInputs::DEFAULT, &[external])
            .expect("copy project");

        assert!(
            copy.project.root.ends_with("services/app"),
            "the project sits at its ancestor-relative position: {}",
            copy.project.root
        );
        assert!(copy.project.manifest.exists());
        let staged_external = copy
            .project
            .root
            .parent()
            .and_then(camino::Utf8Path::parent)
            .expect("copy has ancestor levels")
            .join("packages/python/lib/pyproject.toml");
        assert!(
            staged_external.exists(),
            "the external root's manifest is staged at its relative position: {staged_external}"
        );
    }

    /// A `path` source references an archive file, not a directory: the copy reproduces the file
    /// verbatim at its relative position — the selective tree copy would filter it out as neither
    /// manifest nor source, yet the archive is exactly what the resolver reads.
    #[test]
    fn copy_stages_a_single_file_external_root_verbatim() {
        let repo = tempfile::tempdir().expect("repo");
        let base = repo.path();
        std::fs::create_dir_all(base.join("services/app")).expect("mkdir");
        std::fs::create_dir_all(base.join("vendor")).expect("mkdir");
        std::fs::write(base.join("services/app/pyproject.toml"), "[project]").expect("write");
        std::fs::write(base.join("vendor/pkg-1.0.tar.gz"), "archive-bytes").expect("write");
        let canonical = |path: std::path::PathBuf| {
            camino::Utf8PathBuf::from_path_buf(
                std::fs::canonicalize(path).expect("canonicalize fixture path"),
            )
            .expect("UTF-8 fixture path")
        };
        let root = canonical(base.join("services/app"));
        let archive = canonical(base.join("vendor/pkg-1.0.tar.gz"));
        let project = Project {
            root: root.clone(),
            kind: ToolId("test"),
            manifest: root.join("pyproject.toml"),
            exclude_newer: None,
        };

        let copy = ProjectCopy::create(&project, &ResolveInputs::DEFAULT, &[archive])
            .expect("copy project");

        let staged = copy
            .project
            .root
            .parent()
            .and_then(camino::Utf8Path::parent)
            .expect("copy has ancestor levels")
            .join("vendor/pkg-1.0.tar.gz");
        assert_eq!(
            std::fs::read_to_string(&staged).expect("staged archive"),
            "archive-bytes",
            "the archive's bytes are staged at its relative position"
        );
    }

    /// A container layout's only shared ancestor may be the filesystem root itself — still a
    /// legitimate staging base, since only the declared roots are copied, never the whole
    /// filesystem.
    #[test]
    fn filesystem_root_is_a_legitimate_staging_ancestor() {
        assert_eq!(
            staging_ancestor(
                camino::Utf8Path::new("/app"),
                &[camino::Utf8PathBuf::from("/deps")]
            )
            .as_deref(),
            Some(camino::Utf8Path::new("/")),
        );
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

        let error = ProjectCopy::create(&project, &ResolveInputs::DEFAULT, &[])
            .err()
            .expect("an outside-root manifest must be rejected");

        std::assert_matches!(error, cooldown_core::CoreError::System(detail) if detail.contains("outside project root"));
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

    /// `source_extensions` is opt-in per generic preview adapter.
    /// Go includes its source because resolution validates imports against it, while the
    /// declaration-only default does not.
    #[test]
    fn source_extensions_are_opt_in_per_tool() {
        let src = tempfile::tempdir().expect("src");
        let s = src.path();
        std::fs::write(s.join("go.mod"), "module example.com/x\n").expect("write");
        std::fs::create_dir_all(s.join("src")).expect("mkdir");
        std::fs::write(s.join("src/lib.go"), "package src\n").expect("write");
        std::fs::create_dir_all(s.join("_internal")).expect("mkdir");
        std::fs::write(s.join("_internal/helper.go"), "package internal\n").expect("write");

        // Go opts into `.go`, so its source is copied.
        let go_inputs = ResolveInputs {
            source_extensions: &["go"],
            ..ResolveInputs::DEFAULT
        };
        let dest = tempfile::tempdir().expect("dest");
        copy_project_tree(s, dest.path(), &go_inputs).expect("copy");
        assert!(dest.path().join("go.mod").exists());
        assert!(
            dest.path().join("src/lib.go").exists(),
            "Go copies .go source so resolution sees package imports"
        );
        assert!(
            dest.path().join("_internal/helper.go").exists(),
            "underscore source dirs are valid resolver input locations"
        );

        // The default (no source extensions) skips it.
        let dest_default = tempfile::tempdir().expect("dest");
        copy_project_tree(s, dest_default.path(), &ResolveInputs::DEFAULT).expect("copy");
        assert!(
            !dest_default.path().join("src/lib.go").exists(),
            "the declaration-only default never copies source"
        );
        assert!(
            !dest_default.path().join("_internal/helper.go").exists(),
            "underscore source still requires an opted-in source extension"
        );
    }
}
