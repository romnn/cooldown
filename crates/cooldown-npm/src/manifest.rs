//! Reading and rewriting `package.json` dependency declarations.
//!
//! A dependency is "direct" exactly when the project declares it, regardless of which package
//! manager produced the lock, so the read side is the package-manager-agnostic source of truth for
//! the direct/transitive split. The write side widens the declaring manifest's version range before
//! the adapter asks the package manager to refresh the lockfile, which keeps workspace-member
//! mutations explicit instead of relying on root-scoped `add` commands.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, MemberRef, Result, RewriteMode};
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

/// The manifest fields a widen may rewrite: what the package *installs*.
///
/// `peerDependencies` is deliberately absent. That field is not a consumption declaration but a
/// contract the package publishes to its consumers, and [`bump_range`] *shifts* ranges rather than
/// only loosening them (`>=5.6.0 <5.6.2` → `^5.6.2`, `^18` → `^19`), which drops consumers the
/// author still supports — a breaking change to the package's own API, made as a side effect of
/// moving a lockfile pin. A local peer contract that excludes a target holds the move instead
/// (`partition_peer_held`), so the author edits that range deliberately; keeping the field
/// unwritten is also what lets the pre-apply peer snapshot stay valid for post-resolve
/// verification, since nothing in the run can move the contract underneath it.
const WIDENABLE_FIELDS: [&str; 3] = ["dependencies", "devDependencies", "optionalDependencies"];

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
        Err(e) => return Err(CoreError::Filesystem(format!("{manifest}: {e}"))),
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

/// Which declarations name a dependency and where npm must run a member-owned exact pin.
///
/// The apply distinguishes "nobody declares this" (skip it) from "declared, but only in a field
/// cooldown may not write" (move the lock while restoring every manifest to its authorized bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declarations {
    /// Where candidate manifests declare it in install fields.
    pub install: Option<InstallScope>,
    /// Some candidate manifest declares it in a published-contract field.
    pub peer: bool,
}

/// The npm command scope that reaches declarations the root does not install itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallScope {
    /// The root manifest has an install declaration, so npm's default scope reaches it.
    Root,
    /// Only workspace members install the dependency.
    Workspaces(Vec<String>),
}

impl Declarations {
    /// No manifest declares the dependency at all: the caller has nothing to widen and nothing to
    /// preserve, so the change is not eligible (rewriting would add a spurious root dependency).
    #[must_use]
    pub const fn absent(&self) -> bool {
        self.install.is_none() && !self.peer
    }

    /// Whether at least one candidate manifest has an install declaration.
    #[must_use]
    pub const fn has_install(&self) -> bool {
        self.install.is_some()
    }

    /// Workspace paths needed to scope npm's exact pin, or an empty slice for root and peer-only
    /// declarations.
    #[must_use]
    pub fn install_workspaces(&self) -> &[String] {
        match &self.install {
            Some(InstallScope::Workspaces(paths)) => paths,
            Some(InstallScope::Root) | None => &[],
        }
    }

    fn record_install(&mut self, rel: &Utf8Path) {
        if rel == Utf8Path::new("package.json") {
            self.install = Some(InstallScope::Root);
            return;
        }
        if matches!(&self.install, Some(InstallScope::Root)) {
            return;
        }
        let Some(parent) = rel.parent().filter(|path| !path.as_str().is_empty()) else {
            return;
        };
        match &mut self.install {
            Some(InstallScope::Workspaces(paths)) => paths.push(parent.as_str().to_string()),
            Some(InstallScope::Root) => {}
            None => {
                self.install = Some(InstallScope::Workspaces(vec![parent.as_str().to_string()]));
            }
        }
    }
}

/// Classifies how the manifests that could own `change` declare `name` (see [`Declarations`]).
///
/// # Errors
///
/// Returns a [`CoreError`] if a candidate manifest exists but cannot be read or parsed.
pub fn declarations(root: &Utf8Path, members: &[MemberRef], name: &str) -> Result<Declarations> {
    let mut out = Declarations {
        install: None,
        peer: false,
    };
    for rel in manifest_rels(members) {
        let abs = root.join(&rel);
        let content = match std::fs::read_to_string(&abs) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        let doc: Value =
            serde_json::from_str(&content).map_err(|e| CoreError::Parse(format!("{abs}: {e}")))?;
        let declares = |field: &str| {
            doc.get(field)
                .and_then(|section| section.get(name))
                .and_then(Value::as_str)
                .is_some()
        };
        let install = WIDENABLE_FIELDS.iter().copied().any(declares);
        if install {
            out.record_install(&rel);
        }
        out.peer |= declares("peerDependencies");
    }
    Ok(out)
}

/// The `package.json` of the workspace member at `path` (`.` or empty for the root).
#[must_use]
pub fn member_manifest(root: &Utf8Path, path: &str) -> Utf8PathBuf {
    if path.is_empty() || path == "." {
        root.join("package.json")
    } else {
        root.join(path).join("package.json")
    }
}

/// How one member manifest declares a package: the fields naming it, and whether the package is
/// private.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberDeclaration {
    /// The manifest fields naming the package, in [`DEPENDENCY_FIELDS`] order.
    pub fields: Vec<&'static str>,
    /// The manifest's `private: true` — an application nobody consumes, whose `peerDependencies`
    /// publish a contract to no one.
    pub private: bool,
}

impl MemberDeclaration {
    /// Whether the package is declared only as a published peer contract.
    /// pnpm auto-installs such a peer and records it in the importer's lock entry, but
    /// `pnpm update` has no install field to advance, so the importer's copy never moves.
    #[must_use]
    pub fn is_peer_only(&self) -> bool {
        self.fields == ["peerDependencies"]
    }
}

/// Reads how the member at `path` declares `name`: `None` when the manifest is absent or does not
/// declare it in any field.
///
/// # Errors
///
/// Returns a [`CoreError`] if the manifest exists but cannot be read or is not valid JSON.
pub fn member_declaration(
    root: &Utf8Path,
    path: &str,
    name: &str,
) -> Result<Option<MemberDeclaration>> {
    let manifest = member_manifest(root, path);
    let content = match std::fs::read_to_string(&manifest) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(CoreError::Filesystem(format!("{manifest}: {e}"))),
    };
    let doc: Value =
        serde_json::from_str(&content).map_err(|e| CoreError::Parse(format!("{manifest}: {e}")))?;
    let fields: Vec<&'static str> = DEPENDENCY_FIELDS
        .iter()
        .copied()
        .filter(|field| {
            doc.get(field)
                .and_then(|section| section.get(name))
                .and_then(Value::as_str)
                .is_some()
        })
        .collect();
    if fields.is_empty() {
        return Ok(None);
    }
    Ok(Some(MemberDeclaration {
        fields,
        private: doc.get("private").and_then(Value::as_bool).unwrap_or(false),
    }))
}

/// Finds the most restrictive explicit upper bound among the manifests declaring this resolved
/// dependency line.
///
/// # Errors
///
/// Returns a [`CoreError`] if a candidate manifest cannot be read or parsed.
pub fn declared_bound(
    root: &Utf8Path,
    members: &[MemberRef],
    name: &str,
) -> Result<Option<String>> {
    let manifests: BTreeSet<Utf8PathBuf> = if members.is_empty() {
        BTreeSet::from([member_manifest(root, ".")])
    } else {
        members
            .iter()
            .map(|member| member_manifest(root, &member.path))
            .collect()
    };
    let mut ranges = Vec::new();
    for manifest in manifests {
        if let Some(range) = declared_range(&manifest, name)? {
            ranges.push(range);
        }
    }
    Ok(crate::version::most_restrictive_declared_bound(ranges))
}

/// The manifests that may declare a dependency change, as project-root-relative paths.
///
/// With member attribution these are exactly the declaring members' manifests, in attribution
/// order and deduplicated, the root's among them only when the root importer (`.`) is one of them:
/// a run that excludes or does not select the root must never rewrite its manifest, so the root is
/// not a fallback owner.
/// Without attribution (a legacy lock, a single-package project) the root manifest is the only
/// candidate.
#[must_use]
pub fn manifest_rels(members: &[MemberRef]) -> Vec<Utf8PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    if members.is_empty() {
        push_manifest_rel(&mut out, &mut seen, Utf8PathBuf::from("package.json"));
    }
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

/// Rewrites `name` in each declaring `package.json` according to `mode`.
///
/// [`RewriteMode::Auto`] preserves declarations that already admit `target` and widens only the
/// incompatible install declarations. [`RewriteMode::Always`] rewrites every install declaration.
///
/// An empty write set means no install declaration was authorized to change: the target may already
/// be compatible, the dependency may be peer-only, or it may be undeclared. Callers use
/// [`declarations`] to distinguish those eligibility states.
pub fn widen_constraints(
    root: &Utf8Path,
    members: &[MemberRef],
    name: &str,
    target: &str,
    mode: RewriteMode,
) -> Result<ManifestRewrite> {
    let mut rewrite = ManifestRewrite::default();
    for rel in manifest_rels(members) {
        let abs = root.join(&rel);
        if widen_manifest(&abs, name, target, mode)? {
            rewrite.modified.push(rel);
        }
    }
    Ok(rewrite)
}

fn widen_manifest(
    manifest: &Utf8Path,
    name: &str,
    target: &str,
    mode: RewriteMode,
) -> Result<bool> {
    let content = match std::fs::read_to_string(manifest) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let doc: Value =
        serde_json::from_str(&content).map_err(|e| CoreError::Parse(format!("{manifest}: {e}")))?;
    let mut rewritten = content;
    let mut changed = false;
    for field in WIDENABLE_FIELDS {
        let Some(range) = doc
            .get(field)
            .and_then(|section| section.get(name))
            .and_then(Value::as_str)
        else {
            continue;
        };
        // A protocol-form specifier (`catalog:`, `workspace:^`, `npm:pkg@^1`, `file:…`, `git+…`)
        // is not a semver range: rewriting it to `^target` would sever the indirection it encodes
        // (a pnpm catalog pin, a workspace link, an alias) instead of widening a constraint —
        // permanently, since a widen is a committed edit. This holds in every mode: even
        // `Always` may only reshape ranges, never replace a protocol reference. The resolve and
        // its verification still report whether the target landed.
        if range.contains(':') {
            continue;
        }
        if mode == RewriteMode::Auto && crate::version::version_in_range(range, target) {
            continue;
        }
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

/// Replace the string value of `name` within the top-level `field` object, returning the rewritten
/// document or `None` when the `field`→`name`→`old` path is not present verbatim.
///
/// The edit is byte-targeted: it locates the value by walking to the named top-level object, then to
/// the key *inside* that object, and rewrites only the value span. So a key that happens to equal the
/// value (a bare-specifier import), or an identical string living in another top-level object (a Deno
/// `scopes` entry), is never mistaken for the value — and the rest of the file stays byte-identical.
pub(crate) fn replace_declared_range(
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
    let span = find_string_value_for_key(section, &name_key, &old_value)?;
    let value_start = object_start + 1 + span.start;
    let value_end = object_start + 1 + span.end;
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

/// The byte span of a quoted JSON string value within the searched section, quotes included.
struct StringValueSpan {
    /// Byte offset of the value's opening quote.
    start: usize,
    /// Byte offset one past the value's closing quote.
    end: usize,
}

fn find_string_value_for_key(section: &str, key: &str, value: &str) -> Option<StringValueSpan> {
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
                            return Some(StringValueSpan {
                                start: value_start,
                                end: value_end,
                            });
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

    /// A `package.json` written into a temporary directory.
    struct ManifestFixture {
        /// Owns the temporary directory; dropping it deletes the manifest, so it must stay bound
        /// for as long as the path is read.
        guard: tempfile::TempDir,
        /// The path of the `package.json` inside the temporary directory.
        path: Utf8PathBuf,
    }

    fn manifest(contents: &str) -> ManifestFixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = Utf8PathBuf::from_path_buf(dir.path().join("package.json")).expect("utf8 path");
        std::fs::write(&path, contents).expect("write");
        ManifestFixture { guard: dir, path }
    }

    #[test]
    fn declared_range_finds_across_fields_and_reports_absence() {
        let ManifestFixture {
            guard: _guard,
            path,
        } = manifest(
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
    fn declared_bound_uses_the_strictest_declaring_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 root");
        std::fs::create_dir_all(root.join("apps/a")).expect("mkdir a");
        std::fs::create_dir_all(root.join("apps/b")).expect("mkdir b");
        std::fs::write(
            root.join("apps/a/package.json"),
            r#"{ "dependencies": { "typescript": ">=5 <7" } }"#,
        )
        .expect("write a");
        std::fs::write(
            root.join("apps/b/package.json"),
            r#"{ "dependencies": { "typescript": ">=5 <6" } }"#,
        )
        .expect("write b");

        let bound = declared_bound(
            &root,
            &[member("a", "apps/a"), member("b", "apps/b")],
            "typescript",
        )
        .expect("read bounds");

        assert_eq!(bound.as_deref(), Some(">=5 <6"));
    }

    /// A widen rewrites what the package installs and nothing else. `peerDependencies` states what
    /// the package requires *of its consumers*, and [`bump_range`] shifts rather than only loosens,
    /// so rewriting it would drop supported consumers as a side effect of moving a lock pin.
    #[test]
    fn widen_leaves_published_peer_contracts_untouched() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        std::fs::write(
            root.join("package.json"),
            indoc::indoc! {r#"{
                "dependencies": { "chalk": "^5.6.0" },
                "devDependencies": { "vitest": "~1.2.0" },
                "optionalDependencies": { "fsevents": "^2.3.0" },
                "peerDependencies": { "chalk": ">=5.6.0 <5.6.2", "react": "^18.0.0" }
            }"#},
        )
        .expect("manifest");

        for (name, target) in [("chalk", "5.6.2"), ("react", "19.0.0")] {
            widen_constraints(&root, &[], name, target, RewriteMode::Always).expect("widen");
        }
        let after = std::fs::read_to_string(root.join("package.json")).expect("read");

        assert!(
            after.contains(r#""chalk": "^5.6.2""#),
            "the install declaration widens: {after}"
        );
        assert!(
            after.contains(r#""chalk": ">=5.6.0 <5.6.2""#),
            "the peer contract on the same package is untouched: {after}"
        );
        assert!(
            after.contains(r#""react": "^18.0.0""#),
            "a peer-only contract is untouched even with no install declaration: {after}"
        );
    }

    /// A protocol-form specifier is a reference, not a range: rewriting `catalog:` to `^19.2.0`
    /// would permanently sever the workspace-catalog indirection (and likewise a `workspace:` link
    /// or `npm:` alias) — in every mode, since a widen is a committed edit.
    #[test]
    fn widen_never_rewrites_protocol_specifiers() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 path");
        let before = indoc::indoc! {r#"{
            "dependencies": {
                "react": "catalog:",
                "vue": "catalog:vue3",
                "shared": "workspace:^",
                "renamed": "npm:actual@^1.0.0",
                "vendored": "file:../vendor/pkg",
                "chalk": "^4.0.0"
            }
        }"#};
        std::fs::write(root.join("package.json"), before).expect("manifest");

        for mode in [RewriteMode::Auto, RewriteMode::Always] {
            for name in ["react", "vue", "shared", "renamed", "vendored"] {
                let rewrite = widen_constraints(&root, &[], name, "99.0.0", mode).expect("widen");
                assert!(
                    rewrite.modified.is_empty(),
                    "{name} under {mode:?} must stay a protocol reference"
                );
            }
        }
        let after = std::fs::read_to_string(root.join("package.json")).expect("read");
        assert_eq!(after, before, "protocol specifiers survive byte-exactly");

        let rewrite =
            widen_constraints(&root, &[], "chalk", "5.6.2", RewriteMode::Auto).expect("widen");
        assert!(
            !rewrite.modified.is_empty(),
            "a plain semver range beside them still widens"
        );
    }

    /// Automatic widening preserves every compatible declaration even when another workspace
    /// member must cross its own range boundary.
    #[test]
    fn auto_widen_changes_only_incompatible_workspace_declarations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf8 root");
        std::fs::create_dir_all(root.join("apps/a")).expect("mkdir a");
        let root_manifest = indoc::indoc! {r#"{
            "dependencies": { "chalk": ">=5 <7" }
        }"#};
        std::fs::write(root.join("package.json"), root_manifest).expect("root manifest");
        std::fs::write(
            root.join("apps/a/package.json"),
            r#"{ "dependencies": { "chalk": "^5.6.0" } }"#,
        )
        .expect("member manifest");

        let rewrite = widen_constraints(
            &root,
            &[member("a", "apps/a")],
            "chalk",
            "6.0.0",
            RewriteMode::Auto,
        )
        .expect("widen");

        assert_eq!(rewrite.modified, [Utf8PathBuf::from("apps/a/package.json")]);
        assert_eq!(
            std::fs::read_to_string(root.join("package.json")).expect("read root"),
            root_manifest,
            "the root already admits the target and remains byte-identical"
        );
        assert!(
            std::fs::read_to_string(root.join("apps/a/package.json"))
                .expect("read member")
                .contains(r#""chalk": "^6.0.0""#),
            "the incompatible member crosses to the authorized target range"
        );
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

    /// The root manifest is an owner only when the root importer is attributed: with members
    /// present it is exactly the members' manifests, without any it is the root alone.
    #[test]
    fn manifest_rels_include_the_root_only_when_attributed() {
        let member = |path: &str| MemberRef {
            name: path.to_string(),
            path: path.to_string(),
        };
        assert_eq!(manifest_rels(&[]), vec![Utf8PathBuf::from("package.json")]);
        assert_eq!(
            manifest_rels(&[member("apps/a")]),
            vec![Utf8PathBuf::from("apps/a/package.json")]
        );
        assert_eq!(
            manifest_rels(&[member("."), member("apps/a"), member("apps/a")]),
            vec![
                Utf8PathBuf::from("package.json"),
                Utf8PathBuf::from("apps/a/package.json")
            ]
        );
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
            RewriteMode::Always,
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
