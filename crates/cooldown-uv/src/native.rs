use camino::Utf8Path;
use cooldown_core::{
    CoreError, NativePolicyLayer, NativeRule, PatternGlob, RawWindow, Result, Selector,
};
use cooldown_toml_util::read_toml_file;

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
    use super::parse_native;
    use camino::Utf8PathBuf;
    use cooldown_core::{CoreError, RawWindow};

    fn write_manifest(contents: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write manifest");
        (dir, path)
    }

    #[test]
    fn parse_native_errors_on_invalid_package_rule() {
        let (_dir, manifest) = write_manifest(
            r#"
[tool.uv]
exclude-newer = "2026-06-01"

[tool.uv.exclude-newer-package]
"[" = false
"#,
        );

        let err = parse_native(&manifest).expect_err("invalid native config must fail");
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn parse_native_reads_valid_rules() {
        let (_dir, manifest) = write_manifest(
            r#"
[tool.uv]
exclude-newer = "2026-06-01"

[tool.uv.exclude-newer-package]
requests = false
"#,
        );

        let layer = parse_native(&manifest)
            .expect("valid native config")
            .expect("native layer");
        assert_eq!(layer.rules.len(), 2);
        assert!(matches!(layer.rules[0].window, RawWindow::AbsoluteDate(_)));
        assert!(matches!(layer.rules[1].window, RawWindow::OptOut));
    }
}
