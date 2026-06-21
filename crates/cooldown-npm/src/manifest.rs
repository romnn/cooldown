//! Reading and rewriting `package.json` dependency declarations.
//!
//! A dependency is "direct" exactly when the project declares it, regardless of which package
//! manager produced the lock, so the read side is the package-manager-agnostic source of truth for
//! the direct/transitive split. The write side widens the declaring manifest's version range before
//! the adapter asks the package manager to refresh the lockfile, which keeps workspace-member
//! mutations explicit instead of relying on root-scoped `add` commands.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, MemberRef, Result};
use semver::Version;
use serde_json::Value;
use std::collections::{BTreeSet, HashSet};

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

/// The manifests that may declare a dependency change, as project-root-relative paths.
///
/// The root manifest is always included for legacy locks without workspace attribution and for root
/// importers (`.`). Member manifests are then appended in attribution order, deduplicated.
#[must_use]
pub fn manifest_rels(members: &[MemberRef]) -> Vec<Utf8PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    push_manifest_rel(&mut out, &mut seen, Utf8PathBuf::from("package.json"));
    for member in members {
        let rel = if member.path.is_empty() || member.path == "." {
            Utf8PathBuf::from("package.json")
        } else {
            Utf8Path::new(&member.path).join("package.json")
        };
        push_manifest_rel(&mut out, &mut seen, rel);
    }
    out
}

fn push_manifest_rel(
    out: &mut Vec<Utf8PathBuf>,
    seen: &mut BTreeSet<Utf8PathBuf>,
    rel: Utf8PathBuf,
) {
    if seen.insert(rel.clone()) {
        out.push(rel);
    }
}

/// The package manifests rewritten for one change.
#[derive(Debug, Default)]
pub struct ManifestRewrite {
    /// Project-root-relative paths of the manifests that were modified.
    pub modified: Vec<Utf8PathBuf>,
}

/// Widen `name` in every declaring `package.json` so the manifest admits `target`.
///
/// Returns an empty write set when the dependency is not declared in any candidate manifest, so the
/// caller can skip the change rather than accidentally adding a new root dependency.
pub fn widen_constraints(
    root: &Utf8Path,
    members: &[MemberRef],
    name: &str,
    target: &str,
) -> Result<ManifestRewrite> {
    let mut rewrite = ManifestRewrite::default();
    for rel in manifest_rels(members) {
        let abs = root.join(&rel);
        if widen_manifest(&abs, name, target)? {
            rewrite.modified.push(rel);
        }
    }
    Ok(rewrite)
}

fn widen_manifest(manifest: &Utf8Path, name: &str, target: &str) -> Result<bool> {
    let content = match std::fs::read_to_string(manifest) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let doc: Value =
        serde_json::from_str(&content).map_err(|e| CoreError::Parse(format!("{manifest}: {e}")))?;
    let mut rewritten = content;
    let mut changed = false;
    for field in DEPENDENCY_FIELDS {
        let Some(range) = doc
            .get(field)
            .and_then(|section| section.get(name))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let next = bump_range(range, target);
        if next == range {
            continue;
        }
        rewritten =
            replace_declared_range(&rewritten, field, name, range, &next).ok_or_else(|| {
                CoreError::Parse(format!("{manifest}: could not locate {field}.{name}"))
            })?;
        changed = true;
    }
    if changed {
        std::fs::write(manifest, rewritten)?;
    }
    Ok(changed)
}

/// Produce an npm range admitting `target`, preserving safe leading operators.
///
/// Build metadata on `target` (`1.2.3+build` → `1.2.3`) is stripped first: npm's semver ignores it in
/// range matching, so carrying it into the declared range would be meaningless noise. A prerelease
/// segment (`-rc1`) is kept — unlike build metadata, it is significant to a range.
fn bump_range(old: &str, target: &str) -> String {
    let target = target.split_once('+').map_or(target, |(base, _)| base);
    let trimmed = old.trim();
    if trimmed.is_empty()
        || trimmed.contains("||")
        || trimmed.contains(" - ")
        || trimmed.contains(',')
        || trimmed.contains('*')
        || trimmed.contains('x')
        || trimmed.contains('X')
        || trimmed.contains(char::is_whitespace)
    {
        return format!("^{target}");
    }
    if trimmed.starts_with('<') || trimmed.starts_with("!=") {
        return format!("^{target}");
    }
    if trimmed.starts_with('>') {
        return format!(">={target}");
    }
    for op in ["^", "~", "="] {
        if trimmed.starts_with(op) {
            return format!("{op}{target}");
        }
    }
    if Version::parse(trimmed).is_ok() {
        target.to_string()
    } else {
        format!("^{target}")
    }
}

fn replace_declared_range(
    content: &str,
    field: &str,
    name: &str,
    old: &str,
    new: &str,
) -> Option<String> {
    let field_key = serde_json::to_string(field).ok()?;
    let name_key = serde_json::to_string(name).ok()?;
    let old_value = serde_json::to_string(old).ok()?;
    let new_value = serde_json::to_string(new).ok()?;
    let object_start = find_top_level_object_for_key(content, &field_key)?;
    let object_end = find_matching_brace(content, object_start)?;
    let section = content.get(object_start + 1..object_end)?;
    let (value_start, value_end) = find_string_value_for_key(section, &name_key, &old_value)?;
    let value_start = object_start + 1 + value_start;
    let value_end = object_start + 1 + value_end;
    let mut out = content.to_string();
    out.replace_range(value_start..value_end, &new_value);
    Some(out)
}

fn find_top_level_object_for_key(content: &str, key: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes.get(index).copied()?;
        match byte {
            b'"' => {
                let end = scan_string_end(bytes, index)?;
                if depth == 1 && content.get(index..end) == Some(key) {
                    let colon = skip_ws(bytes, end);
                    if bytes.get(colon) == Some(&b':') {
                        let value = skip_ws(bytes, colon + 1);
                        if bytes.get(value) == Some(&b'{') {
                            return Some(value);
                        }
                    }
                }
                index = end;
            }
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

fn find_string_value_for_key(section: &str, key: &str, value: &str) -> Option<(usize, usize)> {
    let bytes = section.as_bytes();
    let mut depth = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes.get(index).copied()?;
        match byte {
            b'"' => {
                let end = scan_string_end(bytes, index)?;
                if depth == 0 && section.get(index..end) == Some(key) {
                    let colon = skip_ws(bytes, end);
                    if bytes.get(colon) == Some(&b':') {
                        let value_start = skip_ws(bytes, colon + 1);
                        let value_end = scan_string_end(bytes, value_start)?;
                        if section.get(value_start..value_end) == Some(value) {
                            return Some((value_start, value_end));
                        }
                    }
                }
                index = end;
            }
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

fn find_matching_brace(content: &str, open: usize) -> Option<usize> {
    let bytes = content.as_bytes();
    if bytes.get(open) != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut index = open;
    while index < bytes.len() {
        let byte = bytes.get(index).copied()?;
        match byte {
            b'"' => index = scan_string_end(bytes, index)?,
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

fn scan_string_end(bytes: &[u8], start: usize) -> Option<usize> {
    if bytes.get(start) != Some(&b'"') {
        return None;
    }
    let mut escaped = false;
    let mut index = start + 1;
    while index < bytes.len() {
        let byte = bytes.get(index).copied()?;
        match byte {
            b'\\' if !escaped => escaped = true,
            b'"' if !escaped => return Some(index + 1),
            _ => escaped = false,
        }
        index += 1;
    }
    None
}

fn skip_ws(bytes: &[u8], mut index: usize) -> usize {
    while bytes
        .get(index)
        .is_some_and(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t'))
    {
        index += 1;
    }
    index
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
        assert_eq!(
            declared_range(&path, "nanoid").expect("read").as_deref(),
            Some("^3.0.0")
        );
        assert_eq!(
            declared_range(&path, "vitest").expect("read").as_deref(),
            Some("~1.2.0")
        );
        assert_eq!(declared_range(&path, "absent").expect("read"), None);
    }

    #[test]
    fn declared_range_on_missing_manifest_is_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("nope.json")).expect("utf8 path");
        assert_eq!(declared_range(&path, "nanoid").expect("read"), None);
    }

    fn member(name: &str, path: &str) -> MemberRef {
        MemberRef {
            name: name.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn bump_range_preserves_safe_operator_family() {
        assert_eq!(bump_range("^3.0.0", "5.0.0"), "^5.0.0");
        assert_eq!(bump_range("~3.0.0", "3.3.0"), "~3.3.0");
        assert_eq!(bump_range(">=3.0.0", "3.3.0"), ">=3.3.0");
        assert_eq!(bump_range(">3.0.0", "3.3.0"), ">=3.3.0");
        assert_eq!(bump_range("3.0.0", "3.3.0"), "3.3.0");
        assert_eq!(bump_range("<4.0.0", "5.0.0"), "^5.0.0");
        assert_eq!(bump_range(">=3 <4", "5.0.0"), "^5.0.0");
    }

    #[test]
    fn bump_range_strips_build_metadata_from_the_target() {
        // npm's semver ignores build metadata in range matching, so a resolved `1.2.3+build` must not
        // leak into the declared range — across every operator family. A prerelease is preserved.
        assert_eq!(bump_range("^3.0.0", "5.0.0+build.7"), "^5.0.0");
        assert_eq!(bump_range("~3.0.0", "3.3.0+build.7"), "~3.3.0");
        assert_eq!(bump_range(">=3.0.0", "3.3.0+build.7"), ">=3.3.0");
        assert_eq!(bump_range(">3.0.0", "3.3.0+build.7"), ">=3.3.0");
        assert_eq!(bump_range("3.0.0", "3.3.0+build.7"), "3.3.0");
        assert_eq!(bump_range("<4.0.0", "5.0.0+build.7"), "^5.0.0");
        assert_eq!(bump_range("3.0.0", "2.0.0-rc1+build.5"), "2.0.0-rc1");
    }

    #[test]
    fn widen_constraints_rewrites_declaring_members_without_reformatting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 root");
        std::fs::create_dir_all(root.join("apps/a")).expect("mkdir a");
        std::fs::create_dir_all(root.join("apps/b")).expect("mkdir b");
        std::fs::write(root.join("package.json"), "{\n  \"name\": \"root\"\n}\n")
            .expect("root manifest");
        std::fs::write(
            root.join("apps/a/package.json"),
            "{\n  \"name\": \"a\",\n  \"scripts\": { \"show\": \"nanoid@^3.0.0\" },\n  \"dependencies\": { \"nanoid\": \"^3.0.0\", \"left-pad\": \"~1.0.0\" }\n}\n",
        )
        .expect("manifest a");
        std::fs::write(
            root.join("apps/b/package.json"),
            "{\n  \"name\": \"b\",\n  \"devDependencies\": {\n    \"nanoid\" : \"<4.0.0\"\n  }\n}\n",
        )
        .expect("manifest b");

        let rewrite = widen_constraints(
            &root,
            &[member("a", "apps/a"), member("b", "apps/b")],
            "nanoid",
            "5.0.0",
        )
        .expect("widen");

        assert_eq!(
            rewrite.modified,
            vec![
                Utf8PathBuf::from("apps/a/package.json"),
                Utf8PathBuf::from("apps/b/package.json")
            ]
        );
        let a = std::fs::read_to_string(root.join("apps/a/package.json")).expect("read a");
        assert!(a.contains("\"nanoid\": \"^5.0.0\""), "{a}");
        assert!(a.contains("\"show\": \"nanoid@^3.0.0\""), "{a}");
        assert!(a.contains("\"left-pad\": \"~1.0.0\""), "{a}");
        let b = std::fs::read_to_string(root.join("apps/b/package.json")).expect("read b");
        assert!(b.contains("\"nanoid\" : \"^5.0.0\""), "{b}");
        let root_after = std::fs::read_to_string(root.join("package.json")).expect("read root");
        assert_eq!(root_after, "{\n  \"name\": \"root\"\n}\n");
    }
}
