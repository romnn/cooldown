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

/// Replace the value `item` holds with `value`, keeping the old value's decor: the spacing around
/// it and a trailing same-line comment (`dep = "1"  # must match other-dep`), which `toml_edit`
/// stores on the value rather than the key.
///
/// An `item` that holds no value (a table, or nothing) is replaced with `value` in default decor.
pub fn replace_value(item: &mut toml_edit::Item, value: impl Into<toml_edit::Value>) {
    let mut value = value.into();
    if let Some(old) = item.as_value() {
        *value.decor_mut() = old.decor().clone();
    }
    *item = toml_edit::Item::Value(value);
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
    set_toml_value(path, keys, Some(val.into()), dry_run)
}

/// Set or remove a nested value in a TOML file, format-preserving.
///
/// Like [`set_toml_string`], but for any value, including an inline table, and `None` removes the
/// leaf instead, so a caller that owns a key can make it disappear again.
/// The comparison is semantic: an existing value spelled differently (a standard
/// `[exclude-newer-package]` table instead of an inline one, other key order or spacing) that holds
/// the same data counts as unchanged and is left as written.
/// Removing a key that is absent, including from a missing file, changes nothing and creates no
/// file.
///
/// # Errors
///
/// Returns [`CoreError::Filesystem`](cooldown_core::CoreError::Filesystem) if the file cannot be
/// read or written, or [`CoreError::Config`](cooldown_core::CoreError::Config) if it is not valid
/// TOML, the key path is empty, or an intermediate key exists but is not a table.
pub fn set_toml_value(
    path: &Utf8Path,
    keys: &[&str],
    val: Option<toml_edit::Value>,
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

    // Reject a non-table parent before the dry-run return, so a dry run fails exactly where the
    // real write would.
    let mut parent = Some(doc.as_table());
    for key in parents {
        parent = match parent.and_then(|table| table.get(key)) {
            None => None,
            Some(item) => Some(
                item.as_table()
                    .ok_or_else(|| CoreError::Config(format!("{path}: [{key}] is not a table")))?,
            ),
        };
    }
    let existing = existing_value(&content, keys)
        .map_err(|e| CoreError::Config(format!("{path}: invalid TOML: {e}")))?;
    let wanted = val
        .as_ref()
        .map(plain_value)
        .transpose()
        .map_err(|e| CoreError::Config(format!("{path}: unrepresentable TOML value: {e}")))?;
    if existing == wanted {
        return Ok(false);
    }
    if dry_run {
        return Ok(true);
    }

    let mut table = doc.as_table_mut();
    for key in parents {
        table = table
            .entry(key)
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_mut()
            .ok_or_else(|| CoreError::Config(format!("{path}: [{key}] is not a table")))?;
    }
    // Replacing the value via the existing key keeps the key's prefix decor (a leading `#` comment)
    // and the value's own (a trailing one), so a documented `exclude-newer` line keeps its
    // comments; a missing key is inserted fresh.
    match (val, table.get_mut(last)) {
        (Some(val), Some(item)) => replace_value(item, val),
        (Some(val), None) => {
            table.insert(last, toml_edit::Item::Value(val));
        }
        (None, _) => {
            table.remove(last);
        }
    }
    std::fs::write(path, doc.to_string())
        .map_err(|e| CoreError::Filesystem(format!("{path}: {e}")))?;
    Ok(true)
}

/// The plain data at `keys` in the TOML `content`, or `None` when any key on the path is absent.
fn existing_value(content: &str, keys: &[&str]) -> Result<Option<toml::Value>, toml::de::Error> {
    let mut value = toml::Value::Table(toml::from_str::<toml::Table>(content)?);
    for key in keys {
        match value {
            toml::Value::Table(mut table) => match table.remove(*key) {
                Some(next) => value = next,
                None => return Ok(None),
            },
            _ => return Ok(None),
        }
    }
    Ok(Some(value))
}

/// The plain data a format-preserving value holds, for a comparison that ignores its spelling.
fn plain_value(value: &toml_edit::Value) -> Result<toml::Value, toml::de::Error> {
    let mut table = toml::from_str::<toml::Table>(&format!("value = {value}"))?;
    table
        .remove("value")
        .ok_or_else(|| serde::de::Error::custom("the parsed document lost its only key"))
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
        std::assert_matches!(err, CoreError::Config(_));
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
    fn set_toml_string_keeps_a_trailing_comment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("uv.toml")).expect("utf8 path");
        std::fs::write(
            &path,
            "exclude-newer = \"7 days\"  # mirrors cooldown.toml\n",
        )
        .expect("write");

        set_toml_string(&path, &["exclude-newer"], "14 days", false).expect("set");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "exclude-newer = \"14 days\"  # mirrors cooldown.toml\n"
        );
    }

    #[test]
    fn replace_value_keeps_the_old_values_decor() {
        let mut doc = "a = \"1\"         # must match b\nb = { version = \"1\" }\n"
            .parse::<toml_edit::DocumentMut>()
            .expect("parse");

        replace_value(&mut doc["a"], "2");
        replace_value(&mut doc["b"]["version"], "2");

        assert_eq!(
            doc.to_string(),
            "a = \"2\"         # must match b\nb = { version = \"2\" }\n"
        );
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

    fn inline_table(entries: &[(&str, toml_edit::Value)]) -> toml_edit::Value {
        let mut table = toml_edit::InlineTable::new();
        for (key, value) in entries {
            table.insert(*key, value.clone());
        }
        table.fmt();
        table.into()
    }

    /// An owned table is inserted, rewritten when its entries change, and removed again, while
    /// the neighbouring key and its comment stay untouched.
    #[test]
    fn set_toml_value_inserts_rewrites_and_removes_a_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("uv.toml")).expect("utf8 path");
        std::fs::write(&path, "# repo window\nexclude-newer = \"14 days\"\n").expect("write");
        let key = ["exclude-newer-package"];

        // Insert
        let exempt = inline_table(&[("onnxruntime", false.into())]);
        assert!(set_toml_value(&path, &key, Some(exempt.clone()), false).expect("insert"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "# repo window\nexclude-newer = \"14 days\"\nexclude-newer-package = { onnxruntime = false }\n"
        );
        assert!(!set_toml_value(&path, &key, Some(exempt), false).expect("idempotent"));

        // Rewrite
        let both = inline_table(&[("numpy", "30 days".into()), ("onnxruntime", false.into())]);
        assert!(set_toml_value(&path, &key, Some(both), false).expect("rewrite"));
        assert!(
            std::fs::read_to_string(&path)
                .expect("read")
                .contains("exclude-newer-package = { numpy = \"30 days\", onnxruntime = false }")
        );

        // Remove
        assert!(set_toml_value(&path, &key, None, false).expect("remove"));
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "# repo window\nexclude-newer = \"14 days\"\n"
        );
        assert!(!set_toml_value(&path, &key, None, false).expect("already absent"));
    }

    /// The same data spelled as a standard table, in another order, is already in sync, so a
    /// hand-formatted table is not churned.
    #[test]
    fn set_toml_value_compares_data_not_spelling() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("uv.toml")).expect("utf8 path");
        let before = "[exclude-newer-package]\nonnxruntime = false\nnumpy = \"30 days\"\n";
        std::fs::write(&path, before).expect("write");

        let same = inline_table(&[("numpy", "30 days".into()), ("onnxruntime", false.into())]);
        assert!(
            !set_toml_value(&path, &["exclude-newer-package"], Some(same), false).expect("set")
        );
        assert_eq!(std::fs::read_to_string(&path).expect("read"), before);
    }

    /// Removing a key never creates a file, and a dry run reports a change without writing one.
    #[test]
    fn set_toml_value_removal_and_dry_run_leave_the_disk_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("uv.toml")).expect("utf8 path");
        let key = ["exclude-newer-package"];

        assert!(!set_toml_value(&path, &key, None, false).expect("remove from missing file"));
        let exempt = inline_table(&[("onnxruntime", false.into())]);
        assert!(set_toml_value(&path, &key, Some(exempt), true).expect("dry run"));
        assert!(!path.exists(), "neither call may create the file");
    }

    /// A dry run refuses a non-table parent the same way the real write does.
    #[test]
    fn set_toml_value_dry_run_rejects_a_non_table_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&path, "tool = 1\n").expect("write");
        let keys = ["tool", "uv", "exclude-newer"];

        for dry_run in [true, false] {
            let err = set_toml_value(&path, &keys, Some("14 days".into()), dry_run)
                .expect_err("a scalar parent must be rejected");
            std::assert_matches!(err, CoreError::Config(_));
        }
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "tool = 1\n");
    }
}
