//! Parsing `conda-lock.yml` (the conda-lock tool's output), which pins a mix of conda-channel
//! packages and PyPI packages, so each resolved entry carries whether it came from conda or pip —
//! the adapter routes its publish time to the matching registry. The lockfile is read with a small
//! line scanner rather than a full YAML dependency, since only a few fields per entry are needed.
//! The conda match-spec, archive-name and package-name helpers are shared with the pixi adapter.

use std::collections::{HashMap, HashSet};

/// One resolved dependency from a conda-world lock: its name, pinned version, and which registry
/// owns it (`conda` ⇒ anaconda.org, otherwise PyPI).
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct CondaDep {
    /// The package name.
    pub name: String,
    /// The pinned version.
    pub version: String,
    /// Whether the package is a conda-channel package (vs a PyPI one).
    pub conda: bool,
}

/// Parses `conda-lock.yml`: a `package:` list whose entries carry `name`, `version`, and `manager`
/// (`conda` or `pip`). Per-platform duplicates are collapsed.
#[must_use]
pub fn parse_conda_lock(content: &str) -> Vec<CondaDep> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut in_package = false;
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut manager: Option<String> = None;
    let flush = |name: &mut Option<String>,
                 version: &mut Option<String>,
                 manager: &mut Option<String>,
                 seen: &mut HashSet<CondaDep>,
                 out: &mut Vec<CondaDep>| {
        if let (Some(n), Some(v)) = (name.take(), version.take()) {
            let dep = CondaDep {
                name: n,
                version: v,
                conda: manager.take().as_deref() != Some("pip"),
            };
            if seen.insert(CondaDep {
                name: dep.name.clone(),
                version: dep.version.clone(),
                conda: dep.conda,
            }) {
                out.push(dep);
            }
        }
    };

    for line in content.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            flush(&mut name, &mut version, &mut manager, &mut seen, &mut out);
            in_package = line.starts_with("package:");
            continue;
        }
        if !in_package {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("- name:") {
            flush(&mut name, &mut version, &mut manager, &mut seen, &mut out);
            name = Some(clean(rest));
        } else if let Some(rest) = trimmed.strip_prefix("name:") {
            name = Some(clean(rest));
        } else if let Some(rest) = trimmed.strip_prefix("version:") {
            version = Some(clean(rest));
        } else if let Some(rest) = trimmed.strip_prefix("manager:") {
            manager = Some(clean(rest));
        }
    }
    flush(&mut name, &mut version, &mut manager, &mut seen, &mut out);
    out
}

fn clean(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).to_string()
}

/// Records `version` as one of the exact pins requirers impose on `name`, de-duplicated. A name maps
/// to *all* distinct exact pins, not a single one: a multi-platform lock can pin the same name to
/// different versions per platform, so collapsing to one value (last-write-wins) could drop the pin
/// that matches the resolved version. The consumer picks the pin equal to the resolved version.
fn push_ceiling(ceilings: &mut HashMap<String, Vec<String>>, name: String, version: String) {
    let versions = ceilings.entry(name).or_default();
    if !versions.contains(&version) {
        versions.push(version);
    }
}

/// The graph ceiling candidates for each conda package some *requirer* pins exactly in
/// `conda-lock.yml`: every `package:` entry's `dependencies:` mapping (`name: "spec"`) is scanned, and
/// a `name: "==X"` or bare `name: "X [build]"` value caps that dep at `X` (a trailing build string is
/// dropped, matching the pixi path). Indent-aware: only entries nested below the `dependencies:` key
/// are mapping values, not sibling fields (`version:`, `manager:`).
#[must_use]
pub fn conda_lock_ceilings(content: &str) -> HashMap<String, Vec<String>> {
    let mut ceilings = HashMap::new();
    let mut in_package = false;
    let mut deps_indent: Option<usize> = None;
    for line in content.lines() {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            in_package = line.starts_with("package:");
            deps_indent = None;
            continue;
        }
        if !in_package {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_start();
        if let Some(open) = deps_indent {
            if indent > open {
                if let Some((key, spec)) = trimmed.split_once(':') {
                    let cleaned = clean(spec);
                    // The value is a match-spec; drop a trailing build string before testing it,
                    // exactly as `exact_match_spec` does for the pixi path.
                    if let Some(version) = cleaned.split_whitespace().next().and_then(exact_version)
                    {
                        push_ceiling(
                            &mut ceilings,
                            normalize_name(key.trim()),
                            version.to_string(),
                        );
                    }
                }
                continue;
            }
            deps_indent = None; // dedented back to the entry's fields: the mapping has ended
        }
        // A new list entry resets per-entry state; `dependencies:` opens the mapping block. `field`
        // has any `- ` list marker stripped, so a new package entry surfaces as a bare `name:`.
        let field = trimmed.strip_prefix("- ").unwrap_or(trimmed);
        if field.starts_with("name:") {
            deps_indent = None;
        }
        if field.starts_with("dependencies:") {
            deps_indent = Some(indent);
        }
    }
    ceilings
}

/// A conda match-spec that pins a package to one exact version, borrowing from the spec.
#[derive(Debug, PartialEq)]
pub(crate) struct ExactMatchSpec<'a> {
    /// The package name (the spec's first token, not normalized).
    pub name: &'a str,
    /// The exactly pinned version (any trailing build string dropped).
    pub version: &'a str,
}

/// The name and pinned version of an exact conda match-spec — `libprotobuf 6.31.1` or
/// `ld_impl_linux-64 2.44 h1423503_1` (the trailing build is ignored). `None` for an unconstrained
/// name or a relational/fuzzy spec (`python >=3.9`, `numpy 1.21.*`).
pub(crate) fn exact_match_spec(spec: &str) -> Option<ExactMatchSpec<'_>> {
    let mut tokens = spec.split_whitespace();
    let name = tokens.next()?;
    let version = exact_version(tokens.next()?)?;
    Some(ExactMatchSpec { name, version })
}

/// The version from an exact conda version spec — a bare version (`6.31.1`) or `==X`. `None` for a
/// relational/fuzzy spec (`>=3.9`, `1.21.*`, `>=2.44,<2.45`) or an empty spec.
fn exact_version(spec: &str) -> Option<&str> {
    let version = spec.strip_prefix("==").unwrap_or(spec).trim();
    // Reject relational/fuzzy operators, plus PEP 508 marker (`;`) and extra (`[`) punctuation that a
    // pypi-style requirement in a pixi list could carry, so only a bare version is treated as exact.
    // `!` is *not* rejected: it is the conda/PEP 440 epoch separator (`1!2.0.3`); the relational `!=`
    // is still caught by the `=` it contains.
    if version.is_empty() || version.contains(['<', '>', '=', '~', '*', ',', '|', ' ', ';', '[']) {
        return None;
    }
    Some(version)
}

/// Extracts `(name, version)` from a conda artifact URL: the filename is
/// `{name}-{version}-{build}.{conda,tar.bz2}`, where neither version nor build contains a `-`.
pub(crate) fn conda_from_url(url: &str) -> Option<CondaDep> {
    let file = url.rsplit('/').next()?;
    let stem = file
        .strip_suffix(".conda")
        .or_else(|| file.strip_suffix(".tar.bz2"))?;
    let mut parts = stem.rsplitn(3, '-');
    let _build = parts.next()?;
    let version = parts.next()?;
    let name = parts.next()?;
    Some(CondaDep {
        name: name.to_string(),
        version: version.to_string(),
        conda: true,
    })
}

/// Extracts `(name, version)` from a PyPI artifact URL: a wheel (`{name}-{version}-…​.whl`) or an
/// sdist (`{name}-{version}.tar.gz`).
pub(crate) fn pypi_from_url(url: &str) -> Option<CondaDep> {
    let file = url.rsplit('/').next()?;
    if let Some(stem) = file.strip_suffix(".whl") {
        let mut parts = stem.split('-');
        let name = parts.next()?;
        let version = parts.next()?;
        return Some(CondaDep {
            name: name.to_string(),
            version: version.to_string(),
            conda: false,
        });
    }
    let stem = file.strip_suffix(".tar.gz")?;
    let (name, version) = stem.rsplit_once('-')?;
    Some(CondaDep {
        name: name.to_string(),
        version: version.to_string(),
        conda: false,
    })
}

/// Normalize a package name so the manifest's spelling matches the lock's. PyPI names are
/// case-insensitive and fold runs of `-`, `_`, `.` to a single `-` (PEP 503); conda names lower-case
/// the same way. So `scikit-learn`/`scikit_learn`, `Flask`/`flask`, and `ruamel.yaml`/`ruamel-yaml`
/// all compare equal — the manifest and the URL-derived lock name need not be spelled identically.
#[must_use]
pub fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_sep = false;
    for c in name.trim().chars() {
        if matches!(c, '-' | '_' | '.') {
            if !prev_sep {
                out.push('-');
            }
            prev_sep = true;
        } else {
            out.extend(c.to_lowercase());
            prev_sep = false;
        }
    }
    out
}

/// The conda/PyPI package name of a manifest dependency entry, stripped of a channel prefix
/// (`conda-forge::numpy`), version constraint (`numpy>=1.20`), or extras (`requests[security]`).
/// `None` for a blank or nested-mapping entry.
fn spec_name(spec: &str) -> Option<String> {
    let cleaned = clean(spec);
    let bare = cleaned.rsplit("::").next().unwrap_or(cleaned.as_str());
    let end = bare
        .find(['=', '<', '>', '!', '~', ' ', '[', ';'])
        .unwrap_or(bare.len());
    let name = bare[..end].trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The directly-declared package names in an `environment.yml` — every `dependencies:` list entry,
/// including the names under a nested `- pip:` list. Used to split the resolved lock into direct vs.
/// transitive (the lock itself does not say). A best-effort line scan, matching the lock parsers.
#[must_use]
pub fn environment_yml_direct(content: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let mut in_deps = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        // A top-level mapping key (indent 0, not a list item) opens or closes the deps block.
        if line.starts_with(|c: char| !c.is_whitespace()) && !trimmed.starts_with('-') {
            in_deps = trimmed.starts_with("dependencies:");
            continue;
        }
        if !in_deps {
            continue;
        }
        let Some(item) = trimmed.strip_prefix('-') else {
            continue;
        };
        let item = item.trim();
        // `- pip:` (or any nested mapping) is a marker, not a package.
        if item.is_empty() || item.ends_with(':') {
            continue;
        }
        if let Some(name) = spec_name(item) {
            names.insert(normalize_name(&name));
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conda_lock_mixes_conda_and_pip() {
        let lock = "version: 1\npackage:\n  - name: numpy\n    version: \"1.24.0\"\n    manager: conda\n    platform: linux-64\n  - name: numpy\n    version: \"1.24.0\"\n    manager: conda\n    platform: osx-64\n  - name: requests\n    version: \"2.28.0\"\n    manager: pip\n    platform: linux-64\nmetadata: {}\n";
        let deps = parse_conda_lock(lock);
        assert_eq!(deps.len(), 2); // numpy de-duplicated across platforms
        assert!(deps.contains(&CondaDep {
            name: "numpy".into(),
            version: "1.24.0".into(),
            conda: true
        }));
        assert!(deps.contains(&CondaDep {
            name: "requests".into(),
            version: "2.28.0".into(),
            conda: false
        }));
    }

    #[test]
    fn normalize_name_folds_case_and_separators() {
        assert_eq!(normalize_name("Flask"), "flask");
        assert_eq!(normalize_name("scikit_learn"), "scikit-learn");
        assert_eq!(normalize_name("ruamel.yaml"), "ruamel-yaml");
        assert_eq!(normalize_name("PyYAML"), "pyyaml");
        // Runs of separators collapse to a single dash (PEP 503).
        assert_eq!(normalize_name("foo__bar"), "foo-bar");
    }

    #[test]
    fn environment_yml_direct_collects_and_normalizes_names() {
        let yml = "name: myenv\nchannels:\n  - conda-forge\ndependencies:\n  - python=3.9\n  - conda-forge::numpy>=1.20\n  - pandas\n  - pip\n  - pip:\n    - Flask==2.0\n    - scikit_learn>=1.0\n";
        let direct = environment_yml_direct(yml);
        // PEP 503-normalized so the manifest spelling matches the URL-derived lock name
        // (`Flask`→`flask`, `scikit_learn`→`scikit-learn`).
        for name in ["python", "numpy", "pandas", "pip", "flask", "scikit-learn"] {
            assert!(direct.contains(name), "missing {name}: {direct:?}");
        }
        // The `pip:` nested-list marker is not itself a package, and `channels:` entries are ignored.
        assert!(!direct.contains("conda-forge"));
    }

    #[test]
    fn conda_lock_ceilings_reads_dependencies_mapping_exact_pins() {
        // numpy's `dependencies:` mapping pins libprotobuf exactly and python as a range; the
        // sibling `version:`/`manager:` fields are not mistaken for dependency entries.
        let lock = "version: 1\npackage:\n  - name: numpy\n    version: \"1.24.0\"\n    manager: conda\n    dependencies:\n      libprotobuf: \"==6.31.1\"\n      python: \">=3.9,<3.13\"\n  - name: requests\n    version: \"2.28.0\"\n    manager: pip\nmetadata: {}\n";
        let ceilings = conda_lock_ceilings(lock);
        assert_eq!(
            ceilings.get("libprotobuf").map(Vec::as_slice),
            Some(["6.31.1".to_string()].as_slice())
        );
        assert_eq!(ceilings.get("python"), None); // compound range
        assert_eq!(ceilings.get("version"), None); // a sibling field, not a dependency
        assert_eq!(ceilings.len(), 1);
    }

    #[test]
    fn conda_lock_ceilings_drops_build_suffix_on_value() {
        // A build-qualified exact value (`6.31.1 h1234_0`) is recognized; the build string is dropped.
        let lock = "version: 1\npackage:\n  - name: numpy\n    version: \"1.24.0\"\n    manager: conda\n    dependencies:\n      libprotobuf: \"6.31.1 h1234_0\"\nmetadata: {}\n";
        let ceilings = conda_lock_ceilings(lock);
        assert_eq!(
            ceilings.get("libprotobuf").map(Vec::as_slice),
            Some(["6.31.1".to_string()].as_slice())
        );
    }

    #[test]
    fn exact_match_spec_and_version_recognise_only_exact_pins() {
        assert_eq!(
            exact_match_spec("libprotobuf 6.31.1"),
            Some(ExactMatchSpec {
                name: "libprotobuf",
                version: "6.31.1"
            })
        );
        assert_eq!(
            exact_match_spec("ld_impl 2.44 h1423503_1"),
            Some(ExactMatchSpec {
                name: "ld_impl",
                version: "2.44"
            })
        );
        assert_eq!(exact_match_spec("python >=3.9"), None);
        assert_eq!(exact_match_spec("numpy 1.21.*"), None);
        assert_eq!(exact_match_spec("solo-name"), None); // unconstrained
        assert_eq!(exact_version("==6.31.1"), Some("6.31.1"));
        assert_eq!(exact_version("6.31.1"), Some("6.31.1"));
        assert_eq!(exact_version(">=2.44,<2.45"), None);
        assert_eq!(exact_version("3.12.*"), None);
        assert_eq!(exact_version("!=2.0"), None); // relational not-equal, caught by the `=`
        // A conda/PEP 440 epoch version is exact — the `!` separator must be accepted.
        assert_eq!(exact_version("1!2.0.3"), Some("1!2.0.3"));
        assert_eq!(
            exact_match_spec("openssl 1!1.1.1"),
            Some(ExactMatchSpec {
                name: "openssl",
                version: "1!1.1.1"
            })
        );
        // A PEP 508 marker token must not be mistaken for a version (a pypi-style list entry).
        assert_eq!(
            exact_match_spec("tomli ; python_full_version < \"3.11\""),
            None
        );
    }
}
