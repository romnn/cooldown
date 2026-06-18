//! Shared TOML manifest helpers used by ecosystem adapters.

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
/// Returns [`CoreError::Io`](cooldown_core::CoreError::Io) when the file exists but cannot be
/// read, or [`CoreError::Config`](cooldown_core::CoreError::Config) when its contents are not valid
/// TOML for `T`.
pub fn read_toml_file<T>(path: &Utf8Path, doc_name: &str) -> Result<Option<T>, CoreError>
where
    T: DeserializeOwned,
{
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CoreError::Io(format!("{path}: {e}"))),
    };
    toml::from_str(&content)
        .map(Some)
        .map_err(|e| CoreError::Config(format!("{path}: invalid {doc_name}: {e}")))
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
}
