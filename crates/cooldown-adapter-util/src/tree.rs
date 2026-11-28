//! The files of a project tree that a scratch copy holds, walked the same way for the copy a
//! preview runs in and the copy an apply verifies in, so the tool reads the same files in both.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, Result};

/// Every file below `dir`, a symbolic link to one included, leaving out each directory `skipped`
/// accepts.
///
/// # Errors
///
/// Returns [`CoreError::System`] for a symbolic link to a directory, which could loop, or lead
/// the tool's writes out of the copy into the project, and a filesystem error when a directory
/// cannot be read.
pub fn files_below(
    dir: &Utf8Path,
    skipped: &dyn Fn(&Utf8Path) -> bool,
) -> Result<Vec<Utf8PathBuf>> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(current) = pending.pop() {
        for entry in current.read_dir_utf8()? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_file() {
                files.push(entry.into_path());
                continue;
            }
            let path = entry.into_path();
            if skipped(&path) {
                continue;
            }
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_symlink() {
                match std::fs::metadata(&path) {
                    Ok(target) if target.is_dir() => {
                        let rel = path.strip_prefix(dir).unwrap_or(Utf8Path::new(""));
                        return Err(CoreError::System(format!(
                            "{} is a symbolic link to a directory, which cooldown does not \
                             follow into the copy it verifies the project in",
                            slash_path(rel)
                        )));
                    }
                    Ok(target) if target.is_file() => files.push(path),
                    // A link to nothing is no file to the tool either.
                    Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                        return Err(error.into());
                    }
                    _ => {}
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

/// `path` spelled with `/` on every platform, `.` when empty.
#[must_use]
pub fn slash_path(path: &Utf8Path) -> String {
    let parts: Vec<&str> = path
        .components()
        .map(|component| component.as_str())
        .collect();
    if parts.is_empty() {
        ".".to_owned()
    } else {
        parts.join("/")
    }
}

/// `path` as a UTF-8 path.
///
/// # Errors
///
/// Returns [`CoreError::PathEncoding`] when `path` is not valid UTF-8.
pub fn utf8(path: &std::path::Path) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(path.to_path_buf())
        .map_err(|path| CoreError::PathEncoding(format!("{} is not valid UTF-8", path.display())))
}
