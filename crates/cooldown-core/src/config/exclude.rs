//! Compiling the two kinds of scan-exclude pattern into validated [`GlobSet`]s.
//!
//! `exclude-folders` and `exclude-packages` are deliberately different matchers, because users
//! reach for them with different mental models:
//!
//! - **folders** use `.gitignore` semantics — the same rules as the `.gitignore` the scan already
//!   honors, so there is one model to learn. A bare name (`target`) matches that directory at any
//!   depth; a leading slash (`/build`) anchors it to the scan root; an interior slash
//!   (`third_party/grammars`) is likewise root-anchored; a trailing slash is allowed and ignored;
//!   `**` is supported.
//! - **packages** use plain name globs — the same flavor as the `[package."…"]` policy selector, so
//!   `@scope/*` matches a whole npm scope and `serde_*` a family of crates. No registry permits `*`
//!   in a package name, so `*` is always a wildcard and nothing needs escaping.
//!
//! Both are compiled here at config-load time so an invalid glob surfaces as a
//! [`CoreError::Config`] when the config is parsed, not deep inside a later scan.

use crate::error::CoreError;
use globset::{Glob, GlobSet, GlobSetBuilder};

/// Compile `exclude-folders` patterns (`.gitignore` semantics) into a validated [`GlobSet`] meant to
/// be matched against a directory's path relative to the scan root.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if a pattern is not a valid glob.
pub fn compile_folder_globset(patterns: &[String]) -> Result<GlobSet, CoreError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let trimmed = pat.trim();
        if let Some(anchored) = trimmed.strip_prefix('/') {
            // A leading slash anchors to the repo root (`.gitignore`'s `/build`): match the path
            // exactly, with no depth-independent `**/` variant.
            let anchored = anchored.trim_end_matches('/');
            if anchored.is_empty() {
                continue;
            }
            builder.add(folder_glob(anchored)?);
        } else {
            // A trailing slash is the natural directory-exclude idiom; the walk yields directory
            // paths without one, so normalize it away before the bare-name test below — that is what
            // lets `examples/` behave like `examples` and still earn the any-depth `**/` variant.
            let bare = trimmed.trim_end_matches('/');
            if bare.is_empty() {
                continue;
            }
            builder.add(folder_glob(bare)?);
            // A name with no interior slash is unanchored: like `.gitignore`, it prunes that
            // directory at every depth, so add the `**/` variant. An interior slash (`a/b`) is
            // already root-anchored and gets no variant.
            if !bare.contains('/') {
                builder.add(folder_glob(&format!("**/{bare}"))?);
            }
        }
    }
    builder
        .build()
        .map_err(|error| CoreError::Config(format!("invalid exclude-folders set: {error}")))
}

/// Compile `exclude-packages` patterns (name globs) into a validated [`GlobSet`] meant to be matched
/// against a package name.
///
/// # Errors
///
/// Returns [`CoreError::Config`] if a pattern is not a valid glob.
pub fn compile_package_globset(patterns: &[String]) -> Result<GlobSet, CoreError> {
    let mut builder = GlobSetBuilder::new();
    for pat in patterns {
        let trimmed = pat.trim();
        if trimmed.is_empty() {
            continue;
        }
        builder.add(package_glob(trimmed)?);
    }
    builder
        .build()
        .map_err(|error| CoreError::Config(format!("invalid exclude-packages set: {error}")))
}

fn folder_glob(pattern: &str) -> Result<Glob, CoreError> {
    Glob::new(pattern).map_err(|error| {
        CoreError::Config(format!("invalid exclude-folders glob {pattern:?}: {error}"))
    })
}

fn package_glob(pattern: &str) -> Result<Glob, CoreError> {
    // `*` crosses `/` (literal_separator = false) so `@scope/*` matches a whole scope — the same
    // flavor as the `[package."…"]` policy selector (see `PatternGlob`).
    globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .map_err(|error| {
            CoreError::Config(format!(
                "invalid exclude-packages glob {pattern:?}: {error}"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn folder_bare_name_matches_at_any_depth() {
        let set = compile_folder_globset(&["target".to_string()]).expect("compile");
        assert!(set.is_match(Path::new("target")));
        assert!(set.is_match(Path::new("crates/foo/target")));
    }

    #[test]
    fn folder_trailing_slash_is_ignored() {
        let bare = compile_folder_globset(&["examples".to_string()]).expect("compile");
        let slashed = compile_folder_globset(&["examples/".to_string()]).expect("compile");
        for set in [&bare, &slashed] {
            assert!(set.is_match(Path::new("examples")));
            assert!(set.is_match(Path::new("nested/examples")));
        }
    }

    #[test]
    fn folder_leading_slash_anchors_to_root() {
        let set = compile_folder_globset(&["/examples".to_string()]).expect("compile");
        assert!(set.is_match(Path::new("examples")), "root-level matches");
        assert!(
            !set.is_match(Path::new("nested/examples")),
            "a nested examples is NOT anchored away"
        );
    }

    #[test]
    fn folder_interior_slash_is_anchored() {
        let set = compile_folder_globset(&["third_party/grammars".to_string()]).expect("compile");
        assert!(set.is_match(Path::new("third_party/grammars")));
        assert!(!set.is_match(Path::new("vendor/third_party/grammars")));
    }

    #[test]
    fn package_glob_matches_scope_and_family() {
        let set =
            compile_package_globset(&["@scope/*".to_string(), "serde_*".to_string()]).expect("ok");
        assert!(set.is_match(Path::new("@scope/api")));
        assert!(set.is_match(Path::new("serde_json")));
        assert!(!set.is_match(Path::new("@other/api")));
        assert!(!set.is_match(Path::new("tokio")));
    }

    #[test]
    fn invalid_glob_is_a_config_error() {
        assert!(matches!(
            compile_folder_globset(&["a/**/[".to_string()]),
            Err(CoreError::Config(_))
        ));
        assert!(matches!(
            compile_package_globset(&["[".to_string()]),
            Err(CoreError::Config(_))
        ));
    }
}
