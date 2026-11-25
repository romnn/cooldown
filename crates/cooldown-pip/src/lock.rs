//! Parsing the two PyPI-backed manifests cooldown reads for non-uv Python projects: pip's
//! `requirements.txt` (pinned `name==version` lines) and Poetry's `poetry.lock` (a TOML list of
//! resolved packages), with `pyproject.toml` supplying Poetry's direct-dependency set.

use cooldown_core::CoreError;
use std::collections::HashSet;
use toml::{Table, Value};

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

/// A distribution and the exact version a requirements/lock file resolves it to.
#[derive(Debug, PartialEq)]
pub struct ResolvedPin {
    /// The distribution name as recorded in the file (extras stripped, not normalised).
    pub name: String,
    /// The exact version the file pins the distribution to.
    pub version: String,
    /// Whether the pin's origin is provably PyPI, given everything the file records: a
    /// requirements pin in a file without custom index/location directives, or a poetry entry
    /// without a `[package.source]` table (poetry records one exactly when the package did
    /// *not* come from the default PyPI source).
    pub pypi: bool,
}

/// Parses pinned `name==version` requirements. Unpinned (`>=`, `~=`), options (`-r`, `-e`,
/// `--hash`), environment markers (`; python_version < …`), extras (`pkg[extra]`), and comments are
/// handled or skipped, leaving the exact-pinned distributions cooldown can reason about.
///
/// pip records no per-distribution source, so one custom index or location directive anywhere
/// in the file makes every pin's origin unprovable — the whole file parses `pypi: false`.
#[must_use]
pub fn parse_requirements(content: &str) -> Vec<ResolvedPin> {
    let pypi = !declares_custom_index(content);
    let mut out = Vec::new();
    for raw in content.lines() {
        if let Some(pin) = parse_requirement_pin(raw) {
            out.push(ResolvedPin {
                name: pin.name.to_string(),
                version: pin.version.to_string(),
                pypi,
            });
        }
    }
    out
}

/// Whether the requirements file routes installation somewhere other than the default index.
/// Single-file testimony only; the adapter's `ambient` module follows `-r`/`-c` includes and the
/// out-of-file configuration surfaces a bare content parse cannot see.
fn declares_custom_index(content: &str) -> bool {
    content.lines().any(|raw| {
        let line = raw.split('#').next().unwrap_or("").trim();
        line_routes_custom(line)
    })
}

/// Whether one comment-stripped, trimmed requirements line routes installation somewhere other
/// than the default index: a custom index (`--index-url`/`-i`, `--extra-index-url`), direct
/// archive locations (`--find-links`/`-f`), or index suppression (`--no-index`).
pub(crate) fn line_routes_custom(line: &str) -> bool {
    [
        "--index-url",
        "--extra-index-url",
        "--find-links",
        "--no-index",
        "-i",
        "-f",
    ]
    .iter()
    .any(|directive| matches_directive(line, directive))
}

/// The file a `-r`/`--requirement`/`-c`/`--constraint` line includes, when it names one.
/// pip resolves the target relative to the file containing the line — the caller's business.
pub(crate) fn line_include_target(line: &str) -> Option<&str> {
    ["--requirement", "--constraint", "-r", "-c"]
        .iter()
        .find_map(|directive| {
            line.strip_prefix(directive)
                .and_then(|rest| rest.strip_prefix([' ', '\t', '=']))
                .map(str::trim)
        })
        .filter(|target| !target.is_empty())
}

fn matches_directive(line: &str, directive: &str) -> bool {
    line.strip_prefix(directive)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t', '=']))
}

/// An exact `name==version` pin found on one requirements line, borrowing from that line.
struct RequirementPin<'a> {
    /// The distribution name (extras stripped, not normalised).
    name: &'a str,
    /// The exactly pinned version.
    version: &'a str,
}

fn parse_requirement_pin(raw: &str) -> Option<RequirementPin<'_>> {
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
    (!version.is_empty()).then_some(RequirementPin { name, version })
}

/// Parses the `[[package]]` entries of a `poetry.lock` into resolved [`ResolvedPin`]s.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] if the lock is not TOML, or for a lock
/// [`poetry_lock_pins`] refuses.
pub fn parse_poetry_lock(content: &str) -> Result<Vec<ResolvedPin>, CoreError> {
    let lock: Table = toml::from_str(content)
        .map_err(|err| CoreError::LockUnreadable(format!("poetry.lock: {err}")))?;
    poetry_lock_pins(&lock)
}

/// Reads the `[[package]]` entries of a parsed `poetry.lock` as resolved [`ResolvedPin`]s.
///
/// Poetry writes the `package` array even when it is empty, so a lock without one is refused
/// rather than read as resolving nothing.
/// A package records a `source` table exactly when it did not come from the default PyPI source:
/// a `legacy` (custom index), `git`, `directory`, `file`, or `url` source.
///
/// # Errors
///
/// Returns [`CoreError::LockUnreadable`] if the lock lacks the `package` array or a package's name
/// or version.
pub(crate) fn poetry_lock_pins(lock: &Table) -> Result<Vec<ResolvedPin>, CoreError> {
    let unreadable = |what: &str| CoreError::LockUnreadable(format!("poetry.lock: {what}"));
    let packages = lock
        .get("package")
        .and_then(Value::as_array)
        .ok_or_else(|| unreadable("no `package` array"))?;
    packages
        .iter()
        .map(|package| {
            let field = |key: &str| {
                package
                    .get(key)
                    .and_then(Value::as_str)
                    .ok_or_else(|| unreadable(&format!("a package has no `{key}` string")))
            };
            Ok(ResolvedPin {
                name: field("name")?.to_string(),
                version: field("version")?.to_string(),
                pypi: package.get("source").is_none(),
            })
        })
        .collect()
}

/// Returns the normalised set of distributions a Poetry `pyproject.toml` declares directly: every
/// name in a table of [`poetry_dependency_tables`] or a list of [`requirement_lists`].
/// The implicit `python` constraint is excluded.
#[must_use]
pub fn parse_poetry_direct(manifest: &str) -> HashSet<String> {
    toml::from_str::<Table>(manifest)
        .map(|manifest| poetry_declared(&manifest))
        .unwrap_or_default()
}

/// [`parse_poetry_direct`] for a parsed `manifest`.
pub(crate) fn poetry_declared(manifest: &Table) -> HashSet<String> {
    declared_names(
        poetry_dependency_tables(manifest),
        requirement_lists(manifest),
    )
}

/// Returns the normalised set of distributions a Poetry `pyproject.toml` declares among its main
/// dependencies, `[tool.poetry.dependencies]` and `project.dependencies`, which is where
/// `poetry add` without `--group` or `--optional` writes.
#[must_use]
pub fn parse_poetry_main(manifest: &str) -> HashSet<String> {
    let Ok(manifest) = toml::from_str::<Table>(manifest) else {
        return HashSet::new();
    };
    let main = manifest
        .get("tool")
        .and_then(|tool| tool.get("poetry"))
        .and_then(|poetry| poetry.get("dependencies"))
        .and_then(Value::as_table);
    let requirements = manifest
        .get("project")
        .and_then(|project| project.get("dependencies"))
        .and_then(Value::as_array);
    declared_names(main, requirements)
}

/// The normalised names that the Poetry dependency `tables` and the PEP 508 requirement `lists`
/// declare, the implicit `python` constraint aside.
fn declared_names<'a>(
    tables: impl IntoIterator<Item = &'a Table>,
    lists: impl IntoIterator<Item = &'a Vec<Value>>,
) -> HashSet<String> {
    let tables = tables
        .into_iter()
        .flat_map(|table| table.keys())
        .filter(|name| *name != "python")
        .map(String::as_str);
    let requirements = lists
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(pep508_name);
    tables.chain(requirements).map(normalize).collect()
}

/// The Poetry dependency tables of a `pyproject.toml`: the main and the legacy development
/// dependencies, and every group's.
pub(crate) fn poetry_dependency_tables(manifest: &Table) -> Vec<&Table> {
    let Some(poetry) = manifest
        .get("tool")
        .and_then(|tool| tool.get("poetry"))
        .and_then(Value::as_table)
    else {
        return Vec::new();
    };
    let groups = poetry
        .get("group")
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(|groups| groups.values())
        .filter_map(|group| group.get("dependencies"));
    ["dependencies", "dev-dependencies"]
        .into_iter()
        .filter_map(|key| poetry.get(key))
        .chain(groups)
        .filter_map(Value::as_table)
        .collect()
}

/// The PEP 508 requirement lists of a `pyproject.toml`: `project.dependencies`, every
/// `project.optional-dependencies` extra, and every PEP 735 dependency group.
///
/// A dependency group may also hold `include-group` tables, which name no package.
pub(crate) fn requirement_lists(manifest: &Table) -> Vec<&Vec<Value>> {
    let project = manifest.get("project");
    let extras = project
        .and_then(|project| project.get("optional-dependencies"))
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(|extras| extras.values());
    let groups = manifest
        .get("dependency-groups")
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(|groups| groups.values());
    project
        .and_then(|project| project.get("dependencies"))
        .into_iter()
        .chain(extras)
        .chain(groups)
        .filter_map(Value::as_array)
        .collect()
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

    fn pin(name: &str, version: &str) -> ResolvedPin {
        ResolvedPin {
            name: name.to_string(),
            version: version.to_string(),
            pypi: true,
        }
    }

    #[test]
    fn requirements_take_exact_pins_only() {
        let reqs = "# comment\nrequests==2.28.0\nignored===1.0.0\nflask == 2.2.0  # web\nclick>=8.0\n-e .\nrich[jupyter] == 12.0.0 ; python_version >= '3.7'\n";
        let mut got = parse_requirements(reqs);
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got,
            vec![
                pin("flask", "2.2.0"),
                pin("requests", "2.28.0"),
                pin("rich", "12.0.0"),
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
        let mut got = parse_poetry_lock(lock).expect("lock");
        got.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got,
            vec![pin("requests", "2.28.0"), pin("urllib3", "1.26.0")]
        );

        let manifest = "[tool.poetry.dependencies]\npython = \"^3.10\"\nRequests = \"^2.28\"\n";
        let direct = parse_poetry_direct(manifest);
        assert!(is_direct(&direct, "requests")); // normalised match
        assert!(!is_direct(&direct, "urllib3"));
    }

    /// A lock that is not TOML, or that lacks the `package` array Poetry always writes, is an
    /// error rather than an empty inventory, while an explicit empty array reads as no packages.
    #[test]
    fn poetry_lock_reader_refuses_what_poetry_never_writes() {
        assert!(matches!(
            parse_poetry_lock("[[package]\n"),
            Err(CoreError::LockUnreadable(_))
        ));
        assert!(matches!(
            parse_poetry_lock(indoc! {r#"
                [metadata]
                lock-version = "2.1"
            "#}),
            Err(CoreError::LockUnreadable(_))
        ));
        assert_eq!(
            parse_poetry_lock(indoc! {r#"
                package = []

                [metadata]
                lock-version = "2.1"
            "#})
            .expect("empty lock"),
            Vec::new()
        );
    }
}
