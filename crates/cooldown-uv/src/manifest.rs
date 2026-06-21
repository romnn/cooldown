//! Format-preserving version-constraint rewrites for uv `pyproject.toml` manifests.
//!
//! `uv lock --upgrade-package` can only move the lock *within* a project's declared requirement, so a
//! bump past the constraint (a capped range, or a cross-major past an upper bound) needs the
//! `pyproject.toml` requirement itself rewritten. PEP 621 dependencies are PEP 508 requirement
//! strings in TOML arrays (`[project.dependencies]`, `[project.optional-dependencies]`, and PEP 735
//! `[dependency-groups]`); this rewrites just the version specifier of the matching entry via
//! `toml_edit`, preserving its name, extras, environment marker, and the array's own formatting.

use crate::native;
use camino::Utf8Path;
use cooldown_core::CoreError;
use cooldown_toml_util::{parse_document, write_document};
use toml_edit::{Array, DocumentMut, Formatted, Item, TableLike, Value};

/// Widen the requirement on `crate_name` in `manifest` so it admits `target`, across every PEP 621
/// dependency array.
///
/// Returns whether the manifest was modified. `false` means the dependency was declared with no
/// version specifier (a bare name or path/URL source) or is not declared in this manifest — there
/// was nothing to widen, so a lock-only move is the only available action.
///
/// # Errors
///
/// Returns a [`CoreError`] if the manifest exists but cannot be read, parsed, or written back.
pub fn widen_constraint(
    manifest: &Utf8Path,
    crate_name: &str,
    target: &str,
) -> Result<bool, CoreError> {
    let Some(mut doc) = parse_document(manifest)? else {
        return Ok(false);
    };
    let normalized = native::normalize_name(crate_name);
    if rewrite_document(&mut doc, &normalized, target) {
        write_document(manifest, &doc)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn rewrite_document(doc: &mut DocumentMut, name: &str, target: &str) -> bool {
    let mut changed = false;
    if let Some(array) = doc
        .get_mut("project")
        .and_then(|project| project.get_mut("dependencies"))
        .and_then(Item::as_array_mut)
    {
        changed |= rewrite_array(array, name, target);
    }
    changed |= rewrite_group_tables(doc, &["project", "optional-dependencies"], name, target);
    changed |= rewrite_group_tables(doc, &["dependency-groups"], name, target);
    changed
}

/// Rewrite the matching entry in every requirement array of a table-of-arrays section
/// (`[project.optional-dependencies]`, `[dependency-groups]`).
fn rewrite_group_tables(doc: &mut DocumentMut, path: &[&str], name: &str, target: &str) -> bool {
    let Some(table) = navigate_mut(doc, path) else {
        return false;
    };
    let keys: Vec<String> = table.iter().map(|(key, _)| key.to_string()).collect();
    let mut changed = false;
    for key in keys {
        if let Some(array) = table.get_mut(&key).and_then(Item::as_array_mut) {
            changed |= rewrite_array(array, name, target);
        }
    }
    changed
}

fn rewrite_array(array: &mut Array, name: &str, target: &str) -> bool {
    let mut changed = false;
    for value in array.iter_mut() {
        let Some(requirement) = value.as_str() else {
            continue;
        };
        if native::requirement_name(requirement).as_deref() != Some(name) {
            continue;
        }
        if let Some(rewritten) = bump_requirement(requirement, target) {
            set_string_preserving_decor(value, rewritten);
            changed = true;
        }
    }
    changed
}

fn navigate_mut<'doc>(
    doc: &'doc mut DocumentMut,
    path: &[&str],
) -> Option<&'doc mut dyn TableLike> {
    let mut table: &mut dyn TableLike = doc.as_table_mut();
    for key in path {
        table = table.get_mut(key)?.as_table_like_mut()?;
    }
    Some(table)
}

/// Replace an array element's string while keeping its surrounding whitespace and comments, so a
/// one-requirement-per-line array keeps its formatting.
fn set_string_preserving_decor(value: &mut Value, new: String) {
    if let Value::String(formatted) = value {
        let decor = formatted.decor().clone();
        let mut replacement = Formatted::new(new);
        *replacement.decor_mut() = decor;
        *formatted = replacement;
    }
}

/// Rewrite a PEP 508 requirement's version specifier to admit `target`, preserving the package name,
/// any extras, and a trailing environment marker. Returns `None` when there is no version specifier
/// to widen (a bare name or a path/URL source).
fn bump_requirement(requirement: &str, target: &str) -> Option<String> {
    let (head, marker) = match requirement.split_once(';') {
        Some((head, marker)) => (head, Some(marker.trim())),
        None => (requirement, None),
    };
    let head = head.trim();
    let name_end =
        head.find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))?;
    let (name, after_name) = head.split_at(name_end);
    if name.is_empty() {
        return None; // a specifier with no package name is not a rewritable requirement
    }
    let after_name = after_name.trim_start();
    let (extras, after_extras) = match after_name.strip_prefix('[') {
        Some(rest) => {
            let (inside, tail) = rest.split_once(']')?;
            (Some(inside), tail.trim_start())
        }
        None => (None, after_name),
    };
    let specifier = after_extras.trim();
    if specifier.is_empty() {
        return None; // bare name / path source: no version specifier to widen
    }
    let mut rewritten = name.to_string();
    if let Some(extras) = extras {
        rewritten.push('[');
        rewritten.push_str(extras);
        rewritten.push(']');
    }
    rewritten.push_str(&bump_specifier(specifier, target));
    if let Some(marker) = marker {
        rewritten.push_str("; ");
        rewritten.push_str(marker);
    }
    Some(rewritten)
}

/// Produce a PEP 440 specifier admitting `target`, preserving safe leading operators. A strict
/// lower bound becomes inclusive (`>1` → `>=target`). A compound, upper-bound-only, or not-equal
/// specifier widens to `>=target`, the least-surprising default that actually admits the target;
/// `==`/`===` pins never reach here (they are held and skipped before apply).
fn bump_specifier(specifier: &str, target: &str) -> String {
    // Unlike the cargo/npm rewriters, `target` is NOT stripped of a `+…` suffix: in PEP 440 a `+local`
    // segment is a *local version identifier* (significant, unlike semver build metadata), and PyPI
    // rejects local versions — so a registry-resolved target never carries one to begin with.
    let specifier = specifier.trim();
    if specifier.contains(',') || specifier.starts_with('<') || specifier.starts_with("!=") {
        return format!(">={target}");
    }
    if specifier.starts_with('>') {
        return format!(">={target}");
    }
    for op in ["===", "==", "~="] {
        if specifier.starts_with(op) {
            return format!("{op}{target}");
        }
    }
    format!(">={target}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;

    fn write_manifest(contents: &str) -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path =
            Utf8PathBuf::from_path_buf(dir.path().join("pyproject.toml")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write");
        (dir, path)
    }

    #[test]
    fn bump_requirement_preserves_name_extras_and_marker() {
        assert_eq!(
            bump_requirement("httpx>=0.27", "0.30.0").as_deref(),
            Some("httpx>=0.30.0")
        );
        assert_eq!(
            bump_requirement("httpx[http2]>=0.27", "0.30.0").as_deref(),
            Some("httpx[http2]>=0.30.0")
        );
        assert_eq!(
            bump_requirement("httpx>=0.27 ; python_version < '3.13'", "0.30.0").as_deref(),
            Some("httpx>=0.30.0; python_version < '3.13'")
        );
        assert_eq!(bump_requirement(">=1,<2", "2.0.0"), None); // no name
        assert_eq!(bump_requirement("proto-shared", "1.0.0"), None); // bare, nothing to widen
        assert_eq!(
            bump_requirement("httpx~=0.27", "0.30.0").as_deref(),
            Some("httpx~=0.30.0")
        );
        assert_eq!(
            bump_requirement("httpx>=0.1,<0.3", "0.30.0").as_deref(),
            Some("httpx>=0.30.0")
        );
        assert_eq!(
            bump_requirement("httpx<0.30.0", "0.30.0").as_deref(),
            Some("httpx>=0.30.0")
        );
        assert_eq!(
            bump_requirement("httpx!=0.30.0", "0.30.0").as_deref(),
            Some("httpx>=0.30.0")
        );
        assert_eq!(
            bump_requirement("httpx>0.27", "0.30.0").as_deref(),
            Some("httpx>=0.30.0")
        );
    }

    #[test]
    fn rewrites_across_all_pep621_sections_preserving_layout() {
        let (_dir, manifest) = write_manifest(
            "[project]\ndependencies = [\n  # keep me\n  \"httpx>=0.27\",\n  \"protobuf==6.0\",\n]\n\n[project.optional-dependencies]\ngrpc = [\"httpx>=0.27\"]\n\n[dependency-groups]\ndev = [\"ruff>=0.1\", \"httpx>=0.27\"]\n",
        );

        let changed = widen_constraint(&manifest, "httpx", "0.30.0").expect("widen");
        assert!(changed);
        let after = std::fs::read_to_string(&manifest).expect("read");
        assert_eq!(
            after.matches("httpx>=0.30.0").count(),
            3,
            "all three entries widened: {after}"
        );
        assert!(after.contains("# keep me"), "array comment kept: {after}");
        assert!(
            after.contains("protobuf==6.0"),
            "other deps untouched: {after}"
        );
        assert!(after.contains("ruff>=0.1"), "other deps untouched: {after}");
    }

    #[test]
    fn unconstrained_or_absent_dependency_reports_no_change() {
        let (_dir, manifest) =
            write_manifest("[project]\ndependencies = [\"proto-shared\", \"httpx>=0.27\"]\n");
        // A bare workspace dep has no specifier to widen.
        assert!(!widen_constraint(&manifest, "proto-shared", "1.0.0").expect("widen"));
        // A dependency declared nowhere here.
        assert!(!widen_constraint(&manifest, "tokio", "1.0.0").expect("widen"));
    }
}
