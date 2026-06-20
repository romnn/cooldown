//! Reading the direct-dependency names from a `package.json` manifest. A dependency is "direct"
//! exactly when the project declares it, regardless of which package manager produced the lock —
//! so this manifest read is the package-manager-agnostic source of truth for the direct/transitive
//! split, while the lockfile supplies the resolved versions.

use camino::Utf8Path;
use cooldown_core::{CoreError, Result};
use std::collections::HashSet;

/// The manifest fields whose keys name a directly-declared dependency.
const DEPENDENCY_FIELDS: [&str; 4] = [
    "dependencies",
    "devDependencies",
    "optionalDependencies",
    "peerDependencies",
];

/// Returns the set of package names the manifest declares as direct dependencies (across the
/// regular, dev, optional, and peer fields).
///
/// # Errors
///
/// Returns a [`CoreError`] if the manifest cannot be read or is not valid JSON.
pub fn direct_names(manifest: &Utf8Path) -> Result<HashSet<String>> {
    let content = std::fs::read_to_string(manifest)?;
    let doc: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| CoreError::Parse(format!("{manifest}: {e}")))?;
    let mut names = HashSet::new();
    for field in DEPENDENCY_FIELDS {
        if let Some(obj) = doc.get(field).and_then(|v| v.as_object()) {
            names.extend(obj.keys().cloned());
        }
    }
    Ok(names)
}

/// The declared version range/specifier for `name` in this manifest (the first match across the
/// regular, dev, optional, and peer fields), or `None` if the manifest is absent or does not declare
/// `name`. Used to decide whether an upgrade target stays within the author's range.
///
/// # Errors
///
/// Returns a [`CoreError`] if the manifest exists but cannot be read or is not valid JSON.
pub fn declared_range(manifest: &Utf8Path, name: &str) -> Result<Option<String>> {
    let content = match std::fs::read_to_string(manifest) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let doc: serde_json::Value =
        serde_json::from_str(&content).map_err(|e| CoreError::Parse(format!("{manifest}: {e}")))?;
    for field in DEPENDENCY_FIELDS {
        if let Some(range) = doc
            .get(field)
            .and_then(|section| section.get(name))
            .and_then(serde_json::Value::as_str)
        {
            return Ok(Some(range.to_string()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn manifest(contents: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("package.json")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write");
        (dir, path)
    }

    #[test]
    fn declared_range_finds_across_fields_and_reports_absence() {
        let (_dir, path) = manifest(
            r#"{ "dependencies": { "nanoid": "^3.0.0" }, "devDependencies": { "vitest": "~1.2.0" } }"#,
        );
        assert_eq!(declared_range(&path, "nanoid").expect("read").as_deref(), Some("^3.0.0"));
        assert_eq!(declared_range(&path, "vitest").expect("read").as_deref(), Some("~1.2.0"));
        assert_eq!(declared_range(&path, "absent").expect("read"), None);
    }

    #[test]
    fn declared_range_on_missing_manifest_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("nope.json")).expect("utf8 path");
        assert_eq!(declared_range(&path, "nanoid").expect("read"), None);
    }
}
