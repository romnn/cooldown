//! Shared TOML manifest helpers used by tool adapters.

use camino::Utf8Path;
use cooldown_core::CoreError;
use serde::de::DeserializeOwned;

/// Read a TOML document from `path`, annotating I/O and parse errors with the local file path.
///
/// Returns `Ok(None)` when the file does not exist, so callers can treat absent optional manifests
/// as "no native policy" without special casing the filesystem.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`](cooldown_core::CoreError::Filesystem) when the file exists
/// but cannot be read, or [`CoreError::Config`](cooldown_core::CoreError::Config) when its
/// contents are not valid TOML for `T`.
pub fn read_toml_file<T>(path: &Utf8Path, doc_name: &str) -> Result<Option<T>, CoreError>
where
    T: DeserializeOwned,
{
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CoreError::Filesystem(format!("{path}: {e}"))),
    };
    toml::from_str(&content)
        .map(Some)
        .map_err(|e| CoreError::Config(format!("{path}: invalid {doc_name}: {e}")))
}

/// Parse a TOML file into a format-preserving [`toml_edit::DocumentMut`], or `None` if it is absent.
///
/// Use this for in-place manifest edits that must preserve comments, key order, and spacing (e.g.
/// rewriting a dependency's version requirement); pair it with [`write_document`].
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`](cooldown_core::CoreError::Filesystem) when the file exists but
/// cannot be read, or [`CoreError::Config`](cooldown_core::CoreError::Config) when it is not valid
/// TOML.
pub fn parse_document(path: &Utf8Path) -> Result<Option<toml_edit::DocumentMut>, CoreError> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CoreError::Filesystem(format!("{path}: {e}"))),
    };
    content
        .parse::<toml_edit::DocumentMut>()
        .map(Some)
        .map_err(|e| CoreError::Config(format!("{path}: invalid TOML: {e}")))
}

/// Write a format-preserving [`toml_edit::DocumentMut`] back to disk.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`](cooldown_core::CoreError::Filesystem) if the file cannot be
/// written.
pub fn write_document(path: &Utf8Path, doc: &toml_edit::DocumentMut) -> Result<(), CoreError> {
    std::fs::write(path, doc.to_string()).map_err(|e| CoreError::Filesystem(format!("{path}: {e}")))
}

/// Set a nested string value in a TOML file, format-preserving.
///
/// Navigates (creating intermediate tables as needed) to `keys` and sets the leaf to `val`, leaving
/// the rest of the document — comments, key order, spacing — untouched. The file is rewritten only
/// when the value actually changes: returns `Ok(true)` if it was (or, under `dry_run`, would be)
/// written, `Ok(false)` if the leaf already equalled `val` (so `sync` is idempotent and does not
/// churn unchanged manifests).
///
/// A missing file is treated as an empty document, so the leaf is written into a freshly created
/// file (needed for a repo-level `uv.toml` that does not exist yet).
///
/// When `dry_run` is set the file is never written; the return value still reports whether it would
/// have changed.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`](cooldown_core::CoreError::Filesystem) if the file cannot be
/// read or written, or [`CoreError::Config`](cooldown_core::CoreError::Config) if it is not valid
/// TOML, the key path is empty, or an intermediate key exists but is not a table.
pub fn set_toml_string(
    path: &Utf8Path,
    keys: &[&str],
    val: &str,
    dry_run: bool,
) -> Result<bool, CoreError> {
    let (last, parents) = keys
        .split_last()
        .ok_or_else(|| CoreError::Config("empty TOML key path".to_string()))?;
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(CoreError::Filesystem(format!("{path}: {e}"))),
    };
    let mut doc = content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| CoreError::Config(format!("{path}: invalid TOML: {e}")))?;

    let mut table = doc.as_table_mut();
    for key in parents {
        table = table
            .entry(key)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .ok_or_else(|| CoreError::Config(format!("{path}: [{key}] is not a table")))?;
    }

    if table.get(last).and_then(toml_edit::Item::as_str) == Some(val) {
        return Ok(false);
    }
    if dry_run {
        return Ok(true);
    }
    // Replacing the value via the existing key keeps the key's prefix decor (a leading `#` comment),
    // so a documented `exclude-newer` line keeps its comment; a missing key is inserted fresh.
    match table.get_mut(last) {
        Some(item) => *item = toml_edit::value(val),
        None => {
            table.insert(last, toml_edit::value(val));
        }
    }
    std::fs::write(path, doc.to_string())
        .map_err(|e| CoreError::Filesystem(format!("{path}: {e}")))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    #[derive(Debug, serde::Deserialize, PartialEq, Eq)]
    struct Doc {
        value: String,
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("missing.toml")).expect("utf8 path");
        let parsed = read_toml_file::<Doc>(&path, "missing.toml").expect("missing is allowed");
        assert!(parsed.is_none());
    }

    #[test]
    fn invalid_toml_is_annotated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("broken.toml")).expect("utf8 path");
        std::fs::write(&path, "value = [").expect("write");

        let err = read_toml_file::<Doc>(&path, "broken.toml").expect_err("must fail");
        assert!(matches!(err, CoreError::Config(_)));
    }

    #[test]
    fn valid_toml_parses() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("doc.toml")).expect("utf8 path");
        std::fs::write(&path, "value = \"ok\"").expect("write");

        let parsed = read_toml_file::<Doc>(&path, "doc.toml").expect("parse");
        assert_eq!(parsed.expect("document"), Doc { value: "ok".into() });
    }

    #[test]
    fn set_toml_string_updates_value_keeps_comment_and_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(
            &path,
            "[project]\nname = \"demo\"\n\n[tool.uv]\n# managed: edit the policy source instead\nexclude-newer = \"7 days\"\n",
        )
        .expect("write");

        let changed = set_toml_string(&path, &["tool", "uv", "exclude-newer"], "14 days", false)
            .expect("set");
        assert!(changed);
        let after = std::fs::read_to_string(&path).expect("read");
        assert!(after.contains("exclude-newer = \"14 days\""));
        assert!(
            after.contains("# managed: edit the policy source instead"),
            "the key's leading comment must be preserved"
        );
        assert!(after.contains("name = \"demo\""), "other tables untouched");

        // Idempotent: setting the same value again reports no change and rewrites nothing.
        let again = set_toml_string(&path, &["tool", "uv", "exclude-newer"], "14 days", false)
            .expect("set again");
        assert!(!again);
    }

    #[test]
    fn set_toml_string_dry_run_reports_change_without_writing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        let before = "[tool.uv]\nexclude-newer = \"7 days\"\n";
        std::fs::write(&path, before).expect("write");

        // Dry run reports it *would* change but leaves the file byte-for-byte identical.
        let would_change =
            set_toml_string(&path, &["tool", "uv", "exclude-newer"], "14 days", true).expect("dry");
        assert!(would_change);
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);

        // A dry run on an already-matching value reports no change either.
        let no_change =
            set_toml_string(&path, &["tool", "uv", "exclude-newer"], "7 days", true).expect("dry");
        assert!(!no_change);
    }

    #[test]
    fn set_toml_string_creates_missing_tables() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&path, "[project]\nname = \"demo\"\n").expect("write");

        let changed = set_toml_string(&path, &["tool", "uv", "exclude-newer"], "14 days", false)
            .expect("set");
        assert!(changed);
        let parsed = read_toml_file::<toml::Value>(&path, "pyproject.toml")
            .expect("read")
            .expect("doc");
        assert_eq!(
            parsed
                .get("tool")
                .and_then(|t| t.get("uv"))
                .and_then(|u| u.get("exclude-newer"))
                .and_then(toml::Value::as_str),
            Some("14 days")
        );
    }
}
