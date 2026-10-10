//! Patch files a package manager applies on install, which a trial resolve must be able to read.
//!
//! pnpm hashes every patch named in `patchedDependencies` while it resolves, so a resolve in a
//! preview copy fails with `ENOENT` unless each patch file is staged into that copy.
//! The patch paths live only in configuration (the lockfile records just their hashes), and a
//! `.patch` basename matches no resolver input, so the selective copy never picks them up alone.

use camino::{Utf8Path, Utf8PathBuf};
use std::collections::{BTreeMap, BTreeSet};

/// Returns the canonical paths of every patch file the project's configuration declares.
///
/// Three declarations are read, each relative to `root`:
///
/// - `patchedDependencies` in `pnpm-workspace.yaml` (pnpm 10 and later).
/// - `pnpm.patchedDependencies` in `package.json` (earlier pnpm releases).
/// - `patchedDependencies` in `package.json` (bun).
///
/// Best-effort: an unreadable or malformed file contributes nothing, and a declared patch that does
/// not exist is skipped, so the resolver reports the missing file exactly as it would in place.
pub(crate) fn declared_patch_files(root: &Utf8Path) -> Vec<Utf8PathBuf> {
    let mut relative = Vec::new();
    if let Ok(content) = std::fs::read_to_string(root.join("pnpm-workspace.yaml"))
        && let Ok(workspace) = serde_saphyr::from_str::<PnpmWorkspace>(&content)
    {
        relative.extend(workspace.patched_dependencies.into_values());
    }
    if let Ok(content) = std::fs::read_to_string(root.join("package.json"))
        && let Ok(manifest) = serde_json::from_str::<PackageJson>(&content)
    {
        relative.extend(manifest.patched_dependencies.into_values());
        if let Some(pnpm) = manifest.pnpm {
            relative.extend(pnpm.patched_dependencies.into_values());
        }
    }

    let mut patches = BTreeSet::new();
    for path in relative {
        let Ok(canonical) = std::fs::canonicalize(root.join(path).as_std_path()) else {
            continue;
        };
        let Ok(canonical) = Utf8PathBuf::from_path_buf(canonical) else {
            continue;
        };
        if canonical.is_file() {
            patches.insert(canonical);
        }
    }
    patches.into_iter().collect()
}

#[derive(serde::Deserialize)]
struct PnpmWorkspace {
    #[serde(default, rename = "patchedDependencies")]
    patched_dependencies: BTreeMap<String, String>,
}

#[derive(serde::Deserialize)]
struct PackageJson {
    #[serde(default, rename = "patchedDependencies")]
    patched_dependencies: BTreeMap<String, String>,
    pnpm: Option<PnpmManifestSettings>,
}

#[derive(serde::Deserialize)]
struct PnpmManifestSettings {
    #[serde(default, rename = "patchedDependencies")]
    patched_dependencies: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use color_eyre::eyre;

    fn project() -> eyre::Result<(tempfile::TempDir, Utf8PathBuf)> {
        let dir = tempfile::tempdir()?;
        let root = Utf8PathBuf::from_path_buf(std::fs::canonicalize(dir.path())?)
            .map_err(|path| eyre::eyre!("temp dir is not UTF-8: {}", path.display()))?;
        Ok((dir, root))
    }

    fn write(root: &Utf8Path, relative: &str, content: &str) -> eyre::Result<()> {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, content)?;
        Ok(())
    }

    /// A pnpm 10 workspace declares its patches in `pnpm-workspace.yaml`.
    #[test]
    fn reads_patches_from_the_pnpm_workspace_file() -> eyre::Result<()> {
        let (_dir, root) = project()?;
        write(
            &root,
            "pnpm-workspace.yaml",
            indoc::indoc! {r#"
                packages:
                  - "apps/*"
                patchedDependencies:
                  "@kobalte/utils@0.9.2": patches/@kobalte__utils@0.9.2.patch
            "#},
        )?;
        write(&root, "patches/@kobalte__utils@0.9.2.patch", "diff\n")?;

        assert_eq!(
            declared_patch_files(&root),
            [root.join("patches/@kobalte__utils@0.9.2.patch")]
        );
        Ok(())
    }

    /// Older pnpm and bun declare patches in `package.json`; both shapes are staged.
    #[test]
    fn reads_patches_from_package_json() -> eyre::Result<()> {
        let (_dir, root) = project()?;
        write(
            &root,
            "package.json",
            r#"{"patchedDependencies": {"left-pad@1.3.0": "patches/bun.patch"},
                "pnpm": {"patchedDependencies": {"is-odd@3.0.1": "patches/pnpm.patch"}}}"#,
        )?;
        write(&root, "patches/bun.patch", "diff\n")?;
        write(&root, "patches/pnpm.patch", "diff\n")?;

        assert_eq!(
            declared_patch_files(&root),
            [
                root.join("patches/bun.patch"),
                root.join("patches/pnpm.patch")
            ]
        );
        Ok(())
    }

    /// A project without patches, a malformed file, and a dangling declaration all yield nothing.
    #[test]
    fn skips_absent_malformed_and_missing_declarations() -> eyre::Result<()> {
        let (_dir, root) = project()?;

        // No configuration
        assert!(declared_patch_files(&root).is_empty());

        // Malformed manifest and a declared patch that does not exist
        write(&root, "package.json", "{not json")?;
        write(
            &root,
            "pnpm-workspace.yaml",
            "patchedDependencies:\n  \"a@1.0.0\": patches/missing.patch\n",
        )?;
        assert!(declared_patch_files(&root).is_empty());
        Ok(())
    }
}
