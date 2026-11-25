//! Keeping a lock probe's scratch directories outside the project it verifies.

use camino::{Utf8Path, Utf8PathBuf};

/// Returns `path` with every symbolic link resolved, spelled without Windows' verbatim prefix.
///
/// # Errors
///
/// Returns the I/O error of resolving `path`, or an [`std::io::ErrorKind::Other`] error when the
/// resolved path is not valid UTF-8.
pub fn canonical_path(path: &Utf8Path) -> std::io::Result<Utf8PathBuf> {
    let resolved = dunce::canonicalize(path)?;
    Utf8PathBuf::from_path_buf(resolved)
        .map_err(|path| std::io::Error::other(format!("{} is not valid UTF-8", path.display())))
}

/// Returns `directory` resolved through the symbolic links of its existing ancestors, itself
/// included, or `None` when it lies inside the canonical `root` or cannot be resolved.
///
/// cooldown accepts a cache directory inside the project, and creating a directory there would
/// itself change the project.
#[must_use]
pub fn outside_project(root: &Utf8Path, directory: &Utf8Path) -> Option<Utf8PathBuf> {
    for ancestor in directory.ancestors() {
        match canonical_path(ancestor) {
            Ok(resolved) => {
                let resolved = resolved.join(directory.strip_prefix(ancestor).ok()?);
                return (!resolved.starts_with(root)).then_some(resolved);
            }
            // An entry that exists without resolving, such as a dangling link, could lead anywhere.
            Err(_) if std::fs::symlink_metadata(ancestor).is_ok() => return None,
            Err(_) => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{canonical_path, outside_project};
    use camino::Utf8Path;

    /// A scratch directory below the project is refused, through a link as well, while one outside
    /// it resolves through its existing ancestors.
    #[test]
    fn the_scratch_directory_must_lie_outside_the_project() -> std::io::Result<()> {
        let project_dir = tempfile::tempdir()?;
        let elsewhere_dir = tempfile::tempdir()?;
        let utf8 = |path: &std::path::Path| {
            Utf8Path::from_path(path)
                .ok_or_else(|| std::io::Error::other("temporary path is not UTF-8"))
                .and_then(canonical_path)
        };
        let root = utf8(project_dir.path())?;
        let elsewhere = utf8(elsewhere_dir.path())?;

        assert_eq!(outside_project(&root, &root.join(".cache/bun")), None);
        let scratch = elsewhere.join("cache/bun-verify");
        assert_eq!(outside_project(&root, &scratch), Some(scratch));

        #[cfg(unix)]
        {
            let link = elsewhere.join("link");
            std::os::unix::fs::symlink(&root, &link)?;
            assert_eq!(outside_project(&root, &link.join("cache")), None);
        }
        Ok(())
    }
}
