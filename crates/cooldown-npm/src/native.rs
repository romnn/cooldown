//! pnpm native cooldown configuration parsing and text-preserving updates.

use camino::Utf8Path;
use cooldown_core::{CoreError, Result, WindowSpec};
use std::collections::BTreeMap;

#[derive(serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum ConfigStringList {
    One(String),
    Many(Vec<String>),
}

impl ConfigStringList {
    pub(crate) fn into_vec(self) -> Vec<String> {
        match self {
            ConfigStringList::One(value) => vec![value],
            ConfigStringList::Many(values) => values,
        }
    }
}

impl Default for ConfigStringList {
    fn default() -> Self {
        ConfigStringList::Many(Vec::new())
    }
}

/// Converts a cooldown window into pnpm's rolling whole-minute representation.
pub(crate) fn window_minutes(spec: &WindowSpec) -> Option<i64> {
    match spec {
        WindowSpec::MinAge(duration) => {
            let minutes = duration.as_secs() / 60;
            (minutes > 0).then_some(minutes)
        }
        WindowSpec::Freeze(_) | WindowSpec::Latest => None,
    }
}

/// Sets one top-level YAML scalar while preserving comments and key order.
///
/// pnpm settings are top-level scalars, so a line-level edit avoids a full YAML round trip that
/// would drop comments.
/// A missing file is created unless `dry_run` is enabled, and the return value reports whether the
/// file changed or would change.
///
/// # Errors
///
/// Returns a [`CoreError`] when the file cannot be read or written.
pub(crate) fn set_yaml_scalar(
    path: &Utf8Path,
    key: &str,
    value: &str,
    dry_run: bool,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
    };
    let target = format!("{key}: {value}");
    let prefix = format!("{key}:");
    let mut lines: Vec<String> = Vec::new();
    let mut found = false;
    let mut changed = false;
    for line in content.lines() {
        // The colon prevents a short key from matching a longer key with the same prefix.
        if !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            if line == target {
                lines.push(line.to_string());
            } else {
                changed = true;
                lines.push(target.clone());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    if !found {
        if !dry_run {
            let mut out = target;
            out.push('\n');
            out.push_str(&content);
            std::fs::write(path, out)
                .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
        }
        return Ok(true);
    }
    if changed && !dry_run {
        let mut out = lines.join("\n");
        if content.ends_with('\n') {
            out.push('\n');
        }
        std::fs::write(path, out)
            .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    }
    Ok(changed)
}

/// Sets one top-level YAML block sequence while preserving the rest of the document.
///
/// An empty item list removes the key and its block so native configuration never retains an empty
/// exemption list.
/// Items are emitted as double-quoted scalars in caller-provided order, which safely preserves
/// scoped names such as `@scope/pkg` and glob patterns such as `@scope/*`.
/// A missing file is created only for a non-empty list and when `dry_run` is disabled.
///
/// # Errors
///
/// Returns a [`CoreError`] when the file cannot be read or written.
pub(crate) fn set_yaml_block_list(
    path: &Utf8Path,
    key: &str,
    items: &[String],
    dry_run: bool,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
    };
    // An empty desired block means the key should be absent.
    let desired: Vec<String> = if items.is_empty() {
        Vec::new()
    } else {
        std::iter::once(format!("{key}:"))
            .chain(items.iter().map(|item| format!("  - \"{item}\"")))
            .collect()
    };

    let prefix = format!("{key}:");
    let mut out: Vec<String> = Vec::new();
    let mut existing: Vec<String> = Vec::new();
    let mut found = false;
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            // A top-level key has no indentation; its block is the following indented lines.
            found = true;
            existing.push(line.to_string());
            while lines
                .peek()
                .is_some_and(|next| next.starts_with(char::is_whitespace))
            {
                existing.push(lines.next().unwrap_or_default().to_string());
            }
            // Replace the old block in place, or remove it when the desired block is empty.
            out.extend(desired.iter().cloned());
        } else {
            out.push(line.to_string());
        }
    }

    let changed = if found {
        existing != desired
    } else {
        !desired.is_empty()
    };
    if !changed || dry_run {
        return Ok(changed);
    }

    let mut text = if found {
        out.join("\n")
    } else {
        // New keys follow the existing document without disturbing its text.
        let mut text = content.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&desired.join("\n"));
        text
    };
    if content.ends_with('\n') || !found {
        text.push('\n');
    }
    std::fs::write(path, text)
        .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    Ok(true)
}

/// Sets one top-level YAML string map while preserving the rest of the document.
///
/// The repair path restores the original file after using this temporary map, so comments inside
/// the original block reappear unchanged.
///
/// # Errors
///
/// Returns a [`CoreError`] when values cannot be serialized or the file cannot be read or written.
pub(crate) fn set_yaml_string_map(
    path: &Utf8Path,
    key: &str,
    items: &BTreeMap<String, String>,
) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(CoreError::Filesystem(format!("{path}: {error}"))),
    };
    let mut desired = Vec::new();
    if !items.is_empty() {
        desired.push(format!("{key}:"));
        for (item_key, value) in items {
            let item_key = serde_json::to_string(item_key)
                .map_err(|error| CoreError::Serialization(error.to_string()))?;
            let value = serde_json::to_string(value)
                .map_err(|error| CoreError::Serialization(error.to_string()))?;
            desired.push(format!("  {item_key}: {value}"));
        }
    }

    let prefix = format!("{key}:");
    let mut out = Vec::new();
    let mut existing = Vec::new();
    let mut found = false;
    let mut lines = content.lines().peekable();
    while let Some(line) = lines.next() {
        if !found && !line.starts_with(char::is_whitespace) && line.starts_with(&prefix) {
            found = true;
            existing.push(line.to_string());
            while lines
                .peek()
                .is_some_and(|next| next.starts_with(char::is_whitespace))
            {
                existing.push(lines.next().unwrap_or_default().to_string());
            }
            out.extend(desired.iter().cloned());
        } else {
            out.push(line.to_string());
        }
    }

    let changed = if found {
        existing != desired
    } else {
        !desired.is_empty()
    };
    if !changed {
        return Ok(false);
    }

    let mut text = if found {
        out.join("\n")
    } else {
        let mut text = content.clone();
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(&desired.join("\n"));
        text
    };
    if content.ends_with('\n') || !found {
        text.push('\n');
    }
    std::fs::write(path, text)
        .map_err(|error| CoreError::Filesystem(format!("{path}: {error}")))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use color_eyre::eyre;

    #[test]
    fn configured_string_list_accepts_pnpm_singletons_and_arrays() -> eyre::Result<()> {
        let one = serde_json::from_str::<ConfigStringList>("\"nanoid\"")?;
        let many = serde_json::from_str::<ConfigStringList>("[\"nanoid\", \"eslint\"]")?;

        assert_eq!(one.into_vec(), vec!["nanoid".to_string()]);
        assert_eq!(
            many.into_vec(),
            vec!["nanoid".to_string(), "eslint".to_string()]
        );
        Ok(())
    }

    #[test]
    fn set_yaml_scalar_adds_updates_and_is_idempotent() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(&path, "packages:\n  - \"a\"\n# keep me\n")?;

        // Absent key → prepended, comments and existing content preserved.
        assert!(set_yaml_scalar(&path, "minimumReleaseAge", "20160", false)?);
        let after = std::fs::read_to_string(&path)?;
        assert!(after.contains("minimumReleaseAge: 20160"));
        assert!(after.contains("# keep me"), "comments preserved");
        assert!(after.contains("packages:"), "existing content preserved");

        // Idempotent.
        assert!(!set_yaml_scalar(
            &path,
            "minimumReleaseAge",
            "20160",
            false
        )?);

        // Update in place.
        assert!(set_yaml_scalar(&path, "minimumReleaseAge", "30", false)?);
        let updated = std::fs::read_to_string(&path)?;
        assert!(updated.contains("minimumReleaseAge: 30"));
        assert!(!updated.contains("20160"));
        Ok(())
    }

    #[test]
    fn set_yaml_scalar_dry_run_reports_change_without_writing() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        let before = "packages:\n  - \"a\"\n";
        std::fs::write(&path, before)?;

        // Dry run on an absent key reports it would change but writes nothing.
        assert!(set_yaml_scalar(&path, "minimumReleaseAge", "20160", true)?);
        assert_eq!(std::fs::read_to_string(&path)?, before);

        // Dry run on a missing file reports a change but does not create the file.
        let missing = Utf8PathBuf::from_path_buf(dir.path().join("absent.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        assert!(set_yaml_scalar(
            &missing,
            "minimumReleaseAge",
            "20160",
            true
        )?);
        assert!(!missing.exists(), "dry run must not create the file");
        Ok(())
    }

    #[test]
    fn set_yaml_block_list_adds_updates_removes_and_is_idempotent() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            "minimumReleaseAge: 20160\npackages:\n  - \"a\"\n# keep me\n",
        )?;

        // Absent key → block appended, the rest of the document (scalar, packages, comment) preserved.
        let items = vec![
            "@typescript/native-preview".to_string(),
            "@scope/*".to_string(),
        ];
        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &items,
            false
        )?);
        let after = std::fs::read_to_string(&path)?;
        assert!(after.contains(
            "minimumReleaseAgeExclude:\n  - \"@typescript/native-preview\"\n  - \"@scope/*\""
        ));
        assert!(
            after.contains("minimumReleaseAge: 20160"),
            "scalar preserved"
        );
        assert!(after.contains("packages:"), "packages preserved");
        assert!(after.contains("# keep me"), "comment preserved");

        // Idempotent: the same items rewrite nothing.
        assert!(!set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &items,
            false
        )?);

        // Update in place: a different list replaces the block.
        let fewer = vec!["@typescript/native-preview".to_string()];
        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &fewer,
            false
        )?);
        let updated = std::fs::read_to_string(&path)?;
        assert!(
            updated.contains("minimumReleaseAgeExclude:\n  - \"@typescript/native-preview\"\n")
        );
        assert!(!updated.contains("@scope/*"), "dropped item is gone");
        assert!(updated.contains("# keep me"), "comment still preserved");

        // Empty list → the key and its block are removed entirely.
        assert!(set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &[],
            false
        )?);
        let removed = std::fs::read_to_string(&path)?;
        assert!(!removed.contains("minimumReleaseAgeExclude"), "key removed");
        assert!(
            removed.contains("minimumReleaseAge: 20160"),
            "scalar untouched"
        );
        // Removing again is a no-op.
        assert!(!set_yaml_block_list(
            &path,
            "minimumReleaseAgeExclude",
            &[],
            false
        )?);
        Ok(())
    }

    #[test]
    fn set_yaml_string_map_replaces_only_the_requested_block() -> eyre::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = Utf8PathBuf::from_path_buf(dir.path().join("pnpm-workspace.yaml"))
            .map_err(|path| eyre::eyre!("temporary path is not UTF-8: {}", path.display()))?;
        std::fs::write(
            &path,
            "minimumReleaseAge: 20160\noverrides:\n  existing: \"1.0.0\"\npackages:\n  - \"a\"\n# keep me\n",
        )?;
        let items = BTreeMap::from([
            ("@scope/pkg".to_string(), "2.0.0".to_string()),
            ("existing".to_string(), "1.1.0".to_string()),
        ]);

        assert!(set_yaml_string_map(&path, "overrides", &items)?);
        let written = std::fs::read_to_string(&path)?;
        assert!(
            written
                .contains("overrides:\n  \"@scope/pkg\": \"2.0.0\"\n  \"existing\": \"1.1.0\"\n")
        );
        assert!(written.contains("minimumReleaseAge: 20160"));
        assert!(written.contains("packages:\n  - \"a\""));
        assert!(written.contains("# keep me"));
        assert!(!set_yaml_string_map(&path, "overrides", &items)?);
        Ok(())
    }
}
