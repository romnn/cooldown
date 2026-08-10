//! Path helpers shared by this crate's unit tests.

use camino::{Utf8Path, Utf8PathBuf};
use color_eyre::eyre;

/// Resolves `path` to the canonical spelling the production entry points work in.
///
/// Publication, staging, and recovery all derive project identity through `std::fs::canonicalize`,
/// so the paths they accept and echo back are canonical.
/// `tempfile` hands out the platform temporary directory verbatim, which is a symlink on macOS
/// (`/var` -> `/private/var`) and an 8.3 short name on Windows (`C:\Users\RUNNER~1`), so a raw
/// handout and a canonical result are two spellings of one directory that compare unequal.
fn canonical_path(path: &Utf8Path) -> eyre::Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(std::fs::canonicalize(path)?)
        .map_err(|path| eyre::eyre!("canonical path is not UTF-8: {}", path.display()))
}

/// Returns `directory`'s canonical root, standing in for the canonical project root the CLI
/// resolves before any Cargo mutation or recovery entry point sees it.
pub(crate) fn canonical_root(directory: &tempfile::TempDir) -> eyre::Result<Utf8PathBuf> {
    let root = Utf8Path::from_path(directory.path())
        .ok_or_else(|| eyre::eyre!("temporary directory is not UTF-8"))?;
    canonical_path(root)
}
