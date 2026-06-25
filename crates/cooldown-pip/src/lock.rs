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

/// Rewrite every exact pinned requirement for `name` to `target`, preserving comments and markers.
///
/// Returns `None` when the requirements file does not contain an editable exact pin for the package
/// or when every matching pin already names `target`.
#[must_use]
pub(crate) fn rewrite_requirement_pin(content: &str, name: &str, target: &str) -> Option<String> {
    let wanted = normalize(name);
    if requirement_pin_has_hashes(content, &wanted) {
        return None;
    }
    let mut changed = false;
    let mut out = String::with_capacity(content.len());
    for raw in content.split_inclusive('\n') {
        if let Some(rewritten) = rewrite_requirement_line(raw, &wanted, target) {
            changed = true;
            out.push_str(&rewritten);
        } else {
            out.push_str(raw);
        }
    }
    changed.then_some(out)
}

fn requirement_pin_has_hashes(content: &str, wanted: &str) -> bool {
    let require_hashes = content
        .lines()
        .any(|line| line_has_option(line, "--require-hashes"));
    let mut matching_continuation = false;
    let mut matching_pin = false;
    for raw in content.split_inclusive('\n') {
        if matching_continuation {
            if line_has_hash_option(raw) {
                return true;
            }
            matching_continuation = requirement_line_continues(raw);
            continue;
        }
        if requirement_line_matches(raw, wanted) {
            matching_pin = true;
            if line_has_hash_option(raw) {
                return true;
            }
            matching_continuation = requirement_line_continues(raw);
        }
    }
    require_hashes && matching_pin
}

fn requirement_line_matches(raw: &str, wanted: &str) -> bool {
    let body = line_without_ending(raw);
    let comment_start = body.find('#').unwrap_or(body.len());
    let Some(equals) = exact_pin_operator(body) else {
        return false;
    };
    if equals > comment_start {
        return false;
    }
    let requirement_name = body[..equals].trim().split('[').next().unwrap_or("");
    normalize(requirement_name) == wanted
}

fn requirement_line_continues(raw: &str) -> bool {
    line_without_ending(raw).trim_end().ends_with('\\')
}

fn line_has_hash_option(line: &str) -> bool {
    line_has_option(line, "--hash")
}

fn line_has_option(line: &str, option: &str) -> bool {
    let option_with_value = format!("{option}=");
    line.split('#')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .any(|token| token == option || token.starts_with(&option_with_value))
}

fn line_without_ending(raw: &str) -> &str {
    let without_lf = raw.strip_suffix('\n').unwrap_or(raw);
    without_lf.strip_suffix('\r').unwrap_or(without_lf)
}

fn exact_pin_operator(line: &str) -> Option<usize> {
    let mut start = 0;
    while let Some(offset) = line[start..].find("==") {
        let equals = start + offset;
        let previous = equals
            .checked_sub(1)
            .and_then(|index| line.as_bytes().get(index));
        let next = line.as_bytes().get(equals + 2);
        if previous != Some(&b'=') && next != Some(&b'=') {
            return Some(equals);
        }
        start = equals + 1;
    }
    None
}

fn rewrite_requirement_line(raw: &str, wanted: &str, target: &str) -> Option<String> {
    let (body, line_ending) = raw
        .strip_suffix('\n')
        .map_or((raw, ""), |body| (body, "\n"));
    let (body, carriage) = body
        .strip_suffix('\r')
        .map_or((body, ""), |body| (body, "\r"));
    let comment_start = body.find('#').unwrap_or(body.len());
    let equals = exact_pin_operator(body)?;
    if equals > comment_start {
        return None;
    }
    let requirement_name = body[..equals].trim().split('[').next().unwrap_or("");
    if normalize(requirement_name) != wanted {
        return None;
    }

    let mut version_start = equals + 2;
    while body
        .as_bytes()
        .get(version_start)
        .is_some_and(u8::is_ascii_whitespace)
    {
        version_start += 1;
    }
    let mut version_end = version_start;
    for (offset, ch) in body[version_start..].char_indices() {
        if ch.is_whitespace() || ch == ';' || ch == '#' {
            break;
        }
        version_end = version_start + offset + ch.len_utf8();
    }
    if version_end == version_start || body.get(version_start..version_end) == Some(target) {
        return None;
    }

    let mut rewritten = body.to_string();
    rewritten.replace_range(version_start..version_end, target);
    rewritten.push_str(carriage);
    rewritten.push_str(line_ending);
    Some(rewritten)
}

/// Parses pinned `name==version` requirements. Unpinned (`>=`, `~=`), options (`-r`, `-e`,
/// `--hash`), environment markers (`; python_version < …`), extras (`pkg[extra]`), and comments are
/// handled or skipped, leaving the exact-pinned distributions cooldown can reason about.
#[must_use]
pub fn parse_requirements(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for raw in content.lines() {
        if let Some((name, version)) = parse_requirement_pin(raw) {
            out.push((name.to_string(), version.to_string()));
        }
    }
    out
}

fn parse_requirement_pin(raw: &str) -> Option<(&str, &str)> {
    let line = raw.split('#').next().unwrap_or("").trim();
    if line.is_empty() || line.starts_with('-') {
        return None;
    }
    let line = line.split(';').next().unwrap_or("").trim();
    let equals = exact_pin_operator(line)?;
    let name = line[..equals].trim().split('[').next().unwrap_or("").trim();
    if name.is_empty() {
        return None;
    }

    let mut version_start = equals + 2;
    while line
        .as_bytes()
        .get(version_start)
        .is_some_and(u8::is_ascii_whitespace)
    {
        version_start += 1;
    }
    let mut version_end = version_start;
    for (offset, ch) in line[version_start..].char_indices() {
        if ch.is_whitespace() {
            break;
        }
        version_end = version_start + offset + ch.len_utf8();
    }
    let version = line.get(version_start..version_end)?;
    (!version.is_empty()).then_some((name, version))
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
    use indoc::indoc;

    #[test]
    fn requirements_take_exact_pins_only() {
        let reqs = "# comment\nrequests==2.28.0\nignored===1.0.0\nflask == 2.2.0  # web\nclick>=8.0\n-e .\nrich[jupyter] == 12.0.0 ; python_version >= '3.7'\n";
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
    fn rewrites_exact_requirement_pin_preserving_context() {
        let reqs = indoc! {"
            # comment
            Requests[socks] == 2.28.0 ; python_version >= '3.10'
            flask==2.2.0  # web
            click>=8.0
        "};
        let got = rewrite_requirement_pin(reqs, "requests", "2.31.0").expect("rewrite");
        assert_eq!(
            got,
            indoc! {"
                # comment
                Requests[socks] == 2.31.0 ; python_version >= '3.10'
                flask==2.2.0  # web
                click>=8.0
            "}
        );
    }

    #[test]
    fn rewrite_requirement_pin_ignores_uneditable_requirements() {
        let reqs = indoc! {"
            requests>=2.28
            requests===2.28.0
            -r base.txt
        "};
        assert!(rewrite_requirement_pin(reqs, "requests", "2.31.0").is_none());
    }

    #[test]
    fn rewrite_requirement_pin_refuses_hash_checked_requirements() {
        let inline = indoc! {"
            requests==2.28.0 --hash=sha256:old
        "};
        assert!(rewrite_requirement_pin(inline, "requests", "2.31.0").is_none());

        let continuation = indoc! {"
            requests==2.28.0 \\
                --hash=sha256:old
        "};
        assert!(rewrite_requirement_pin(continuation, "requests", "2.31.0").is_none());

        let require_hashes = indoc! {"
            --require-hashes
            requests==2.28.0
        "};
        assert!(rewrite_requirement_pin(require_hashes, "requests", "2.31.0").is_none());
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
