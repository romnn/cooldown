//! Spells a filesystem path the way it leaves the process as data.
//!
//! A diagnostic `path`, a report row's `path`, and a project label are read by machines that
//! match them against project files, so they must not depend on where the checkout lives or which
//! platform ran cooldown.
//! They are therefore relative to a base the report names and `/`-separated everywhere.

use camino::{Utf8Path, Utf8PathBuf};

/// Returns `path` relative to `base` with `/` separators, `.` when the two are equal, or `None`
/// when `path` does not lie under `base`.
///
/// The segments are joined with `/` rather than [`Utf8Path::join`]'s native separator, so a
/// Windows run spells the value the same way as a Unix one.
/// A path outside `base` has no relative spelling here; each caller decides what to report then
/// instead of leaking the absolute path by default.
pub(crate) fn relative_report_path(path: &Utf8Path, base: &Utf8Path) -> Option<Utf8PathBuf> {
    let relative = path.strip_prefix(base).ok()?;
    if relative.as_str().is_empty() {
        return Some(Utf8PathBuf::from("."));
    }
    Some(Utf8PathBuf::from(
        relative
            .components()
            .map(|component| component.as_str())
            .collect::<Vec<_>>()
            .join("/"),
    ))
}

#[cfg(test)]
mod tests {
    use super::relative_report_path;
    use camino::{Utf8Path, Utf8PathBuf};

    #[test]
    fn a_path_under_the_base_is_relative_to_it() {
        let base = Utf8Path::new("/repo");
        assert_eq!(
            relative_report_path(Utf8Path::new("/repo"), base),
            Some(Utf8PathBuf::from("."))
        );
        assert_eq!(
            relative_report_path(Utf8Path::new("/repo/tools/x"), base),
            Some(Utf8PathBuf::from("tools/x"))
        );
        assert_eq!(
            relative_report_path(Utf8Path::new("/repo/apps/web/package.json"), base),
            Some(Utf8PathBuf::from("apps/web/package.json"))
        );
    }

    /// A path outside the base, such as a linked worktree's shared Git directory, has no relative
    /// spelling, so the absolute path is never returned in its place.
    #[test]
    fn a_path_outside_the_base_has_no_relative_spelling() {
        assert_eq!(
            relative_report_path(Utf8Path::new("/elsewhere/app"), Utf8Path::new("/repo")),
            None
        );
        // A sibling that only shares a string prefix is outside as well.
        assert_eq!(
            relative_report_path(Utf8Path::new("/repo-other/app"), Utf8Path::new("/repo")),
            None
        );
    }

    /// The path is reported, so it spells `/` even where the platform's own separator is `\`.
    #[cfg(windows)]
    #[test]
    fn a_relative_path_is_slash_separated_on_windows() {
        assert_eq!(
            relative_report_path(
                Utf8Path::new("C:\\repo\\tools\\x\\Cargo.toml"),
                Utf8Path::new("C:\\repo")
            ),
            Some(Utf8PathBuf::from("tools/x/Cargo.toml"))
        );
    }
}
