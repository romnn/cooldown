use camino::Utf8Path;
use cooldown_core::{
    CoreError, NativePolicyLayer, NativeRule, PatternGlob, RawWindow, Result, Selector,
};
use cooldown_toml_util::read_toml_file;
use std::collections::HashSet;

/// The PEP 503-normalized names of the project's exact-pinned (`==x.y.z` / `===x.y.z`) dependencies,
/// read from `pyproject.toml`'s `[project.dependencies]`, `[project.optional-dependencies]`, and
/// PEP 735 `[dependency-groups]`. Such pins are held by default: uv cannot move them without editing
/// the manifest. A missing/unreadable manifest yields an empty set.
pub(crate) fn exact_pinned_names(manifest: &Utf8Path) -> HashSet<String> {
    let Ok(Some(value)) = read_toml_file::<toml::Value>(manifest, "pyproject.toml") else {
        return HashSet::new();
    };
    let mut names = HashSet::new();
    let mut scan = |reqs: Option<&toml::Value>| {
        if let Some(array) = reqs.and_then(toml::Value::as_array) {
            for req in array.iter().filter_map(toml::Value::as_str) {
                if let Some(name) = exact_pin(req) {
                    names.insert(name);
                }
            }
        }
    };
    let project = value.get("project");
    scan(project.and_then(|p| p.get("dependencies")));
    // `[project.optional-dependencies]` and `[dependency-groups]` are tables of requirement arrays.
    for table in [
        project.and_then(|p| p.get("optional-dependencies")),
        value.get("dependency-groups"),
    ]
    .into_iter()
    .flatten()
    .filter_map(toml::Value::as_table)
    {
        for group in table.values() {
            scan(Some(group));
        }
    }
    names
}

/// The PEP 503-normalized name of a requirement that is exact-pinned (`==X`/`===X`, no wildcard and
/// no second clause), or `None`. Handles the common PEP 508 shapes: `name==1.2.3`, `name == 1.2.3`,
/// `name[extra]==1.2.3`, and a trailing `; marker`.
fn exact_pin(requirement: &str) -> Option<String> {
    let req = requirement.split(';').next().unwrap_or(requirement).trim();
    let name_end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .unwrap_or(req.len());
    let name = &req[..name_end];
    if name.is_empty() {
        return None;
    }
    let mut rest = req[name_end..].trim_start();
    if let Some(after_bracket) = rest.strip_prefix('[') {
        rest = after_bracket
            .split_once(']')
            .map_or("", |(_, tail)| tail.trim_start());
    }
    if rest.contains(',') {
        return None; // a compound specifier (e.g. `>=1,<2`) is not an exact pin
    }
    let version = rest
        .strip_prefix("===")
        .or_else(|| rest.strip_prefix("=="))?
        .trim();
    (!version.is_empty() && !version.contains('*')).then(|| normalize_name(name))
}

/// The PEP 503-normalized package name of a PEP 508 requirement string (`httpx[http2]>=0.27 ; …` →
/// `httpx`), or `None` when no name is present. Unlike [`exact_pin`], this ignores the specifier — it
/// is used to match a requirement entry for rewriting, regardless of its constraint.
pub(crate) fn requirement_name(requirement: &str) -> Option<String> {
    let req = requirement.split(';').next().unwrap_or(requirement).trim();
    let name_end = req
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))
        .unwrap_or(req.len());
    let name = &req[..name_end];
    (!name.is_empty()).then(|| normalize_name(name))
}

/// PEP 503 name normalization: lowercase, with each run of `-`, `_`, `.` collapsed to a single `-`.
pub(crate) fn normalize_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut at_separator = false;
    for ch in name.chars() {
        if matches!(ch, '-' | '_' | '.') {
            if !at_separator && !out.is_empty() {
                out.push('-');
                at_separator = true;
            }
        } else {
            out.push(ch.to_ascii_lowercase());
            at_separator = false;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Parse `[tool.uv] exclude-newer` / `exclude-newer-package` into native rules.
pub(crate) fn parse_native(manifest: &Utf8Path) -> Result<Option<NativePolicyLayer>> {
    let Some(value) = read_toml_file::<toml::Value>(manifest, "pyproject.toml")? else {
        return Ok(None);
    };
    let Some(uv) = value.get("tool").and_then(|tool| tool.get("uv")) else {
        return Ok(None);
    };
    let mut rules = Vec::new();

    if let Some(exclude_newer) = uv.get("exclude-newer").and_then(|value| value.as_str())
        && let Some(window) = parse_raw_window(exclude_newer)
    {
        rules.push(NativeRule {
            selector: Selector::Default,
            window,
        });
    } else if uv.get("exclude-newer").is_some() {
        return Err(CoreError::Config(format!(
            "{manifest}: [tool.uv].exclude-newer must be a valid date/datetime or duration string"
        )));
    }
    if let Some(table) = uv
        .get("exclude-newer-package")
        .and_then(|value| value.as_table())
    {
        for (package, value) in table {
            let glob = PatternGlob::new(package).map_err(|error| {
                CoreError::Config(format!(
                    "{manifest}: invalid [tool.uv].exclude-newer-package pattern {package:?}: {error}"
                ))
            })?;
            let selector = Selector::Package(glob);
            if let Some(false) = value.as_bool() {
                rules.push(NativeRule {
                    selector,
                    window: RawWindow::OptOut,
                });
            } else if let Some(text) = value.as_str()
                && let Some(window) = parse_raw_window(text)
            {
                rules.push(NativeRule { selector, window });
            } else {
                return Err(CoreError::Config(format!(
                    "{manifest}: [tool.uv].exclude-newer-package.{package:?} must be false or a valid date/datetime or duration string"
                )));
            }
        }
    } else if uv.get("exclude-newer-package").is_some() {
        return Err(CoreError::Config(format!(
            "{manifest}: [tool.uv].exclude-newer-package must be a table"
        )));
    }
    if rules.is_empty() {
        Ok(None)
    } else {
        Ok(Some(NativePolicyLayer { rules }))
    }
}

/// `exclude-newer` is an RFC3339/date cutoff or a friendly/ISO span.
fn parse_raw_window(s: &str) -> Option<RawWindow> {
    if let Ok(timestamp) = cooldown_core::duration::parse_freeze(s) {
        return Some(RawWindow::AbsoluteDate(timestamp));
    }
    cooldown_core::duration::parse_duration(s)
        .ok()
        .map(RawWindow::RelativeDuration)
}

#[cfg(test)]
mod tests {
    use super::{exact_pin, exact_pinned_names, parse_native};
    use camino::Utf8PathBuf;
    use cooldown_core::{CoreError, RawWindow};
    use indoc::indoc;

    #[test]
    fn exact_pin_recognizes_only_exact_specifiers() {
        assert_eq!(exact_pin("protobuf==6.33.5").as_deref(), Some("protobuf"));
        assert_eq!(exact_pin("protobuf == 6.33.5").as_deref(), Some("protobuf"));
        assert_eq!(
            exact_pin("Types_Protobuf==6.1").as_deref(),
            Some("types-protobuf")
        );
        assert_eq!(exact_pin("httpx[http2]==0.27.0").as_deref(), Some("httpx"));
        assert_eq!(exact_pin("ruff===0.14.0").as_deref(), Some("ruff"));
        assert_eq!(
            exact_pin("foo==1.0.0 ; python_version<'3.13'").as_deref(),
            Some("foo")
        );
        // Ranges, prefix wildcards, and compound specifiers are not exact pins.
        assert_eq!(exact_pin("protobuf>=6.33"), None);
        assert_eq!(exact_pin("protobuf==6.33.*"), None);
        assert_eq!(exact_pin("protobuf>=6,<7"), None);
        assert_eq!(exact_pin("protobuf"), None);
    }

    #[test]
    fn exact_pinned_names_collects_across_dependency_tables() {
        let (_dir, manifest) = write_manifest(indoc! {r#"
            [project]
            dependencies = ["protobuf==6.33.5", "httpx>=0.27"]

            [project.optional-dependencies]
            grpc = ["grpcio==1.81.0"]

            [dependency-groups]
            dev = ["ruff>=0.14", "mypy==1.19.1"]
        "#});
        let pins = exact_pinned_names(&manifest);
        assert!(pins.contains("protobuf") && pins.contains("grpcio") && pins.contains("mypy"));
        assert!(!pins.contains("httpx") && !pins.contains("ruff"));
    }

    fn write_manifest(contents: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write manifest");
        (dir, path)
    }

    #[test]
    fn parse_native_errors_on_invalid_package_rule() {
        let (_dir, manifest) = write_manifest(indoc! {r#"
            [tool.uv]
            exclude-newer = "2026-06-01"

            [tool.uv.exclude-newer-package]
            "[" = false
        "#});

        let err = parse_native(&manifest).expect_err("invalid native config must fail");
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn parse_native_reads_valid_rules() {
        let (_dir, manifest) = write_manifest(indoc! {r#"
            [tool.uv]
            exclude-newer = "2026-06-01"

            [tool.uv.exclude-newer-package]
            requests = false
        "#});

        let layer = parse_native(&manifest)
            .expect("valid native config")
            .expect("native layer");
        assert_eq!(layer.rules.len(), 2);
        assert!(matches!(layer.rules[0].window, RawWindow::AbsoluteDate(_)));
        assert!(matches!(layer.rules[1].window, RawWindow::OptOut));
    }
}
