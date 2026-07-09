//! Format-preserving version-constraint rewrites for Cargo manifests.
//!
//! `cargo update --precise` can only move the lock *within* a manifest's existing requirement, so a
//! cross-major bump (past a caret range) or any move outside the declared constraint needs the
//! `Cargo.toml` itself rewritten. This module finds every manifest entry that declares a crate's
//! requirement — a member's `[dependencies]`/`[dev-dependencies]`/`[build-dependencies]` entries
//! (and the `[target.<cfg>.*]` variants), or, when the member inherits with `workspace = true`, the
//! root `[workspace.dependencies]` entry — and rewrites just those requirements via `toml_edit`,
//! leaving comments, key order, and every other field of each entry untouched. Every section is
//! visited, not just the first that declares the crate: a member can declare one crate twice (e.g.
//! `[dependencies] toml = "1"` beside `[build-dependencies] toml = "0.5"`), and an untouched second
//! entry would keep the old-major line in the lock no matter how the first is widened.

use camino::{Utf8Path, Utf8PathBuf};
use cooldown_core::{CoreError, MemberRef};
use cooldown_toml_util::{parse_document, write_document};
use std::collections::BTreeSet;
use toml_edit::{DocumentMut, Item, TableLike};

/// The manifests a single rewrite touched, relative to the workspace root — used to journal the
/// write set for rollback and to tell the caller whether anything was actually editable.
#[derive(Debug, Default)]
pub struct ManifestRewrite {
    /// Project-root-relative paths of the manifests that were modified.
    pub modified: Vec<Utf8PathBuf>,
}

/// Widen the requirement on `crate_name` so it admits `target`, across every member manifest that
/// declares it, redirecting an inherited (`workspace = true`) entry to the root
/// `[workspace.dependencies]`. When attribution gave no members (or none declared it directly), the
/// root manifest is the best-effort fallback.
///
/// Returns the modified manifest paths. An empty result means no editable requirement was found —
/// the crate is pulled in only transitively, or via a path/git source with no version — so the
/// caller should report the change as not-applied rather than re-locking.
///
/// # Errors
///
/// Returns a [`CoreError`] if a manifest exists but cannot be read, parsed, or written back.
pub fn widen_constraint(
    root: &Utf8Path,
    members: &[MemberRef],
    crate_name: &str,
    target: &str,
) -> Result<ManifestRewrite, CoreError> {
    let mut rewrite = ManifestRewrite::default();
    let mut needs_workspace = false;
    let mut seen: BTreeSet<Utf8PathBuf> = BTreeSet::new();

    for member in members {
        let rel = member_manifest_rel(&member.path);
        if !seen.insert(rel.clone()) {
            continue;
        }
        let abs = root.join(&rel);
        let Some(mut doc) = parse_document(&abs)? else {
            continue;
        };
        let edits = rewrite_member(&mut doc, crate_name, target);
        if edits.edited {
            write_document(&abs, &doc)?;
            rewrite.modified.push(rel);
        }
        needs_workspace |= edits.inherited;
    }

    // An inherited entry lives in the root `[workspace.dependencies]`; the members-empty / nothing-
    // found case falls back to the root manifest (workspace table first, then its own dep sections).
    if needs_workspace || rewrite.modified.is_empty() {
        let rel = Utf8PathBuf::from("Cargo.toml");
        let abs = root.join(&rel);
        if let Some(mut doc) = parse_document(&abs)? {
            let changed = if needs_workspace {
                rewrite_workspace(&mut doc, crate_name, target)
            } else {
                rewrite_workspace(&mut doc, crate_name, target)
                    || rewrite_member(&mut doc, crate_name, target).edited
            };
            if changed && !rewrite.modified.iter().any(|path| path == &rel) {
                write_document(&abs, &doc)?;
                rewrite.modified.push(rel);
            }
        }
    }

    Ok(rewrite)
}

/// The project-root-relative path of a member's `Cargo.toml` (`.` is the root crate).
pub(crate) fn member_manifest_rel(member_path: &str) -> Utf8PathBuf {
    if member_path.is_empty() || member_path == "." {
        Utf8PathBuf::from("Cargo.toml")
    } else {
        Utf8Path::new(member_path).join("Cargo.toml")
    }
}

/// What happened when looking for a crate's requirement in one manifest.
enum Edit {
    /// The crate is not declared in any of this manifest's dependency sections.
    NotFound,
    /// The crate inherits from the workspace (`crate = { workspace = true }`).
    Inherited,
    /// The crate is declared but carries no version requirement (a path/git source).
    NoVersion,
    /// The requirement was rewritten in place.
    Done,
}

/// What rewriting one member manifest changed, aggregated across all of its dependency sections.
#[derive(Default)]
struct MemberEdits {
    /// At least one requirement was rewritten in place.
    edited: bool,
    /// At least one entry inherits from the workspace (`crate = { workspace = true }`).
    inherited: bool,
}

/// Rewrite the crate's requirement in **every** dependency section of a member manifest that
/// declares it. Stopping at the first hit is not enough: a crate declared twice (`[dependencies]
/// toml = "1"` beside `[build-dependencies] toml = "0.5"`) keeps its old-major lock line alive
/// through the second, untouched entry, so the planned move can never complete.
fn rewrite_member(doc: &mut DocumentMut, crate_name: &str, target: &str) -> MemberEdits {
    let mut edits = MemberEdits::default();
    for section in dependency_section_paths(doc) {
        let keys: Vec<&str> = section.iter().map(String::as_str).collect();
        match rewrite_entry(doc, &keys, crate_name, target) {
            Edit::Done => edits.edited = true,
            Edit::Inherited => edits.inherited = true,
            Edit::NoVersion | Edit::NotFound => {}
        }
    }
    edits
}

/// Rewrite a crate's requirement in the root `[workspace.dependencies]` table, if present.
fn rewrite_workspace(doc: &mut DocumentMut, crate_name: &str, target: &str) -> bool {
    matches!(
        rewrite_entry(doc, &["workspace", "dependencies"], crate_name, target),
        Edit::Done
    )
}

/// The dotted key paths of every dependency table in a manifest, including per-target sections.
fn dependency_section_paths(doc: &DocumentMut) -> Vec<Vec<String>> {
    let kinds = ["dependencies", "dev-dependencies", "build-dependencies"];
    let mut paths: Vec<Vec<String>> = kinds.iter().map(|kind| vec![(*kind).to_string()]).collect();
    if let Some(target) = doc.get("target").and_then(Item::as_table_like) {
        for (cfg, _) in target.iter() {
            for kind in kinds {
                paths.push(vec![
                    "target".to_string(),
                    cfg.to_string(),
                    kind.to_string(),
                ]);
            }
        }
    }
    paths
}

/// Rewrite `crate_name`'s requirement under the table at `section`, if it is declared there.
fn rewrite_entry(doc: &mut DocumentMut, section: &[&str], crate_name: &str, target: &str) -> Edit {
    let Some(table) = navigate_mut(doc, section) else {
        return Edit::NotFound;
    };
    let Some(item) = table.get_mut(crate_name) else {
        return Edit::NotFound;
    };
    rewrite_dep_item(item, target)
}

/// Rewrite one dependency entry, handling the bare-string form (`dep = "1"`) and the table form
/// (`dep = { version = "1", … }` / `[deps.dep]`), preserving every other field.
fn rewrite_dep_item(item: &mut Item, target: &str) -> Edit {
    if let Some(req) = item.as_str().map(str::to_owned) {
        *item = toml_edit::value(bump_req(&req, target));
        return Edit::Done;
    }
    let Some(table) = item.as_table_like_mut() else {
        return Edit::NotFound;
    };
    if table.get("workspace").and_then(Item::as_bool) == Some(true) {
        return Edit::Inherited;
    }
    if let Some(version) = table.get_mut("version")
        && let Some(req) = version.as_str().map(str::to_owned)
    {
        *version = toml_edit::value(bump_req(&req, target));
        return Edit::Done;
    }
    Edit::NoVersion
}

/// Descend `path` into a mutable table-like node, or `None` if any segment is missing or not a table.
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

/// Produce a requirement that admits `target`, preserving safe leading comparators.
///
/// Build metadata on `target` (`0.25.12+spec-1.1.0` → `0.25.12`) is stripped first: cargo ignores it
/// in a version requirement and warns on every invocation, so it must never reach the constraint. A
/// prerelease segment (`-rc1`) is kept — unlike build metadata, it is significant to a requirement.
///
/// A bare or caret requirement maps to the caret-equivalent on the target (`^1` → `^2.3.0`, `1` →
/// `2.3.0`); safe single comparators keep their operator (`>=1` → `>=2.3.0`, `~1.2` → `~2.3.0`).
/// A strict lower bound becomes inclusive (`>1` → `>=2.3.0`). A multi-comparator, wildcard,
/// upper-bound-only, or not-equal requirement is replaced with a caret on the target, the least
/// surprising default that actually admits the target. Exact `=` pins never reach here — they are
/// held and skipped before apply.
fn bump_req(old: &str, target: &str) -> String {
    let target = target.split_once('+').map_or(target, |(base, _)| base);
    let trimmed = old.trim();
    if trimmed.is_empty()
        || trimmed.contains(',')
        || trimmed.contains('*')
        || trimmed.contains('|')
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
    target.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(name: &str, path: &str) -> MemberRef {
        MemberRef {
            name: name.to_string(),
            path: path.to_string(),
        }
    }

    #[test]
    fn bump_req_preserves_operator_family() {
        assert_eq!(bump_req("1", "2.3.0"), "2.3.0");
        assert_eq!(bump_req("^1.2", "2.3.0"), "^2.3.0");
        assert_eq!(bump_req("~1.2", "2.3.0"), "~2.3.0");
        assert_eq!(bump_req(">=1.0", "2.3.0"), ">=2.3.0");
        assert_eq!(bump_req(">1.0", "2.3.0"), ">=2.3.0");
        assert_eq!(bump_req(">=1, <2", "2.3.0"), "^2.3.0");
        assert_eq!(bump_req("<2", "2.3.0"), "^2.3.0");
        assert_eq!(bump_req("<=2", "2.3.0"), "^2.3.0");
    }

    #[test]
    fn bump_req_strips_build_metadata_from_the_target() {
        // The toml ecosystem publishes versions like `0.25.12+spec-1.1.0`. Cargo ignores build
        // metadata in a requirement and warns, so it must not leak from the resolved version into the
        // rewritten constraint — across every comparator family. A prerelease segment is preserved.
        assert_eq!(bump_req("0.23", "0.25.12+spec-1.1.0"), "0.25.12");
        assert_eq!(bump_req("^0.23", "0.25.12+spec-1.1.0"), "^0.25.12");
        assert_eq!(bump_req("~0.23", "0.25.12+spec-1.1.0"), "~0.25.12");
        assert_eq!(bump_req(">=0.23", "0.25.12+spec-1.1.0"), ">=0.25.12");
        assert_eq!(bump_req(">0.23", "0.25.12+spec-1.1.0"), ">=0.25.12");
        assert_eq!(bump_req("<0.30", "0.25.12+spec-1.1.0"), "^0.25.12");
        // Prerelease is significant to a requirement and must survive the strip.
        assert_eq!(bump_req("1", "2.0.0-rc1+build.5"), "2.0.0-rc1");
    }

    #[test]
    fn rewrites_bare_string_requirement_in_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("crates/app")).expect("mkdir");
        std::fs::write(
            root.join("crates/app/Cargo.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\n# pinned for a reason\nserde = \"1\"\n",
        )
        .expect("write");

        let rewrite = widen_constraint(root, &[member("app", "crates/app")], "serde", "2.3.0")
            .expect("widen");

        assert_eq!(
            rewrite.modified,
            vec![Utf8PathBuf::from("crates/app/Cargo.toml")]
        );
        let after = std::fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read");
        assert!(after.contains("serde = \"2.3.0\""), "{after}");
        assert!(
            after.contains("# pinned for a reason"),
            "comment kept: {after}"
        );
    }

    #[test]
    fn rewrites_every_section_declaring_the_crate() {
        // A crate declared in `[dependencies]` and again in `[build-dependencies]` (rawloader's
        // `toml = "1"` beside `toml = "0.5"`) needs both entries widened: stopping at the first
        // leaves the second demanding the old major, and the stale lock line it owns can then never
        // move — while masking the failure behind the already-satisfied first entry.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("crates/app")).expect("mkdir");
        std::fs::write(
            root.join("crates/app/Cargo.toml"),
            "[package]\nname = \"app\"\n\n[dependencies]\ntoml = \"1\"\n\n[build-dependencies]\ntoml = \"0.5\"\n",
        )
        .expect("write");

        let rewrite =
            widen_constraint(root, &[member("app", "crates/app")], "toml", "1.1.2").expect("widen");

        assert_eq!(
            rewrite.modified,
            vec![Utf8PathBuf::from("crates/app/Cargo.toml")]
        );
        let after = std::fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read");
        assert!(
            !after.contains("\"0.5\""),
            "build-dependencies entry must be widened too: {after}"
        );
        assert_eq!(
            after.matches("toml = \"1.1.2\"").count(),
            2,
            "both entries land on the target: {after}"
        );
    }

    #[test]
    fn rewrites_table_version_and_keeps_features() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(
            root.join("Cargo.toml"),
            "[dependencies]\nserde = { version = \"^1.0\", features = [\"derive\"] }\n",
        )
        .expect("write");

        let rewrite =
            widen_constraint(root, &[member("root", ".")], "serde", "2.3.0").expect("widen");

        assert_eq!(rewrite.modified, vec![Utf8PathBuf::from("Cargo.toml")]);
        let after = std::fs::read_to_string(root.join("Cargo.toml")).expect("read");
        assert!(after.contains("version = \"^2.3.0\""), "{after}");
        assert!(
            after.contains("features = [\"derive\"]"),
            "features kept: {after}"
        );
    }

    #[test]
    fn inherited_member_rewrites_workspace_dependencies_not_the_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("crates/app")).expect("mkdir");
        std::fs::write(
            root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/app\"]\n\n[workspace.dependencies]\nserde = \"1\"\n",
        )
        .expect("write root");
        let member_manifest = "[package]\nname = \"app\"\n\n[dependencies]\nserde = { workspace = true, features = [\"derive\"] }\n";
        std::fs::write(root.join("crates/app/Cargo.toml"), member_manifest).expect("write member");

        let rewrite = widen_constraint(root, &[member("app", "crates/app")], "serde", "2.3.0")
            .expect("widen");

        assert_eq!(rewrite.modified, vec![Utf8PathBuf::from("Cargo.toml")]);
        let root_after = std::fs::read_to_string(root.join("Cargo.toml")).expect("read root");
        assert!(root_after.contains("serde = \"2.3.0\""), "{root_after}");
        let member_after =
            std::fs::read_to_string(root.join("crates/app/Cargo.toml")).expect("read member");
        assert_eq!(
            member_after, member_manifest,
            "inherited member is untouched"
        );
    }

    #[test]
    fn rewrites_target_gated_dependency_in_member() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::create_dir_all(root.join("crates/mcp")).expect("mkdir");
        std::fs::write(
            root.join("crates/mcp/Cargo.toml"),
            indoc::indoc! {r#"
                [package]
                name = "mcp"

                [target.'cfg(unix)'.dependencies]
                nix = { version = "0.28", features = ["signal"] }
            "#},
        )
        .expect("write");

        let rewrite =
            widen_constraint(root, &[member("mcp", "crates/mcp")], "nix", "0.31.3").expect("widen");

        assert_eq!(
            rewrite.modified,
            vec![Utf8PathBuf::from("crates/mcp/Cargo.toml")]
        );
        let after = std::fs::read_to_string(root.join("crates/mcp/Cargo.toml")).expect("read");
        assert!(after.contains(r#"version = "0.31.3""#), "{after}");
        assert!(
            after.contains(r#"features = ["signal"]"#),
            "features kept: {after}"
        );
    }

    #[test]
    fn transitive_only_dependency_is_not_editable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = Utf8Path::from_path(dir.path()).expect("utf8");
        std::fs::write(root.join("Cargo.toml"), "[dependencies]\nserde = \"1\"\n").expect("write");

        // `tokio` is declared nowhere — a transitive-only crate cannot be widened.
        let rewrite =
            widen_constraint(root, &[member("root", ".")], "tokio", "2.3.0").expect("widen");
        assert!(rewrite.modified.is_empty());
    }
}
