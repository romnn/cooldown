use camino::Utf8Path;
use cooldown_core::{CoreError, NativePolicyLayer, NativeRule, RawWindow, Result, Selector};
use cooldown_toml_util::read_toml_file;

/// Parse `[package.metadata.cooldown]` / `[workspace.metadata.cooldown]` `min-age` into a native rule.
pub(crate) fn parse_native(manifest: &Utf8Path) -> Result<Option<NativePolicyLayer>> {
    let Some(value) = read_toml_file::<toml::Value>(manifest, "Cargo.toml")? else {
        return Ok(None);
    };
    let cooldown = value
        .get("package")
        .and_then(|package| package.get("metadata"))
        .and_then(|metadata| metadata.get("cooldown"))
        .or_else(|| {
            value
                .get("workspace")
                .and_then(|workspace| workspace.get("metadata"))
                .and_then(|metadata| metadata.get("cooldown"))
        });
    let Some(cooldown) = cooldown else {
        return Ok(None);
    };
    let min_age = cooldown
        .get("min-age")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| {
            CoreError::Config(format!(
                "{manifest}: [package.metadata.cooldown] min-age must be a string"
            ))
        })?;
    let window = cooldown_core::duration::parse_duration(min_age)
        .map(RawWindow::RelativeDuration)
        .map_err(|error| {
            CoreError::Config(format!("{manifest}: invalid native min-age: {error}"))
        })?;
    Ok(Some(NativePolicyLayer {
        rules: vec![NativeRule {
            selector: Selector::Default,
            window,
        }],
    }))
}

#[cfg(test)]
mod tests {
    use super::parse_native;
    use camino::Utf8PathBuf;
    use cooldown_core::{CoreError, RawWindow};
    use indoc::indoc;

    /// A `Cargo.toml` written into its own temporary directory.
    struct TempManifest {
        /// Owns the temporary directory; dropping it deletes the manifest from disk.
        directory: tempfile::TempDir,
        /// The path of the `Cargo.toml` inside the temporary directory.
        manifest: Utf8PathBuf,
    }

    fn write_manifest(contents: &str) -> TempManifest {
        let directory = tempfile::tempdir().expect("tempdir");
        let manifest =
            Utf8PathBuf::from_path_buf(directory.path().join("Cargo.toml")).expect("utf8 path");
        std::fs::write(&manifest, contents).expect("write manifest");
        TempManifest {
            directory,
            manifest,
        }
    }

    #[test]
    fn parse_native_errors_on_invalid_min_age() {
        let TempManifest {
            directory: _directory,
            manifest,
        } = write_manifest(indoc! {r#"

            [package]
            name = "demo"
            version = "0.1.0"

            [package.metadata.cooldown]
            min-age = "not-a-duration"
        "#});

        let err = parse_native(&manifest).expect_err("invalid native config must fail");
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn parse_native_reads_valid_package_metadata() {
        let TempManifest {
            directory: _directory,
            manifest,
        } = write_manifest(indoc! {r#"

            [package]
            name = "demo"
            version = "0.1.0"

            [package.metadata.cooldown]
            min-age = "14d"
        "#});

        let layer = parse_native(&manifest)
            .expect("valid native config")
            .expect("native layer");
        assert_eq!(layer.rules.len(), 1);
        assert!(matches!(
            layer.rules[0].window,
            RawWindow::RelativeDuration(_)
        ));
    }
}
