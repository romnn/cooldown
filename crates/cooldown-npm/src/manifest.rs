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
