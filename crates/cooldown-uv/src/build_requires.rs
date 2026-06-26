//! PEP 517 build-backend requirements (`[build-system].requires`, e.g. `hatchling`).
//!
//! uv resolves the build backend in an isolated environment at build time and never records it in
//! `uv.lock`, so the lock-driven dependency graph cannot see it. We read the requirement directly so
//! `outdated`/`upgrade` can surface and raise its lower-bound floor — exactly the surface Dependabot
//! manages — while the lock-based `check`/`fix` gate (which reads only the resolved graph) ignores
//! it: there is no locked version to gate.

use crate::native;
use crate::version;
use camino::Utf8Path;
use cooldown_core::Result;
use cooldown_toml_util::read_toml_file;
use std::collections::HashSet;

/// One `[build-system].requires` entry, reduced to what the core needs to evaluate it: the
/// normalized package name, the declared lower-bound floor (used as the `current` version to compare
/// newer releases against), and whether that floor is an exact pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildRequire {
    /// The PEP 503-normalized package name.
    pub name: String,
    /// The declared lower-bound version — what Dependabot raises and we treat as the `current` pin.
    pub floor: String,
    /// The requirement is an exact `==`/`===` pin, so it is held (shown but not auto-upgraded), the
    /// same way the lock-driven graph treats a manifest `==` pin.
    pub pinned: bool,
}

/// The build-backend requirements declared in `[build-system].requires`, each reduced to a
/// [`BuildRequire`]. A missing manifest, an absent table, or an entry with no lower bound (a bare
/// name or an upper-bound-only specifier — nothing to anchor a `current` on) yields nothing.
///
/// # Errors
///
/// Returns an error when `pyproject.toml` exists but cannot be read or parsed.
pub(crate) fn build_requires(manifest: &Utf8Path) -> Result<Vec<BuildRequire>> {
    Ok(requires_array(manifest)?
        .iter()
        .filter_map(toml::Value::as_str)
        .filter_map(parse_build_require)
        .collect())
}

/// Every PEP 503-normalized name declared in `[build-system].requires`, regardless of whether it
/// carries a lower bound. `apply` uses this to route a planned change to the manifest-floor path
/// (rewrite the requirement) instead of the lock path (which has no entry for the build backend).
///
/// # Errors
///
/// Returns an error when `pyproject.toml` exists but cannot be read or parsed.
pub(crate) fn build_require_names(manifest: &Utf8Path) -> Result<HashSet<String>> {
    Ok(requires_array(manifest)?
        .iter()
        .filter_map(toml::Value::as_str)
        // `requirement_name` already strips a trailing `; marker` itself, so no pre-split is needed.
        .filter_map(native::requirement_name)
        .collect())
}

fn requires_array(manifest: &Utf8Path) -> Result<Vec<toml::Value>> {
    let Some(value) = read_toml_file::<toml::Value>(manifest, "pyproject.toml")? else {
        return Ok(Vec::new());
    };
    Ok(value
        .get("build-system")
        .and_then(|table| table.get("requires"))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Parse one PEP 508 requirement string into its name, lower-bound floor, and pin flag. Returns
/// `None` when there is no name or no lower bound to evaluate against.
fn parse_build_require(requirement: &str) -> Option<BuildRequire> {
    // Drop a trailing environment marker (`; python_version < '3.13'`); it gates applicability, not
    // the version, and PyPI release ages are environment-agnostic here.
    let head = requirement.split(';').next().unwrap_or(requirement).trim();
    let name_end = head
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .unwrap_or(head.len());
    let name = &head[..name_end];
    if name.is_empty() {
        return None;
    }
    let mut rest = head[name_end..].trim_start();
    // Skip an extras group (`hatchling[trove]`); it never carries a version specifier.
    if let Some(after_bracket) = rest.strip_prefix('[') {
        rest = after_bracket
            .split_once(']')
            .map_or("", |(_, tail)| tail.trim_start());
    }
    let (floor, pinned) = lower_bound(rest)?;
    Some(BuildRequire {
        name: native::normalize_name(name),
        floor,
        pinned,
    })
}

/// The declared lower-bound version of a PEP 440 specifier set, and whether it is an exact pin.
///
/// `>=1.28.0` / `>1.27` / `~=1.2` / `>=1.2,<2` yield the lower-bound version (not pinned); `==1.28.0`
/// / `===1.28.0` yield that version, pinned. An upper-bound-only (`<2`, `<=1`, `!=1.0`), a
/// prefix-wildcard pin (`==1.*`), or an empty specifier yields `None` — there is no concrete floor to
/// anchor a `current` on.
fn lower_bound(specifier: &str) -> Option<(String, bool)> {
    let mut floor: Option<String> = None;
    for clause in specifier.split(',') {
        let clause = clause.trim();
        if let Some(version) = clause.strip_prefix("===") {
            return Some((version.trim().to_string(), true));
        }
        if let Some(version) = clause.strip_prefix("==") {
            let version = version.trim();
            if version.contains('*') {
                return None; // a prefix-wildcard pin is not a single concrete floor
            }
            return Some((version.to_string(), true));
        }
        if let Some(version) = clause.strip_prefix(">=") {
            push_floor(&mut floor, version.trim());
            continue;
        }
        if let Some(version) = clause.strip_prefix("~=") {
            push_floor(&mut floor, version.trim());
            continue;
        }
        // `>` must be tested after `>=`; a `<`/`<=`/`!=` clause carries no lower bound, so keep
        // scanning the remaining clauses for one.
        if let Some(version) = clause.strip_prefix('>') {
            push_floor(&mut floor, version.trim());
        }
    }
    floor.map(|version| (version, false))
}

fn push_floor(floor: &mut Option<String>, candidate: &str) {
    if candidate.is_empty() {
        return;
    }
    if floor
        .as_deref()
        .is_none_or(|current| version::compare(candidate, current).is_gt())
    {
        *floor = Some(candidate.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BuildRequire, build_require_names, build_requires, lower_bound, parse_build_require,
    };
    use camino::Utf8PathBuf;
    use indoc::indoc;

    fn require(name: &str, floor: &str, pinned: bool) -> BuildRequire {
        BuildRequire {
            name: name.to_string(),
            floor: floor.to_string(),
            pinned,
        }
    }

    #[test]
    fn lower_bound_reads_the_floor_and_pin_flag() {
        assert_eq!(lower_bound(">=1.28.0"), Some(("1.28.0".into(), false)));
        assert_eq!(lower_bound(">= 1.28.0"), Some(("1.28.0".into(), false)));
        assert_eq!(lower_bound(">1.27"), Some(("1.27".into(), false)));
        assert_eq!(lower_bound("~=1.2"), Some(("1.2".into(), false)));
        assert_eq!(lower_bound(">=1.2,<2"), Some(("1.2".into(), false)));
        // Clause order does not matter: the highest lower bound is found wherever it sits.
        assert_eq!(lower_bound("<2,>=1.2"), Some(("1.2".into(), false)));
        assert_eq!(lower_bound(">=1.2,>=1.4"), Some(("1.4".into(), false)));
        assert_eq!(lower_bound("==1.28.0"), Some(("1.28.0".into(), true)));
        assert_eq!(lower_bound("===1.28.0"), Some(("1.28.0".into(), true)));
        // No lower bound to anchor on.
        assert_eq!(lower_bound("<2"), None);
        assert_eq!(lower_bound("<=1.4"), None);
        assert_eq!(lower_bound("!=1.0"), None);
        assert_eq!(lower_bound("==1.*"), None);
        assert_eq!(lower_bound(""), None);
    }

    #[test]
    fn parse_build_require_handles_name_extras_and_marker() {
        assert_eq!(
            parse_build_require("hatchling>=1.28.0"),
            Some(require("hatchling", "1.28.0", false))
        );
        assert_eq!(
            parse_build_require("Hatchling >= 1.28.0"),
            Some(require("hatchling", "1.28.0", false))
        );
        assert_eq!(
            parse_build_require("setuptools[core]>=70"),
            Some(require("setuptools", "70", false))
        );
        assert_eq!(
            parse_build_require("hatchling>=1.28.0 ; python_version < '3.13'"),
            Some(require("hatchling", "1.28.0", false))
        );
        assert_eq!(
            parse_build_require("hatchling==1.28.0"),
            Some(require("hatchling", "1.28.0", true))
        );
        // A bare requirement has no floor to compare against, so it is not surfaced.
        assert_eq!(parse_build_require("hatchling"), None);
        assert_eq!(parse_build_require("flit_core <4"), None);
    }

    fn write_manifest(contents: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write manifest");
        (dir, path)
    }

    #[test]
    fn build_requires_reads_the_build_system_table() {
        let (_dir, manifest) = write_manifest(indoc! {r#"
            [project]
            name = "demo"
            dependencies = ["httpx>=0.27"]

            [build-system]
            requires = ["hatchling>=1.28.0", "hatchling-vcs", "setuptools==70.0.0"]
            build-backend = "hatchling.build"
        "#});

        assert_eq!(
            build_requires(&manifest).expect("build requires"),
            vec![
                require("hatchling", "1.28.0", false),
                // `hatchling-vcs` is bare (no floor) and so is omitted from the evaluated set,
                require("setuptools", "70.0.0", true),
            ]
        );
        // …but every declared name is still reported for apply's routing decision.
        let names = build_require_names(&manifest).expect("build names");
        assert!(names.contains("hatchling"));
        assert!(names.contains("hatchling-vcs"));
        assert!(names.contains("setuptools"));
    }

    #[test]
    fn no_build_system_table_yields_nothing() {
        let (_dir, manifest) =
            write_manifest("[project]\nname = \"demo\"\ndependencies = [\"httpx>=0.27\"]\n");
        assert!(
            build_requires(&manifest)
                .expect("build requires")
                .is_empty()
        );
        assert!(
            build_require_names(&manifest)
                .expect("build names")
                .is_empty()
        );
    }

    #[test]
    fn invalid_manifest_is_reported() {
        let (_dir, manifest) = write_manifest("[build-system]\nrequires = [");

        assert!(build_requires(&manifest).is_err());
        assert!(build_require_names(&manifest).is_err());
    }
}
