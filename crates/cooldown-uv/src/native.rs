use camino::Utf8Path;
use cooldown_core::{
    CoreError, NativePolicyLayer, NativeRule, PatternGlob, RawWindow, ResolvedPolicy, Result,
    Selector, SyncReport, WindowSpec,
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

const SECS_PER_DAY: i64 = 86_400;

/// Write the resolved default window into `[tool.uv] exclude-newer`, format-preserving and
/// idempotent (via [`cooldown_toml_util::set_toml_string`]).
///
/// uv has a single `exclude-newer`, so only the policy's default window is synced — per-kind windows
/// and per-package rules are not expressed. A `Latest`/zero window means "no cooldown", which leaves
/// the manifest untouched (reported as [`SyncReport::Unchanged`]).
///
/// Under `dry_run` the manifest is never written; the report still reflects what would change.
///
/// # Errors
///
/// Returns a [`CoreError`] if the manifest cannot be parsed or written.
pub(crate) fn write_native(
    manifest: &Utf8Path,
    policy: &ResolvedPolicy,
    dry_run: bool,
) -> Result<SyncReport> {
    let Some(value) = policy
        .default_window
        .as_ref()
        .and_then(format_exclude_newer)
    else {
        // No default window, or an opt-out — nothing to bake into the native config.
        return Ok(SyncReport::Unchanged {
            path: manifest.to_owned(),
        });
    };
    let changed = cooldown_toml_util::set_toml_string(
        manifest,
        &["tool", "uv", "exclude-newer"],
        &value,
        dry_run,
    )?;
    let path = manifest.to_owned();
    Ok(if changed {
        SyncReport::Written { path }
    } else {
        SyncReport::Unchanged { path }
    })
}

/// Render a window as a uv `exclude-newer` string that round-trips through `parse_duration`: whole
/// days/hours as a friendly span ("14 days", "36 hours"), otherwise seconds; an absolute freeze as
/// its RFC3339 instant. `Latest` (an explicit opt-out / zero window) yields `None`.
fn format_exclude_newer(spec: &WindowSpec) -> Option<String> {
    match spec {
        WindowSpec::MinAge(duration) => {
            let secs = duration.as_secs();
            if secs <= 0 {
                None
            } else if secs % SECS_PER_DAY == 0 {
                Some(friendly_span(secs / SECS_PER_DAY, "day"))
            } else if secs % 3_600 == 0 {
                Some(friendly_span(secs / 3_600, "hour"))
            } else {
                Some(friendly_span(secs, "second"))
            }
        }
        WindowSpec::Freeze(timestamp) => Some(timestamp.to_string()),
        WindowSpec::Latest => None,
    }
}

fn friendly_span(count: i64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
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
