//! Parsing the two PyPI-backed manifests cooldown reads for non-uv Python projects: pip's
//! `requirements.txt` (pinned `name==version` lines) and Poetry's `poetry.lock` (a TOML list of
//! resolved packages), with `pyproject.toml` supplying Poetry's direct-dependency set.

use std::collections::HashSet;

/// Normalises a distribution name per PEP 503 (lowercase; runs of `_`/`.`/`-` collapse to a single
/// `-`), so a `pyproject.toml` key matches the name Poetry records in the lock.
fn normalize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut prev_dash = false;
    for c in name.trim().chars() {
        if matches!(c, '_' | '.' | '-') {
            if !prev_dash {
                out.push('-');
                prev_dash = true;
            }
        } else {
            out.extend(c.to_lowercase());
            prev_dash = false;
        }
    }
    out
}

/// Parses pinned `name==version` requirements. Unpinned (`>=`, `~=`), options (`-r`, `-e`,
/// `--hash`), environment markers (`; python_version < …`), extras (`pkg[extra]`), and comments are
/// handled or skipped, leaving the exact-pinned distributions cooldown can reason about.
#[must_use]
pub fn parse_requirements(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in content.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('-') {
            continue;
        }
        // Drop an environment marker and any trailing hash options.
        let line = line.split(';').next().unwrap_or("").trim();
        let line = line.split_whitespace().next().unwrap_or("");
        if let Some((name, version)) = line.split_once("==") {
            let name = name.split('[').next().unwrap_or(name).trim();
            let version = version.trim();
            if !name.is_empty() && !version.is_empty() {
                out.push((name.to_string(), version.to_string()));
            }
        }
    }
    out
}

/// Parses the `[[package]]` entries of a `poetry.lock` into resolved `(name, version)`.
#[must_use]
pub fn parse_poetry_lock(content: &str) -> Vec<(String, String)> {
    #[derive(serde::Deserialize)]
    struct Lock {
        #[serde(default)]
        package: Vec<Package>,
    }
    #[derive(serde::Deserialize)]
    struct Package {
        name: String,
        version: String,
    }
    toml::from_str::<Lock>(content)
        .map(|lock| {
            lock.package
                .into_iter()
                .map(|p| (p.name, p.version))
                .collect()
        })
        .unwrap_or_default()
}

/// Returns the normalised set of distributions a Poetry `pyproject.toml` declares directly, reading
/// both the classic `[tool.poetry.dependencies]` table and the PEP 621 `[project.dependencies]`
/// list. The implicit `python` constraint is excluded.
#[must_use]
pub fn parse_poetry_direct(manifest: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    let Ok(doc) = toml::from_str::<toml::Value>(manifest) else {
        return out;
    };
    if let Some(table) = doc
        .get("tool")
        .and_then(|t| t.get("poetry"))
        .and_then(|p| p.get("dependencies"))
        .and_then(toml::Value::as_table)
    {
        for key in table.keys() {
            if key != "python" {
                out.insert(normalize(key));
            }
        }
    }
    if let Some(list) = doc
        .get("project")
        .and_then(|p| p.get("dependencies"))
        .and_then(toml::Value::as_array)
    {
        for item in list.iter().filter_map(toml::Value::as_str) {
            if let Some(name) = pep508_name(item) {
                out.insert(normalize(name));
            }
        }
    }
    out
}

/// Extracts the distribution name from a PEP 508 requirement string (`requests[socks]>=2.28` →
/// `requests`), i.e. the leading run of name characters.
fn pep508_name(requirement: &str) -> Option<&str> {
    let req = requirement.trim();
    let end = req
        .find(|c: char| !(c.is_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(req.len());
    let name = req.get(..end)?;
    (!name.is_empty()).then_some(name)
}

/// Whether `lock_name` (as recorded in `poetry.lock`) is in the normalised `direct` set.
#[must_use]
pub fn is_direct<S: std::hash::BuildHasher>(direct: &HashSet<String, S>, lock_name: &str) -> bool {
    direct.contains(&normalize(lock_name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requirements_take_exact_pins_only() {
        let reqs = "# comment\nrequests==2.28.0\nflask==2.2.0  # web\nclick>=8.0\n-e .\nrich[jupyter]==12.0.0 ; python_version >= '3.7'\n";
        let mut got = parse_requirements(reqs);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("flask".to_string(), "2.2.0".to_string()),
                ("requests".to_string(), "2.28.0".to_string()),
                ("rich".to_string(), "12.0.0".to_string()),
            ]
        );
    }

    #[test]
    fn poetry_lock_and_direct() {
        let lock = "[[package]]\nname = \"requests\"\nversion = \"2.28.0\"\n\n[[package]]\nname = \"urllib3\"\nversion = \"1.26.0\"\n";
        let mut got = parse_poetry_lock(lock);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("requests".to_string(), "2.28.0".to_string()),
                ("urllib3".to_string(), "1.26.0".to_string()),
            ]
        );

        let manifest = "[tool.poetry.dependencies]\npython = \"^3.10\"\nRequests = \"^2.28\"\n";
        let direct = parse_poetry_direct(manifest);
        assert!(is_direct(&direct, "requests")); // normalised match
        assert!(!is_direct(&direct, "urllib3"));
    }
}
