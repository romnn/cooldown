//! Parsing the two conda-world lockfiles cooldown reads: `conda-lock.yml` (the conda-lock tool's
//! output) and `pixi.lock` (pixi's). Both pin a mix of conda-channel packages and PyPI packages, so
//! each resolved entry carries whether it came from conda or pip — the adapter routes its publish
//! time to the matching registry. The lockfiles are read with a small line scanner rather than a
//! full YAML dependency, since only a few fields per entry are needed.

use std::collections::HashSet;

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

/// Parses `pixi.lock`: a `packages:` list whose entries are `- conda: <url>` (name/version encoded
/// in the artifact filename) or `- pypi: <url>` (with explicit `name`/`version` fields, falling
/// back to the wheel/sdist filename). Duplicates across platforms are collapsed.
#[must_use]
pub fn parse_pixi_lock(content: &str) -> Vec<CondaDep> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut in_packages = false;
    // The in-progress entry: (is_conda, url, explicit name, explicit version).
    let mut cur: Option<(bool, String, Option<String>, Option<String>)> = None;

    let flush = |cur: &mut Option<(bool, String, Option<String>, Option<String>)>,
                 seen: &mut HashSet<CondaDep>,
                 out: &mut Vec<CondaDep>| {
        let Some((conda, url, name, version)) = cur.take() else {
            return;
        };
        let dep = if conda {
            conda_from_url(&url)
        } else {
            match (name, version) {
                (Some(n), Some(v)) => Some(CondaDep {
                    name: n,
                    version: v,
                    conda: false,
                }),
                _ => pypi_from_url(&url),
            }
        };
        if let Some(dep) = dep
            && seen.insert(CondaDep {
                name: dep.name.clone(),
                version: dep.version.clone(),
                conda: dep.conda,
            })
        {
            out.push(dep);
        }
    };

    for line in content.lines() {
        if !line.starts_with([' ', '-']) && !line.trim().is_empty() {
            flush(&mut cur, &mut seen, &mut out);
            in_packages = line.starts_with("packages:");
            continue;
        }
        if !in_packages {
            continue;
        }
        let trimmed = line.trim_start();
        if let Some(url) = trimmed.strip_prefix("- conda:") {
            flush(&mut cur, &mut seen, &mut out);
            cur = Some((true, clean(url), None, None));
        } else if let Some(url) = trimmed.strip_prefix("- pypi:") {
            flush(&mut cur, &mut seen, &mut out);
            cur = Some((false, clean(url), None, None));
        } else if let Some(rest) = trimmed.strip_prefix("name:")
            && let Some(entry) = cur.as_mut()
        {
            entry.2 = Some(clean(rest));
        } else if let Some(rest) = trimmed.strip_prefix("version:")
            && let Some(entry) = cur.as_mut()
        {
            entry.3 = Some(clean(rest));
        }
    }
    flush(&mut cur, &mut seen, &mut out);
    out
}

fn clean(s: &str) -> String {
    s.trim().trim_matches(['"', '\'']).to_string()
}

/// Extracts `(name, version)` from a conda artifact URL: the filename is
/// `{name}-{version}-{build}.{conda,tar.bz2}`, where neither version nor build contains a `-`.
fn conda_from_url(url: &str) -> Option<CondaDep> {
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
fn pypi_from_url(url: &str) -> Option<CondaDep> {
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
    fn pixi_lock_reads_urls_and_fields() {
        let lock = "version: 6\npackages:\n- conda: https://conda.anaconda.org/conda-forge/linux-64/python-dateutil-2.8.2-pyhd8ed1ab_0.conda\n  sha256: abc\n- pypi: https://files.pythonhosted.org/x/requests-2.28.0-py3-none-any.whl\n  name: requests\n  version: 2.28.0\n";
        let deps = parse_pixi_lock(lock);
        assert!(deps.contains(&CondaDep {
            name: "python-dateutil".into(),
            version: "2.8.2".into(),
            conda: true
        }));
        assert!(deps.contains(&CondaDep {
            name: "requests".into(),
            version: "2.28.0".into(),
            conda: false
        }));
    }
}
